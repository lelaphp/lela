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

use super::surfaces::Held;
use super::workspace::Present;
use crate::generated::cargo_env;
use crate::manifest::Manifest;
use crate::oracle::Own;

/// A package that links a native library the oracle links too: the
/// `[env]` variable that points its build at the library, the forms of it
/// that build reads ([`Reads`]), and the library php loads, by its file's
/// stem.
struct Native {
    package: &'static str,
    variable: &'static str,
    reads: Reads,
    library: &'static str,
}

/// Which forms of a native's variable the build that links it reads: the
/// bare name, the name after the target's (`AARCH64_APPLE_DARWIN_`), or
/// either, each as the `build.rs` that reads it is written, so a binding
/// is held where the build reads it and no form it ignores binds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Reads {
    /// openssl-sys 0.9's `env`, the target's form and then the bare one
    /// (`build/main.rs:42-46`).
    Either,
    /// libsqlite3-sys 0.38's `{env_prefix}_LIB_DIR`, the bare name alone
    /// (`build.rs:468`).
    Bare,
    /// ext/curl's own `build.rs`, the target's form alone; curl-sys reads
    /// no `CURL_LIB_DIR` of any form.
    Target,
}

/// The packages the workspace links a native library of the oracle's
/// through, each read where `.cargo/config.toml` binds it.
static NATIVES: [Native; 3] = [
    Native {
        package: "openssl-sys",
        variable: "OPENSSL_DIR",
        reads: Reads::Either,
        library: "libssl",
    },
    Native {
        package: "libsqlite3-sys",
        variable: "SQLITE3_LIB_DIR",
        reads: Reads::Bare,
        library: "libsqlite3",
    },
    Native {
        package: "curl-sys",
        variable: "CURL_LIB_DIR",
        reads: Reads::Target,
        library: "libcurl",
    },
];

impl Native {
    /// Whether `variable` is this package's on the target the harness is
    /// built for, the one the engine's build is, in a form its build reads
    /// ([`Reads`]), so another target's binding is not held to this
    /// machine's php and a form the build ignores binds nothing.
    fn names(&self, variable: &str) -> bool {
        let host = env!("XTASK_TARGET").to_uppercase().replace('-', "_");
        let bare = variable == self.variable;
        let target = variable.strip_prefix(&host) == Some(&format!("_{}", self.variable));
        match self.reads {
            Reads::Either => bare || target,
            Reads::Bare => bare,
            Reads::Target => target,
        }
    }
}

/// One `[env]` entry that binds a native: the configuration it is in, the
/// variable, and the directory it names.
pub(super) struct Bound {
    config: &'static str,
    variable: String,
    path: PathBuf,
    native: &'static Native,
}

