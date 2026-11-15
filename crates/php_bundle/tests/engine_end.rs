//! The tripwire the ruling of 2026-10-07 asks for: each extension closes
//! the persistent entries of its own `abi::Pool`s in its own engine-end
//! function, which `end_phase` serves in reverse bundle order, where php
//! destroys every extension's entries in `EG(persistent_list)` before
//! any `MSHUTDOWN` (`Z/Zend/zend.c:1166-1170`). That order is not
//! observable while the pools that close there are dba's, mysqli's and
//! PDO's and no extension their entries close through declares an engine
//! end of its own, which would be served before the drain that needs it.
//! A further pool-holding extension that declares an engine end, or an
//! engine end in a crate that requires one of those three, fails here,
//! and the ruling's option 2 (a phase that closes persistent entries alone,
//! served in reverse bundle order before the engine-end phase) is decided
//! then. A pool whose entries only release, with no engine end, is a
//! `Drop` (ADR-0010) and trips nothing: ext/curl's persistent shares
//! (daeef7b3) are one. Read from the sources' syntax and the
//! manifests' tables, so a crate's commit trips it whatever the features
//! of this build.

use std::path::{Path, PathBuf};

use syn::visit::{self, Visit};

/// The extensions that hold an `abi::Pool`, each closing it in its
/// engine-end function, as the ruling of 2026-10-07 left them.
const POOLED_ENDS: &[&str] = &["dba", "mysqli", "pdo"];

/// The face's pool, as a path from the crate root names it.
const POOL: [&str; 3] = ["php_ext", "abi", "Pool"];

/// One `ext/*` crate as its sources say it.
struct Crate {
    name: String,
    /// Whether its `src/` calls `.engine_end(..)` on its builder.
    engine_end: bool,
    /// Whether its `src/` names the face's `abi::Pool`, by its whole path
    /// or through what a `use` binds; a `Pool` of the crate's own
    /// (mysqlnd's arena for a fetched row set) is not one.
    pool: bool,
    /// Its `[dependencies]`, by package name.
    dependencies: toml::Table,
}

impl Crate {
    /// Whether its library links `ext/<name>`'s crate.
    fn requires(&self, name: &str) -> bool {
        self.dependencies.contains_key(&format!("php_ext_{name}"))
    }
}

/// What one file's syntax holds: the names its `use` items bind, the
/// paths they glob, and every path written outside them. A path inside
/// a macro's arguments is tokens to syn and is not read.
#[derive(Default)]
struct File {
    engine_end: bool,
    /// Each name a `use` binds, with the path it stands for.
    bound: Vec<(String, Vec<String>)>,
    /// Each path a `use` globs, `a::b` of `use a::b::*`.
    globbed: Vec<Vec<String>>,
    paths: Vec<Vec<String>>,
}

impl File {
    /// What `use` tree `tree` binds under the segments `prefix`.
    fn bind(&mut self, prefix: &mut Vec<String>, tree: &syn::UseTree) {
        let mut bound = |prefix: &[String], ident: &syn::Ident, local: &syn::Ident| {
            let mut full = prefix.to_vec();
            if ident != "self" {
                full.push(ident.to_string());
            }
            let local = if local == "self" {
                full.last().cloned().unwrap_or_default()
            } else {
                local.to_string()
            };
            self.bound.push((local, full));
        };
        match tree {
            syn::UseTree::Path(path) => {
                prefix.push(path.ident.to_string());
                self.bind(prefix, &path.tree);
                prefix.pop();
            }
            syn::UseTree::Name(name) => bound(prefix, &name.ident, &name.ident),
            syn::UseTree::Rename(rename) => bound(prefix, &rename.ident, &rename.rename),
            syn::UseTree::Glob(_) => self.globbed.push(prefix.clone()),
            syn::UseTree::Group(group) => {
                for tree in &group.items {
                    self.bind(prefix, tree);
                }
            }
        }
    }

