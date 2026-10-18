//! The files the manifest's `generated` predicate names, each rendered
//! from every rule whose scope reaches its directory ([`reaches`];
//! harness-design.md §4.1, §13 D.19): the row names the files and no rule
//! (#2473). Each is a `clippy.toml`, which clippy finds by nearest-file
//! search: the one at `<dir>/clippy.toml` carries every rule whose scope
//! reaches `<dir>`, so a crate under it is held to all of them.
//! `deny_types` renders to `disallowed-types`, with the rule's text as each reason and wildcards
//! expanded from `Cargo.lock` to the paths a package's types are reachable
//! at (#1910); its `deny_macros` to `disallowed-macros` (#1781); and
//! `deny_methods` to `disallowed-methods` (#1950). The committed copies are
//! held to the rendering by `budget`.
//!
//! Clippy reads the nearest configuration alone and merges nothing, and
//! in one directory a `.clippy.toml` before a `clippy.toml`: a crate
//! carrying either of its own, below a generated file, is held to none of
//! the rules that file carries. `budget` refuses each such file
//! ([`nearer`], #2475). `CLIPPY_CONF_DIR`, set, starts clippy's search in
//! one directory for every crate, past every generated file. Neither the
//! workflows, the scripts nor `xtask` set it, and three things keep it
//! from `xtask clippy`, the instrument CI and pre-commit run: its cargo
//! runs without it in the environment ([`crate::batch::Run::of`]); cargo
//! hands its `[env]` table to clippy-driver all the same, so `budget`
//! refuses a cargo configuration of the checkout that sets it
//! ([`conf_dir_set`]); and a configuration above the checkout or under
//! `CARGO_HOME` is the machine's, not the tree's, and CI's runner carries
//! none.

use std::collections::BTreeMap;
use std::fmt::Write;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::git;
use crate::lock::{CargoLock, Expanded, package_of};
use crate::manifest::{Manifest, Rule};

/// The variable that starts clippy's search for its configuration in one
/// directory for every crate.
pub(crate) const CONF_DIR: &str = "CLIPPY_CONF_DIR";

/// The names clippy reads its configuration under, in the order it takes
/// them within one directory.
const CONF_NAMES: [&str; 2] = [".clippy.toml", "clippy.toml"];

/// The cargo configurations of the checkout, the ones cargo reads at the
/// root it runs in.
const CARGO_CONFIGS: [&str; 2] = [".cargo/config.toml", ".cargo/config"];

/// Every generated file the manifest names, with its rendering.
pub(crate) fn render_all(root: &Path, manifest: &Manifest) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for file in manifest.generated_files() {
        out.insert(file.to_owned(), render(root, manifest, file)?);
    }
    Ok(out)
}

/// Write every generated file under the root; the paths written.
pub fn write_all(root: &Path, manifest: &Manifest) -> Result<Vec<PathBuf>> {
    let mut written = Vec::new();
    for (file, text) in render_all(root, manifest)? {
        let path = root.join(&file);
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        crate::write(&path, text)?;
        written.push(path);
    }
    Ok(written)
}

/// Render one generated file from the rules that reach it. Only a
/// `clippy.toml` has a generator, its rules read off the scopes; a file of
/// another kind would have no scope to read its source off, so it would
/// name that source again in the row, as a `{ file, from }` entry beside
/// the paths, and its generator would be an arm here.
pub(crate) fn render(root: &Path, manifest: &Manifest, file: &str) -> Result<String> {
    let dir = dir_of(file);
    match file
        .strip_prefix(dir)
        .map(|name| name.trim_start_matches('/'))
    {
        Some("clippy.toml") => clippy_toml(root, manifest, file, dir),
        _ => bail!(
            "no generator renders {file}: only a clippy.toml is generated, from the rules whose scope reaches it"
        ),
    }
}

/// Whether a rule renders into a `clippy.toml`: a `deny_types` rule, with
/// the `deny_macros` beside it, or a `deny_methods` rule.
fn renders_to_clippy(rule: &Rule) -> bool {
    rule.predicate.deny_types.is_some() || rule.predicate.deny_methods.is_some()
}

