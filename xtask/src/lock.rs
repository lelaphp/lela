//! `Cargo.lock` as the harness reads it: the package list, for the
//! `lock_packages` ceiling and for expanding a manifest wildcard into the
//! types a locked package exports (harness-design.md §13 D.19).

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

/// The workspace lock file.
pub const FILE: &str = "Cargo.lock";

/// `Cargo.lock`'s package list: name and version.
#[derive(Debug, Default, Deserialize)]
pub struct CargoLock {
    #[serde(default)]
    pub package: Vec<Package>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Package {
    pub name: String,
    pub version: String,
}

impl CargoLock {
    pub fn load(root: &Path) -> Result<CargoLock> {
        let path = root.join(FILE);
        let text =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("{}", path.display()))
    }

    /// The number of packages: `[budget] lock_packages` measures this.
    pub fn packages(&self) -> usize {
        self.package.len()
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
    /// library module comes from the table below; a package's root module
    /// comes from its source in the cargo registry at the locked version;
    /// a package not in the lock expands to nothing, since no extension
    /// can name a type of a crate the workspace does not link.
    pub fn expand(&self, prefix: &str) -> Result<Vec<String>> {
        if let Some((_, types)) = STD.iter().find(|(module, _)| *module == prefix) {
            return Ok(types.iter().map(|t| format!("{prefix}::{t}")).collect());
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
        let source = registry_root(found)?;
        let text = fs::read_to_string(&source)
            .with_context(|| format!("reading {} to expand {prefix}::*", source.display()))?;
        Ok(root_types(&text)
            .into_iter()
            .map(|t| format!("{prefix}::{t}"))
            .collect())
    }
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

/// The type names a crate root makes `pub`: its own `struct`, `enum`,
/// `union` and `type` items and the CamelCase names of its `pub use`
/// re-exports (functions and macros are lower-case and left out).
fn root_types(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut pending = String::new();
    for raw in text.lines() {
        let line = raw.split("//").next().unwrap_or("").trim();
        if !pending.is_empty() {
            pending.push(' ');
            pending.push_str(line);
            if line.ends_with(';') {
                out.extend(reexports(&pending));
                pending.clear();
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("pub use ") {
            if line.ends_with(';') {
                out.extend(reexports(rest));
            } else {
                rest.clone_into(&mut pending);
            }
            continue;
        }
        let Some(rest) = line.strip_prefix("pub ") else {
            continue;
        };
        for keyword in ["struct ", "enum ", "union ", "type "] {
            if let Some(name) = rest.strip_prefix(keyword) {
                let name: String = name
                    .chars()
                    .take_while(|c| c.is_alphanumeric() || *c == '_')
                    .collect();
                if is_type_name(&name) {
                    out.push(name);
                }
            }
        }
    }
    out.sort();
    out.dedup();
    out
}

/// The names a `pub use` statement brings in, `as` renames applied.
fn reexports(stmt: &str) -> Vec<String> {
    let stmt = stmt.trim_end_matches(';').trim();
    let list = match stmt.split_once('{') {
        Some((_, list)) => list.trim_end_matches('}'),
        None => stmt,
    };
    list.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != "self" && *s != "*")
        .map(|s| {
            let s = s.split_once(" as ").map_or(s, |(_, alias)| alias);
            s.rsplit("::").next().unwrap_or(s).trim().to_owned()
        })
        .filter(|s| is_type_name(s))
        .collect()
}

fn is_type_name(name: &str) -> bool {
    name.chars().next().is_some_and(char::is_uppercase)
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
    use super::{CargoLock, closure, reexports, root_types};

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

    #[test]
    fn a_crate_root_yields_its_camel_case_public_types() {
        let text = "pub use self::mutex::{const_mutex, MappedMutexGuard, Mutex, MutexGuard};\n\
                    pub use self::raw_mutex::RawMutex;\n\
                    pub use self::rwlock::{\n    RwLock, RwLockReadGuard,\n};\n\
                    pub use ::lock_api;\n\
                    pub struct Once(u8);\n\
                    pub type Verdict<T> = core::result::Result<T, E>;\n\
                    pub fn f() {}\n\
                    struct Private;\n";
        assert_eq!(
            root_types(text),
            [
                "MappedMutexGuard",
                "Mutex",
                "MutexGuard",
                "Once",
                "RawMutex",
                "RwLock",
                "RwLockReadGuard",
                "Verdict"
            ]
        );
    }

    #[test]
    fn a_renamed_reexport_is_named_by_its_alias() {
        assert_eq!(reexports("a::{B as C, d}"), ["C"]);
    }

    #[test]
    fn a_package_missing_from_the_lock_expands_to_nothing() {
        let lock = CargoLock::default();
        assert!(lock.expand("parking_lot").unwrap().is_empty());
        assert!(lock.expand("std::cell").is_err());
    }
}
