//! The key a reading is stamped with and held to (#1318): the hash of the
//! scope's sources and, where the scope is a workspace crate, of every
//! workspace crate in its dependency closure, as cargo's fingerprint
//! takes it. A crate stands only while its own tree and the trees it is
//! built from are the ones it was measured over, so a change to
//! `php_engine` marks the reading of every crate above it stale, and a
//! change to one leaf extension marks that extension and the crates that
//! depend on it, and no other.
//!
//! [`of`] is the one function that computes the key, over the working
//! tree, the index or a commit; the stamp, `done`, pre-commit's standing
//! readings, `witness --unmeasured` and `permit`'s `unless` all ask it.
//! The closure is read from `cargo metadata` once per process and root
//! ([`Graph::of`]), not once per crate.
//!
//! The closure is the crate, each workspace crate it depends on directly
//! as a normal, build or dev dependency, and, from those, each reached
//! through normal and build dependencies alone: a dependency's dev
//! dependencies are not part of what the crate is built from. A build
//! dependency is taken whole, since what it runs can generate code. The
//! edges are the ones the manifests declare (`--no-deps`), an optional
//! dependency among them whether a feature turns it on or not: the graph
//! is read without resolving a version or a feature, so it needs no
//! registry and writes no `Cargo.lock`, and a key that counts one crate
//! too many re-measures a crate where one too few would let a reading
//! stand over code it never ran. The hash is one listing over the union
//! of the closure's files, so a crate with no workspace dependency keeps
//! the key its own tree gave it.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::git::{self, Index};

