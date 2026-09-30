//! The crates that exist on disk, found through the manifest's `[[crate]]`
//! rows: a row whose directory holds a `Cargo.toml` is present, and its
//! workspace dependencies are read from that file. Nothing is built and
//! cargo is not run; the instrument reads manifests and sources.

use std::fs;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result};
use toml::Value;

use crate::layout;
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
            let dir = self.dir.join(sub);
            if dir.is_dir() {
                walk(&dir, &mut out)?;
            }
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
        let mut present = Vec::new();
        for row in &manifest.crates {
            let dir = root.join(row.path());
            let cargo = dir.join("Cargo.toml");
            if !cargo.is_file() {
                continue;
            }
            let text = fs::read_to_string(&cargo)
                .with_context(|| format!("reading {}", cargo.display()))?;
            let table: toml::Table =
                toml::from_str(&text).with_context(|| format!("{}", cargo.display()))?;
            let package = table
                .get("package")
                .and_then(|p| p.get("name"))
                .and_then(Value::as_str)
                .with_context(|| format!("{} has no package name", cargo.display()))?
                .to_owned();
            present.push(Present {
                name: row.name.clone(),
                deps: deps(&table, &dir, root),
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

/// The workspace dependencies of one `Cargo.toml`, of every kind.
fn deps(table: &toml::Table, dir: &Path, root: &Path) -> Vec<Dep> {
    let mut sections: Vec<&toml::Table> = Vec::new();
    for key in ["dependencies", "dev-dependencies", "build-dependencies"] {
        sections.extend(table.get(key).and_then(Value::as_table));
    }
    if let Some(targets) = table.get("target").and_then(Value::as_table) {
        for target in targets.values() {
            for key in ["dependencies", "dev-dependencies", "build-dependencies"] {
                sections.extend(target.get(key).and_then(Value::as_table));
            }
        }
    }
    let mut out: Vec<Dep> = Vec::new();
    for section in sections {
        for (key, value) in section {
            let Some(name) = dep_crate(key, value, dir, root) else {
                continue;
            };
            let dep = Dep {
                name,
                alias: key.replace('-', "_"),
            };
            if !out.contains(&dep) {
                out.push(dep);
            }
        }
    }
    out
}

/// The manifest crate one dependency entry names: by its `path` when it
/// has one, else by its package name. An entry that names neither is not
/// a workspace crate.
fn dep_crate(key: &str, value: &Value, dir: &Path, root: &Path) -> Option<String> {
    let entry = value.as_table()?;
    if let Some(path) = entry.get("path").and_then(Value::as_str) {
        let target = normalize(&dir.join(path));
        let rel = target.strip_prefix(normalize(root)).ok()?;
        return layout::name_of_path(rel);
    }
    let package = entry
        .get("package")
        .and_then(Value::as_str)
        .unwrap_or(key)
        .replace('-', "_");
    layout::name_of_package(&package)
}

/// A path with its `.` and `..` components folded, without touching the
/// filesystem.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(dir).with_context(|| format!("listing {}", dir.display()))? {
        let path = entry?.path();
        if path.is_dir() {
            walk(&path, out)?;
        } else {
            out.push(path);
        }
    }
    Ok(())
}
