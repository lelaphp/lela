//! The third layer of #2020's ruling (#2353): an item of an extension's
//! surface may carry `#[mirrors("ext/x/php_x.h", "sym")]`, the php-src
//! declaration it stands for, and the hold reads the pinned php-src for it.
//! The header is named from the tree's root, as a dependent extension's
//! `#include` spells it, and must be a file there that declares `sym` with
//! `PHPAPI`, `PDO_API` or `ZEND_API` (design §4.10: "what php-src declares
//! `PHPAPI` in a header the dependent extension includes"). An item with
//! no marker is not held, so the marker lands crate by crate.
//!
//! Every marker under the crate's `src/` is read, on whatever item it
//! stands, in a crate with a surface or without one: an attribute whose
//! path ends in `mirrors`, so one renamed by `use .. as` is not read, and
//! a file the text `mirrors` is not in is not parsed.
//!
//! The pinned tree is found once per run, by the first marker that asks
//! ([`Pinned`]). Where it is at neither place [`Source::tree`] tries, the
//! markers are not held and the reading says so, unless the machine
//! promised the tree, when that fails it; a tree standing there and
//! refused, at another commit, fails it always.
//!
//! The header is read with its comments taken out, a line at a time, and
//! not compiled or preprocessed, so a declaration under `#if 0` counts. A
//! line declares `sym` when, not a preprocessor line, it carries one of
//! the three macros as a word and the last identifier after it, before
//! the first `(`, `;`, `[`, `=` or `{`, is `sym`. A declaration whose name
//! stands on a later line than its macro is not read, so a marker naming
//! it fails. Where that identifier is not the declared name, as in a
//! function pointer's `int (*hook)(..)` or after an attribute macro with
//! arguments, `ZEND_ATTRIBUTE_ALLOC_SIZE(1) void *_emalloc(..)`, the line
//! is read as declaring the type or the macro, which no marker names.

use std::cell::OnceCell;
use std::fs;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};
use syn::punctuated::Punctuated;
use syn::visit::{self, Visit};
use syn::{Attribute, ImplItem, Item, LitStr, Token, TraitItem, Type};

use super::surfaces::Held;
use super::workspace::Present;
use crate::oracle::{Promise, Promised, Source};

/// The macros a declaration a marker names may carry.
const MACROS: [&str; 3] = ["PHPAPI", "PDO_API", "ZEND_API"];

/// One `#[mirrors]` as the sources write it.
#[derive(Debug)]
pub(super) struct Marker {
    /// The file, relative to the crate, and the item's path within it:
    /// `src/rng.rs`'s `Rng::default_engine`.
    file: PathBuf,
    item: String,
    /// What the marker names, or why its arguments name nothing.
    named: Result<Declaration, String>,
}

/// What a marker names: a header, a path from php-src's root, and the
/// symbol it declares.
#[derive(Debug)]
struct Declaration {
    header: String,
    symbol: String,
}

/// The pin's php-src, which every marker of a run is held against.
#[derive(Debug, Default)]
pub(super) struct Pinned {
    source: Source,
    /// Where the tree is on this machine, asked once, by the first marker
    /// that needs it.
    tree: OnceCell<Tree>,
}

/// The pinned php-src as this machine has it.
#[derive(Debug)]
enum Tree {
    At(PathBuf),
    /// At neither place [`Source::tree`] tries: the markers are not held.
    Absent(String),
    /// At one of them, and refused there, at another commit: the markers
    /// fail, since a tree at the wrong revision holds them to the wrong
    /// declarations.
    Refused(String),
}

impl Tree {
    fn of(source: &Source) -> Tree {
        match source.tree() {
            Ok(tree) => Tree::At(tree),
            Err(e)
                if [source.named(), Some(source.path())]
                    .into_iter()
                    .flatten()
                    .any(Path::is_dir) =>
            {
                Tree::Refused(format!("{e:#}"))
            }
            Err(e) => Tree::Absent(format!("{e:#}")),
        }
    }
}

impl Pinned {
    pub(super) fn new(source: &Source) -> Pinned {
        Pinned {
            source: source.clone(),
            tree: OnceCell::new(),
        }
    }

