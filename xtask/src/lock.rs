//! `Cargo.lock` as the harness reads it: the package list, for the
//! `lock_packages` ceiling and for expanding a manifest wildcard into the
//! types a locked package exports (harness-design.md §13 D.19).

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

/// The workspace lock file.
pub const FILE: &str = "Cargo.lock";

/// `Cargo.lock`'s package list: name, version and where it comes from.
#[derive(Debug, Default, Deserialize)]
pub struct CargoLock {
    #[serde(default)]
    pub package: Vec<Package>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Package {
    pub name: String,
    pub version: String,
    /// The registry or repository a third-party package comes from; a
    /// workspace crate has none.
    #[serde(default)]
    pub source: Option<String>,
}

impl CargoLock {
    pub fn load(root: &Path) -> Result<CargoLock> {
        let path = root.join(FILE);
        let text = crate::read(&path)?;
        toml::from_str(&text).with_context(|| format!("{}", path.display()))
    }

    /// The number of third-party packages, those with a `source`:
    /// `[budget] lock_packages` measures this. A workspace crate is held by
    /// its `[[crate]]` row and its line ceiling, so it is not counted (#1335).
    pub fn packages(&self) -> usize {
        self.package.iter().filter(|p| p.source.is_some()).count()
    }

    /// The package a crate name refers to; `Cargo.lock` writes `-` where a
    /// path writes `_`, so both are tried.
    pub fn package(&self, name: &str) -> Option<&Package> {
        let dashed = name.replace('_', "-");
        self.package
            .iter()
            .find(|p| p.name == name || p.name == dashed)
    }

    /// Expand `prefix::*` to the type paths the module holds. A standard
    /// library module comes from the table below; a package from its
    /// source in the cargo registry at the locked version, each type at
    /// the path its public modules make it reachable at ([`public_types`],
    /// #1910); a package not in the lock expands to nothing, since no
    /// extension can name a type of a crate the workspace does not link.
    pub(crate) fn expand(&self, prefix: &str) -> Result<Vec<Expanded>> {
        if let Some((_, types)) = STD.iter().find(|(module, _)| *module == prefix) {
            return Ok(types
                .iter()
                .map(|t| Expanded {
                    path: format!("{prefix}::{t}"),
                    gated: false,
                })
                .collect());
        }
        let package = package_of(prefix);
        if ["std", "core", "alloc"].contains(&package) {
            bail!(
                "{prefix}::* names a standard library module the generator has no table for; list its types"
            );
        }
        let Some(found) = self.package(package) else {
            return Ok(Vec::new());
        };
        if prefix.contains("::") {
            bail!("{prefix}::* reaches into a package; only `<package>::*` is expanded");
        }
        public_types(&registry_root(found)?, prefix)
            .with_context(|| format!("expanding {prefix}::*"))
    }
}

/// One type path a wildcard expands to.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct Expanded {
    /// Where the type is reachable: `once_cell::sync::OnceCell`.
    pub(crate) path: String,
    /// Whether a `#[cfg]` on the type, or on a module on its path, leaves
    /// it out of some build of the package, where clippy calls the path
    /// unreachable.
    pub(crate) gated: bool,
}

/// The crate a path starts with.
pub fn package_of(path: &str) -> &str {
    path.split("::").next().unwrap_or(path)
}

/// The stable types of the standard library modules a wildcard may name.
const STD: [(&str, &[&str]); 1] = [(
    "std::sync::atomic",
    &[
        "AtomicBool",
        "AtomicI8",
        "AtomicI16",
        "AtomicI32",
        "AtomicI64",
        "AtomicIsize",
        "AtomicPtr",
        "AtomicU8",
        "AtomicU16",
        "AtomicU32",
        "AtomicU64",
        "AtomicUsize",
    ],
)];

