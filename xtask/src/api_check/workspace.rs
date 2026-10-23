//! The crates that exist on disk, found through the manifest's `[[crate]]`
//! rows: a row whose directory holds a `Cargo.toml` is present, and its
//! workspace dependencies are read from that file. Nothing is built and
//! cargo is not run; the instrument reads manifests and sources.

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
    /// The cargo package name; `php_ext_random` for `ext/random`.
    pub package: String,
    pub deps: Vec<Dep>,
    /// Whether its `Cargo.toml` carries `[lints] workspace = true`, the one
    /// way the root manifest's `[workspace.lints]` reaches a member (#2111).
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

    /// Every `.rs` file under the crate's source directories, in path
    /// order.
    pub fn sources(&self) -> Result<Vec<PathBuf>> {
        let mut out = Vec::new();
        for sub in ["src", "tests", "benches", "examples"] {
            crate::tally::files_under(&self.dir.join(sub), &mut out)?;
        }
        let build = self.dir.join("build.rs");
        if build.is_file() {
            out.push(build);
        }
        out.retain(|p| p.extension().is_some_and(|e| e == "rs"));
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
            present.push(Present {
                name: row.name.clone(),
                deps: deps(&table, &dir, root, &layout),
                inherits_lints,
                dir,
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
/// entry per crate depended on: linked when any of its entries is.
fn deps(table: &toml::Table, dir: &Path, root: &Path, layout: &Layout) -> Vec<Dep> {
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
    let mut out: Vec<Dep> = Vec::new();
    for (section, linked) in sections {
        for (key, value) in section {
            let Some(name) = dep_crate(key, value, dir, root, layout) else {
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
    out
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
    let package = entry
        .get("package")
        .and_then(Value::as_str)
        .unwrap_or(key)
        .replace('-', "_");
    layout.name_of_package(&package)
}
