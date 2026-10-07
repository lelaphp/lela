//! The tripwire #1960's ruling asks for: each extension closes the
//! persistent entries of its own `api::Pool`s in its own engine-end
//! function, which `end_phase` serves in reverse bundle order, where php
//! destroys every extension's entries in `EG(persistent_list)` before
//! any `MSHUTDOWN` (`Z/Zend/zend.c:1166-1170`). That order is not
//! observable while the pools that close there are dba's, mysqli's and
//! PDO's and no extension their entries close through declares an engine
//! end of its own, which would be served before the drain that needs it.
//! A new pool, or an engine end in a crate that requires one of those
//! three, fails here, and option 2 of #1960 (a phase that closes
//! persistent entries alone, served in reverse bundle order before the
//! engine-end phase) is decided then. Read from the sources, as
//! `configure.rs` reads them, so a crate's commit trips it whatever the
//! features of this build.

use std::path::{Path, PathBuf};

/// The extensions that hold an `api::Pool`, each closing it in its
/// engine-end function, as #1960's ruling and #1961 left them.
const POOLED_ENDS: &[&str] = &["dba", "mysqli", "pdo"];

/// One `ext/*` crate as its sources say it.
struct Crate {
    name: String,
    /// Every `.rs` file under its `src/`, one after another.
    source: String,
    manifest: String,
}

impl Crate {
    fn ends(&self) -> bool {
        self.source.contains(".engine_end(")
    }

    /// Whether it names `api::Pool`, as an engine state's field or the
    /// pool it builds; a `Pool` of its own (lexbor's arena) is not one.
    fn pools(&self) -> bool {
        let ident = |c: char| c.is_alphanumeric() || c == '_';
        let mut named = false;
        for (at, _) in self.source.match_indices("Pool") {
            let (before, after) = (&self.source[..at], &self.source[at + 4..]);
            if before.ends_with(ident) || after.starts_with(ident) {
                continue;
            }
            if before.ends_with("struct ") {
                return false;
            }
            named |= after.starts_with('<') || after.starts_with("::new(");
        }
        named
    }

    /// Whether its library links `ext/<name>`'s crate.
    fn requires(&self, name: &str) -> bool {
        let dependencies = self
            .manifest
            .split("[dependencies]")
            .nth(1)
            .and_then(|rest| rest.split("\n[").next())
            .unwrap_or_default();
        dependencies.contains(&format!("php_ext_{name} "))
    }
}

fn sources(dir: &Path, into: &mut String) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    paths.sort();
    for path in paths {
        if path.is_dir() {
            sources(&path, into);
        } else if path.extension().is_some_and(|e| e == "rs") {
            into.push_str(&std::fs::read_to_string(&path).unwrap_or_default());
        }
    }
}

fn crates() -> Vec<Crate> {
    let ext = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../ext");
    let mut dirs: Vec<PathBuf> = std::fs::read_dir(&ext)
        .expect("ext/ is read")
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.join("Cargo.toml").is_file())
        .collect();
    dirs.sort();
    dirs.into_iter()
        .map(|dir| {
            let mut source = String::new();
            sources(&dir.join("src"), &mut source);
            Crate {
                name: dir.file_name().unwrap().to_string_lossy().into_owned(),
                source,
                manifest: std::fs::read_to_string(dir.join("Cargo.toml")).unwrap_or_default(),
            }
        })
        .collect()
}

#[test]
fn only_dba_mysqli_and_pdo_hold_a_pool_each_closed_in_its_engine_end_as_1960_ruled() {
    let crates = crates();
    assert!(crates.len() > 50, "ext/ read: {} crates", crates.len());
    let pooled: Vec<&Crate> = crates.iter().filter(|c| c.pools()).collect();
    assert_eq!(
        pooled.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(),
        POOLED_ENDS,
        "the extensions holding an api::Pool are no longer dba, mysqli and pdo: a new one closes \
         its entries in its own engine end (ADR-0010), drained in reverse bundle order while the \
         others' entries are still open, where php closes them all before any MSHUTDOWN, and \
         #1960's option 2 (a phase closing persistent entries alone, in reverse bundle order, \
         before the engine-end phase) is decided now; one that stopped is this list to shorten"
    );
    for c in pooled {
        assert!(
            c.ends(),
            "{} holds an api::Pool and declares no engine end, so its idle entries are dropped \
             with the engine state, never closed through the extension (ADR-0010, #1961)",
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
