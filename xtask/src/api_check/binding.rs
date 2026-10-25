//! A crate's binding to the keg the oracle loads (#3458, as #3414 ruled):
//! a package that links a native library php links too, openssl-sys,
//! libsqlite3-sys or curl-sys, finds it where a cargo configuration's
//! `[env]` points its build, a Homebrew keg. That keg is the one the
//! pinned php loads the library from by its own load commands ([`Own`]),
//! so a pin that moves OpenSSL's major fails the reading of each crate
//! that links openssl-sys, naming both kegs, where it would otherwise show
//! first as a `crosscheck` line of `--ri openssl`. A crate whose package
//! the `[env]` binds to nothing fails too: its build links whichever
//! library its search finds first.
//!
//! The binding is one of the crate's edges, to a library outside the
//! workspace, so it is held in the crate's api-check reading, the reading
//! every commit takes and `done` reads of every crate: `oracle.toml` and
//! `.cargo/` reach every reading, so a change to either measures it again
//! (`sources::Reach`). It reads the pin, the configuration and the
//! binary's load commands, and writes nothing. Where the pinned php is
//! not on the machine or stands under no Cellar, a Linux runner's build,
//! nothing is held.

use std::path::{Path, PathBuf};

use anyhow::Result;

use super::workspace::Present;
use crate::generated::cargo_env;
use crate::manifest::Manifest;
use crate::oracle::Own;

/// A package that links a native library the oracle links too: the
/// `[env]` variable that points its build at the library, bare or after
/// a target's name (`AARCH64_APPLE_DARWIN_OPENSSL_DIR`), and the library
/// php loads, by its file's stem.
struct Native {
    package: &'static str,
    variable: &'static str,
    library: &'static str,
}

/// The packages the workspace links a native library of the oracle's
/// through, each read where `.cargo/config.toml` binds it.
const NATIVES: [Native; 3] = [
    Native {
        package: "openssl-sys",
        variable: "OPENSSL_DIR",
        library: "libssl",
    },
    Native {
        package: "libsqlite3-sys",
        variable: "SQLITE3_LIB_DIR",
        library: "libsqlite3",
    },
    Native {
        package: "curl-sys",
        variable: "CURL_LIB_DIR",
        library: "libcurl",
    },
];

impl Native {
    /// Whether `variable` is this package's, bare or after a target.
    fn names(&self, variable: &str) -> bool {
        let before = variable.strip_suffix(self.variable);
        before.is_some_and(|before| before.is_empty() || before.ends_with('_'))
    }
}

/// What a crate's bindings are held to on this machine.
pub(super) enum Bindings {
    /// No pin, or a pinned php under no Homebrew Cellar here.
    Unheld,
    /// The pinned php's own loads could not be read, as the sentence says.
    Unread(String),
    /// The pinned php's own loads, and every `[env]` entry as a path,
    /// each with its configuration's path and its variable.
    Held {
        own: Own,
        env: Vec<(&'static str, String, PathBuf)>,
    },
}

impl Bindings {
    /// The bindings of the workspace at `root` and the pin `manifest`
    /// carries.
    pub(super) fn read(manifest: &Manifest, root: &Path) -> Result<Bindings> {
        let Some(pin) = &manifest.pin else {
            return Ok(Bindings::Unheld);
        };
        let own = match Own::of(&pin.oracle.path) {
            Ok(Some(own)) => own,
            Ok(None) => return Ok(Bindings::Unheld),
            Err(err) => return Ok(Bindings::Unread(format!("{err:#}"))),
        };
        let mut env = Vec::new();
        for (config, entries) in cargo_env(root)? {
            let paths = entries.into_iter();
            env.extend(paths.map(|(variable, entry)| (config, variable, entry.path(root))));
        }
        Ok(Bindings::Held { own, env })
    }

