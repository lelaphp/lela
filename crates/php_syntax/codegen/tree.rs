//! The typed nodes emitted from the parsed schema into `OUT_DIR/tree.rs`,
//! and the display table into `OUT_DIR/display.rs` (design §4.9,
//! ADR-0003).
//!
//! One struct per struct node of `php.ungram`, its children named fields
//! whose types say what the schema says: a child is `NodeId<Child>` for a
//! struct node, the enum itself for an alternation, `TokenId` for a token,
//! wrapped in `Option` unless marked `!` (convention A2) and in a boxed
//! slice when repeated. The builder fills the fields once from the
//! parser's event stream, so a read is a field read and nothing scans; a
//! required child is a plain field, which is the `!` promise held by the
//! type rather than by an accessor that unwraps. An alternation is an
//! enum whose variants carry the index of the node they name, or the
//! token, and a mixin is a trait over the fields it injects (A5). Before
//! the fields stand the two facts every node carries whatever its kind:
//! `range`, where it is in the source, and `line`, php's
//! `zend_ast.lineno`, which the builder computes at `Finish` and hands
//! to `resolve` beside the doc comment (design §4.9, ledger 27); the
//! `Node` trait answers both.
//!
//! The slot model implements the schema's conventions: A2 arity honesty,
//! A3 labeled tokens, A4 anchored resolution for same-typed siblings, A5
//! mixins. Flattening decides which field each child becomes, and the
//! anchor assignment holds A4's precondition, that a same-typed sibling
//! sits behind a reliable anchor token, as a schema error at build time.
//! The anchors are what the resolution reads against: beside each struct
//! is its `resolve`, one read per field over the node's children, the
//! `Nodes` arena holds one vector per kind present, the dispatcher
//! `resolve` pushes a finished node into its vector by kind, and each alternation
//! enum reads one child as its variant through `Child`. The reads
//! themselves, `first`, `first_after`, `all`, `token`, `tokens` and
//! `member`, are written by hand in `src/tree/mod.rs`.
//!
//! Two cross-cutting decisions:
//!
//! - **Expressions can be bare tokens.** Token-only alternations
//!   (`NameRef`) have no node kind (A1), so a constant fetch is an `IDENT`
//!   token sitting directly in its parent, and an enum over such an
//!   alternative carries a `(TokenId)` variant.
//! - **Contextual keywords are remapped.** The parser re-kinds keyword
//!   tokens consumed as names (`Foo::list`, `enum(1)`) to `IDENT` at
//!   tree-build time, so a `<member_ident>` child matches `IDENT` first and
//!   keyword kinds only defensively.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use super::kind::{TOKEN_CLASSES, keyword_variant, punct_variant, screaming_snake, snake_case};
use super::ungram::{Def, DefClass, Error, Grammar, Rule, classify};

/// Display phrases for token classes, in the reader's terms. Cross-checked
/// against [`TOKEN_CLASSES`] at generation.
const CLASS_DISPLAY: &[(&str, &str)] = &[
    ("ident", "an identifier"),
    ("member_ident", "a member name"),
    ("variable", "a variable"),
    ("name_qualified", "a qualified name"),
    ("name_fully_qualified", "a fully qualified name"),
    ("name_relative", "a `namespace\\\\`-relative name"),
    ("int", "an integer literal"),
    ("float", "a float literal"),
    ("single_quoted", "a string literal"),
    ("string_text", "string text"),
    ("heredoc_start", "a heredoc opener"),
    ("heredoc_end", "a heredoc closing marker"),
    ("encaps_int", "an integer offset"),
    ("magic_const", "a magic constant"),
    ("cast", "a cast"),
    ("cast_removed", "a removed cast"),
    ("inline_html", "inline HTML"),
    ("halt_data", "halt-compiler data"),
    ("open_tag_echo", "`<?=`"),
];

/// Display phrases for the extras (mirrors `kind::EXTRAS` names).
const EXTRA_DISPLAY: &[(&str, &str)] = &[
    ("WHITESPACE", "whitespace"),
    ("LINE_COMMENT", "a comment"),
    ("BLOCK_COMMENT", "a comment"),
    ("DOC_COMMENT", "a doc comment"),
    ("HEREDOC_INDENT", "heredoc indentation"),
    ("OPEN_TAG", "`<?php`"),
    ("CLOSE_TAG", "`?>`"),
    ("ERROR", "invalid source"),
    ("EOF", "end of file"),
];

/// Rust keywords that occur as schema labels; emitted as raw identifiers.
const RAW_LABELS: &[&str] = &["ref", "trait", "type", "else", "use", "as", "in"];

/// The field every node opting into `HasDocComment` carries: the token
/// index of the doc comment the node took where php's grammar reaches a
/// `backup_doc_comment`, which the parser emits as an event and the
/// builder hands to `resolve` (design §4.9).
const DOC_COMMENT: &str = "doc_comment";

// ---------------------------------------------------------------------------
// Slot model
// ---------------------------------------------------------------------------

/// What a slot's field holds.
#[derive(Clone, Debug, PartialEq, Eq)]
enum SlotTy {
    /// Reference to a schema definition (struct node, enum, or token-alt).
    Ref(String),
    /// A fixed token-kind set (labeled literals/classes).
    Tokens(Vec<String>),
    /// The `<member_ident>` class: `IDENT` plus remap-defensive keywords.
    MemberIdent,
    /// Labeled alternation of node refs (generates an ad-hoc enum).
    AdHoc(Vec<String>),
}

#[derive(Clone, Debug)]
struct Slot {
    name: String,
    ty: SlotTy,
    repeated: bool,
    required: bool,
    /// Set when the slot sits in an `Opt(…)` or in an alternation branch —
    /// such token slots are unreliable as anchors.
    optional: bool,
    /// The anchor tokens the builder resolves this slot after (A4); empty
    /// when the slot's kind set collides with no earlier sibling's.
    anchors: Vec<String>,
    /// The tokens the resolution stops at, when a colliding sibling sits
    /// beyond them.
    stops: Vec<String>,
}

#[derive(Clone, Debug)]
enum FlatItem {
    /// Unlabeled quoted token: an anchor for A4 resolution, no field.
    Anchor {
        kinds: Vec<String>,
        optional: bool,
    },
    Slot(Slot),
}

struct FlattenCtx<'a> {
    def_name: &'a str,
    errors: Vec<Error>,
}

fn resolve_token(text: &str) -> Result<String, String> {
    let first = text.as_bytes().first().copied().unwrap_or(b' ');
    if first.is_ascii_alphabetic() || first == b'_' {
        Ok(keyword_variant(text))
    } else {
        punct_variant(text)
            .map(str::to_owned)
            .ok_or_else(|| format!("symbol literal `'{text}'` not in punctuation table"))
    }
}

/// The three slot modifiers the flattening recursion threads through
/// itself, as ONE value.
///
/// They were three positional `bool` parameters, which made every one of
/// the seven recursive call sites a place where two of them could be
/// swapped with no type error and no test failure — the arguments
/// `(ctx, rule, label, optional, repeated, required, out)` are read
/// positionally by a human and by nothing else. `optional`/`repeated` in
/// particular differ only in which field shape they produce, so a swap
/// survives compilation and shows up as an emitted struct that is wrong
/// in a way nothing diffs.
///
/// The `with_*` constructors are what remove the hazard rather than
/// renaming it: a recursion step now names the ONE modifier it sets and
/// inherits the rest, instead of respelling all three in order.
#[derive(Clone, Copy, Default)]
struct SlotMods {
    optional: bool,
    repeated: bool,
    required: bool,
}

impl SlotMods {
    fn with_optional(self) -> Self {
        Self {
            optional: true,
            ..self
        }
    }

    fn with_repeated(self) -> Self {
        Self {
            repeated: true,
            ..self
        }
    }

    fn with_required(self) -> Self {
        Self {
            required: true,
            ..self
        }
    }
}