/// What a crate's bindings are held to on this machine.
pub(super) enum Bindings {
    /// No pin, or a pinned php under no Homebrew Cellar here.
    Unheld,
    /// The pinned php's own loads, which could not be read.
    Unread(anyhow::Error),
    /// The pinned php's own loads, and each `[env]` entry that binds a
    /// native on this target.
    Read { own: Own, bound: Vec<Bound> },
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
            Err(err) => return Ok(Bindings::Unread(err)),
        };
        let mut bound = Vec::new();
        for (config, entries) in cargo_env(root)? {
            for (variable, entry) in entries {
                let Some(native) = NATIVES.iter().find(|n| n.names(&variable)) else {
                    continue;
                };
                let path = entry.path(root);
                bound.push(Bound {
                    config,
                    variable,
                    path,
                    native,
                });
            }
        }
        Ok(Bindings::Read { own, bound })
    }

    /// What `krate`'s links to the natives come to: each binding that
    /// does not hold, and a note for each that does.
    pub(super) fn hold(&self, krate: &Present) -> Held {
        let mut held = Held::default();
        let linked = NATIVES
            .iter()
            .filter(|n| krate.external.contains(n.package));
        let (own, bound) = match self {
            Bindings::Unheld => return held,
            Bindings::Unread(why) => {
                held.problems.extend(linked.map(|native| format!(
                    "{} links {}, whose binding is held to the libraries the pinned php loads, and those are unread: {why:#}",
                    krate.name, native.package
                )));
                return held;
            }
            Bindings::Read { own, bound } => (own, bound),
        };
        for native in linked {
            let (name, package, library) = (&krate.name, native.package, native.library);
            let Some(loaded) = own.keg_loading(library) else {
                held.problems.push(format!(
                    "{name} links {package}, and the pinned php loads no {library} of its own, so no keg holds the binding (#3458)"
                ));
                continue;
            };
            let mut binds = bound
                .iter()
                .filter(|b| b.native.package == native.package)
                .peekable();
            if binds.peek().is_none() {
                held.problems.push(format!(
                    "{name} links {package}, and no cargo configuration's [env] names its {}, so its build links the first {library} a search finds where the pinned php loads {loaded}'s (#3458)",
                    native.variable
                ));
            }
            for Bound {
                config,
                variable,
                path,
                ..
            } in binds
            {
                match own.keg_at(path) {
                    Some(keg) if keg == loaded => held.notes.push(format!(
                        "{package} bound to {keg}, the keg the pinned php loads {library} from"
                    )),
                    keg => held.problems.push(format!(
                        "{config}'s {variable} binds {package} to {}, and the pinned php loads {library} from {loaded}: the binding moves with the oracle (#3414, #3458)",
                        keg.unwrap_or_else(|| format!("{}, no keg of the pinned php's Homebrew", path.display()))
                    )),
                }
            }
        }
        held
    }
}

#[cfg(test)]
mod names {
    use super::NATIVES;

    /// Each native's variable binds in the forms its build reads and no
    /// other (#3458): openssl-sys's bare or after the name of the target
    /// the harness is built for, libsqlite3-sys's bare alone, and ext/curl's
    /// after the target alone; none after another target's name or any
    /// other prefix.
    #[test]
    fn a_variable_binds_in_the_forms_its_build_reads_alone() {
        let host = env!("XTASK_TARGET").to_uppercase().replace('-', "_");
        let forms = |variable: &str| {
            let reads = NATIVES.iter().find(|n| n.variable == variable).unwrap();
            [
                variable.to_owned(),
                format!("{host}_{variable}"),
                format!("POWERPC64_IBM_AIX_{variable}"),
                format!("DEP_{variable}"),
            ]
            .map(|form| reads.names(&form))
        };
        assert_eq!(forms("OPENSSL_DIR"), [true, true, false, false]);
        assert_eq!(forms("SQLITE3_LIB_DIR"), [true, false, false, false]);
        assert_eq!(forms("CURL_LIB_DIR"), [false, true, false, false]);
        assert!(!NATIVES[0].names(&format!("{host}_OPENSSL_LIB_DIR")));
    }
}

#[cfg(test)]
#[cfg(tracked)]
mod tests {
    use std::fs;

    use crate::manifest::Manifest;
    use crate::oracle::stand_in::macho;
    use crate::reading::{Reading, Status};
    use crate::scratch::{Tree, repository};

    /// `xtask/fixtures/api-check/binding` laid at a scratch tree, its
    /// `.cargo/config.toml` binding ext/openssl's openssl-sys to openssl@4.
    fn fixture(name: &str) -> Tree {
        let fixture = crate::package_dir().join("fixtures/api-check/binding");
        let tree = Tree::new(&format!("api-check-binding-{name}")).unwrap();
        repository(&fixture, tree.path(), &["."]).unwrap();
        tree
    }

    /// The load command that names libssl in the fixture's keg of `major`.
    fn libssl(major: &str) -> String {
        let full = if major == "4" { "4.0.3" } else { "5.0.0" };
        format!("@loader_path/../../../openssl@{major}/{full}/lib/libssl.{major}.dylib")
    }

    /// ext/openssl's reading where the pinned php is a stand-in build at
    /// `version` in the tree's Cellar naming `loads`.
    fn reading(tree: &Tree, version: &str, loads: &[&str]) -> Reading {
        let bin = tree.path().join(format!("brew/Cellar/php/{version}/bin"));
        fs::create_dir_all(&bin).unwrap();
        fs::write(bin.join("php"), macho(loads)).unwrap();
        let mut manifest = Manifest::load(crate::package_dir().parent().unwrap()).unwrap();
        manifest.pin.as_mut().unwrap().oracle.path = bin.join("php");
        let readings = super::super::hold(&manifest, tree.path(), false).unwrap();
        (readings.into_iter())
            .find(|r| r.scope == "ext/openssl")
            .unwrap()
    }

