//! `php_bundle`'s registrations held to the `ext/*` graph under the
//! `order = "registration"` of the bundle's `[[edge]]` row, as #1778
//! ruled (design §4.10, ADR-0004), the one order api-check serves
//! (#2255): the order written is php's registration order for the
//! oracle's enabled set, which the engine sorts as `zend_sort_modules`
//! does and `php_bundle`'s witness holds to the oracle, and which Cargo
//! edges do not constrain; each extension's feature switches on the
//! features of every extension it declares `Required` through
//! `ExtensionBuilder::dependencies` (php's `ZEND_MOD_REQUIRED`), so a
//! build that registers an extension registers what php requires loaded
//! beside it (#1915), read as [`declared`] says. What its crate links
//! requires nothing: `ext/sqlite3` links `ext/pdo_sqlite` for its
//! libsqlite3 binding and declares no dependency, as php's does, so its
//! feature registers no PDO.
//!
//! The feature an extension registers behind is read from the
//! `#[cfg(feature = "...")]` on its `php_ext_<name>::extension` path, or
//! on an expression, statement or item around it. Any other `cfg` there
//! is named in the reading as one the instrument does not read, never
//! taken for "always registered".

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, Result};
use syn::visit::{self, Visit};
use toml::Value;

use super::module_of;
use super::workspace::{Crates, Present};

mod declared;

use declared::Declared;

/// The one `order` the bundle's `[[edge]]` row may name (#1778, #2255).
pub(super) const REGISTRATION: &str = "registration";

/// What the instrument reads of `php_bundle`: its registrations, in the
/// order written, and its `[features]`.
#[derive(Debug)]
pub(super) struct Bundle {
    registered: Vec<Registration>,
    /// Each feature's name and the features it switches on directly.
    features: BTreeMap<String, Vec<String>>,
}

impl Bundle {
    /// Read the bundle `krate`: every `php_ext_<name>::extension` path in
    /// its `src/` files, in the order written, files in path order, and
    /// its `Cargo.toml`'s features.
    pub(super) fn read(krate: &Present) -> Result<Bundle> {
        let mut registered = Vec::new();
        for file in krate.sources()? {
            if module_of(krate, &file).is_some() {
                registered.extend(registrations(&file)?);
            }
        }
        Ok(Bundle {
            registered,
            features: features(krate)?,
        })
    }

    /// The extensions registered, each once, in the order first written.
    pub(super) fn names(&self) -> Vec<&str> {
        let mut names: Vec<&str> = Vec::new();
        for registration in &self.registered {
            if !names.contains(&registration.name.as_str()) {
                names.push(&registration.name);
            }
        }
        names
    }

    /// Every feature `on` switches on, themselves included, as cargo
    /// resolves them.
    fn closure<'a>(&'a self, on: &'a BTreeSet<String>) -> BTreeSet<&'a str> {
        let mut seen = BTreeSet::new();
        let mut next: Vec<&str> = on.iter().map(String::as_str).collect();
        while let Some(name) = next.pop() {
            if seen.insert(name) {
                next.extend(
                    self.features
                        .get(name)
                        .into_iter()
                        .flatten()
                        .map(String::as_str),
                );
            }
        }
        seen
    }
}

/// One `php_ext_<name>::extension` path in the bundle's sources.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Registration {
    /// The extension, as its manifest crate name (`ext/date`).
    name: String,
    /// The features every enclosing `#[cfg(feature = "...")]` names: the
    /// extension registers when all are on, and in every build when there
    /// are none.
    features: BTreeSet<String>,
    /// Each enclosing `cfg` of another shape, as written.
    unread: Vec<String>,
}