fn flatten(
    ctx: &mut FlattenCtx<'_>,
    rule: &Rule,
    label: Option<&str>,
    mods: SlotMods,
    out: &mut Vec<FlatItem>,
) {
    let SlotMods {
        optional,
        repeated,
        required,
    } = mods;
    match rule {
        Rule::Token { text, line } => match resolve_token(text) {
            Ok(kind) => {
                if let Some(label) = label {
                    out.push(FlatItem::Slot(Slot {
                        name: label.to_owned(),
                        ty: SlotTy::Tokens(vec![kind]),
                        repeated,
                        required,
                        optional,
                        anchors: Vec::new(),
                        stops: Vec::new(),
                    }));
                } else {
                    out.push(FlatItem::Anchor {
                        kinds: vec![kind],
                        optional,
                    });
                }
            }
            Err(msg) => ctx.errors.push(Error::at(*line, msg)),
        },
        Rule::Class { name, .. } => {
            let ty = if name == "member_ident" {
                SlotTy::MemberIdent
            } else {
                SlotTy::Tokens(vec![name.to_ascii_uppercase()])
            };
            out.push(FlatItem::Slot(Slot {
                name: label.unwrap_or(name).to_owned(),
                ty,
                repeated,
                required,
                optional,
                anchors: Vec::new(),
                stops: Vec::new(),
            }));
        }
        Rule::Node { name, .. } => {
            let slot_name = if let Some(label) = label {
                label.to_owned()
            } else if repeated {
                format!("{}s", snake_case(name))
            } else {
                snake_case(name)
            };
            out.push(FlatItem::Slot(Slot {
                name: slot_name,
                ty: SlotTy::Ref(name.clone()),
                repeated,
                required,
                optional,
                anchors: Vec::new(),
                stops: Vec::new(),
            }));
        }
        Rule::Labeled { label: l, rule } => {
            flatten(ctx, rule, Some(l), mods, out);
        }
        Rule::Seq(items) => {
            if label.is_some() {
                ctx.errors.push(Error::global(format!(
                    "{}: labeled sequence is not supported by the typed-node emitter",
                    ctx.def_name
                )));
                return;
            }
            for item in items {
                flatten(ctx, item, None, mods, out);
            }
        }
        Rule::Alt(alts) => flatten_alt(ctx, alts, label, mods, out),
        Rule::Opt(rule) => flatten(ctx, rule, label, mods.with_optional(), out),
        Rule::Rep(rule) | Rule::CommaList { rule, .. } => {
            flatten(ctx, rule, label, mods.with_repeated(), out);
        }
        Rule::Required(rule) => flatten(ctx, rule, label, mods.with_required(), out),
    }
}

/// A labeled alternation collapses into one slot; an unlabeled one
/// contributes each branch with `optional` set.
fn flatten_alt(
    ctx: &mut FlattenCtx<'_>,
    alts: &[Rule],
    label: Option<&str>,
    mods: SlotMods,
    out: &mut Vec<FlatItem>,
) {
    let SlotMods {
        optional,
        repeated,
        required,
    } = mods;
    let Some(label) = label else {
        for alt in alts {
            flatten(ctx, alt, None, mods.with_optional(), out);
        }
        return;
    };

    // Labeled alternation: all-token(ish) → token-set slot; all-node →
    // ad-hoc enum. A `Seq` branch (`'-' <encaps_int>`) contributes its
    // LAST atom's kinds — the payload, not the prefix punctuation.
    let mut token_kinds: Vec<String> = Vec::new();
    let mut node_refs: Vec<String> = Vec::new();
    let mut ok = true;
    for alt in alts {
        let atom = match alt {
            Rule::Seq(items) => items.last().unwrap_or(alt),
            _ => alt,
        };
        match atom {
            Rule::Token { text, line } => match resolve_token(text) {
                Ok(kind) => token_kinds.push(kind),
                Err(msg) => ctx.errors.push(Error::at(*line, msg)),
            },
            Rule::Class { name, .. } => token_kinds.push(name.to_ascii_uppercase()),
            Rule::Node { name, .. } => node_refs.push(name.clone()),
            _ => ok = false,
        }
    }
    // `'-' <encaps_int>` beside `<encaps_int>` names one kind twice.
    let mut seen = BTreeSet::new();
    token_kinds.retain(|kind| seen.insert(kind.clone()));
    if !ok || (!token_kinds.is_empty() && !node_refs.is_empty()) {
        ctx.errors.push(Error::global(format!(
            "{}: labeled alternation `{label}` mixes nodes and tokens — introduce a wrapper \
             node (convention A4)",
            ctx.def_name
        )));
        return;
    }
    let ty = if node_refs.is_empty() {
        SlotTy::Tokens(token_kinds)
    } else {
        SlotTy::AdHoc(node_refs)
    };
    out.push(FlatItem::Slot(Slot {
        name: label.to_owned(),
        ty,
        repeated,
        required,
        optional,
        anchors: Vec::new(),
        stops: Vec::new(),
    }));
}

// ---------------------------------------------------------------------------
// Kind sets and anchor assignment
// ---------------------------------------------------------------------------

/// The set of `SyntaxKind`s a definition's node or token can have.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct KindSet {
    kinds: BTreeSet<String>,
    /// `<member_ident>` defensiveness: also matches any keyword kind.
    any_keyword: bool,
}

impl KindSet {
    fn intersects(&self, other: &KindSet) -> bool {
        if self.kinds.intersection(&other.kinds).next().is_some() {
            return true;
        }
        let kw = |s: &KindSet, o: &KindSet| {
            s.any_keyword && (o.any_keyword || o.kinds.iter().any(|k| k.ends_with("_KW")))
        };
        kw(self, other) || kw(other, self)
    }
}

/// What the generator needs of one definition.
struct DefInfo<'a> {
    def: &'a Def,
    class: DefClass,
}

struct Model<'a> {
    defs: Vec<DefInfo<'a>>,
    by_name: BTreeMap<&'a str, usize>,
}

impl<'a> Model<'a> {
    fn build(grammar: &'a Grammar) -> Model<'a> {
        let defs: Vec<DefInfo<'a>> = grammar
            .defs
            .iter()
            .map(|def| DefInfo {
                def,
                class: classify(def),
            })
            .collect();
        let by_name = defs
            .iter()
            .enumerate()
            .map(|(i, info)| (info.def.name.as_str(), i))
            .collect();
        Model { defs, by_name }
    }

    fn info(&self, name: &str) -> Option<&DefInfo<'a>> {
        self.by_name.get(name).map(|&i| &self.defs[i])
    }

    fn class_of(&self, name: &str) -> Option<DefClass> {
        self.info(name).map(|info| info.class)
    }