    /// Whether a path written here is the face's pool: as written from the
    /// crate root, with its first segment read through what a `use` binds,
    /// or under a path a `use` globs.
    fn names_pool(&self) -> bool {
        let pool = |path: &[String]| path.iter().map(String::as_str).take(POOL.len()).eq(POOL);
        self.paths.iter().any(|path| {
            let Some((first, rest)) = path.split_first() else {
                return false;
            };
            pool(path)
                || self
                    .bound
                    .iter()
                    .filter(|(local, _)| local == first)
                    .any(|(_, full)| pool(&[full.as_slice(), rest].concat()))
                || self
                    .globbed
                    .iter()
                    .any(|prefix| pool(&[prefix.as_slice(), path].concat()))
        })
    }
}

impl<'ast> Visit<'ast> for File {
    fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
        self.engine_end |= call.method == "engine_end";
        visit::visit_expr_method_call(self, call);
    }

    fn visit_item_use(&mut self, item: &'ast syn::ItemUse) {
        self.bind(&mut Vec::new(), &item.tree);
    }

    fn visit_path(&mut self, path: &'ast syn::Path) {
        self.paths
            .push(path.segments.iter().map(|s| s.ident.to_string()).collect());
        visit::visit_path(self, path);
    }
}

/// Every `.rs` file under `dir`, parsed and visited, into `krate`.
fn visit_sources(dir: &Path, krate: &mut Crate) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    paths.sort();
    for path in paths {
        if path.is_dir() {
            visit_sources(&path, krate);
        } else if path.extension().is_some_and(|e| e == "rs") {
            let text = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("{} is read: {e}", path.display()));
            let syntax =
                syn::parse_file(&text).unwrap_or_else(|e| panic!("{} parses: {e}", path.display()));
            let mut file = File::default();
            file.visit_file(&syntax);
            krate.engine_end |= file.engine_end;
            krate.pool |= file.names_pool();
        }
    }
}

/// This crate's directory as cargo sets `CARGO_MANIFEST_DIR` when it runs
/// the test, never the path `env!` compiled in: cargo reuses a test binary
/// across two checkouts that share a target directory.
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
            let manifest = dir.join("Cargo.toml");
            let mut table: toml::Table = std::fs::read_to_string(&manifest)
                .map_err(|e| e.to_string())
                .and_then(|text| toml::from_str(&text).map_err(|e| e.to_string()))
                .unwrap_or_else(|e| panic!("{} parses: {e}", manifest.display()));
            let dependencies = match table.remove("dependencies") {
                Some(toml::Value::Table(dependencies)) => dependencies,
                _ => toml::Table::new(),
            };
            let mut krate = Crate {
                name: dir.file_name().unwrap().to_string_lossy().into_owned(),
                engine_end: false,
                pool: false,
                dependencies,
            };
            visit_sources(&dir.join("src"), &mut krate);
            krate
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
            c.pool && c.engine_end,
            "{name} no longer holds an abi::Pool closed in its own engine end (ADR-0010): this \
             list is to shorten"
        );
    }
    if let Some(c) = crates
        .iter()
        .find(|c| c.pool && c.engine_end && !POOLED_ENDS.contains(&c.name.as_str()))
    {
        panic!(
            "{} holds an abi::Pool and declares an engine end beside dba, mysqli and pdo: its \
             entries close in its own engine end (ADR-0010), drained in reverse bundle order \
             while the others' entries are still open, where php closes them all before any \
             MSHUTDOWN, and option 2 of the ruling of 2026-10-07 (a phase closing persistent \
             entries alone, in reverse bundle order, before the engine-end phase) is decided now",
            c.name
        );
    }
    for c in crates.iter().filter(|c| c.engine_end) {
        if let Some(pool) = POOLED_ENDS.iter().find(|&&p| p != c.name && c.requires(p)) {
            panic!(
                "{0} requires {pool} and declares an engine end, which end_phase serves before \
                 {pool}'s drain closes the entries {0} may close through: option 2 of the ruling \
                 of 2026-10-07 (a phase closing persistent entries alone, in reverse bundle \
                 order, before the engine-end phase) is decided now",
                c.name
            );
        }
    }
}
