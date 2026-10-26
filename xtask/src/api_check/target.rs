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

/// The variable that names the macOS a link is for, the one a crate's key
/// moves with ([`crate::key::REKEYED`]).
const VARIABLE: &str = crate::key::DEPLOYMENT_TARGET;

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
        let none = target(Some("fifteen")).hold("sapi/cgi").problems;
        assert!(
            none[0].contains("MACOSX_DEPLOYMENT_TARGET is fifteen"),
            "{none:?}"
        );
        let library = target(None).hold("crates/php_engine");
        assert!(library.problems.is_empty() && library.notes.is_empty());
    }

    /// api-check over `xtask/fixtures/api-check/host-edge` laid at a
    /// scratch tree, its configuration's `[env]` written as `env`, under a
    /// pin whose php is `php`'s bytes, none for a php not there: the
    /// reading of `sapi/cgi`, the fixture's host within its edges.
    fn cgi(name: &str, env: &str, php: Option<&[u8]>) -> crate::reading::Reading {
        let fixture = crate::package_dir().join("fixtures/api-check/host-edge");
        let tree = crate::scratch::Tree::new(&format!("api-check-target-{name}")).unwrap();
        crate::scratch::repository(&fixture, tree.path(), &["."]).unwrap();
        std::fs::write(tree.join(".cargo/config.toml"), env).unwrap();
        let at = tree.join("php");
        if let Some(bytes) = php {
            std::fs::write(&at, bytes).unwrap();
        }
        let root = crate::package_dir().parent().unwrap().to_path_buf();
        let mut manifest = crate::manifest::Manifest::load(&root).unwrap();
        manifest.pin.as_mut().unwrap().oracle.path = at;
        let readings = super::super::hold(&manifest, tree.path(), false).unwrap();
        (readings.into_iter())
            .find(|r| r.scope == "sapi/cgi")
            .unwrap()
    }

    /// The hold as api-check runs it, `Target::read` over a stand-in php
    /// linked for macOS 15.0 and the workspace's configuration: a host's
    /// reading notes the target where `[env]` names 15.0, fails naming
    /// both where it names 11.0, and fails where nothing names it; a pin
    /// whose load commands cannot be read fails it as unread, and one not
    /// there or no Mach-O file holds nothing.
    #[test]
    fn a_host_s_reading_holds_its_target_to_the_pinned_php_s() {
        use crate::oracle::stand_in::macos_15;
        use crate::reading::Status;
        let named = |value: &str| format!("[env]\nMACOSX_DEPLOYMENT_TARGET = \"{value}\"\n");
        let held = cgi("agrees", &named("15.0"), Some(&macos_15()));
        assert_eq!(held.status, Status::Pass, "{}", held.message);
        let note = "linked for macOS 15.0, the pinned php's minos";
        assert!(held.message.contains(note), "{}", held.message);
        for (name, env) in [("differs", named("11.0")), ("unnamed", String::new())] {
            let held = cgi(name, &env, Some(&macos_15()));
            assert_eq!(held.status, Status::Fail, "{name}: {}", held.message);
            let said = held.message.contains("MACOSX_DEPLOYMENT_TARGET");
            assert!(said, "{name}: {}", held.message);
        }
        // A 32-bit Mach-O header, whose load commands are not read past.
        let mut magic = vec![0; 32];
        magic[..4].copy_from_slice(&0xfeed_face_u32.to_le_bytes());
        let unread = cgi("unread", &named("15.0"), Some(&magic));
        assert_eq!(unread.status, Status::Fail, "{}", unread.message);
        let said = unread.message.contains("its load commands are unread");
        assert!(said, "{}", unread.message);
        for (name, php) in [("absent", None), ("text", Some(&b"#!/bin/sh\n"[..]))] {
            let held = cgi(name, &named("11.0"), php);
            assert_eq!(held.status, Status::Pass, "{name}: {}", held.message);
            assert!(!held.message.contains("macOS"), "{name}: {}", held.message);
        }
    }
}