/// The `lib.rs` of a locked package in the cargo registry.
fn registry_root(package: &Package) -> Result<PathBuf> {
    let home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cargo")))
        .context("neither CARGO_HOME nor HOME is set")?;
    let src = home.join("registry").join("src");
    let dir = format!("{}-{}", package.name, package.version);
    for index in fs::read_dir(&src)
        .with_context(|| format!("listing {}", src.display()))?
        .flatten()
    {
        let candidate = index.path().join(&dir).join("src").join("lib.rs");
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    bail!(
        "{dir} is in Cargo.lock but not under {}; run `cargo fetch`",
        src.display()
    )
}

/// The types a package makes public, read from its root file `lib` and
/// named under `package`: each `pub struct`, `enum`, `union` and `type`,
/// and each CamelCase name a `pub use` brings in, at the path of the
/// module that holds it. A `pub mod` is walked by its module path, inline
/// or from its file as rustc finds it; a private module is not, since
/// what it holds is reachable only where a public module re-exports it.
/// What a macro expands to is not read, and a `pub use` glob or a `pub
/// mod` at a `#[path]` is refused, naming where, so that what the
/// expansion lists is the package's whole public set.
fn public_types(lib: &Path, package: &str) -> Result<Vec<Expanded>> {
    let dir = lib.parent().unwrap_or(Path::new("."));
    let mut out = Vec::new();
    Module {
        path: package.to_owned(),
        dir: dir.to_owned(),
        gated: false,
    }
    .file(lib, &mut out)?;
    out.sort();
    out.dedup();
    Ok(out)
}

/// A public module being walked: its path, the directory its file
/// modules are found in, and whether a `#[cfg]` on its path gates it.
struct Module {
    path: String,
    dir: PathBuf,
    gated: bool,
}

impl Module {
    /// The items of `file`, this module's source.
    fn file(&self, file: &Path, out: &mut Vec<Expanded>) -> Result<()> {
        let text = crate::read(file)?;
        let parsed =
            syn::parse_file(&text).with_context(|| format!("parsing {}", file.display()))?;
        self.items(&parsed.items, out)
    }

    fn items(&self, items: &[syn::Item], out: &mut Vec<Expanded>) -> Result<()> {
        for item in items {
            let (vis, attrs) = match item {
                syn::Item::Struct(i) => (&i.vis, &i.attrs),
                syn::Item::Enum(i) => (&i.vis, &i.attrs),
                syn::Item::Union(i) => (&i.vis, &i.attrs),
                syn::Item::Type(i) => (&i.vis, &i.attrs),
                syn::Item::Use(i) => (&i.vis, &i.attrs),
                syn::Item::Mod(i) => (&i.vis, &i.attrs),
                _ => continue,
            };
            if !matches!(vis, syn::Visibility::Public(_)) {
                continue;
            }
            let gated = self.gated || attrs.iter().any(|a| a.path().is_ident("cfg"));
            let mut names = Vec::new();
            match item {
                syn::Item::Struct(i) => names.push(&i.ident),
                syn::Item::Enum(i) => names.push(&i.ident),
                syn::Item::Union(i) => names.push(&i.ident),
                syn::Item::Type(i) => names.push(&i.ident),
                syn::Item::Use(i) => self.reexports(&i.tree, &mut names)?,
                syn::Item::Mod(i) => self.module(i, gated, out)?,
                _ => {}
            }
            out.extend(
                names
                    .into_iter()
                    .map(ToString::to_string)
                    .filter(|n| n.starts_with(char::is_uppercase))
                    .map(|n| Expanded {
                        path: format!("{}::{n}", self.path),
                        gated,
                    }),
            );
        }
        Ok(())
    }

    /// The names a `pub use` brings in, `as` renames applied.
    fn reexports<'a>(&self, tree: &'a syn::UseTree, names: &mut Vec<&'a syn::Ident>) -> Result<()> {
        match tree {
            syn::UseTree::Path(p) => self.reexports(&p.tree, names)?,
            syn::UseTree::Name(n) => names.push(&n.ident),
            syn::UseTree::Rename(r) => names.push(&r.rename),
            syn::UseTree::Glob(_) => bail!(
                "{} re-exports a glob, which the generator does not read; list the types it names",
                self.path
            ),
            syn::UseTree::Group(g) => {
                for tree in &g.items {
                    self.reexports(tree, names)?;
                }
            }
        }
        Ok(())
    }

    /// A `pub mod`: inline, walked in place; a file, found as rustc finds
    /// it, `<dir>/<name>.rs` or `<dir>/<name>/mod.rs`.
    fn module(&self, item: &syn::ItemMod, gated: bool, out: &mut Vec<Expanded>) -> Result<()> {
        let ident = item.ident.to_string();
        let path = format!("{}::{ident}", self.path);
        if let Some((_, items)) = &item.content {
            let inner = Module {
                path,
                dir: self.dir.join(&ident),
                gated,
            };
            return inner.items(items, out);
        }
        if item.attrs.iter().any(|a| a.path().is_ident("path")) {
            bail!(
                "{path} is a file module at a #[path], which the generator does not follow; list its types"
            );
        }
        let flat = self.dir.join(format!("{ident}.rs"));
        let (file, dir) = if flat.is_file() {
            (flat, self.dir.join(&ident))
        } else {
            let nested = self.dir.join(&ident);
            (nested.join("mod.rs"), nested)
        };
        Module { path, dir, gated }.file(&file, out)
    }
}

