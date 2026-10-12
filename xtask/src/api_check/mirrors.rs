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
//! stands; a file the text `mirrors` is not in is not parsed.
//!
//! The header is read a line at a time and not compiled. A line declares
//! `sym` when, outside a comment line and a preprocessor line, it carries
//! one of the three macros as a word and the last identifier after it,
//! before the first `(`, `;`, `[`, `=` or `{`, is `sym`. Not read, so a
//! marker naming one fails: a declaration whose name stands on a later
//! line than its macro, one whose name follows a macro taking arguments
//! (`ZEND_ATTRIBUTE_FORMAT(..)`), and a function pointer's.

use std::fs;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};
use syn::punctuated::Punctuated;
use syn::visit::{self, Visit};
use syn::{Attribute, ImplItem, Item, LitStr, Token, TraitItem, Type};

use super::workspace::Present;

/// The macros a declaration a marker names may carry.
const MACROS: [&str; 3] = ["PHPAPI", "PDO_API", "ZEND_API"];

/// One `#[mirrors]` as the sources write it.
#[derive(Debug)]
pub(super) struct Marker {
    /// The file, relative to the crate, and the item's path within it:
    /// `src/rng.rs`'s `Rng::default_engine`.
    file: PathBuf,
    item: String,
    /// The header and the symbol, or why the arguments are not those.
    named: Result<(String, String), String>,
}

/// The markers under `krate`'s `src/`.
pub(super) fn markers(krate: &Present) -> Result<Vec<Marker>> {
    let src = krate.dir.join("src");
    let mut found = Found::default();
    for file in krate.sources()?.into_iter().filter(|f| f.starts_with(&src)) {
        let text =
            fs::read_to_string(&file).with_context(|| format!("reading {}", file.display()))?;
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
pub(super) fn hold(markers: &[Marker], tree: &Path, tag: &str) -> Result<Vec<String>> {
    let mut problems = Vec::new();
    for marker in markers {
        let at = format!("{}'s `{}`", marker.file.display(), marker.item);
        let (header, symbol) = match &marker.named {
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
            Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }
    Ok(problems)
}

/// Whether `header`, a C header's text, declares `symbol` with one of
/// [`MACROS`], as the module's rustdoc says a line is read.
fn declares(header: &str, symbol: &str) -> bool {
    header.lines().any(|line| {
        let line = line.trim_start();
        if ["//", "/*", "*", "#"].iter().any(|p| line.starts_with(p)) {
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

    fn inside(&mut self, owner: String, walk: impl FnOnce(&mut Found)) {
        let outer = self.owner.replace(owner);
        walk(self);
        self.owner = outer;
    }
}

/// The marker's header and symbol: two non-empty string literals.
fn named(attr: &Attribute) -> Result<(String, String), String> {
    let parsed = attr
        .parse_args_with(Punctuated::<LitStr, Token![,]>::parse_terminated)
        .map_err(|e| format!("does not parse: {e}"))?;
    match parsed
        .iter()
        .map(LitStr::value)
        .collect::<Vec<_>>()
        .as_slice()
    {
        [header, symbol] if !header.is_empty() && !symbol.is_empty() => {
            Ok((header.clone(), symbol.clone()))
        }
        _ => Err("names no header and symbol".to_owned()),
    }
}

impl<'ast> Visit<'ast> for Found {
    fn visit_item(&mut self, item: &'ast Item) {
        let named = match item {
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
        if let Some((attrs, ident)) = named {
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
    use super::declares;

    const HEADER: &str = "\
#define PHPAPI_FAKE php_random_default_engine
/* PHPAPI void php_commented(void); */
 * PHPAPI void php_in_a_block(void);
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
            "php_mt_rand",
            "zend_long",
            "min",
            "php_on_the_next_line",
        ] {
            assert!(!declares(HEADER, symbol), "{symbol}");
        }
    }
}