    /// What `krate`'s links to the natives come to: each binding that
    /// does not hold, and a note for each that does.
    pub(super) fn hold(&self, krate: &Present) -> (Vec<String>, Vec<String>) {
        let (mut problems, mut notes) = (Vec::new(), Vec::new());
        for native in NATIVES
            .iter()
            .filter(|n| krate.packages.contains(n.package))
        {
            let (name, package, library) = (&krate.name, native.package, native.library);
            let (own, env) = match self {
                Bindings::Unheld => continue,
                Bindings::Unread(why) => {
                    problems.push(format!(
                        "{name} links {package}, whose binding is held to the libraries the pinned php loads, and those are unread: {why}"
                    ));
                    continue;
                }
                Bindings::Held { own, env } => (own, env),
            };
            let Some(loaded) = own.keg_loading(library) else {
                problems.push(format!(
                    "{name} links {package}, and the pinned php loads no {library} of its own, so no keg holds the binding (#3458)"
                ));
                continue;
            };
            let bound: Vec<_> = env.iter().filter(|(_, v, _)| native.names(v)).collect();
            if bound.is_empty() {
                problems.push(format!(
                    "{name} links {package}, and no cargo configuration's [env] names its {}, so its build links the first {library} a search finds where the pinned php loads {loaded}'s (#3458)",
                    native.variable
                ));
            }
            for (config, variable, path) in bound {
                match own.keg_at(path) {
                    Some(keg) if keg == loaded => notes.push(format!(
                        "{package} bound to {keg}, the keg the pinned php loads {library} from"
                    )),
                    keg => problems.push(format!(
                        "{config}'s {variable} binds {package} to {}, and the pinned php loads {library} from {loaded}: the binding moves with the oracle (#3414, #3458)",
                        keg.unwrap_or_else(|| format!("{}, no keg of the pinned php's Homebrew", path.display()))
                    )),
                }
            }
        }
        (problems, notes)
    }
}

#[cfg(test)]
#[cfg(tracked)]
mod tests {
    use std::fs;

    use crate::manifest::Manifest;
    use crate::oracle::macho;
    use crate::reading::Status;
    use crate::scratch::{Tree, repository};

    /// A crate's binding is the keg the pinned php loads its library from
    /// (#3458): `xtask/fixtures/api-check/binding` binds ext/openssl's
    /// openssl-sys to openssl@4, and of two stand-in php builds laid in
    /// its Cellar, 1.0 loads libssl from openssl@4 and holds it, and
    /// 2.0, a pin that moved OpenSSL's major, loads it from openssl@5
    /// and fails naming both.
    #[test]
    fn a_binding_to_another_major_than_the_pinned_php_loads_fails_naming_both() {
        let fixture = crate::package_dir().join("fixtures/api-check/binding");
        let tree = Tree::new("api-check-binding").unwrap();
        repository(&fixture, tree.path(), &["."]).unwrap();
        let openssl = |version: &str, major: &str| {
            let bin = tree.path().join(format!("brew/Cellar/php/{version}/bin"));
            fs::create_dir_all(&bin).unwrap();
            let full = if major == "4" { "4.0.3" } else { "5.0.0" };
            let libssl =
                format!("@loader_path/../../../openssl@{major}/{full}/lib/libssl.{major}.dylib");
            fs::write(bin.join("php"), macho(&[&libssl])).unwrap();
            let mut manifest = Manifest::load(crate::package_dir().parent().unwrap()).unwrap();
            manifest.pin.as_mut().unwrap().oracle.path = bin.join("php");
            let readings = super::super::hold(&manifest, tree.path(), false).unwrap();
            readings
                .into_iter()
                .find(|r| r.scope == "ext/openssl")
                .unwrap()
        };
        let held = openssl("1.0", "4");
        assert_eq!(held.status, Status::Pass, "{}", held.message);
        let note = "openssl-sys bound to openssl@4, the keg the pinned php loads libssl from";
        assert!(held.message.ends_with(note), "{}", held.message);
        let moved = openssl("2.0", "5");
        assert_eq!(moved.status, Status::Fail, "{}", moved.message);
        assert!(
            moved.message.starts_with(
                ".cargo/config.toml's AARCH64_APPLE_DARWIN_OPENSSL_DIR binds openssl-sys to openssl@4, \
                 and the pinned php loads libssl from openssl@5: the binding moves with the oracle"
            ),
            "{}",
            moved.message
        );
    }
}