/// Why a registration's features do not switch on what its extension
/// requires: a `cfg` around it the instrument does not read, or a
/// declaration of its extension the instrument does not read; or, for
/// each extension it declares `Required`, one the bundle registers
/// nowhere, or registers only behind features the first one's do not
/// switch on, named together in one line per registration. What its
/// crate links is not read: a link with no declaration behind it, as
/// `ext/sqlite3`'s of `ext/pdo_sqlite`, requires nothing (#1915).
pub(super) fn unswitched(crates: &Crates, bundle: &Bundle) -> Vec<String> {
    let mut out = Vec::new();
    let mut read: BTreeMap<&str, Option<Declared>> = BTreeMap::new();
    for registration in &bundle.registered {
        let name = &registration.name;
        if !registration.unread.is_empty() {
            out.push(format!(
                "the bundle registers {name} behind {}, which api-check does not read; it reads #[cfg(feature = \"...\")] alone (design §4.10)",
                and(&registration.unread)
            ));
            continue;
        }
        let Some(krate) = crates.named(name) else {
            continue;
        };
        let declared = read
            .entry(name)
            .or_insert_with(|| declarations(name, krate, &mut out));
        let Some(declared) = declared else {
            continue;
        };
        let on = bundle.closure(&registration.features);
        // (the features not yet on, the extension they register)
        let mut missing: Vec<(String, &str)> = Vec::new();
        for required in &declared.required {
            let candidates: Vec<&Registration> = bundle
                .registered
                .iter()
                .filter(|r| &r.name == required)
                .collect();
            if candidates.is_empty() {
                out.push(format!(
                    "the bundle registers {name} but not {required}, which it requires"
                ));
                continue;
            }
            // A candidate behind an unread cfg is named on its own line.
            let needed = candidates
                .iter()
                .filter(|r| r.unread.is_empty())
                .map(|r| {
                    r.features
                        .iter()
                        .filter(|f| !on.contains(f.as_str()))
                        .map(String::as_str)
                        .collect::<Vec<_>>()
                })
                .min_by_key(Vec::len);
            if let Some(needed) = needed
                && !needed.is_empty()
            {
                missing.push((and(&needed), required));
            }
        }
        if missing.is_empty() {
            continue;
        }
        missing.sort_unstable();
        let (needed, required): (Vec<String>, Vec<&str>) = missing.into_iter().unzip();
        let own: Vec<&str> = registration.features.iter().map(String::as_str).collect();
        out.push(match own.as_slice() {
            [] => format!(
                "the bundle registers {name} behind no feature but {} behind {}, which {name} requires (design §4.10)",
                and(&required),
                and(&needed)
            ),
            [one] => format!(
                "the feature {one} registers {name} but does not switch on {}, {} {}, which {name} requires (design §4.10)",
                and(&needed),
                if needed.len() == 1 {
                    "the feature that registers"
                } else {
                    "the features that register"
                },
                and(&required)
            ),
            several => format!(
                "the features {} register {name} but do not switch on {}, which register {}, which {name} requires (design §4.10)",
                and(several),
                and(&needed),
                and(&required)
            ),
        });
    }
    out
}

/// What extension `name` declares, read once; `None`, with the reason in
/// `out`, when its sources cannot be read. A declaration the instrument
/// does not read is named in `out` and the rest are held.
fn declarations(name: &str, krate: &Present, out: &mut Vec<String>) -> Option<Declared> {
    match Declared::read(krate) {
        Ok(declared) => {
            if !declared.unread.is_empty() {
                out.push(format!(
                    "{name} declares a dependency in {} that api-check does not read; it reads a `Dependency` row or a call naming the module as a string literal and its kind as `DependencyKind::...`, in a `vec!` or array handed to `.dependencies(...)` or returned by a function called there (design §4.10)",
                    and(&declared.unread)
                ));
            }
            Some(declared)
        }
        Err(e) => {
            out.push(format!(
                "api-check could not read what {name} declares it requires: {e:#}"
            ));
            None
        }
    }
}

