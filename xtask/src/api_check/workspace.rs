//! The crates that exist on disk, found through the manifest's `[[crate]]`
//! rows: a row whose directory holds a `Cargo.toml` is present, and its
//! workspace dependencies are read from that file. Nothing is built and
//! cargo is not run; the instrument reads manifests and sources.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use toml::Value;

use crate::layout::{Layout, normalize};
use crate::manifest::Manifest;

/// One crate that exists, by its manifest name (`php_heap`, `ext/random`,
/// `xtask`).
#[derive(Debug, Clone)]
pub struct Present {
    pub name: String,
    /// The crate's directory, absolute.
    pub dir: PathBuf,
    /// The workspace root the crate is under, which git lists its sources
    /// from.
    pub root: PathBuf,
    /// The cargo package name; `php_ext_random` for `ext/random`.
    pub package: String,
    pub deps: Vec<Dep>,
    /// The packages from outside the workspace it depends on, of every
    /// kind, by package name (`openssl-sys`).
    pub external: BTreeSet<String>,
    /// Whether its `Cargo.toml` carries `[lints] workspace = true`, the one
    /// way the root manifest's `[workspace.lints]` reaches a member.
    pub inherits_lints: bool,
}

/// One dependency on another workspace crate, of any kind (normal, dev,
/// build, target-specific).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dep {
    /// The manifest name of the crate depended on.
    pub name: String,
    /// The identifier the code uses for it: the dependency key, with `-`
    /// as `_`.
    pub alias: String,
    /// Whether the crate's library links it: a `[dependencies]` entry,
    /// plain or under a target. A crate that only dev- or build-depends
    /// on another does not need it at run time. An `optional = true`
    /// entry counts as linked, since which of the crate's own features a
    /// dependent turns on is not read here; no `ext/*` crate has one.
    pub linked: bool,
}

impl Present {
    /// The identifier the crate's own code is reached by from a dependent
    /// that does not rename it.
    pub fn ident(&self) -> String {
        self.package.replace('-', "_")
    }

    /// The manifest name of the workspace crate `ident` names in this
    /// crate's code, if it is a declared dependency.
    pub fn dep_named(&self, ident: &str) -> Option<&Dep> {
        self.deps.iter().find(|d| d.alias == ident)
    }

    /// The crate root, `src/lib.rs`, if the crate has a library.
    pub fn lib(&self) -> Option<PathBuf> {
        let lib = self.dir.join("src/lib.rs");
        lib.is_file().then_some(lib)
    }

    /// Every `.rs` file under the crate's source directories and its
    /// `build.rs`, in path order, as git lists them ([`crate::git::files`]):
    /// a file git ignores is in no commit and moves no key, so it is not
    /// read. Git lists from the workspace root with the crate's
    /// directories as the pathspecs, as budget does: run in the crate's
    /// directory under the `GIT_DIR` a linked worktree's hook exports, git
    /// takes that directory for the top of the work tree and reads no
    /// `.gitignore` above it.
    pub fn sources(&self) -> Result<Vec<PathBuf>> {
        let path = self.dir.strip_prefix(&self.root).unwrap_or(&self.dir);
        let scopes = ["src", "tests", "benches", "examples", "build.rs"].map(|s| path.join(s));
        let mut out: Vec<PathBuf> = (crate::git::files_under(&self.root, &scopes)?.into_iter())
            .filter(|p| Path::new(p).extension().is_some_and(|e| e == "rs"))
            .map(|p| self.root.join(p))
            .collect();
        out.sort();
        Ok(out)
    }

    /// A source file's path relative to the crate directory, for messages.
    pub fn relative<'a>(&self, file: &'a Path) -> &'a Path {
        file.strip_prefix(&self.dir).unwrap_or(file)
    }
}

/// The crates present in one workspace.
#[derive(Debug, Clone)]
pub struct Crates {
    root: PathBuf,
    present: Vec<Present>,
}