    /// Hold `krate`'s markers against the tree: a problem per marker it
    /// does not bear out, and a note of the count where all hold.
    pub(super) fn hold(&self, krate: &Present) -> Result<Held> {
        let mut held = Held::default();
        let markers = markers(krate)?;
        if markers.is_empty() {
            return Ok(held);
        }
        let n = markers.len();
        let tag = &self.source.tag;
        match self.tree.get_or_init(|| Tree::of(&self.source)) {
            Tree::At(tree) => {
                held.problems = hold(&markers, tree, tag);
                if held.problems.is_empty() {
                    let names = if n == 1 {
                        "marker names"
                    } else {
                        "markers name"
                    };
                    held.notes.push(format!(
                        "{n} #[mirrors] {names} what php-src at {tag} declares"
                    ));
                }
            }
            Tree::Refused(why) => held
                .problems
                .push(format!("{n} #[mirrors] held against nothing: {why}")),
            Tree::Absent(why) if Promised::load()?.holds(Promise::Tree) => {
                held.problems.push(format!(
                    "{n} #[mirrors] held against nothing: {why}, and this machine promised the {}",
                    Promise::Tree.word()
                ));
            }
            Tree::Absent(why) => held.notes.push(format!("{n} #[mirrors] not held: {why}")),
        }
        Ok(held)
    }
}

/// The markers under `krate`'s `src/`.
fn markers(krate: &Present) -> Result<Vec<Marker>> {
    let src = krate.dir.join("src");
    let mut found = Found::default();
    for file in krate.sources()?.into_iter().filter(|f| f.starts_with(&src)) {
        let text = crate::read(&file)?;
        if !text.contains("mirrors") {
            continue;
        }
        let parsed =
            syn::parse_file(&text).with_context(|| format!("parsing {}", file.display()))?;
        found.file = krate.relative(&file).to_path_buf();
        found.visit_file(&parsed);
    }
    Ok(found.markers)
}

/// What `markers` holds against the tree at `tree`, of tag `tag`: one
/// problem per marker whose header the tree lacks or does not declare its
/// symbol in.
fn hold(markers: &[Marker], tree: &Path, tag: &str) -> Vec<String> {
    let mut problems = Vec::new();
    for marker in markers {
        let at = format!("{}'s `{}`", marker.file.display(), marker.item);
        let Declaration { header, symbol } = match &marker.named {
            Ok(named) => named,
            Err(why) => {
                problems.push(format!("{at} carries a #[mirrors] that {why}"));
                continue;
            }
        };
        if !Path::new(header)
            .components()
            .all(|c| matches!(c, Component::Normal(_)))
        {
            problems.push(format!(
                "{at} mirrors {header}, which is no path from php-src's root"
            ));
            continue;
        }
        let path = tree.join(header);
        match fs::read_to_string(&path) {
            Ok(text) if declares(&text, symbol) => {}
            Ok(_) => problems.push(format!(
                "{at} mirrors `{symbol}`, which {header} at {tag} declares with none of {}",
                MACROS.join(", ")
            )),
            Err(e) if e.kind() == ErrorKind::NotFound => {
                problems.push(format!(
                    "{at} mirrors {header}, which php-src at {tag} lacks"
                ));
            }
            Err(e) => problems.push(format!(
                "{at} mirrors {header}, which php-src at {tag} holds as no header it reads: {e}"
            )),
        }
    }
    problems
}

/// Whether `header`, a C header's text, declares `symbol` with one of
/// [`MACROS`], as the module's rustdoc says a line is read.
fn declares(header: &str, symbol: &str) -> bool {
    uncommented(header).lines().any(|line| {
        let line = line.trim_start();
        if line.starts_with('#') {
            return false;
        }
        let Some(after) = MACROS.iter().filter_map(|m| word_end(line, m)).min() else {
            return false;
        };
        let rest = &line[after..];
        let head = rest
            .find(['(', ';', '[', '=', '{'])
            .map_or(rest, |end| &rest[..end]);
        head.split(|c: char| !is_ident(c))
            .rfind(|word| !word.is_empty())
            == Some(symbol)
    })
}

/// `header` with each `/* .. */` and `// ..` comment taken out, a block
/// comment's lines kept as empty ones. String literals are not told apart,
/// which a declaration line does not carry.
fn uncommented(header: &str) -> String {
    let mut out = String::with_capacity(header.len());
    let mut rest = header;
    while let Some(at) = rest.find(['/']) {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        if let Some(after) = rest.strip_prefix("/*") {
            let end = after.find("*/").map_or(after.len(), |e| e + 2);
            out.extend(after[..end].chars().filter(|&c| c == '\n'));
            rest = &after[end..];
        } else if let Some(after) = rest.strip_prefix("//") {
            rest = &after[after.find('\n').unwrap_or(after.len())..];
        } else {
            out.push('/');
            rest = &rest[1..];
        }
    }
    out.push_str(rest);
    out
}