/// `a`, `a and b`, `a, b and c`.
fn and<S: AsRef<str>>(names: &[S]) -> String {
    let names: Vec<&str> = names.iter().map(AsRef::as_ref).collect();
    match names.as_slice() {
        [] => String::new(),
        [one] => (*one).to_owned(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// The bundle's `[features]`: each feature's name and the names of the
/// features it switches on directly. `dep:` entries and `<dep>/<feature>`
/// entries name no feature of the bundle and are left out.
fn features(krate: &Present) -> Result<BTreeMap<String, Vec<String>>> {
    let table: toml::Table = crate::read_toml(&krate.dir.join("Cargo.toml"))?;
    let Some(features) = table.get("features").and_then(Value::as_table) else {
        return Ok(BTreeMap::new());
    };
    Ok(features
        .iter()
        .map(|(name, on)| {
            let on = on
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .filter(|entry| !entry.contains(':') && !entry.contains('/'))
                .map(str::to_owned)
                .collect();
            (name.clone(), on)
        })
        .collect())
}

// ---------------------------------------------------------------- sources

/// Every `php_ext_<name>::extension` path in one file, in the order
/// written, with the `cfg`s around it.
fn registrations(file: &Path) -> Result<Vec<Registration>> {
    let text = crate::read(file)?;
    let parsed = syn::parse_file(&text).with_context(|| format!("parsing {}", file.display()))?;
    let mut calls = Calls::default();
    calls.visit_file(&parsed);
    Ok(calls.found)
}

/// One `cfg` around the node being visited.
#[derive(Debug, Clone)]
enum Cfg {
    /// `#[cfg(feature = "x")]`.
    Feature(String),
    /// Any other `cfg`, as written.
    Unread(String),
}

impl Cfg {
    /// The `cfg` an attribute is, if it is one.
    fn of(attr: &syn::Attribute) -> Option<Cfg> {
        if !attr.path().is_ident("cfg") {
            return None;
        }
        let written = match &attr.meta {
            syn::Meta::List(list) => format!("cfg({})", list.tokens),
            _ => String::from("cfg"),
        };
        let feature = attr
            .parse_args::<syn::Meta>()
            .ok()
            .and_then(|meta| match meta {
                syn::Meta::NameValue(nv) if nv.path.is_ident("feature") => match nv.value {
                    syn::Expr::Lit(syn::ExprLit {
                        lit: syn::Lit::Str(name),
                        ..
                    }) => Some(name.value()),
                    _ => None,
                },
                _ => None,
            });
        Some(feature.map_or(Cfg::Unread(written), Cfg::Feature))
    }
}

/// The registrations of one file and the `cfg`s around the node being
/// visited, outermost first. The nodes whose `cfg` is read are the ones
/// a registration list is written with: the path, a call, a method call,
/// an array, a block, a `let`, and a function, constant, static or module.
#[derive(Default)]
struct Calls {
    found: Vec<Registration>,
    around: Vec<Cfg>,
}

impl Calls {
    fn within(&mut self, attrs: &[syn::Attribute], visit: impl FnOnce(&mut Self)) {
        let before = self.around.len();
        self.around.extend(attrs.iter().filter_map(Cfg::of));
        visit(self);
        self.around.truncate(before);
    }
}

/// One `visit_<node>` per node named, reading the node's `cfg`s around
/// the walk under it ([`Calls::within`]).
macro_rules! within_cfgs {
    ($ast:lifetime; $($visit:ident($node:ty)),* $(,)?) => {$(
        fn $visit(&mut self, node: &$ast $node) {
            self.within(&node.attrs, |c| visit::$visit(c, node));
        }
    )*};
}

impl<'ast> Visit<'ast> for Calls {
    within_cfgs!('ast;
        visit_expr_path(syn::ExprPath),
        visit_expr_call(syn::ExprCall),
        visit_expr_method_call(syn::ExprMethodCall),
        visit_expr_array(syn::ExprArray),
        visit_expr_block(syn::ExprBlock),
        visit_local(syn::Local),
        visit_item_fn(syn::ItemFn),
        visit_item_const(syn::ItemConst),
        visit_item_static(syn::ItemStatic),
        visit_item_mod(syn::ItemMod),
    );

    fn visit_path(&mut self, path: &'ast syn::Path) {
        let segments: Vec<String> = path.segments.iter().map(|s| s.ident.to_string()).collect();
        if let [krate, item] = segments.as_slice()
            && item == "extension"
            && let Some(name) = krate.strip_prefix("php_ext_")
        {
            let mut registration = Registration {
                name: format!("ext/{name}"),
                features: BTreeSet::new(),
                unread: Vec::new(),
            };
            for cfg in &self.around {
                match cfg {
                    Cfg::Feature(feature) => {
                        registration.features.insert(feature.clone());
                    }
                    Cfg::Unread(written) => registration.unread.push(written.clone()),
                }
            }
            self.found.push(registration);
        }
        visit::visit_path(self, path);
    }
}