impl Crates {
    /// Scan `root` for the manifest's crates.
    pub fn scan(manifest: &Manifest, root: &Path) -> Result<Crates> {
        let (mut present, layout) = (Vec::new(), manifest.layout());
        for row in &manifest.crates {
            let dir = root.join(row.path());
            let cargo = dir.join("Cargo.toml");
            if !cargo.is_file() {
                continue;
            }
            let table: toml::Table = crate::read_toml(&cargo)?;
            let package = table
                .get("package")
                .and_then(|p| p.get("name"))
                .and_then(Value::as_str)
                .with_context(|| format!("{} has no package name", cargo.display()))?
                .to_owned();
            let inherits_lints = table
                .get("lints")
                .and_then(|lints| lints.get("workspace"))
                .and_then(Value::as_bool)
                == Some(true);
            let (deps, external) = dependencies(&table, &dir, root, &layout);
            present.push(Present {
                name: row.name.clone(),
                deps,
                external,
                inherits_lints,
                dir,
                root: root.to_path_buf(),
                package,
            });
        }
        Ok(Crates {
            root: root.to_path_buf(),
            present,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn iter(&self) -> impl Iterator<Item = &Present> {
        self.present.iter()
    }

    pub fn named(&self, name: &str) -> Option<&Present> {
        self.present.iter().find(|c| c.name == name)
    }

    /// A crate's directory relative to the root, for messages.
    pub fn relative<'a>(&self, path: &'a Path) -> &'a Path {
        path.strip_prefix(&self.root).unwrap_or(path)
    }
}

/// The workspace dependencies of one `Cargo.toml`, of every kind, one
/// entry per crate depended on: linked when any of its entries is; and
/// beside them every other package it depends on, by name.
fn dependencies(
    table: &toml::Table,
    dir: &Path,
    root: &Path,
    layout: &Layout,
) -> (Vec<Dep>, BTreeSet<String>) {
    const KINDS: [(&str, bool); 3] = [
        ("dependencies", true),
        ("dev-dependencies", false),
        ("build-dependencies", false),
    ];
    let targets = table
        .get("target")
        .and_then(Value::as_table)
        .into_iter()
        .flat_map(|targets| targets.values().filter_map(Value::as_table));
    let mut sections: Vec<(&toml::Table, bool)> = Vec::new();
    for at in std::iter::once(table).chain(targets) {
        for (key, linked) in KINDS {
            sections.extend(at.get(key).and_then(Value::as_table).map(|s| (s, linked)));
        }
    }
    let (mut out, mut external): (Vec<Dep>, BTreeSet<String>) = Default::default();
    for (section, linked) in sections {
        for (key, value) in section {
            let Some(name) = dep_crate(key, value, dir, root, layout) else {
                external.insert(package_of(key, value).to_owned());
                continue;
            };
            let alias = key.replace('-', "_");
            match out.iter_mut().find(|d| d.name == name && d.alias == alias) {
                Some(dep) => dep.linked |= linked,
                None => out.push(Dep {
                    name,
                    alias,
                    linked,
                }),
            }
        }
    }
    (out, external)
}

/// The package one dependency entry names: its `package`, or its key.
fn package_of<'a>(key: &'a str, value: &'a Value) -> &'a str {
    let renamed = value.get("package").and_then(Value::as_str);
    renamed.unwrap_or(key)
}

/// The manifest crate one dependency entry names: by its `path` when it
/// has one, else by its package name, as `layout` reads either. An entry
/// that names neither is not a workspace crate.
fn dep_crate(key: &str, value: &Value, dir: &Path, root: &Path, layout: &Layout) -> Option<String> {
    let entry = value.as_table()?;
    if let Some(path) = entry.get("path").and_then(Value::as_str) {
        let target = normalize(&dir.join(path));
        let rel = target.strip_prefix(normalize(root)).ok()?;
        return layout.name_of_path(rel);
    }
    layout.name_of_package(&package_of(key, value).replace('-', "_"))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::Crates;
    use crate::git::git;
    use crate::manifest::Manifest;
    use crate::scratch::Tree;

    /// A crate's sources are the `.rs` files git lists under it: one git
    /// ignores is in no commit and moves no key, so it is not read.
    #[test]
    fn a_source_git_ignores_is_not_among_a_crates() {
        let root = Tree::new("api-check-ignored").unwrap();
        let src = root.join("crates/php_heap/src");
        fs::create_dir_all(&src).unwrap();
        fs::write(root.join(".gitignore"), "*.local.rs\n").unwrap();
        fs::write(
            root.join("crates/php_heap/Cargo.toml"),
            "[package]\nname = \"php_heap\"\n",
        )
        .unwrap();
        fs::write(src.join("lib.rs"), "").unwrap();
        fs::write(src.join("notes.local.rs"), "pub fn notes() {}\n").unwrap();
        git(&root, &["init", "-q"]).unwrap();
        let crates = Crates::scan(&Manifest::named("php_heap"), &root).unwrap();
        let sources = crates.named("php_heap").unwrap().sources().unwrap();
        assert_eq!(sources, [src.join("lib.rs")]);
    }
}
