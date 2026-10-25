//! A host's link held to the macOS the pinned php is linked for (#3356):
//! the `[env]`'s `MACOSX_DEPLOYMENT_TARGET`, whose `.cargo/config.toml`
//! entry says why it is named, is the `minos` the pinned php's load
//! commands carry, so a pin linked for another macOS fails each host's
//! api-check reading naming both, as [`super::binding`] holds a native's
//! keg. Where the pinned php is not on the machine or is no macOS Mach-O
//! file, a Linux runner's build, nothing is held.

use std::path::Path;

use anyhow::Result;

use super::surfaces::Held;
use crate::generated::cargo_env;
use crate::manifest::Manifest;
use crate::oracle::Minos;
use crate::oracle::hosts::{HOSTS, MAKALA};

/// The variable that names the macOS a link is for.
const VARIABLE: &str = "MACOSX_DEPLOYMENT_TARGET";

/// What a host's link is held to on this machine, each arm with the macOS
/// the pinned php is linked for where it was read.
pub(super) enum Target {
    /// No pin, or a pinned php not on this machine or no macOS Mach-O file.
    Unheld,
    /// The pinned php's load commands, which could not be read.
    Unread(anyhow::Error),
    /// The first `[env]` entry naming [`VARIABLE`] names the pinned php's.
    Agrees(Minos),
    /// It names another macOS, or a value that names none: the
    /// configuration it is in and the value as written.
    Differs {
        minos: Minos,
        config: &'static str,
        value: String,
    },
    /// No `[env]` names it, so rustc links for its own default.
    Unnamed(Minos),
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
        Ok(Target::named(minos, named))
    }

    /// The pinned php's `minos` against the `[env]` entry naming
    /// [`VARIABLE`], by its configuration and value, decoded once.
    fn named(minos: Minos, named: Option<(&'static str, String)>) -> Target {
        match named {
            None => Target::Unnamed(minos),
            Some((_, value)) if Minos::parse(&value) == Some(minos) => Target::Agrees(minos),
            Some((config, value)) => Target::Differs {
                minos,
                config,
                value,
            },
        }
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
                "{name} is a host, whose {VARIABLE} is held to the macOS the pinned php is linked for, and its load commands are unread: {why:#}"
            )),
            Target::Agrees(minos) => held
                .notes
                .push(format!("linked for macOS {minos}, the pinned php's minos")),
            Target::Differs {
                minos,
                config,
                value,
            } => held.problems.push(format!(
                "{config}'s {VARIABLE} is {value}, and the pinned php is linked for macOS {minos}: the target moves with the oracle (#3356)"
            )),
            Target::Unnamed(minos) => held.problems.push(format!(
                "{name} is a host, and no cargo configuration's [env] names {VARIABLE}, so rustc links it for its own default where the pinned php is linked for macOS {minos} (#3356)"
            )),
        }
        held
    }
}

#[cfg(test)]
mod tests {
    use super::{Minos, Target};

    /// A host's link holds where the `[env]` names the pinned php's
    /// `minos` written either way, and fails naming both where it names
    /// another macOS, a value that is none, or nothing; a crate that is no
    /// host is not held.
    #[test]
    fn a_host_is_linked_for_the_macos_the_pinned_php_is() {
        let target = |value: Option<&str>| {
            let named = value.map(|value| (".cargo/config.toml", value.to_owned()));
            Target::named(Minos::parse("15.0").unwrap(), named)
        };
        assert!(target(Some("15")).hold("sapi/cli").problems.is_empty());
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
        let unread = target(Some("fifteen")).hold("sapi/cgi").problems;
        assert!(
            unread[0].contains("MACOSX_DEPLOYMENT_TARGET is fifteen"),
            "{unread:?}"
        );
        let library = target(None).hold("crates/php_engine");
        assert!(library.problems.is_empty() && library.notes.is_empty());
    }
}
