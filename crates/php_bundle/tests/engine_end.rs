//! The tripwire #1960's ruling asks for: each extension closes the
//! persistent entries of its own `abi::Pool`s in its own engine-end
//! function, which `end_phase` serves in reverse bundle order, where php
//! destroys every extension's entries in `EG(persistent_list)` before
//! any `MSHUTDOWN` (`Z/Zend/zend.c:1166-1170`). That order is not
//! observable while the pools that close there are dba's, mysqli's and
//! PDO's and no extension their entries close through declares an engine
//! end of its own, which would be served before the drain that needs it.
//! A further pool-holding extension that declares an engine end, or an
//! engine end in a crate that requires one of those three, fails here,
//! and option 2 of #1960 (a phase that closes persistent entries alone,
//! served in reverse bundle order before the engine-end phase) is decided
//! then. A pool whose entries only release, with no engine end, is a
//! `Drop` (ADR-0010) and trips nothing: ext/curl's persistent shares
//! (daeef7b3, #1334) are one. Read from the sources' syntax and the
//! manifests' tables, so a crate's commit trips it whatever the features
//! of this build.

use std::path::{Path, PathBuf};

use syn::visit::{self, Visit};

/// The extensions that hold an `abi::Pool`, each closing it in its
/// engine-end function, as #1960's ruling and #1961 left them.
const POOLED_ENDS: &[&str] = &["dba", "mysqli", "pdo"];

/// The face's pool, as a path names it.
const POOL: [&str; 3] = ["php_ext", "abi", "Pool"];

/// One `ext/*` crate as its sources say it.
struct Crate {
    name: String,
    /// What its `src/` names, read from the syntax.
    named: Named,
    /// Its `[dependencies]`, by package name.
    dependencies: toml::Table,
}

impl Crate {
    fn ends(&self) -> bool {
        self.named.ends
    }

    /// Whether it names the face's `abi::Pool`, imported or by its path; a
    /// `Pool` of its own (mysqlnd's arena for a fetched row set) is not one.
    fn pools(&self) -> bool {
        self.named.pools
    }

    /// Whether its library links `ext/<name>`'s crate.
    fn requires(&self, name: &str) -> bool {
        self.dependencies.contains_key(&format!("php_ext_{name}"))
    }
}

/// What one crate's sources name: a `.engine_end(..)` call on its builder,
/// and the face's `Pool`.
#[derive(Default)]
struct Named {
    ends: bool,
    pools: bool,
}

impl Named {
    /// Whether `use` tree `tree`, under the segments `prefix`, imports the
    /// face's `Pool`.
    fn imports_pool(prefix: &mut Vec<String>, tree: &syn::UseTree) -> bool {
        let names_pool = |prefix: &[String], ident: &syn::Ident| {
            ident == POOL[2]
                && prefix
                    .iter()
                    .map(String::as_str)
                    .eq(POOL[..2].iter().copied())
        };
        match tree {
            syn::UseTree::Path(path) => {
                prefix.push(path.ident.to_string());
                let found = Named::imports_pool(prefix, &path.tree);
                prefix.pop();
                found
            }
            syn::UseTree::Name(name) => names_pool(prefix, &name.ident),
            syn::UseTree::Rename(rename) => names_pool(prefix, &rename.ident),
            syn::UseTree::Group(group) => group
                .items
                .iter()
                .any(|tree| Named::imports_pool(prefix, tree)),
            syn::UseTree::Glob(_) => false,
        }
    }
}

impl<'ast> Visit<'ast> for Named {
    fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
        self.ends |= call.method == "engine_end";
        visit::visit_expr_method_call(self, call);
    }

    fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
        self.pools |= Named::imports_pool(&mut Vec::new(), &item.tree);
    }

    fn visit_path(&mut self, path: &'ast syn::Path) {
        self.pools |= path
            .segments
            .iter()
            .map(|segment| segment.ident.to_string())
            .take(POOL.len())
            .eq(POOL);
        visit::visit_path(self, path);
    }
}

/// Every `.rs` file under `dir`, parsed and visited into `named`.
fn sources(dir: &Path, named: &mut Named) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    paths.sort();
    for path in paths {
        if path.is_dir() {
            sources(&path, named);
        } else if path.extension().is_some_and(|e| e == "rs") {
            let text = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("{} is read: {e}", path.display()));
            let file =
                syn::parse_file(&text).unwrap_or_else(|e| panic!("{} parses: {e}", path.display()));
            named.visit_file(&file);
        }
    }
}

/// This crate's directory as cargo sets `CARGO_MANIFEST_DIR` when it runs
/// the test, never the path `env!` compiled in: cargo reuses a test binary
/// across two checkouts that share a target directory (#2790).
fn manifest_dir() -> PathBuf {
    std::env::var_os("CARGO_MANIFEST_DIR")
        .map(PathBuf::from)
        .expect("cargo sets CARGO_MANIFEST_DIR for a test it runs")
}

fn crates() -> Vec<Crate> {
    let ext = manifest_dir().join("../../ext");
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(&ext)
        .expect("ext/ is read")
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.join("Cargo.toml").is_file())
        .collect();
    dirs.sort();
    dirs.into_iter()
        .map(|dir| {
            let mut named = Named::default();
            sources(&dir.join("src"), &mut named);
            let manifest = dir.join("Cargo.toml");
            let mut table: toml::Table = std::fs::read_to_string(&manifest)
                .map_err(|e| e.to_string())
                .and_then(|text| toml::from_str(&text).map_err(|e| e.to_string()))
                .unwrap_or_else(|e| panic!("{} parses: {e}", manifest.display()));
            let dependencies = match table.remove("dependencies") {
                Some(toml::Value::Table(dependencies)) => dependencies,
                _ => toml::Table::new(),
            };
            Crate {
                name: dir.file_name().unwrap().to_string_lossy().into_owned(),
                named,
                dependencies,
            }
        })
        .collect()
}

#[test]
fn dba_mysqli_and_pdo_alone_close_a_pool_in_an_engine_end_as_1960_ruled() {
    let crates = crates();
    assert!(crates.len() > 50, "ext/ read: {} crates", crates.len());
    for name in POOLED_ENDS {
        let c = crates
            .iter()
            .find(|c| c.name == *name)
            .unwrap_or_else(|| panic!("ext/{name} is read"));
        assert!(
            c.pools() && c.ends(),
            "{name} no longer holds an abi::Pool closed in its own engine end, as #1961 left it \
             (ADR-0010): this list is to shorten"
        );
    }
    if let Some(c) = crates
        .iter()
        .find(|c| c.pools() && c.ends() && !POOLED_ENDS.contains(&c.name.as_str()))
    {
        panic!(
            "{} holds an abi::Pool and declares an engine end beside dba, mysqli and pdo: its \
             entries close in its own engine end (ADR-0010), drained in reverse bundle order \
             while the others' entries are still open, where php closes them all before any \
             MSHUTDOWN, and #1960's option 2 (a phase closing persistent entries alone, in \
             reverse bundle order, before the engine-end phase) is decided now",
            c.name
        );
    }
    for c in crates.iter().filter(|c| c.ends()) {
        if let Some(pool) = POOLED_ENDS.iter().find(|&&p| p != c.name && c.requires(p)) {
            panic!(
                "{0} requires {pool} and declares an engine end, which end_phase serves before \
                 {pool}'s drain closes the entries {0} may close through: #1960's option 2 (a \
                 phase closing persistent entries alone, in reverse bundle order, before the \
                 engine-end phase) is decided now",
                c.name
            );
        }
    }
}