/// Where the first occurrence of `word` in `line` as a whole identifier
/// ends.
fn word_end(line: &str, word: &str) -> Option<usize> {
    line.match_indices(word).find_map(|(at, _)| {
        let end = at + word.len();
        let before = line[..at].chars().next_back().is_none_or(|c| !is_ident(c));
        let after = line[end..].chars().next().is_none_or(|c| !is_ident(c));
        (before && after).then_some(end)
    })
}

fn is_ident(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

/// The visitor gathering markers, with the type or trait whose items it
/// is inside.
#[derive(Default)]
struct Found {
    file: PathBuf,
    owner: Option<String>,
    markers: Vec<Marker>,
}

impl Found {
    /// Gather the markers among `attrs`, the attributes of the item
    /// `name`.
    fn take(&mut self, attrs: &[Attribute], name: &str) {
        for attr in attrs.iter().filter(|a| {
            a.path()
                .segments
                .last()
                .is_some_and(|s| s.ident == "mirrors")
        }) {
            let item = match &self.owner {
                Some(owner) => format!("{owner}::{name}"),
                None => name.to_owned(),
            };
            self.markers.push(Marker {
                file: self.file.clone(),
                item,
                named: named(attr),
            });
        }
    }

    /// `walk` with `owner` as the type or trait whose items are visited.
    fn inside(&mut self, owner: String, walk: impl FnOnce(&mut Found)) {
        let outer = self.owner.replace(owner);
        walk(self);
        self.owner = outer;
    }
}

/// The marker's header and symbol: two non-empty string literals, the
/// shape `php_macros::marker` refuses any other of at compile time; read
/// again here, since the harness builds nothing it holds (ADR-0007 keeps
/// the macro crate a leaf, so no reader is shared).
fn named(attr: &Attribute) -> Result<Declaration, String> {
    let parsed = attr
        .parse_args_with(Punctuated::<LitStr, Token![,]>::parse_terminated)
        .map_err(|e| format!("does not parse: {e}"))?;
    match parsed
        .iter()
        .map(LitStr::value)
        .collect::<Vec<_>>()
        .as_slice()
    {
        [header, symbol] if !header.is_empty() && !symbol.is_empty() => Ok(Declaration {
            header: header.clone(),
            symbol: symbol.clone(),
        }),
        _ => Err("names no header and symbol".to_owned()),
    }
}

impl<'ast> Visit<'ast> for Found {
    fn visit_item(&mut self, item: &'ast Item) {
        let attributed = match item {
            Item::Const(i) => Some((&i.attrs, &i.ident)),
            Item::Enum(i) => Some((&i.attrs, &i.ident)),
            Item::Fn(i) => Some((&i.attrs, &i.sig.ident)),
            Item::Static(i) => Some((&i.attrs, &i.ident)),
            Item::Struct(i) => Some((&i.attrs, &i.ident)),
            Item::Trait(i) => Some((&i.attrs, &i.ident)),
            Item::Type(i) => Some((&i.attrs, &i.ident)),
            Item::Union(i) => Some((&i.attrs, &i.ident)),
            _ => None,
        };
        if let Some((attrs, ident)) = attributed {
            self.take(attrs, &ident.to_string());
        }
        match item {
            Item::Impl(i) => self.inside(type_name(&i.self_ty), |f| visit::visit_item_impl(f, i)),
            Item::Trait(t) => self.inside(t.ident.to_string(), |f| visit::visit_item_trait(f, t)),
            _ => visit::visit_item(self, item),
        }
    }

    fn visit_impl_item(&mut self, item: &'ast ImplItem) {
        match item {
            ImplItem::Const(i) => self.take(&i.attrs, &i.ident.to_string()),
            ImplItem::Fn(i) => self.take(&i.attrs, &i.sig.ident.to_string()),
            ImplItem::Type(i) => self.take(&i.attrs, &i.ident.to_string()),
            _ => {}
        }
        visit::visit_impl_item(self, item);
    }

    fn visit_trait_item(&mut self, item: &'ast TraitItem) {
        match item {
            TraitItem::Const(i) => self.take(&i.attrs, &i.ident.to_string()),
            TraitItem::Fn(i) => self.take(&i.attrs, &i.sig.ident.to_string()),
            TraitItem::Type(i) => self.take(&i.attrs, &i.ident.to_string()),
            _ => {}
        }
        visit::visit_trait_item(self, item);
    }
}

/// The name an impl's items are listed under: its self type's last
/// segment, `Rng` for `impl crate::rng::Rng`, and `impl` for a self type
/// that is no path.
fn type_name(ty: &Type) -> String {
    match ty {
        Type::Path(p) => p.path.segments.last().map(|s| s.ident.to_string()),
        Type::Reference(r) => Some(type_name(&r.elem)),
        _ => None,
    }
    .unwrap_or_else(|| "impl".to_owned())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use super::{Declaration, Marker, declares, hold};

    const HEADER: &str = "\
#define PHPAPI_FAKE php_random_default_engine
/* PHPAPI void php_commented(void); */
/*
   PHPAPI void php_in_a_block(void);
 */
int php_code; /* PHPAPI void php_trailing(void); */
PHPAPI void php_after_a_comment(void); // PHPAPI void php_line_comment(void);
PHPAPI const php_random_algo *php_random_default_algo(void);
extern PHPAPI const php_random_algo php_random_algo_mt19937;
PHPAPI zend_long php_mt_rand_range(zend_long min, zend_long max);
PDO_API void php_pdo_free_statement(pdo_stmt_t *stmt);
ZEND_API zend_result ZEND_FASTCALL zend_parse_arg(uint32_t arg_num);
PHPAPI zend_string *php_split(zend_string *str,
static inline php_random_algo_with_state php_random_default_engine(void)
PHPAPI
php_on_the_next_line(void);
";

    #[test]
    fn a_line_with_the_macro_declares_the_last_name_before_its_parenthesis() {
        for symbol in [
            "php_random_default_algo",
            "php_random_algo_mt19937",
            "php_mt_rand_range",
            "php_pdo_free_statement",
            "zend_parse_arg",
            "php_split",
            "php_after_a_comment",
        ] {
            assert!(declares(HEADER, symbol), "{symbol}");
        }
    }

    #[test]
    fn a_name_the_macro_does_not_declare_on_its_line_is_not_declared() {
        for symbol in [
            "php_random_default_engine",
            "php_commented",
            "php_in_a_block",
            "php_trailing",
            "php_line_comment",
            "php_mt_rand",
            "zend_long",
            "min",
            "php_on_the_next_line",
        ] {
            assert!(!declares(HEADER, symbol), "{symbol}");
        }
    }

    /// A marker whose arguments name nothing, one whose header is no path
    /// from the root, and one whose header is a directory each fail it,
    /// beside one that holds.
    #[test]
    fn a_marker_the_tree_cannot_bear_out_is_a_problem_naming_why() {
        let tree = crate::scratch::Tree::new("mirrors-hold").unwrap();
        fs::create_dir_all(tree.path().join("ext/random")).unwrap();
        fs::write(
            tree.path().join("ext/random/php_random.h"),
            "PHPAPI const php_random_algo *php_random_default_algo(void);\n",
        )
        .unwrap();
        let marker = |header: &str, symbol: &str| Marker {
            file: PathBuf::from("src/lib.rs"),
            item: "f".to_owned(),
            named: Ok(Declaration {
                header: header.to_owned(),
                symbol: symbol.to_owned(),
            }),
        };
        let markers = [
            marker("ext/random/php_random.h", "php_random_default_algo"),
            Marker {
                named: Err("names no header and symbol".to_owned()),
                ..marker("", "")
            },
            marker("../php_random.h", "php_random_default_algo"),
            marker("ext/random", "php_random_default_algo"),
        ];
        let problems = hold(&markers, Path::new(tree.path()), "php-tag");
        assert_eq!(problems.len(), 3, "{problems:?}");
        assert_eq!(
            problems[0],
            "src/lib.rs's `f` carries a #[mirrors] that names no header and symbol"
        );
        assert_eq!(
            problems[1],
            "src/lib.rs's `f` mirrors ../php_random.h, which is no path from php-src's root"
        );
        assert!(
            problems[2].starts_with(
                "src/lib.rs's `f` mirrors ext/random, which php-src at php-tag holds as no header it reads: "
            ),
            "{}",
            problems[2]
        );
    }

    /// A tree at neither place is absent, and one at a place that refuses
    /// it, here a `.git` git cannot read, is refused, never absent.
    #[test]
    fn a_tree_standing_where_the_pin_looks_and_refused_is_not_absent() {
        use super::Tree;

        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let text = fs::read_to_string(root.join("oracle.toml")).unwrap();
        let text = text.replace("path = \"/Users/", "path = \"/nonexistent/");
        let pin = crate::oracle::Pin::parse(&text).unwrap();
        assert!(matches!(Tree::of(&pin.source), Tree::Absent(_)));

        let scratch = crate::scratch::Tree::new("mirrors-refused-tree").unwrap();
        fs::write(scratch.path().join(".git"), "not a checkout").unwrap();
        let pin = pin.on_this_machine(Some(scratch.path().to_path_buf()));
        assert!(matches!(Tree::of(&pin.source), Tree::Refused(_)));
    }
}