    /// The fixture's tree with its `[env]` binding of openssl-sys written
    /// as `binding`, none for no line.
    fn bound(tree: &Tree, binding: Option<&str>) {
        let other =
            "POWERPC64_IBM_AIX_OPENSSL_DIR = { value = \"/nowhere/openssl@9\", force = true }";
        let line = binding.map_or_else(String::new, |b| format!("OPENSSL_DIR = {b}\n"));
        let config = format!("[env]\n{line}{other}\n");
        fs::write(tree.path().join(".cargo/config.toml"), config).unwrap();
    }

    /// A crate's binding is the keg the pinned php loads its library from
    /// (#3458): the fixture binds ext/openssl's openssl-sys to openssl@4,
    /// and of two stand-in php builds laid in its Cellar, 1.0 loads libssl
    /// from openssl@4 and holds it, and 2.0, a pin that moved OpenSSL's
    /// major, loads it from openssl@5 and fails naming both.
    #[test]
    fn a_binding_to_another_major_than_the_pinned_php_loads_fails_naming_both() {
        let tree = fixture("major");
        let held = reading(&tree, "1.0", &[&libssl("4")]);
        assert_eq!(held.status, Status::Pass, "{}", held.message);
        let note = "openssl-sys bound to openssl@4, the keg the pinned php loads libssl from";
        assert!(held.message.ends_with(note), "{}", held.message);
        let moved = reading(&tree, "2.0", &[&libssl("5")]);
        assert_eq!(moved.status, Status::Fail, "{}", moved.message);
        assert!(
            moved.message.starts_with(
                ".cargo/config.toml's OPENSSL_DIR binds openssl-sys to openssl@4, \
                 and the pinned php loads libssl from openssl@5: the binding moves with the oracle"
            ),
            "{}",
            moved.message
        );
    }

    /// Each binding the pinned php cannot hold fails the crate's reading
    /// saying why (#3458): no `[env]` line naming openssl-sys's variable,
    /// so its build links the first libssl a search finds; a binding to a
    /// directory in no keg of the php's Homebrew; a php that loads no
    /// libssl of its own; and a php whose loads cannot be read, a name
    /// under `@executable_path/` the walk refuses to place.
    #[test]
    fn a_binding_the_pinned_php_cannot_hold_fails_saying_why() {
        let tree = fixture("refused");
        let fails = |loads: &[&str], why: &str| {
            let reading = reading(&tree, "1.0", loads);
            assert_eq!(reading.status, Status::Fail, "{why}: {}", reading.message);
            assert!(reading.message.contains(why), "{why}: {}", reading.message);
        };
        let ssl = libssl("4");
        bound(&tree, None);
        fails(
            &[&ssl],
            "ext/openssl links openssl-sys, and no cargo configuration's [env] names its OPENSSL_DIR, \
             so its build links the first libssl a search finds where the pinned php loads openssl@4's",
        );
        bound(
            &tree,
            Some("{ value = \"ext/openssl\", relative = true, force = true }"),
        );
        fails(
            &[&ssl],
            "/ext/openssl, no keg of the pinned php's Homebrew, and the pinned php loads libssl from openssl@4",
        );
        bound(
            &tree,
            Some("{ value = \"brew/Cellar/openssl@4/4.0.3\", relative = true, force = true }"),
        );
        assert_eq!(reading(&tree, "1.0", &[&ssl]).status, Status::Pass);
        fails(
            &[],
            "ext/openssl links openssl-sys, and the pinned php loads no libssl of its own, so no keg holds the binding",
        );
        fails(
            &["@executable_path/../lib/libssl.4.dylib"],
            "ext/openssl links openssl-sys, whose binding is held to the libraries the pinned php loads, and those are unread: ",
        );
    }
}