/// The directories a clippy rule's scope names: each pattern less a
/// trailing `/*` or `/**`, the directory whose `clippy.toml` reaches it.
/// A wildcard anywhere else names files no one `clippy.toml` stands
/// above, and an `exempt` is a path clippy never reads, so both are
/// refused: a site the rule allows carries its own `#[expect]`.
fn clippy_dirs(rule: &Rule) -> Result<Vec<&str>> {
    if !rule.exempt.is_empty() {
        bail!(
            "rule {} renders to clippy.toml and carries `exempt`, which clippy cannot read; a site it allows carries #[expect]",
            rule.id
        );
    }
    rule.scope
        .iter()
        .map(|pattern| {
            let dir = pattern
                .strip_suffix("/**")
                .or_else(|| pattern.strip_suffix("/*"))
                .unwrap_or(pattern)
                .trim_end_matches('/');
            if dir.contains('*') {
                bail!(
                    "rule {}'s scope {pattern} is not a directory or its children, so no clippy.toml covers it",
                    rule.id
                );
            }
            Ok(dir)
        })
        .collect()
}

/// Whether a `clippy.toml` in `dir` carries `rule`: its scope names `dir`
/// or a directory above it, since the nearer file is the one clippy reads
/// for every crate below it.
fn reaches(rule: &Rule, dir: &str) -> Result<bool> {
    Ok(clippy_dirs(rule)?
        .into_iter()
        .any(|scope| dir == scope || scope.is_empty() || dir.starts_with(&format!("{scope}/"))))
}

/// The rules a `clippy.toml` in `dir` carries: every clippy rule whose
/// scope reaches the directory, in the manifest's order.
pub(crate) fn carried<'m>(manifest: &'m Manifest, dir: &str) -> Result<Vec<&'m Rule>> {
    let mut rules = Vec::new();
    for rule in manifest.rules.iter().filter(|r| renders_to_clippy(r)) {
        if reaches(rule, dir)? {
            rules.push(rule);
        }
    }
    Ok(rules)
}

/// The directory a generated file stands in, `""` at the root.
pub(crate) fn dir_of(file: &str) -> &str {
    file.rsplit_once('/').map_or("", |(dir, _)| dir)
}

/// The path of the `clippy.toml` in `dir`, `""` being the root.
fn clippy_toml_in(dir: &str) -> String {
    if dir.is_empty() {
        "clippy.toml".to_owned()
    } else {
        format!("{dir}/clippy.toml")
    }
}

/// The clippy configurations nearer than `file`, one of the generated
/// files `generated` names, to some crate below it: each `clippy.toml` or
/// `.clippy.toml` under its directory, a `.clippy.toml` beside it among
/// them, that is no generated file and for which `file` is the nearest
/// generated one above, so a file under a nested generated one is that
/// one's. Tracked or not, since a local run of clippy reads either.
pub(crate) fn nearer(root: &Path, generated: &[&str], file: &str) -> Result<Vec<String>> {
    let dir = dir_of(file);
    let scope = if dir.is_empty() { "." } else { dir };
    Ok(git::files(root, Path::new(scope))?
        .into_iter()
        .filter(|path| {
            let (at, name) = path.rsplit_once('/').unwrap_or(("", path.as_str()));
            CONF_NAMES.contains(&name)
                && !generated.contains(&path.as_str())
                && nearest_generated(generated, at) == Some(file)
        })
        .collect())
}

/// The generated `clippy.toml` clippy would read for a crate in `dir`
/// with no other configuration about: the one in `dir` or nearest above.
fn nearest_generated<'g>(generated: &[&'g str], dir: &str) -> Option<&'g str> {
    let mut at = dir;
    loop {
        let candidate = clippy_toml_in(at);
        if let Some(found) = generated.iter().find(|g| **g == candidate) {
            return Some(found);
        }
        if at.is_empty() {
            return None;
        }
        at = dir_of(at);
    }
}

/// The cargo configurations of the checkout whose `[env]` sets
/// [`CONF_DIR`]: cargo hands that table to clippy-driver, with
/// `force` or without it once the environment has no value of its own.
pub(crate) fn conf_dir_set(root: &Path) -> Result<Vec<&'static str>> {
    let mut out = Vec::new();
    for config in CARGO_CONFIGS {
        let Ok(text) = fs::read_to_string(root.join(config)) else {
            continue;
        };
        let table: toml::Table =
            toml::from_str(&text).with_context(|| format!("{config} is not TOML"))?;
        let sets = table
            .get("env")
            .and_then(toml::Value::as_table)
            .is_some_and(|env| env.contains_key(CONF_DIR));
        if sets {
            out.push(config);
        }
    }
    Ok(out)
}

/// Every clippy rule's scope has its `clippy.toml` among the generated
/// files, so no such rule renders to nothing.
fn every_scope_rendered(manifest: &Manifest) -> Result<()> {
    let files = manifest.generated_files();
    for rule in manifest.rules.iter().filter(|r| renders_to_clippy(r)) {
        for dir in clippy_dirs(rule)? {
            let file = clippy_toml_in(dir);
            if !files.contains(&file.as_str()) {
                bail!(
                    "rule {}'s scope reaches {dir}/, and no generated rule renders {file}, so clippy would hold nothing there",
                    rule.id
                );
            }
        }
    }
    Ok(())
}