    /// The struct node kinds, in the schema's order: a kind's position
    /// here is its slot in `Nodes`.
    fn structs(&self) -> impl Iterator<Item = &DefInfo<'a>> {
        self.defs
            .iter()
            .filter(|info| info.class == DefClass::StructNode)
    }

    /// The slot of the struct node `name` in `Nodes`.
    fn slot_of(&self, name: &str) -> usize {
        self.structs()
            .position(|info| info.def.name == name)
            .unwrap_or(usize::MAX)
    }

    /// Transitive kind set of a definition.
    fn kind_set(&self, name: &str, seen: &mut BTreeSet<String>) -> KindSet {
        let mut out = KindSet::default();
        if !seen.insert(name.to_owned()) {
            return out;
        }
        let Some(info) = self.info(name) else {
            return out;
        };
        match info.class {
            DefClass::StructNode => {
                out.kinds.insert(screaming_snake(name));
            }
            DefClass::Enum | DefClass::TokenAlt => {
                if let Some(Rule::Alt(alts)) = &info.def.rule {
                    for alt in alts {
                        match alt {
                            Rule::Node { name, .. } => {
                                let sub = self.kind_set(name, seen);
                                out.kinds.extend(sub.kinds);
                                out.any_keyword |= sub.any_keyword;
                            }
                            Rule::Class { name, .. } => {
                                if name == "member_ident" {
                                    out.kinds.insert("IDENT".to_owned());
                                    out.any_keyword = true;
                                } else {
                                    out.kinds.insert(name.to_ascii_uppercase());
                                }
                            }
                            Rule::Token { text, .. } => {
                                if let Ok(kind) = resolve_token(text) {
                                    out.kinds.insert(kind);
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
            DefClass::Trait => {}
        }
        out
    }

    fn slot_kind_set(&self, ty: &SlotTy) -> KindSet {
        match ty {
            SlotTy::Ref(name) => self.kind_set(name, &mut BTreeSet::new()),
            SlotTy::Tokens(kinds) => KindSet {
                kinds: kinds.iter().cloned().collect(),
                any_keyword: false,
            },
            SlotTy::MemberIdent => KindSet {
                kinds: ["IDENT".to_owned(), "MEMBER_IDENT".to_owned()].into(),
                any_keyword: true,
            },
            SlotTy::AdHoc(refs) => {
                let mut out = KindSet::default();
                for r in refs {
                    let sub = self.kind_set(r, &mut BTreeSet::new());
                    out.kinds.extend(sub.kinds);
                    out.any_keyword |= sub.any_keyword;
                }
                out
            }
        }
    }
}

/// One slot per name: `SwitchArm*` in both syntax branches is one
/// field, and a label written at a list's head and again in its tail,
/// `types:TypeExpr ('|' types:TypeExpr)*`, is one list, so the kept slot
/// is repeated once either occurrence is. A label written twice with two
/// types is an error, not a field that reads one of them.
fn dedupe(def: &Def, items: Vec<FlatItem>, errors: &mut Vec<Error>) -> Vec<FlatItem> {
    let mut deduped: Vec<FlatItem> = Vec::new();
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    for item in items {
        let FlatItem::Slot(slot) = item else {
            deduped.push(item);
            continue;
        };
        let Some(&at) = seen.get(&slot.name) else {
            seen.insert(slot.name.clone(), deduped.len());
            deduped.push(FlatItem::Slot(slot));
            continue;
        };
        let FlatItem::Slot(kept) = &mut deduped[at] else {
            continue;
        };
        if kept.ty != slot.ty {
            errors.push(Error::global(format!(
                "{}: field `{}` derived twice with different shapes",
                def.name, slot.name
            )));
        } else if slot.repeated && !kept.repeated {
            kept.repeated = true;
            kept.required = false;
        }
    }
    deduped
}

/// Computes the final slot list for a struct node or trait: flatten,
/// dedupe, assign anchors per convention A4.
fn slots_for(model: &Model<'_>, def: &Def, errors: &mut Vec<Error>) -> Vec<Slot> {
    let mut ctx = FlattenCtx {
        def_name: &def.name,
        errors: Vec::new(),
    };
    let mut items = Vec::new();
    if let Some(rule) = &def.rule {
        flatten(&mut ctx, rule, None, SlotMods::default(), &mut items);
    }
    errors.append(&mut ctx.errors);

    let deduped = dedupe(def, items, errors);

    // Anchor assignment (A4): a slot whose kind set collides with an
    // EARLIER slot's resolves relative to the nearest reliable preceding
    // token; a following anchor becomes a stop set when a colliding slot
    // sits beyond it.
    let kind_sets: Vec<Option<KindSet>> = deduped
        .iter()
        .map(|item| match item {
            FlatItem::Slot(slot) => Some(model.slot_kind_set(&slot.ty)),
            FlatItem::Anchor { .. } => None,
        })
        .collect();

    let mut result: Vec<Slot> = Vec::new();
    for (i, item) in deduped.iter().enumerate() {
        let FlatItem::Slot(slot) = item else {
            continue;
        };
        let mut slot = slot.clone();
        let Some(my_set) = kind_sets[i].as_ref() else {
            continue;
        };

        let collides = |j: usize| {
            kind_sets[j]
                .as_ref()
                .is_some_and(|other| my_set.intersects(other))
        };
        let earlier_collision =
            (0..i).any(|j| matches!(deduped[j], FlatItem::Slot(_)) && collides(j));

        if earlier_collision {
            // Scan back for the nearest reliable anchor: an unlabeled
            // literal, a non-optional token slot, or a token-alt slot
            // (`op:BinaryOp` anchors `rhs`).
            let mut anchors = None;
            for j in (0..i).rev() {
                match &deduped[j] {
                    FlatItem::Anchor { kinds, optional } if !optional => {
                        anchors = Some(kinds.clone());
                        break;
                    }
                    FlatItem::Slot(s) if !s.optional && !s.repeated => match &s.ty {
                        SlotTy::Tokens(kinds) => {
                            anchors = Some(kinds.clone());
                            break;
                        }
                        SlotTy::Ref(name) if model.class_of(name) == Some(DefClass::TokenAlt) => {
                            let set = model.kind_set(name, &mut BTreeSet::new());
                            anchors = Some(set.kinds.into_iter().collect());
                            break;
                        }
                        _ => {}
                    },
                    _ => {}
                }
            }
            match anchors {
                Some(anchors) => slot.anchors = anchors,
                None => errors.push(Error::global(format!(
                    "{}: field `{}` collides with an earlier same-typed sibling and no \
                     reliable anchor token precedes it — introduce a wrapper node \
                     (convention A4)",
                    def.name, slot.name
                ))),
            }
            // Stop set: the nearest following anchor, when a collision
            // sits beyond it (TernaryExpr's `then` stops at `:`).
            if let Some(j) =
                (i + 1..deduped.len()).find(|&j| matches!(deduped[j], FlatItem::Anchor { .. }))
                && let FlatItem::Anchor { kinds, .. } = &deduped[j]
                && (j + 1..deduped.len())
                    .any(|k| matches!(deduped[k], FlatItem::Slot(_)) && collides(k))
            {
                slot.stops.clone_from(kinds);
            }
        }
        result.push(slot);
    }
    result
}

// ---------------------------------------------------------------------------
// Emission
// ---------------------------------------------------------------------------

fn rust_label(name: &str) -> String {
    if RAW_LABELS.contains(&name) {
        format!("r#{name}")
    } else {
        name.to_owned()
    }
}

/// `CLASS_KW` → `ClassKw`, `IDENT` → `Ident`: a token variant's name.
fn variant_from_kind(kind: &str) -> String {
    let mut out = String::new();
    let mut upper = true;
    for ch in kind.chars() {
        if ch == '_' {
            upper = true;
        } else if upper {
            out.push(ch.to_ascii_uppercase());
            upper = false;
        } else {
            out.push(ch.to_ascii_lowercase());
        }
    }
    out
}

/// `SimpleInterpolation` + `suffix` → `SimpleInterpolationSuffix`: the
/// name of the enum a labeled alternation of nodes generates.
fn adhoc_enum_name(def: &Def, slot: &Slot) -> String {
    let mut label = String::new();
    let mut upper = true;
    for ch in slot.name.chars() {
        if ch == '_' {
            upper = true;
        } else if upper {
            label.push(ch.to_ascii_uppercase());
            upper = false;
        } else {
            label.push(ch);
        }
    }
    format!("{}{label}", def.name)
}

/// The type one child of the named definition is held as: the index of a
/// struct node, the enum of an alternation, the token of a token-only
/// alternation.
fn ref_type(model: &Model<'_>, name: &str) -> Result<String, Error> {
    match model.class_of(name) {
        Some(DefClass::StructNode) => Ok(format!("NodeId<{name}>")),
        Some(DefClass::Enum) => Ok(name.to_owned()),
        Some(DefClass::TokenAlt) => Ok("TokenId".to_owned()),
        Some(DefClass::Trait) | None => Err(Error::global(format!(
            "`{name}` is referenced as a child and is not a node, an alternation or a token set"
        ))),
    }
}

/// The type of one slot's field before arity: what one child is.
fn slot_base_type(model: &Model<'_>, def: &Def, slot: &Slot) -> Result<String, Error> {
    match &slot.ty {
        SlotTy::Tokens(_) | SlotTy::MemberIdent => Ok("TokenId".to_owned()),
        SlotTy::Ref(name) => ref_type(model, name),
        SlotTy::AdHoc(_) => Ok(adhoc_enum_name(def, slot)),
    }
}

/// The field's type with its arity (A2): a boxed slice when repeated, the
/// bare type when required, `Option` otherwise.
fn field_type(base: &str, slot: &Slot) -> String {
    if slot.repeated {
        format!("Box<[{base}]>")
    } else if slot.required {
        base.to_owned()
    } else {
        format!("Option<{base}>")
    }
}

/// The field's rustdoc: the child as the schema wrote it, and its arity.
fn slot_doc(slot: &Slot) -> String {
    let what = match &slot.ty {
        SlotTy::Ref(name) => format!("`{name}`"),
        SlotTy::Tokens(kinds) => format!("`{}`", kinds.join(" | ")),
        SlotTy::MemberIdent => "`<member_ident>`".to_owned(),
        SlotTy::AdHoc(refs) => format!("`{}`", refs.join(" | ")),
    };
    let arity = if slot.repeated {
        ", zero or more"
    } else if slot.required {
        ", required (`!`)"
    } else {
        ", absent when not written"
    };
    format!("{what}{arity}")
}

/// One field of an emitted struct, and the trait method that reads it.
struct Field {
    name: String,
    ty: String,
    doc: String,
    /// The trait the field comes from, when a mixin injected it.
    mixin: Option<String>,
    repeated: bool,
    /// The enum a labeled alternation of nodes needs, by name and variants.
    adhoc: Option<(String, Vec<String>)>,
    /// The slot the field resolves from: its kind set, its anchors and its
    /// stops (A4). `None` for the doc comment, which the builder hands in.
    slot: Option<Slot>,
}

impl Field {
    /// The field of `slot` on `def`, from `mixin` when a trait injected it.
    fn of(model: &Model<'_>, def: &Def, slot: &Slot, mixin: Option<&str>) -> Result<Field, Error> {
        let base = slot_base_type(model, def, slot)?;
        let adhoc = match &slot.ty {
            SlotTy::AdHoc(refs) => Some((adhoc_enum_name(def, slot), refs.clone())),
            _ => None,
        };
        Ok(Field {
            name: slot.name.clone(),
            ty: field_type(&base, slot),
            doc: slot_doc(slot),
            mixin: mixin.map(str::to_owned),
            repeated: slot.repeated,
            adhoc,
            slot: Some(slot.clone()),
        })
    }
}

/// The fields of a struct node: its mixins' first, in opt-in order, then
/// its own. A name derived twice, from a mixin and from the node's own
/// rule, is a schema error.
fn fields_of(model: &Model<'_>, def: &Def, errors: &mut Vec<Error>) -> Vec<Field> {
    let mut fields: Vec<Field> = Vec::new();
    let mut push = |field: Field, errors: &mut Vec<Error>| {
        if fields.iter().any(|f| f.name == field.name) {
            errors.push(Error::at(
                def.line,
                format!(
                    "`{}`: field `{}` is derived both from a mixin and from the node's own rule",
                    def.name, field.name
                ),
            ));
        } else {
            fields.push(field);
        }
    };
    for trait_name in &def.traits {
        let Some(info) = model.info(trait_name) else {
            continue;
        };
        if info.def.rule.is_none() {
            push(
                Field {
                    name: DOC_COMMENT.to_owned(),
                    ty: "Option<TokenId>".to_owned(),
                    doc: "the doc comment the node took where php's grammar takes it, a \
                          `DOC_COMMENT` token of the token stream"
                        .to_owned(),
                    mixin: Some(trait_name.clone()),
                    repeated: false,
                    adhoc: None,
                    slot: None,
                },
                errors,
            );
            continue;
        }
        for slot in slots_for(model, info.def, errors) {
            match Field::of(model, info.def, &slot, Some(trait_name)) {
                Ok(field) => push(field, errors),
                Err(e) => errors.push(e),
            }
        }
    }
    for slot in slots_for(model, def, errors) {
        match Field::of(model, def, &slot, None) {
            Ok(field) => push(field, errors),
            Err(e) => errors.push(e),
        }
    }
    fields
}

/// The return type of the trait method that reads `field`: a borrowed
/// slice for a list, the value itself otherwise (every base type is
/// `Copy`).
fn method_type(field: &Field) -> String {
    if field.repeated {
        let inner = field
            .ty
            .strip_prefix("Box<[")
            .and_then(|t| t.strip_suffix("]>"))
            .unwrap_or(&field.ty);
        format!("&[{inner}]")
    } else {
        field.ty.clone()
    }
}

fn method_body(field: &Field) -> String {
    let name = rust_label(&field.name);
    if field.repeated {
        format!("&self.{name}")
    } else {
        format!("self.{name}")
    }
}

fn emit_header(buf: &mut String) {
    buf.push_str(
        "// Generated by build.rs from php.ungram; never committed (design §10).\n\
         // Edit the schema, not this file.\n\
         \n",
    );
}

fn emit_token_sets(buf: &mut String, model: &Model<'_>) {
    let mut any = false;
    for info in &model.defs {
        if info.class != DefClass::TokenAlt {
            continue;
        }
        let set = model.kind_set(&info.def.name, &mut BTreeSet::new());
        let list: Vec<String> = set
            .kinds
            .iter()
            .map(|k| format!("SyntaxKind::{k}"))
            .collect();
        write!(
            buf,
            "/// The token set of `{0}` (token-only alternation, convention A1).\n\
             pub const {1}_TOKENS: &[SyntaxKind] = &[{2}];\n",
            info.def.name,
            screaming_snake(&info.def.name),
            list.join(", "),
        )
        .ok();
        any = true;
    }
    if !any {
        return;
    }
    buf.push_str("\nimpl SyntaxKind {\n");
    for info in &model.defs {
        if info.class != DefClass::TokenAlt {
            continue;
        }
        write!(
            buf,
            "    /// Is this kind in the `{0}` token set?\n    \
             #[must_use]\n    \
             pub fn is_{1}(self) -> bool {{\n        \
             {2}_TOKENS.contains(&self)\n    \
             }}\n",
            info.def.name,
            snake_case(&info.def.name),
            screaming_snake(&info.def.name),
        )
        .ok();
    }
    buf.push_str("}\n\n");
}

fn emit_struct(
    buf: &mut String,
    model: &Model<'_>,
    def: &Def,
    fields: &[Field],
    errors: &mut Vec<Error>,
) {
    let name = &def.name;
    let kind = screaming_snake(name);
    write!(
        buf,
        "/// `{name}` (php.ungram): one node of kind `{kind}`, its children the fields the\n\
         /// builder resolved, no scan behind any of them.\n\
         #[derive(Clone, Debug, PartialEq, Eq)]\n\
         pub struct {name} {{\n    \
             /// Where the node is in the source: from its first significant token to its last.\n    \
             pub range: TextRange,\n    \
             /// php's `zend_ast.lineno`, as the builder records it (design §4.9).\n    \
             pub line: u32,\n"
    ).ok();
    for field in fields {
        let from = field
            .mixin
            .as_ref()
            .map(|m| format!(" (`{m}`)"))
            .unwrap_or_default();
        write!(
            buf,
            "    /// `{}`: {}{from}.\n    pub {}: {},\n",
            field.name,
            field.doc,
            rust_label(&field.name),
            field.ty
        )
        .ok();
    }
    buf.push_str("}\n");
    write!(
        buf,
        "impl Node for {name} {{\n    \
             const KIND: SyntaxKind = SyntaxKind::{kind};\n    \
             fn range(&self) -> TextRange {{\n        self.range\n    }}\n    \
             fn line(&self) -> u32 {{\n        self.line\n    }}\n    \
             fn all(nodes: &Nodes) -> &[Self] {{\n        \
                 match nodes.store({slot}) {{\n            \
                     Some(Store::{name}(nodes)) => nodes,\n            \
                     _ => &[],\n        \
                 }}\n    \
             }}\n\
         }}\n",
        slot = model.slot_of(name)
    )
    .ok();
    emit_resolver(buf, model, def, fields, errors);
    for trait_name in &def.traits {
        if model.class_of(trait_name) != Some(DefClass::Trait) {
            continue;
        }
        writeln!(buf, "impl {trait_name} for {name} {{").ok();
        for field in fields
            .iter()
            .filter(|f| f.mixin.as_deref() == Some(trait_name))
        {
            write!(
                buf,
                "    fn {}(&self) -> {} {{\n        {}\n    }}\n",
                rust_label(&field.name),
                method_type(field),
                method_body(field)
            )
            .ok();
        }
        buf.push_str("}\n");
    }
    buf.push('\n');
}

/// `&[SyntaxKind::A, SyntaxKind::B]`: a kind set as the builder reads it.
fn kinds_expr(kinds: &[String]) -> String {
    let list: Vec<String> = kinds.iter().map(|k| format!("SyntaxKind::{k}")).collect();
    format!("&[{}]", list.join(", "))
}

/// The read that fills one field from a node's children, Mahout's
/// `accessor_body` done once at build time instead of on every call: the
/// first child the slot's kind set admits, the first after an anchor
/// token and before a stop token (A4), or every one, by the slot's shape.
fn field_read(model: &Model<'_>, def: &Def, slot: &Slot) -> Result<String, Error> {
    let anchors = kinds_expr(&slot.anchors);
    let stops = kinds_expr(&slot.stops);
    let token_read = |set: &str| {
        if slot.repeated {
            format!("tokens(children, {set})")
        } else {
            format!("token(children, {anchors}, {set})")
        }
    };
    let node_read = |ty: &str| {
        if slot.repeated {
            format!("all::<{ty}>(children)")
        } else if slot.anchors.is_empty() {
            format!("first::<{ty}>(children)")
        } else {
            format!("first_after::<{ty}>(children, {anchors}, {stops})")
        }
    };
    match &slot.ty {
        SlotTy::Tokens(kinds) => Ok(token_read(&kinds_expr(kinds))),
        SlotTy::MemberIdent => {
            if slot.repeated {
                return Err(Error::global(format!(
                    "{}: field `{}` repeats `<member_ident>`, which the builder reads once \
                     per node",
                    def.name, slot.name
                )));
            }
            Ok(format!("member(children, {anchors})"))
        }
        SlotTy::Ref(name) => match model.class_of(name) {
            Some(DefClass::TokenAlt) => {
                Ok(token_read(&format!("{}_TOKENS", screaming_snake(name))))
            }
            _ => Ok(node_read(&ref_type(model, name)?)),
        },
        SlotTy::AdHoc(_) => Ok(node_read(&adhoc_enum_name(def, slot))),
    }
}

/// `impl Name { fn resolve(...) }`: the node from its children, every
/// field read once, and the name of the required child that is absent
/// where one is (A2: the parser promised it, so the builder reports the
/// broken promise rather than building a node without it).
fn emit_resolver(
    buf: &mut String,
    model: &Model<'_>,
    def: &Def,
    fields: &[Field],
    errors: &mut Vec<Error>,
) {
    let name = &def.name;
    let doc_param = if fields.iter().any(|f| f.slot.is_none()) {
        "doc_comment"
    } else {
        "_doc_comment"
    };
    let children_param = if fields.iter().any(|f| f.slot.is_some()) {
        "children"
    } else {
        "_children"
    };
    // A node with a required child can fail to resolve and says which
    // child; one without cannot, and returns itself.
    let fallible = has_required(fields);
    let (doc, ret, ok) = if fallible {
        (
            "; `Err` names the\n    /// required child (`!`) that is absent",
            "Result<Self, &'static str>",
            "Ok(Self {",
        )
    } else {
        ("", "Self", "Self {")
    };
    write!(
        buf,
        "impl {name} {{\n    \
             /// The node from its children, resolved once (ADR-0003){doc}.\n    \
             fn resolve(\n        \
                 range: TextRange,\n        \
                 line: u32,\n        \
                 {children_param}: &[Element],\n        \
                 {doc_param}: Option<TokenId>,\n    \
             ) -> {ret} {{\n"
    )
    .ok();
    for field in fields {
        let label = rust_label(&field.name);
        let Some(slot) = &field.slot else {
            // The doc comment is the parameter itself.
            continue;
        };
        let read = match field_read(model, def, slot) {
            Ok(read) => read,
            Err(e) => {
                errors.push(e);
                continue;
            }
        };
        if slot.required && !slot.repeated {
            writeln!(
                buf,
                "        let Some({label}) = {read} else {{\n            \
                     return Err(\"{}.{}\");\n        \
                 }};",
                name, field.name
            )
            .ok();
        } else {
            writeln!(buf, "        let {label} = {read};").ok();
        }
    }
    writeln!(buf, "        {ok}\n            range,\n            line,").ok();
    for field in fields {
        writeln!(buf, "            {},", rust_label(&field.name)).ok();
    }
    let close = if fallible { "})" } else { "}" };
    write!(buf, "        {close}\n    }}\n}}\n").ok();
}

/// Does one of `fields` hold a required child (`!`), so that resolving
/// the node can fail?
fn has_required(fields: &[Field]) -> bool {
    fields
        .iter()
        .any(|f| f.slot.as_ref().is_some_and(|s| s.required && !s.repeated))
}

/// `Nodes`, the arena: one vector per struct node kind, so a
/// `NodeId<ClassDecl>` indexes the class declarations and nothing else,
/// and no node pays for the width of another kind's struct. A kind's
/// vector is made when its first node is, so a tree pays for the kinds
/// it holds and not for every kind the schema names: `Store` is one
/// kind's vector, and a slot per kind says where in `stores` it stands.
fn emit_nodes(buf: &mut String, model: &Model<'_>) {
    buf.push_str(
        "/// One kind's vector in the arena, by the kind's struct.\n\
         #[derive(Clone, Debug, PartialEq, Eq)]\n\
         enum Store {\n",
    );
    for info in model.structs() {
        let name = &info.def.name;
        writeln!(buf, "    {name}(Vec<{name}>),").ok();
    }
    buf.push_str("}\n\n");
    write!(
        buf,
        "/// The arena (design §4.9, ADR-0003): every node of the tree, one vector per kind,\n\
         /// each addressed by its `NodeId`. Filled by the builder, read through `Tree`. A\n\
         /// kind's vector is made with its first node, so an arena holds the kinds its\n\
         /// tree has and nothing for the rest.\n\
         #[derive(Clone, Debug, PartialEq, Eq)]\n\
         pub struct Nodes {{\n    \
             /// Where each struct kind's vector stands in `stores`, by the kind's slot:\n    \
             /// its position plus one, and zero while no node of the kind is built.\n    \
             slots: [u8; {count}],\n    \
             /// The vectors, in the order their kinds' first nodes were built.\n    \
             stores: Vec<Store>,\n\
         }}\n\n\
         impl Default for Nodes {{\n    \
             fn default() -> Self {{\n        \
                 Nodes {{\n            \
                     slots: [0; {count}],\n            \
                     stores: Vec::new(),\n        \
                 }}\n    \
             }}\n\
         }}\n\n",
        count = model.structs().count()
    )
    .ok();
}

/// `resolve`, the builder's one call per finished node: the struct of
/// `kind` from its children, pushed into its vector, and its index back;
/// the required child that is absent; or nothing for a kind with no
/// struct, which `ERROR` is.
fn emit_resolve(buf: &mut String, model: &Model<'_>) {
    buf.push_str(
        "/// The node of `kind` from its children, resolved once and pushed into the arena\n\
         /// (ADR-0003): its index, the required child that is absent, or `Untyped` for a\n\
         /// kind with no struct, which `ERROR` is.\n\
         #[allow(clippy::too_many_lines)]\n\
         pub(crate) fn resolve(\n    \
             nodes: &mut Nodes,\n    \
             kind: SyntaxKind,\n    \
             range: TextRange,\n    \
             line: u32,\n    \
             children: &[Element],\n    \
             doc_comment: Option<TokenId>,\n\
         ) -> Resolved {\n    \
             match kind {\n",
    );
    for (slot, info) in model.structs().enumerate() {
        let name = &info.def.name;
        // The fields were derived, and their errors reported, when the
        // struct was emitted.
        let fallible = has_required(&fields_of(model, info.def, &mut Vec::new()));
        let (open, close) = if fallible { ("", "") } else { ("Ok(", ")") };
        writeln!(
            buf,
            "        SyntaxKind::{} => match nodes.open({slot}, Store::{name}) {{\n            \
                 Some(Store::{name}(nodes)) => push(nodes, {open}{name}::resolve(range, line, children, doc_comment){close}),\n            \
                 _ => Resolved::Missing(\"Nodes.{}\"),\n        \
             }},",
            screaming_snake(name),
            snake_case(name)
        ).ok();
    }
    buf.push_str("        _ => Resolved::Untyped,\n    }\n}\n\n");
}

/// One alternation as an enum: a variant per listed node carrying its
/// index (or the alternation enum itself, or the token, by what the node
/// is), and a variant per listed token carrying its `TokenId`.
fn emit_enum(
    buf: &mut String,
    model: &Model<'_>,
    name: &str,
    node_refs: &[String],
    token_variants: &[(String, String)],
    what: &str,
    errors: &mut Vec<Error>,
) {
    write!(
        buf,
        "/// `{name}` ({what}; convention A1): which node or token stands here, resolved\n\
         /// once by the builder.\n\
         #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]\n\
         pub enum {name} {{\n"
    )
    .ok();
    for r in node_refs {
        match ref_type(model, r) {
            Ok(ty) if ty == "TokenId" => {
                writeln!(
                    buf,
                    "    /// A bare `{r}` token (A1 token-only alternation).\n    {r}(TokenId),"
                )
                .ok();
            }
            Ok(ty) => {
                writeln!(buf, "    /// `{r}`.\n    {r}({ty}),").ok();
            }
            Err(e) => errors.push(e),
        }
    }
    for (variant, kind) in token_variants {
        writeln!(buf, "    /// The `{kind}` token.\n    {variant}(TokenId),").ok();
    }
    buf.push_str("}\n");

    // The read the builder makes of one child against this alternation:
    // the listed nodes first, in the schema's order, then the tokens, as
    // Mahout's `cast` tried them.
    let long = if node_refs.len() + token_variants.len() > 30 {
        "    #[allow(clippy::too_many_lines)]\n"
    } else {
        ""
    };
    write!(
        buf,
        "impl Child for {name} {{\n\
         {long}    \
             fn of(element: &Element) -> Option<Self> {{\n"
    )
    .ok();
    for r in node_refs {
        match model.class_of(r) {
            Some(DefClass::StructNode) => {
                writeln!(
                    buf,
                    "        if let Element::Node {{ kind: SyntaxKind::{}, index, .. }} = *element {{\n            \
                         return Some(Self::{r}(NodeId::new(index)));\n        \
                     }}",
                    screaming_snake(r)
                ).ok();
            }
            Some(DefClass::Enum) => {
                writeln!(
                    buf,
                    "        if let Some(inner) = {r}::of(element) {{\n            \
                         return Some(Self::{r}(inner));\n        \
                     }}"
                )
                .ok();
            }
            Some(DefClass::TokenAlt) => {
                writeln!(
                    buf,
                    "        if let Element::Token {{ kind, id }} = *element\n            \
                         && kind.is_{}()\n        \
                     {{\n            \
                         return Some(Self::{r}(id));\n        \
                     }}",
                    snake_case(r)
                )
                .ok();
            }
            Some(DefClass::Trait) | None => {}
        }
    }
    for (variant, kind) in token_variants {
        writeln!(
            buf,
            "        if let Element::Token {{ kind: SyntaxKind::{kind}, id }} = *element {{\n            \
                 return Some(Self::{variant}(id));\n        \
             }}"
        ).ok();
    }
    buf.push_str("        None\n    }\n}\n");
    emit_enum_place(buf, model, name, node_refs, token_variants);
    buf.push('\n');
}

/// `impl Name { fn range, fn line }`: where the child stands, whichever
/// variant it is. A node variant answers from the arena, a token variant
/// from the token stream, a nested alternation from its own; a token's
/// line is the line it starts on, which is what the builder gives a
/// token child (design §4.9). The compiler reads these where php's
/// reads `ast->lineno` of a child it has not matched on yet, and where
/// it hands a child's bytes on by range.
fn emit_enum_place(
    buf: &mut String,
    model: &Model<'_>,
    name: &str,
    node_refs: &[String],
    token_variants: &[(String, String)],
) {
    let mut range_arms = String::new();
    let mut line_arms = String::new();
    // The token variants are one arm, since every one reads the stream
    // the same way; a node variant reads its own kind's arena.
    let mut tokens: Vec<String> = Vec::new();
    for r in node_refs {
        match model.class_of(r) {
            Some(DefClass::StructNode) => {
                writeln!(
                    range_arms,
                    "            Self::{r}(id) => parsed.tree[id].range(),"
                )
                .ok();
                writeln!(
                    line_arms,
                    "            Self::{r}(id) => parsed.tree[id].line(),"
                )
                .ok();
            }
            Some(DefClass::Enum) => {
                writeln!(
                    range_arms,
                    "            Self::{r}(inner) => inner.range(parsed),"
                )
                .ok();
                writeln!(
                    line_arms,
                    "            Self::{r}(inner) => inner.line(parsed),"
                )
                .ok();
            }
            Some(DefClass::TokenAlt) => tokens.push(format!("Self::{r}(id)")),
            Some(DefClass::Trait) | None => {}
        }
    }
    tokens.extend(
        token_variants
            .iter()
            .map(|(variant, _)| format!("Self::{variant}(id)")),
    );
    if !tokens.is_empty() {
        let pattern = tokens.join(" | ");
        writeln!(
            range_arms,
            "            {pattern} => token_range(parsed, id),"
        )
        .ok();
        writeln!(
            line_arms,
            "            {pattern} => token_line(parsed, id),"
        )
        .ok();
    }
    write!(
        buf,
        "impl {name} {{\n    \
             /// Where the child is in the source: the node's range, or the token's.\n    \
             #[must_use]\n    \
             pub fn range(self, parsed: &crate::Parsed) -> TextRange {{\n        \
                 match self {{\n{range_arms}        }}\n    \
             }}\n\n    \
             /// php's line for the child: the node's `zend_ast.lineno`, or the line a\n    \
             /// token starts on (design §4.9).\n    \
             #[must_use]\n    \
             pub fn line(self, parsed: &crate::Parsed) -> u32 {{\n        \
                 match self {{\n{line_arms}        }}\n    \
             }}\n\
         }}\n"
    )
    .ok();
}

/// The mixin traits (A5): one method per injected field, over the nodes
/// that opted in.
fn emit_traits(buf: &mut String, model: &Model<'_>, errors: &mut Vec<Error>) {
    for info in &model.defs {
        if info.class != DefClass::Trait {
            continue;
        }
        let def = info.def;
        write!(
            buf,
            "/// Mixin `{0}` (convention A5): the fields it injects into every node that\n\
             /// opts in, read through one trait.\n\
             pub trait {0} {{\n",
            def.name
        )
        .ok();
        if def.rule.is_none() {
            write!(
                buf,
                "    /// The doc comment the node took where php's grammar takes it \
                 (`backup_doc_comment`).\n    \
                 fn {DOC_COMMENT}(&self) -> Option<TokenId>;\n"
            )
            .ok();
        } else {
            for slot in slots_for(model, def, errors) {
                match Field::of(model, def, &slot, Some(&def.name)) {
                    Ok(field) => {
                        write!(
                            buf,
                            "    /// `{}`: {}.\n    fn {}(&self) -> {};\n",
                            field.name,
                            field.doc,
                            rust_label(&field.name),
                            method_type(&field)
                        )
                        .ok();
                    }
                    Err(e) => errors.push(e),
                }
            }
        }
        buf.push_str("}\n\n");
    }
}

/// The text of `tree.rs` for `grammar`, or every reason it cannot be
/// written. Nothing formats it: it is written to `OUT_DIR` and included,
/// never committed.
pub fn generate(grammar: &Grammar) -> Result<String, Vec<Error>> {
    let model = Model::build(grammar);
    let mut errors = Vec::new();
    let mut buf = String::new();
    emit_header(&mut buf);
    emit_token_sets(&mut buf, &model);

    // --- struct nodes ----------------------------------------------------
    let mut adhoc_enums: Vec<(String, Vec<String>)> = Vec::new();
    for info in &model.defs {
        if info.class != DefClass::StructNode {
            continue;
        }
        let def = info.def;
        let fields = fields_of(&model, def, &mut errors);
        adhoc_enums.extend(fields.iter().filter_map(|f| f.adhoc.clone()));
        emit_struct(&mut buf, &model, def, &fields, &mut errors);
    }

    // --- ad-hoc labeled-alternation enums ---------------------------------
    adhoc_enums.sort();
    adhoc_enums.dedup();
    for (name, refs) in &adhoc_enums {
        emit_enum(
            &mut buf,
            &model,
            name,
            refs,
            &[],
            "labeled alternation",
            &mut errors,
        );
    }

    // --- alternation enums -------------------------------------------------
    for info in &model.defs {
        if info.class != DefClass::Enum {
            continue;
        }
        let Some(Rule::Alt(alts)) = &info.def.rule else {
            continue;
        };
        let mut node_refs = Vec::new();
        let mut token_variants: Vec<(String, String)> = Vec::new(); // (variant, kind)
        for alt in alts {
            match alt {
                Rule::Node { name, .. } => node_refs.push(name.clone()),
                Rule::Token { text, line } => match resolve_token(text) {
                    Ok(kind) => token_variants.push((variant_from_kind(&kind), kind)),
                    Err(msg) => errors.push(Error::at(*line, msg)),
                },
                Rule::Class { name, .. } => {
                    token_variants.push((
                        variant_from_kind(&name.to_ascii_uppercase()),
                        name.to_ascii_uppercase(),
                    ));
                }
                _ => {}
            }
        }
        emit_enum(
            &mut buf,
            &model,
            &info.def.name,
            &node_refs,
            &token_variants,
            "alternation",
            &mut errors,
        );
    }

    emit_traits(&mut buf, &model, &mut errors);
    if model.structs().count() >= usize::from(u8::MAX) {
        errors.push(Error::global(
            "more struct node kinds than a `Nodes` slot (a u8) counts",
        ));
    }
    emit_nodes(&mut buf, &model);
    emit_resolve(&mut buf, &model);

    if errors.is_empty() {
        Ok(buf)
    } else {
        Err(errors)
    }
}

/// The text of `display.rs`: the display name of every token kind, for
/// a diagnostic that names what it saw. A quoted token renders backticked
/// (`` `;` ``), a token class as a phrase; a node kind has none, since a
/// message names a construct in the reader's terms, never by kind.
pub fn generate_display(grammar: &Grammar) -> Result<String, Vec<Error>> {
    let mut errors = Vec::new();
    for (name, _) in CLASS_DISPLAY {
        if !TOKEN_CLASSES.contains(name) {
            errors.push(Error::global(format!(
                "display table names unknown token class `{name}`"
            )));
        }
    }
    for name in TOKEN_CLASSES {
        if !CLASS_DISPLAY.iter().any(|(n, _)| n == name) {
            errors.push(Error::global(format!(
                "token class `{name}` missing a display phrase"
            )));
        }
    }
    if !errors.is_empty() {
        return Err(errors);
    }

    let mut buf = String::new();
    emit_header(&mut buf);
    buf.push_str(
        "impl SyntaxKind {\n    \
         /// The display name of a token kind, for a diagnostic; `None` for a node kind.\n    \
         #[must_use]\n    \
         #[allow(clippy::too_many_lines)]\n    \
         #[allow(clippy::match_same_arms)] // LINE_COMMENT and BLOCK_COMMENT both display as \"a comment\"\n    \
         pub fn display_name(self) -> Option<&'static str> {\n        \
         let name = match self {\n",
    );
    let mut symbols: Vec<(String, u32)> = grammar.symbol_tokens().into_iter().collect();
    symbols.sort();
    for (symbol, _) in symbols {
        if let Some(variant) = punct_variant(&symbol) {
            let display = symbol.replace('\\', "\\\\").replace('"', "\\\"");
            writeln!(buf, "            SyntaxKind::{variant} => \"`{display}`\",").ok();
        }
    }
    if grammar.has_comma_lists() {
        buf.push_str("            SyntaxKind::COMMA => \"`,`\",\n");
    }
    for keyword in grammar.keyword_tokens() {
        writeln!(
            buf,
            "            SyntaxKind::{} => \"`{keyword}`\",",
            keyword_variant(&keyword)
        )
        .ok();
    }
    for (class, display) in CLASS_DISPLAY {
        writeln!(
            buf,
            "            SyntaxKind::{} => \"{display}\",",
            class.to_ascii_uppercase()
        )
        .ok();
    }
    for (extra, display) in EXTRA_DISPLAY {
        writeln!(buf, "            SyntaxKind::{extra} => \"{display}\",").ok();
    }
    buf.push_str("            _ => return None,\n        };\n        Some(name)\n    }\n}\n");
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::super::ungram::parse;
    use super::*;

    fn emitted(schema: &str) -> String {
        let grammar = match parse(schema) {
            Ok(grammar) => grammar,
            Err(errors) => panic!("schema failed to parse: {errors:?}"),
        };
        match generate(&grammar) {
            Ok(text) => text,
            Err(errors) => panic!("generation failed: {errors:?}"),
        }
    }

    fn errors_of(schema: &str) -> Vec<Error> {
        let grammar = match parse(schema) {
            Ok(grammar) => grammar,
            Err(errors) => panic!("schema failed to parse: {errors:?}"),
        };
        match generate(&grammar) {
            Ok(_) => panic!("expected generation errors"),
            Err(errors) => errors,
        }
    }

    /// A2 as types: `!` is the bare type, a repetition a boxed slice,
    /// everything else `Option`; a struct child is its `NodeId`, an
    /// alternation its enum, a token its `TokenId`.
    #[test]
    fn arity_becomes_the_field_type() {
        let text = emitted(
            "Root = Item*\n\
             Item = Decl | <ident>\n\
             Decl = 'function' name:<ident>! params:ParamList! ret:RetType? Item?\n\
             ParamList = '(' <variable>,* ')'\n\
             RetType = ':' name:NameRef\n\
             NameRef = <ident> | <name_qualified>\n",
        );
        for needle in [
            "pub struct Root {",
            "pub range: TextRange,",
            "pub line: u32,",
            "fn line(&self) -> u32 {\n        self.line\n    }",
            "pub items: Box<[Item]>,",
            "pub name: TokenId,",
            "pub params: NodeId<ParamList>,",
            "pub ret: Option<NodeId<RetType>>,",
            "pub item: Option<Item>,",
            "pub variable: Box<[TokenId]>,",
            "pub name: Option<TokenId>,",
            "impl Node for Decl {",
            "const KIND: SyntaxKind = SyntaxKind::DECL;",
            "pub enum Item {",
            "    Decl(NodeId<Decl>),",
            "    Ident(TokenId),",
            "pub const NAME_REF_TOKENS: &[SyntaxKind] = &[SyntaxKind::IDENT, SyntaxKind::NAME_QUALIFIED];",
            "pub fn is_name_ref(self) -> bool",
            "impl Item {",
            "pub fn range(self, parsed: &crate::Parsed) -> TextRange {",
            "            Self::Decl(id) => parsed.tree[id].range(),",
            "            Self::Ident(id) => token_range(parsed, id),",
            "pub fn line(self, parsed: &crate::Parsed) -> u32 {",
            "            Self::Decl(id) => parsed.tree[id].line(),",
            "            Self::Ident(id) => token_line(parsed, id),",
        ] {
            assert!(text.contains(needle), "missing {needle:?} in:\n{text}");
        }
        for absent in [
            "expect(",
            "unwrap(",
            "Visitor",
            "fn visit",
            "syntax: SyntaxNode",
        ] {
            assert!(!text.contains(absent), "found {absent:?} in:\n{text}");
        }
    }

    /// A5 as fields and a trait: a mixin's children are fields of every
    /// node that opts in, first in the struct, and the trait reads them;
    /// `HasDocComment` declares no child and injects the token index the
    /// builder records.
    #[test]
    fn mixins_inject_fields_and_a_trait() {
        let text = emitted(
            "trait HasAttrs = attrs:Attr*\n\
             trait HasDocComment =\n\
             Attr = '#[' name:<ident> ']'\n\
             Decl : HasAttrs HasDocComment = 'class' name:<ident>?\n",
        );
        for needle in [
            "pub trait HasAttrs {",
            "    fn attrs(&self) -> &[NodeId<Attr>];",
            "pub trait HasDocComment {",
            "    fn doc_comment(&self) -> Option<TokenId>;",
            "pub attrs: Box<[NodeId<Attr>]>,",
            "pub doc_comment: Option<TokenId>,",
            "impl HasAttrs for Decl {",
            "    fn attrs(&self) -> &[NodeId<Attr>] {\n        &self.attrs\n    }",
            "impl HasDocComment for Decl {",
            "    fn doc_comment(&self) -> Option<TokenId> {\n        self.doc_comment\n    }",
        ] {
            assert!(text.contains(needle), "missing {needle:?} in:\n{text}");
        }
        let decl = &text[text.find("pub struct Decl {").unwrap_or(0)..];
        let attrs = decl.find("pub attrs:").unwrap_or(usize::MAX);
        let name = decl.find("pub name:").unwrap_or(0);
        assert!(attrs < name, "mixin fields come first");
    }

    /// A labeled alternation of nodes is an ad-hoc enum named for the node
    /// and the label; a labeled alternation of tokens is one `TokenId`; a
    /// mixed alternation is an enum with token variants; a Rust keyword as
    /// a label is a raw identifier.
    #[test]
    fn labeled_alternations_and_raw_labels() {
        let text = emitted(
            "Interp = var:<variable>! suffix:(Dim | Prop)?\n\
             Dim = '[' index:(<ident> | <int>) ']'\n\
             Prop = '->' name:<ident>!\n\
             Modifier = 'public' | 'static' | SetVis\n\
             SetVis = vis:('public' | 'protected') '(' 'set' ')'\n\
             Ref = trait:<ident> '::' ref:<ident>\n",
        );
        for needle in [
            "pub suffix: Option<InterpSuffix>,",
            "pub enum InterpSuffix {",
            "    Dim(NodeId<Dim>),",
            "    Prop(NodeId<Prop>),",
            "pub index: Option<TokenId>,",
            "pub enum Modifier {",
            "    SetVis(NodeId<SetVis>),",
            "    PublicKw(TokenId),",
            "    StaticKw(TokenId),",
            "pub vis: Option<TokenId>,",
            "pub r#trait: Option<TokenId>,",
            "pub r#ref: Option<TokenId>,",
        ] {
            assert!(text.contains(needle), "missing {needle:?} in:\n{text}");
        }
    }

    /// A4's precondition is held at build time: a same-typed sibling with
    /// no anchor token before it is a schema error naming the field.
    #[test]
    fn a_same_typed_sibling_without_an_anchor_is_an_error() {
        let errors = errors_of("Pair = first:Item? second:Item\nItem = <ident>\n");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("`second` collides") && e.message.contains("A4")),
            "{errors:?}"
        );
        // With an anchor between them, the same rule emits.
        let text = emitted("Pair = first:Item? ':' second:Item\nItem = <ident>\n");
        assert!(text.contains("pub second: Option<NodeId<Item>>,"));
    }

    /// A list written head-and-tail, one label at the head and again in
    /// the repetition, is one list field, a struct child or a token by
    /// what the label names; the same label on two types is an error,
    /// not a field that reads one of them.
    #[test]
    fn a_label_at_a_lists_head_and_in_its_tail_is_one_list() {
        let text = emitted(
            "Union = types:Ty ('|' types:Ty)*\n\
             Ty = name:<ident>\n\
             Catch = types:Name ('|' types:Name)*\n\
             Name = <ident> | <name_qualified>\n",
        );
        for needle in [
            "pub types: Box<[NodeId<Ty>]>,",
            "let types = all::<NodeId<Ty>>(children);",
            "pub types: Box<[TokenId]>,",
            "let types = tokens(children, NAME_TOKENS);",
        ] {
            assert!(text.contains(needle), "missing {needle:?} in:\n{text}");
        }
        let union = &text[text.find("pub struct Union {").unwrap_or(0)..];
        let union = &union[..union.find("\n}\n").unwrap_or(union.len())];
        assert_eq!(
            union.matches("\n    pub ").count(),
            3,
            "the range, the line and one list, no head beside the tail:\n{union}"
        );
        let errors =
            errors_of("Mixed = first:Ty ('|' first:Name)*\nTy = name:<ident>\nName = <ident>\n");
        assert!(
            errors.iter().any(|e| e
                .message
                .contains("`first` derived twice with different shapes")),
            "{errors:?}"
        );
    }

    /// A name a mixin injects and the node's own rule derives too is an
    /// error, not a struct with one field twice.
    #[test]
    fn a_field_derived_by_a_mixin_and_the_rule_is_an_error() {
        let errors =
            errors_of("trait HasName = name:<ident>\nDecl : HasName = 'class' name:<ident>\n");
        assert!(
            errors
                .iter()
                .any(|e| e.message.contains("both from a mixin")),
            "{errors:?}"
        );
    }

    /// The one-pass resolution, emitted per node: a read per field in the
    /// shape `slots_for` decided, the required child's name where one is
    /// absent, `Self` where none can be, and the arena, the dispatcher
    /// and the alternation reads beside them.
    #[test]
    fn the_resolution_is_emitted_once_per_node() {
        let text = emitted(
            "trait HasDocComment =\n\
             Root = Item*\n\
             Item = Decl | Ternary | <ident>\n\
             Decl : HasDocComment = 'class' name:<ident>! body:Block\n\
             Block = '{' Item* '}'\n\
             Ternary = cond:Item! '?' then:Item? ':' else_branch:Item?\n\
             Ref = trait:<ident> '::' ref:<ident>\n\
             Member = '->' name:<member_ident>\n",
        );
        for needle in [
            "enum Store {\n    Root(Vec<Root>),\n    Decl(Vec<Decl>),",
            "    slots: [u8; 6],",
            "fn all(nodes: &Nodes) -> &[Self] {\n        match nodes.store(1) {\n            Some(Store::Decl(nodes)) => nodes,\n            _ => &[],\n        }\n    }",
            "impl Decl {",
            "    ) -> Result<Self, &'static str> {",
            "        let Some(name) = token(children, &[], &[SyntaxKind::IDENT]) else {\n            \
                 return Err(\"Decl.name\");\n        };",
            "        let body = first::<NodeId<Block>>(children);",
            "        Ok(Self {\n            range,\n            line,\n            doc_comment,\n            name,\n            body,\n        })",
            "        let items = all::<Item>(children);",
            "    ) -> Self {",
            "        let then = first_after::<Item>(children, &[SyntaxKind::QUESTION], &[SyntaxKind::COLON]);",
            "        let else_branch = first_after::<Item>(children, &[SyntaxKind::COLON], &[]);",
            "        let r#ref = token(children, &[SyntaxKind::COLON_COLON], &[SyntaxKind::IDENT]);",
            "        let name = member(children, &[]);",
            "impl Child for Item {",
            "        if let Element::Node { kind: SyntaxKind::DECL, index, .. } = *element {\n            \
                 return Some(Self::Decl(NodeId::new(index)));",
            "        if let Element::Token { kind: SyntaxKind::IDENT, id } = *element {\n            \
                 return Some(Self::Ident(id));",
            "pub(crate) fn resolve(",
            "        SyntaxKind::DECL => match nodes.open(1, Store::Decl) {\n            Some(Store::Decl(nodes)) => push(nodes, Decl::resolve(range, line, children, doc_comment)),\n            _ => Resolved::Missing(\"Nodes.decl\"),\n        },",
            "        SyntaxKind::ROOT => match nodes.open(0, Store::Root) {\n            Some(Store::Root(nodes)) => push(nodes, Ok(Root::resolve(range, line, children, doc_comment))),\n            _ => Resolved::Missing(\"Nodes.root\"),\n        },",
            "        _ => Resolved::Untyped,",
        ] {
            assert!(text.contains(needle), "missing {needle:?} in:\n{text}");
        }
    }

    #[test]
    fn variant_names() {
        assert_eq!(variant_from_kind("CLASS_KW"), "ClassKw");
        assert_eq!(variant_from_kind("IDENT"), "Ident");
        assert_eq!(
            variant_from_kind("NAME_FULLY_QUALIFIED"),
            "NameFullyQualified"
        );
    }

    /// The display table covers every token class and the extras, and a
    /// node kind has no display name.
    #[test]
    fn display_names_cover_the_token_kinds() {
        let grammar = match parse("Foo = 'class' <ident> ';' Bar,*\nBar = <variable>\n") {
            Ok(grammar) => grammar,
            Err(errors) => panic!("schema failed to parse: {errors:?}"),
        };
        // The real table is held to the 19 classes; this schema uses two,
        // so the cross-check is the one `tests/schema.rs` runs on
        // php.ungram. Here the emitted arms are the claim.
        let text = match generate_display(&grammar) {
            Ok(text) => text,
            Err(errors) => panic!("display generation failed: {errors:?}"),
        };
        for needle in [
            "pub fn display_name(self) -> Option<&'static str>",
            "SyntaxKind::SEMICOLON => \"`;`\",",
            "SyntaxKind::COMMA => \"`,`\",",
            "SyntaxKind::CLASS_KW => \"`class`\",",
            "SyntaxKind::IDENT => \"an identifier\",",
            "SyntaxKind::EOF => \"end of file\",",
            "_ => return None,",
        ] {
            assert!(text.contains(needle), "missing {needle:?} in:\n{text}");
        }
        assert!(!text.contains("SyntaxKind::FOO =>"));
    }
}
