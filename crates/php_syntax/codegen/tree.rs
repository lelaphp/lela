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
//! token, and a mixin is a trait over the fields it injects (A5).
//!
//! The slot model implements the schema's conventions: A2 arity honesty,
//! A3 labeled tokens, A4 anchored resolution for same-typed siblings, A5
//! mixins. Flattening decides which field each child becomes, and the
//! anchor assignment holds A4's precondition, that a same-typed sibling
//! sits behind a reliable anchor token, as a schema error at build time;
//! the anchors themselves are what the builder resolves against, and it
//! reads them when it lands.
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

/// The field every node opting into `HasDocComment` carries: the builder
/// records which comment block is a declaration's doc comment as a token
/// index on the declaration node (design §4.9).
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

    // Dedupe slots by name (e.g. `SwitchArm*` in both syntax branches).
    let mut deduped: Vec<FlatItem> = Vec::new();
    let mut seen: BTreeMap<String, (SlotTy, bool)> = BTreeMap::new();
    for item in items {
        let FlatItem::Slot(slot) = item else {
            deduped.push(item);
            continue;
        };
        if let Some((ty, repeated)) = seen.get(&slot.name) {
            if *ty != slot.ty || *repeated != slot.repeated {
                errors.push(Error::global(format!(
                    "{}: field `{}` derived twice with different shapes",
                    def.name, slot.name
                )));
            }
        } else {
            seen.insert(slot.name.clone(), (slot.ty.clone(), slot.repeated));
            deduped.push(FlatItem::Slot(slot));
        }
    }

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
                    doc: "the doc comment the builder attached to the declaration, a \
                          `DOC_COMMENT` token of the token stream"
                        .to_owned(),
                    mixin: Some(trait_name.clone()),
                    repeated: false,
                    adhoc: None,
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
        let _ = write!(
            buf,
            "/// The token set of `{0}` (token-only alternation, convention A1).\n\
             pub const {1}_TOKENS: &[SyntaxKind] = &[{2}];\n",
            info.def.name,
            screaming_snake(&info.def.name),
            list.join(", "),
        );
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
        let _ = write!(
            buf,
            "    /// Is this kind in the `{0}` token set?\n    \
             #[must_use]\n    \
             pub fn is_{1}(self) -> bool {{\n        \
             {2}_TOKENS.contains(&self)\n    \
             }}\n",
            info.def.name,
            snake_case(&info.def.name),
            screaming_snake(&info.def.name),
        );
    }
    buf.push_str("}\n\n");
}

fn emit_struct(buf: &mut String, model: &Model<'_>, def: &Def, fields: &[Field]) {
    let name = &def.name;
    let kind = screaming_snake(name);
    let _ = write!(
        buf,
        "/// `{name}` (php.ungram): one node of kind `{kind}`, its children the fields the\n\
         /// builder resolved, no scan behind any of them.\n\
         #[derive(Clone, Debug, PartialEq, Eq)]\n\
         pub struct {name} {{\n    \
             /// Where the node is in the source: from its first significant token to its last.\n    \
             pub range: TextRange,\n"
    );
    for field in fields {
        let from = field
            .mixin
            .as_ref()
            .map(|m| format!(" (`{m}`)"))
            .unwrap_or_default();
        let _ = write!(
            buf,
            "    /// `{}`: {}{from}.\n    pub {}: {},\n",
            field.name,
            field.doc,
            rust_label(&field.name),
            field.ty
        );
    }
    buf.push_str("}\n");
    let _ = write!(
        buf,
        "impl Node for {name} {{\n    \
             const KIND: SyntaxKind = SyntaxKind::{kind};\n    \
             fn range(&self) -> TextRange {{\n        self.range\n    }}\n\
         }}\n"
    );
    for trait_name in &def.traits {
        if model.class_of(trait_name) != Some(DefClass::Trait) {
            continue;
        }
        let _ = writeln!(buf, "impl {trait_name} for {name} {{");
        for field in fields
            .iter()
            .filter(|f| f.mixin.as_deref() == Some(trait_name))
        {
            let _ = write!(
                buf,
                "    fn {}(&self) -> {} {{\n        {}\n    }}\n",
                rust_label(&field.name),
                method_type(field),
                method_body(field)
            );
        }
        buf.push_str("}\n");
    }
    buf.push('\n');
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
    let _ = write!(
        buf,
        "/// `{name}` ({what}; convention A1): which node or token stands here, resolved\n\
         /// once by the builder.\n\
         #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]\n\
         pub enum {name} {{\n"
    );
    for r in node_refs {
        match ref_type(model, r) {
            Ok(ty) if ty == "TokenId" => {
                let _ = writeln!(
                    buf,
                    "    /// A bare `{r}` token (A1 token-only alternation).\n    {r}(TokenId),"
                );
            }
            Ok(ty) => {
                let _ = writeln!(buf, "    /// `{r}`.\n    {r}({ty}),");
            }
            Err(e) => errors.push(e),
        }
    }
    for (variant, kind) in token_variants {
        let _ = writeln!(buf, "    /// The `{kind}` token.\n    {variant}(TokenId),");
    }
    buf.push_str("}\n\n");
}

/// The mixin traits (A5): one method per injected field, over the nodes
/// that opted in.
fn emit_traits(buf: &mut String, model: &Model<'_>, errors: &mut Vec<Error>) {
    for info in &model.defs {
        if info.class != DefClass::Trait {
            continue;
        }
        let def = info.def;
        let _ = write!(
            buf,
            "/// Mixin `{0}` (convention A5): the fields it injects into every node that\n\
             /// opts in, read through one trait.\n\
             pub trait {0} {{\n",
            def.name
        );
        if def.rule.is_none() {
            let _ = write!(
                buf,
                "    /// The doc comment the builder attached to the declaration.\n    \
                 fn {DOC_COMMENT}(&self) -> Option<TokenId>;\n"
            );
        } else {
            for slot in slots_for(model, def, errors) {
                match Field::of(model, def, &slot, Some(&def.name)) {
                    Ok(field) => {
                        let _ = write!(
                            buf,
                            "    /// `{}`: {}.\n    fn {}(&self) -> {};\n",
                            field.name,
                            field.doc,
                            rust_label(&field.name),
                            method_type(&field)
                        );
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
        emit_struct(&mut buf, &model, def, &fields);
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
            let _ = writeln!(buf, "            SyntaxKind::{variant} => \"`{display}`\",");
        }
    }
    if grammar.has_comma_lists() {
        buf.push_str("            SyntaxKind::COMMA => \"`,`\",\n");
    }
    for keyword in grammar.keyword_tokens() {
        let _ = writeln!(
            buf,
            "            SyntaxKind::{} => \"`{keyword}`\",",
            keyword_variant(&keyword)
        );
    }
    for (class, display) in CLASS_DISPLAY {
        let _ = writeln!(
            buf,
            "            SyntaxKind::{} => \"{display}\",",
            class.to_ascii_uppercase()
        );
    }
    for (extra, display) in EXTRA_DISPLAY {
        let _ = writeln!(buf, "            SyntaxKind::{extra} => \"{display}\",");
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