/// The `clippy.toml` in `dir`: every clippy rule whose scope reaches the
/// directory, each list a rule renders under its key. A file no rule
/// reaches would carry nothing, and is refused.
fn clippy_toml(root: &Path, manifest: &Manifest, file: &str, dir: &str) -> Result<String> {
    every_scope_rendered(manifest)?;
    let rules = carried(manifest, dir)?;
    if rules.is_empty() {
        bail!(
            "{file} is generated, and no clippy rule's scope reaches {dir}/, so it would carry nothing"
        );
    }
    let lock = CargoLock::load(root)?;
    let named: Vec<String> = rules
        .iter()
        .map(|r| {
            let cited = r.adr.adr_cited();
            if cited.is_empty() {
                format!("{} (no ADR)", r.id)
            } else {
                format!("{} {cited}", r.id)
            }
        })
        .collect();
    let mut out = format!(
        "# Generated by `xtask generate` from seams.toml rule{} {}; do not edit.\n\
         # A copy that differs from the regenerated one fails `xtask budget` under\n\
         # generated.committed. Clippy reads the nearest configuration alone, so a\n\
         # clippy.toml or .clippy.toml nearer to a crate under {dir}/ fails it too.\n",
        if named.len() == 1 { "" } else { "s" },
        named.join(", ")
    );
    let mut types = String::new();
    let mut macros = String::new();
    let mut methods = String::new();
    for rule in &rules {
        let cited = rule.adr.adr_cited();
        let reason = if cited.is_empty() {
            rule.text.clone()
        } else {
            format!("{} {cited}", rule.text)
        };
        for ty in rule.predicate.deny_types.iter().flatten() {
            for ty in expand(&lock, ty, &mut types)? {
                entry(&mut types, &ty.path, &reason, ty.gated);
            }
        }
        for path in rule.predicate.deny_macros.iter().flatten() {
            entry(&mut macros, path, &reason, false);
        }
        for path in rule.predicate.deny_methods.iter().flatten() {
            entry(&mut methods, path, &reason, third_party(&lock, path));
        }
    }
    for (key, rows) in [
        ("disallowed-types", types),
        ("disallowed-macros", macros),
        ("disallowed-methods", methods),
    ] {
        if !rows.is_empty() {
            writeln!(out, "{key} = [\n{rows}]").ok();
        }
    }
    Ok(out)
}

/// One `deny_types` entry as the paths clippy matches: a wildcard expanded
/// from `Cargo.lock`, with a comment in `out` where it expands to nothing.
fn expand(lock: &CargoLock, ty: &str, out: &mut String) -> Result<Vec<Expanded>> {
    let Some(prefix) = ty.strip_suffix("::*") else {
        return Ok(vec![Expanded {
            path: ty.to_owned(),
            gated: false,
        }]);
    };
    let paths = lock.expand(prefix)?;
    if paths.is_empty() {
        writeln!(
            out,
            "    # {ty}: {} is not a Cargo.lock package at this revision; nothing to deny",
            package_of(prefix)
        )
        .ok();
    }
    Ok(paths)
}

/// Whether a function path is a third-party package's, whose items differ
/// by target and by feature: `libc::vfork` and `nix::unistd::daemon` are
/// not on Apple's, and `nix::unistd::fork` needs nix's `process`. Clippy
/// calls such a path unreachable in a build that leaves it out, so it is
/// rendered `allow-invalid`; a fixture under `xtask/fixtures/` holds that
/// each one is reached where the target has it. A path of the standard
/// library or of a workspace crate resolves in every build, and stays a
/// warning when it does not.
fn third_party(lock: &CargoLock, path: &str) -> bool {
    lock.package(package_of(path))
        .is_some_and(|p| p.source.is_some())
}

/// One `{ path, reason }` row of a `disallowed-*` list. A `gated` path,
/// one a build of its package can leave out, carries `allow-invalid`, so
/// clippy denies it where the build has it and says nothing where not.
fn entry(out: &mut String, path: &str, reason: &str, gated: bool) {
    writeln!(
        out,
        "    {{ path = {}, reason = {}{} }},",
        toml_string(path),
        toml_string(reason),
        if gated { ", allow-invalid = true" } else { "" }
    )
    .ok();
}

fn toml_string(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}
