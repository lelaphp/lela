//! A host's link held to the macOS the pinned php is linked for (#3356).
//! Where no deployment target is named, rustc links for macOS 11.0 on
//! aarch64-apple-darwin, and ld64 writes the rebase and bind opcodes of
//! `LC_DYLD_INFO_ONLY`, which dyld applies at launch, dirtying every page
//! of `__DATA_CONST`; linked for the oracle's macOS, a host takes
//! `LC_DYLD_CHAINED_FIXUPS` as php does, which the kernel applies as each
//! page comes in. The macOS is the `[env]`'s `MACOSX_DEPLOYMENT_TARGET`,
//! which rustc reads, and `cc` in each `build.rs`; it is held in each host's
//! api-check reading to the `minos` the pinned php's load commands carry,
//! so a pin linked for another macOS fails each host's reading naming
//! both, as [`super::binding`] holds a native's keg. Where the pinned php
//! is not on the machine or is no macOS Mach-O file, a Linux runner's
//! build, nothing is held.

use std::path::Path;

use anyhow::Result;

use super::surfaces::Held;
use crate::generated::cargo_env;
use crate::manifest::Manifest;
use crate::oracle::Minos;
use crate::oracle::hosts::{HOSTS, MAKALA};

/// The variable that names the macOS a link is for.
const VARIABLE: &str = "MACOSX_DEPLOYMENT_TARGET";

/// What a host's link is held to on this machine.
pub(super) enum Target {
    /// No pin, or a pinned php not on this machine or no macOS Mach-O file.
    Unheld,
    /// The pinned php's load commands, which could not be read.
    Unread(anyhow::Error),
    /// The macOS the pinned php is linked for, and the first `[env]`
    /// entry that names [`VARIABLE`], by its configuration and value.
    Read {
        minos: Minos,
        named: Option<(&'static str, String)>,
    },
}

impl Target {
    /// The target of the workspace at `root` and the pin `manifest`
    /// carries.
    pub(super) fn read(manifest: &Manifest, root: &Path) -> Result<Target> {
        let Some(php) = manifest.pin.as_ref().map(|pin| &pin.oracle.path) else {
            return Ok(Target::Unheld);
        };
        if !php.exists() {
            return Ok(Target::Unheld);
        }
        let minos = match Minos::of(php) {
            Ok(Some(minos)) => minos,
            Ok(None) => return Ok(Target::Unheld),
            Err(err) => return Ok(Target::Unread(err)),
        };
        let named = cargo_env(root)?
            .into_iter()
            .find_map(|(config, mut entries)| {
                Some((config, entries.remove(VARIABLE)?.value().to_owned()))
            });
        Ok(Target::Read { minos, named })
    }

    /// What the link of the crate `name` comes to: nothing for a crate
    /// that is no host, a note where its target is the pinned php's, and
    /// a problem where it is another, none, or unread.
    pub(super) fn hold(&self, name: &str) -> Held {
        let mut held = Held::default();
        if !HOSTS.iter().chain([&MAKALA]).any(|host| host.krate == name) {
            return held;
        }
        match self {
            Target::Unheld => {}
            Target::Unread(why) => held.problems.push(format!(
                "{name} links a host, whose {VARIABLE} is held to the macOS the pinned php is linked for, and its load commands are unread: {why:#}"
            )),
            Target::Read { minos, named: Some((_, value)) } if Minos::parse(value) == Some(*minos) => {
                held.notes.push(format!("linked for macOS {minos}, the pinned php's minos"));
            }
            Target::Read { minos, named: Some((config, value)) } => held.problems.push(format!(
                "{config}'s {VARIABLE} is {value}, and the pinned php is linked for macOS {minos}: the target moves with the oracle (#3356)"
            )),
            Target::Read { minos, named: None } => held.problems.push(format!(
                "{name} links a host, and no cargo configuration's [env] names {VARIABLE}, so rustc links it for its own default where the pinned php is linked for macOS {minos} (#3356)"
            )),
        }
        held
    }
}

#[cfg(test)]
mod tests {
    use super::{Minos, Target};

    /// A host's link holds where the `[env]` names the pinned php's
    /// `minos`, and fails naming both where it names another macOS or
    /// none; a crate that is no host is not held.
    #[test]
    fn a_host_is_linked_for_the_macos_the_pinned_php_is() {
        let target = |value: Option<&str>| Target::Read {
            minos: Minos::parse("15.0").unwrap(),
            named: value.map(|value| (".cargo/config.toml", value.to_owned())),
        };
        let same = target(Some("15.0")).hold("sapi/cli");
        assert!(same.problems.is_empty(), "{:?}", same.problems);
        assert_eq!(
            same.notes,
            ["linked for macOS 15.0, the pinned php's minos"]
        );
        assert_eq!(
            target(Some("11.0")).hold("sapi/fpm").problems,
            [
                ".cargo/config.toml's MACOSX_DEPLOYMENT_TARGET is 11.0, and the pinned php is \
                 linked for macOS 15.0: the target moves with the oracle (#3356)"
            ]
        );
        let unnamed = target(None).hold("sapi/makala").problems;
        assert!(
            unnamed[0].contains("no cargo configuration's [env] names MACOSX_DEPLOYMENT_TARGET"),
            "{unnamed:?}"
        );
        let library = target(None).hold("crates/php_engine");
        assert!(library.problems.is_empty() && library.notes.is_empty());
    }
}
