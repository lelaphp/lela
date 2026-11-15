//! `CONFIGURE` held to the bundle's features: every switch stands
//! behind the feature of the extension it names, and every feature that
//! names an `ext/*` crate php does not always build has its switch, an
//! `--enable` or `--with` behind `cfg(feature)` where php's `config.m4`
//! leaves it out by default, a `--disable` or `--without` behind
//! `cfg(not(feature))` where it builds it. Read from the sources, so a
//! crate's commit that leaves the list stale fails here whatever the
//! features of this build.

use std::collections::BTreeMap;

const LIB: &str = include_str!("../src/lib.rs");
const MANIFEST: &str = include_str!("../Cargo.toml");

/// The extensions php builds with no switch at all: no `PHP_ARG_ENABLE`
/// or `PHP_ARG_WITH` of their own name in their `config*.m4`, or
/// `opcache`, which php 8.5 always builds.
const ALWAYS: &[&str] = &[
    "date",
    "hash",
    "json",
    "lexbor",
    "opcache",
    "pcre",
    "random",
    "reflection",
    "spl",
    "standard",
    "uri",
];

/// The extensions whose `config*.m4` defaults to `yes`, so php's
/// `./configure` builds them and the bundle names the switch that leaves
/// one out.
const BY_DEFAULT: &[&str] = &[
    "ctype",
    "dom",
    "fileinfo",
    "filter",
    "iconv",
    "libxml",
    "pdo",
    "pdo_sqlite",
    "phar",
    "posix",
    "session",
    "simplexml",
    "sqlite3",
    "tokenizer",
    "xml",
    "xmlreader",
    "xmlwriter",
];

/// The extensions whose `config.m4` names their switch otherwise:
/// readline's is `PHP_ARG_WITH([libedit])`, the library the oracle links.
const SPELLED: &[(&str, &str)] = &[("readline", "libedit")];

/// Each feature of the manifest that names an `ext/*` crate, as
/// `name = ["dep:php_ext_<crate>", ...]`.
fn extension_features() -> Vec<&'static str> {
    MANIFEST
        .lines()
        .filter_map(|line| {
            let (name, rest) = line.split_once(" = [")?;
            rest.starts_with("\"dep:php_ext_").then_some(name.trim())
        })
        .collect()
}

/// `CONFIGURE`'s rows as `(cfg attribute, switch)`, the attribute empty
/// where a switch stands unconditional.
fn rows() -> Vec<(String, String)> {
    let body = LIB
        .split_once("pub const CONFIGURE: &[&str] = &[")
        .and_then(|(_, rest)| rest.split_once("];"))
        .map_or_else(|| panic!("no CONFIGURE in src/lib.rs"), |(body, _)| body);
    let mut cfg = String::new();
    let mut rows = Vec::new();
    for line in body.lines().map(str::trim).filter(|line| !line.is_empty()) {
        if line.starts_with("#[cfg(") {
            line.clone_into(&mut cfg);
        } else if let Some(switch) = line.strip_prefix('"') {
            let switch = switch.trim_end_matches(',').trim_end_matches('"');
            rows.push((std::mem::take(&mut cfg), switch.to_owned()));
        }
    }
    rows
}

#[test]
fn every_switch_stands_behind_its_feature() {
    for (cfg, switch) in rows() {
        assert!(!cfg.is_empty(), "{switch} stands unconditional");
    }
}

#[test]
fn every_extension_php_does_not_always_build_has_its_switch() {
    // A feature may carry more than one switch: gd's `--enable-gd
    // --with-external-gd`, as the oracle's build names it.
    let mut by_cfg: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (cfg, switch) in rows() {
        by_cfg.entry(cfg).or_default().push(switch);
    }
    for feature in extension_features() {
        if ALWAYS.contains(&feature) {
            continue;
        }
        let spelled = SPELLED
            .iter()
            .find_map(|&(name, switch)| (name == feature).then(|| switch.to_owned()))
            .unwrap_or_else(|| feature.replace('_', "-"));
        let (cfg, switches) = if BY_DEFAULT.contains(&feature) {
            (
                format!("#[cfg(not(feature = \"{feature}\"))]"),
                [
                    format!("--disable-{spelled}"),
                    format!("--without-{spelled}"),
                ],
            )
        } else {
            (
                format!("#[cfg(feature = \"{feature}\")]"),
                [format!("--enable-{spelled}"), format!("--with-{spelled}")],
            )
        };
        let named = by_cfg.get(&cfg);
        assert!(
            named.is_some_and(|named| named.iter().any(|switch| switches.contains(switch))),
            "{feature}: no {} or {} behind {cfg}",
            switches[0],
            switches[1]
        );
    }
}

#[test]
fn the_features_are_read() {
    let features = extension_features();
    for feature in ["standard", "mbstring", "sqlite3", "gmp"] {
        assert!(features.contains(&feature), "{feature} in {features:?}");
    }
}