/// Which content of the scope the key is taken over.
#[derive(Debug, Clone, Copy)]
pub(crate) enum Content<'a> {
    /// The working tree, tracked and untracked files alike: what an
    /// instrument stamps and what `done` holds a reading to.
    Disk,
    /// What the index holds: what a commit takes, pre-commit's question
    /// (#1143).
    Index(&'a Index),
    /// The tracked files as the commit holds them: what `permit`'s
    /// `unless` reads as evidence a commit was measured (#1448).
    Commit(&'a str),
}

/// The key of the sources under `path`, relative to `root`, over
/// `content`: one hash over the files of the path and, when the path is
/// a workspace crate's directory, of the directories of its dependency
/// closure ([`Graph::paths`]).
pub(crate) fn of(root: &Path, path: &Path, content: Content<'_>) -> Result<String> {
    let paths = Graph::of(root)?.paths(path);
    match content {
        Content::Disk => git::tree_hash(root, &paths),
        Content::Index(index) => Ok(index.hash(&paths)),
        Content::Commit(commit) => {
            let named: Vec<&str> = paths.iter().filter_map(|p| p.to_str()).collect();
            git::tracked_hash(root, &named, Some(commit))
        }
    }
}

/// Each workspace crate's directory and the directories of its
/// dependency closure, relative to the workspace root.
#[derive(Debug, Default)]
pub(crate) struct Graph {
    closures: BTreeMap<PathBuf, BTreeSet<PathBuf>>,
}

/// The graphs read in this process, by root: `cargo metadata` runs once
/// per root and not once per crate.
static READ: OnceLock<Mutex<BTreeMap<PathBuf, Arc<Graph>>>> = OnceLock::new();

impl Graph {
    /// The graph of the workspace at `root`, read once per process. A
    /// root with no `Cargo.toml` is no workspace and has an empty graph,
    /// so every path's closure is the path alone.
    pub(crate) fn of(root: &Path) -> Result<Arc<Graph>> {
        let read = READ.get_or_init(|| Mutex::new(BTreeMap::new()));
        if let Some(graph) = read.lock().ok().and_then(|g| g.get(root).cloned()) {
            return Ok(graph);
        }
        let graph = Arc::new(Graph::read(root)?);
        if let Ok(mut held) = read.lock() {
            held.insert(root.to_path_buf(), Arc::clone(&graph));
        }
        Ok(graph)
    }

    fn read(root: &Path) -> Result<Graph> {
        let manifest = root.join("Cargo.toml");
        if !manifest.is_file() {
            return Ok(Graph::default());
        }
        let out = Command::new("cargo")
            .args([
                "metadata",
                "--format-version",
                "1",
                "--no-deps",
                "--offline",
            ])
            .arg("--manifest-path")
            .arg(&manifest)
            .output()
            .context("running cargo metadata")?;
        if !out.status.success() {
            bail!(
                "cargo metadata over {}: {}",
                manifest.display(),
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
        let text = String::from_utf8(out.stdout).context("cargo metadata wrote no UTF-8")?;
        Graph::from_metadata(&text)
    }

    /// The graph from what `cargo metadata --format-version 1 --no-deps`
    /// writes: its workspace members and the path dependencies among them.
    pub(crate) fn from_metadata(text: &str) -> Result<Graph> {
        let metadata: Metadata =
            serde_json::from_str(text).context("cargo metadata wrote no metadata")?;
        let root = Path::new(&metadata.workspace_root);
        // Each member by the absolute directory a dependency's `path` names.
        let mut dirs: BTreeMap<&Path, PathBuf> = BTreeMap::new();
        let members = metadata
            .packages
            .iter()
            .filter(|p| metadata.workspace_members.contains(&p.id));
        for package in members.clone() {
            let at = Path::new(&package.manifest_path)
                .parent()
                .with_context(|| format!("{} has no directory", package.manifest_path))?;
            let dir = at.strip_prefix(root).with_context(|| {
                format!(
                    "{} is outside the workspace root {}",
                    at.display(),
                    root.display()
                )
            })?;
            dirs.insert(at, dir.to_path_buf());
        }
        // Each member's edges to other members: every kind, and the kinds a
        // dependency passes on, normal and build.
        let mut every: BTreeMap<&Path, Vec<&Path>> = BTreeMap::new();
        let mut built: BTreeMap<&Path, Vec<&Path>> = BTreeMap::new();
        for package in members {
            let Some(at) = Path::new(&package.manifest_path).parent() else {
                continue;
            };
            for dep in &package.dependencies {
                let Some(to) = dep.path.as_deref().map(Path::new) else {
                    continue;
                };
                if !dirs.contains_key(to) {
                    continue;
                }
                every.entry(at).or_default().push(to);
                if dep.kind.as_deref() != Some("dev") {
                    built.entry(at).or_default().push(to);
                }
            }
        }
        let mut closures = BTreeMap::new();
        for (at, dir) in &dirs {
            let mut reached: BTreeSet<&Path> = BTreeSet::from([*at]);
            let mut queue: Vec<&Path> = every.get(at).cloned().unwrap_or_default();
            while let Some(next) = queue.pop() {
                if reached.insert(next) {
                    queue.extend(built.get(next).into_iter().flatten());
                }
            }
            let paths = reached
                .iter()
                .filter_map(|r| dirs.get(r))
                .cloned()
                .collect();
            closures.insert(dir.clone(), paths);
        }
        Ok(Graph { closures })
    }

    /// The paths the key of `path` is taken over, sorted: the path alone,
    /// or a workspace crate's directory and its closure's.
    pub(crate) fn paths(&self, path: &Path) -> Vec<PathBuf> {
        self.closures
            .get(path)
            .map_or_else(|| vec![path.to_path_buf()], |c| c.iter().cloned().collect())
    }
}

#[derive(Deserialize)]
struct Metadata {
    packages: Vec<Package>,
    workspace_members: Vec<String>,
    workspace_root: String,
}

#[derive(Deserialize)]
struct Package {
    id: String,
    manifest_path: String,
    #[serde(default)]
    dependencies: Vec<Dependency>,
}

/// One dependency as the package's manifest declares it: a workspace
/// crate is one with a `path`.
#[derive(Deserialize)]
struct Dependency {
    /// `None` for a normal dependency, `"dev"` or `"build"`.
    kind: Option<String>,
    path: Option<String>,
}

/// The fixture of #1318, `xtask/fixtures/closure`, committed once as a
/// scratch repository of its own: `ext/reflection` and `ext/ctype` built
/// from `php_ext`, which is built from `php_engine`.
#[cfg(test)]
pub(crate) fn fixture(name: &str) -> crate::scratch::Tree {
    let tree = crate::scratch::Tree::new(&format!("closure-{name}")).unwrap();
    let fixture = crate::built_root().join("xtask/fixtures/closure");
    crate::scratch::repository(&fixture, &tree).unwrap();
    tree
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::Graph;
    use crate::built_root;

    fn closure(graph: &Graph, dir: &str) -> Vec<String> {
        graph
            .paths(Path::new(dir))
            .iter()
            .map(|p| p.display().to_string())
            .collect()
    }

    /// The workspace's own graph: an extension's closure reaches
    /// `php_engine` through `php_ext`, `php_engine`'s reaches no
    /// extension, and a leaf extension is in no other leaf's closure.
    #[test]
    fn the_workspace_closure_follows_cargos_graph() {
        let graph = Graph::of(&built_root()).unwrap();
        let reflection = closure(&graph, "ext/reflection");
        assert!(
            reflection.contains(&"crates/php_engine".to_owned()),
            "{reflection:?}"
        );
        assert!(reflection.contains(&"crates/php_ext".to_owned()));
        assert!(!reflection.contains(&"ext/ctype".to_owned()));
        let engine = closure(&graph, "crates/php_engine");
        assert!(engine.iter().all(|p| !p.starts_with("ext/")), "{engine:?}");
        assert!(closure(&graph, "crates/php_bundle").contains(&"ext/ctype".to_owned()));
        assert_eq!(closure(&graph, "docs"), ["docs"]);
        assert_eq!(closure(&graph, "."), ["."]);
    }

    /// The fixture's graph: an extension's closure reaches `php_engine`
    /// through `php_ext`, and neither extension is in the other's.
    #[test]
    fn the_fixtures_closure_reaches_through_php_ext() {
        let tree = super::fixture("graph");
        let graph = Graph::of(&tree).unwrap();
        assert_eq!(
            closure(&graph, "ext/reflection"),
            ["crates/php_engine", "crates/php_ext", "ext/reflection"]
        );
        assert_eq!(closure(&graph, "crates/php_engine"), ["crates/php_engine"]);
    }

    /// A dev dependency is followed from the crate itself and not from a
    /// crate it reaches: `b` dev-depends on `c`, so `c` is in `b`'s
    /// closure and not in `a`'s, which depends on `b`.
    #[test]
    fn a_dependencys_dev_dependencies_are_not_followed() {
        let member = |name: &str| format!("path+file:///w/{name}#0.0.0");
        let edge = |to: &str, kind: &str| format!(r#"{{"kind":{kind},"path":"/w/{to}"}}"#);
        let package = |name: &str, deps: &[String]| {
            format!(
                r#"{{"id":"{}","manifest_path":"/w/{name}/Cargo.toml","dependencies":[{}]}}"#,
                member(name),
                deps.join(",")
            )
        };
        let text = format!(
            r#"{{"packages":[{},{},{}],"workspace_members":["{}","{}","{}"],"workspace_root":"/w"}}"#,
            package(
                "a",
                &[edge("b", "null"), r#"{"kind":null,"path":null}"#.to_owned()]
            ),
            package("b", &[edge("c", r#""dev""#)]),
            package("c", &[edge("elsewhere", "null")]),
            member("a"),
            member("b"),
            member("c"),
        );
        let graph = Graph::from_metadata(&text).unwrap();
        assert_eq!(closure(&graph, "a"), ["a", "b"]);
        assert_eq!(closure(&graph, "b"), ["b", "c"]);
        assert_eq!(graph.paths(Path::new("c")), [PathBuf::from("c")]);
    }
}