/// One `[[package]]` whole, as the lock pins it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub(crate) struct Pinned {
    name: String,
    version: String,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    checksum: Option<String>,
    #[serde(default)]
    dependencies: Vec<String>,
}

#[derive(Deserialize)]
struct Pins {
    #[serde(default)]
    package: Vec<Pinned>,
}

/// The packages `package` reaches in the lock `text`, itself first and
/// the rest in the lock's order, each whole as the lock pins it: two
/// locks resolve a build of the crate alike when the two closures are
/// equal (#1312). A dependency is written `name`, `name version` or
/// `name version (source)`, and matches every package of that name and
/// version. `None` when the text is no lock or names no such package.
pub(crate) fn closure(text: &str, package: &str) -> Option<Vec<Pinned>> {
    let pins: Pins = toml::from_str(text).ok()?;
    let dashed = package.replace('_', "-");
    let first = pins
        .package
        .iter()
        .position(|p| p.source.is_none() && (p.name == package || p.name == dashed))?;
    let mut reached = vec![false; pins.package.len()];
    reached[first] = true;
    let mut queue = vec![first];
    while let Some(at) = queue.pop() {
        for dep in &pins.package[at].dependencies {
            let mut words = dep.split_whitespace();
            let name = words.next().unwrap_or("");
            let version = words.next();
            for (i, p) in pins.package.iter().enumerate() {
                if !reached[i] && p.name == name && version.is_none_or(|v| p.version == v) {
                    reached[i] = true;
                    queue.push(i);
                }
            }
        }
    }
    let mut out = vec![pins.package[first].clone()];
    out.extend(
        pins.package
            .iter()
            .enumerate()
            .filter(|(i, _)| reached[*i] && *i != first)
            .map(|(_, p)| p.clone()),
    );
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::{CargoLock, Expanded, closure, public_types};
    use crate::scratch::Tree;

    const LOCK: &str = "version = 4\n\n\
        [[package]]\nname = \"app\"\nversion = \"0.0.0\"\ndependencies = [\n \"dep 1.0.0\",\n]\n\n\
        [[package]]\nname = \"dep\"\nversion = \"1.0.0\"\nsource = \"registry+x\"\nchecksum = \"aa\"\n\n\
        [[package]]\nname = \"dep\"\nversion = \"2.0.0\"\nsource = \"registry+x\"\nchecksum = \"bb\"\n\n\
        [[package]]\nname = \"other\"\nversion = \"0.0.0\"\ndependencies = [\n \"dep 2.0.0\",\n]\n";

    /// A package's closure moves with what it reaches and with nothing
    /// else: a bump of the `dep` only `other` reaches leaves `app`'s
    /// closure as it was (#1312).
    #[test]
    fn a_closure_moves_only_with_what_the_package_reaches() {
        let bumped = LOCK.replace("checksum = \"bb\"", "checksum = \"cc\"");
        assert_eq!(closure(LOCK, "app").map(|c| c.len()), Some(2));
        assert_eq!(closure(LOCK, "app"), closure(&bumped, "app"));
        assert_ne!(closure(LOCK, "other"), closure(&bumped, "other"));
        assert_eq!(closure(LOCK, "missing"), None);
        assert_eq!(closure("not a lock [", "app"), None);
    }

    /// A package laid out as `once_cell` is (#1910): types at the root, in
    /// an inline `pub mod`, one gated by a `#[cfg]`, in a file module and
    /// a file module of that, re-exported out of a private module, and
    /// held in a private module alone.
    #[test]
    fn a_package_yields_each_type_at_the_path_its_public_modules_reach() {
        let tree = Tree::new("lock-public-types").unwrap();
        std::fs::create_dir_all(tree.join("src/race")).unwrap();
        let lib = tree
            .write(
                "src/lib.rs",
                "pub use self::imp::{\n    Guard as Held, helper,\n};\n\
                 mod imp { pub struct Guard; pub fn helper() {} }\n\
                 pub struct Root(u8);\n\
                 pub type Verdict<T> = Result<T, ()>;\n\
                 pub(crate) struct Crate;\n\
                 pub mod unsync {\n    pub struct OnceCell<T>(T);\n    pub enum State { A }\n}\n\
                 #[cfg(feature = \"std\")]\n\
                 pub mod sync {\n    pub struct Lazy;\n}\n\
                 pub mod race;\n",
            )
            .unwrap();
        tree.write(
            "src/race.rs",
            "pub struct OnceBool;\n\
             pub use self::once_box::OnceBox;\n\
             mod once_box { pub struct OnceBox; }\n\
             pub mod deep;\n",
        )
        .unwrap();
        tree.write("src/race/deep.rs", "pub union Bits { a: u8 }\n")
            .unwrap();
        let found = public_types(&lib, "cell").unwrap();
        let plain = |p: &str| Expanded {
            path: p.to_owned(),
            gated: false,
        };
        assert_eq!(
            found,
            [
                plain("cell::Held"),
                plain("cell::Root"),
                plain("cell::Verdict"),
                plain("cell::race::OnceBool"),
                plain("cell::race::OnceBox"),
                plain("cell::race::deep::Bits"),
                Expanded {
                    path: "cell::sync::Lazy".to_owned(),
                    gated: true,
                },
                plain("cell::unsync::OnceCell"),
                plain("cell::unsync::State"),
            ]
        );
    }

    /// What the walk cannot read whole it refuses, naming where.
    #[test]
    fn a_glob_reexport_or_a_pathed_module_is_refused() {
        let tree = Tree::new("lock-refused").unwrap();
        std::fs::create_dir_all(tree.join("src")).unwrap();
        let lib = tree
            .write(
                "src/lib.rs",
                "pub mod a { pub use super::b::*; }\nmod b {}\n",
            )
            .unwrap();
        let err = format!("{:#}", public_types(&lib, "p").unwrap_err());
        assert!(err.contains("p::a re-exports a glob"), "{err}");
        tree.write("src/lib.rs", "#[path = \"x.rs\"]\npub mod a;\n")
            .unwrap();
        let err = format!("{:#}", public_types(&lib, "p").unwrap_err());
        assert!(err.contains("p::a is a file module at a #[path]"), "{err}");
    }

    #[test]
    fn a_package_missing_from_the_lock_expands_to_nothing() {
        let lock = CargoLock::default();
        assert!(lock.expand("parking_lot").unwrap().is_empty());
        assert!(lock.expand("std::cell").is_err());
    }
}
