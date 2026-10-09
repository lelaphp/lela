//! The files the manifest's `generated` predicate names, rendered from the
//! rules they are derived from (harness-design.md §4.1, §13 D.19). Each is
//! a `clippy.toml`, which clippy finds by nearest-file search: the one at
//! `<dir>/clippy.toml` carries every rule whose scope reaches `<dir>`, so a
//! crate under it is held to all of them. `deny_types` renders to
//! `disallowed-types`, with the rule's text as each reason and wildcards
//! expanded from `Cargo.lock` to the paths a package's types are reachable
//! at (#1910); its `deny_macros` to `disallowed-macros` (#1781); and
//! `deny_methods` to `disallowed-methods` (#1950). The committed copies are
//! held to the rendering by `budget`.

use std::collections::BTreeMap;
use std::fmt::Write;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::lock::{CargoLock, Expanded, package_of};
use crate::manifest::{Manifest, Rule};

/// Every generated file the manifest names, with its rendering.
pub(crate) fn render_all(root: &Path, manifest: &Manifest) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for rule in &manifest.rules {
        for (file, from) in rule.predicate.generated.iter().flatten() {
            out.insert(file.clone(), render(root, manifest, file, from)?);
        }
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
        fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;
        written.push(path);
    }
    Ok(written)
}

/// Render one generated file from the rule it is derived from.
pub(crate) fn render(root: &Path, manifest: &Manifest, file: &str, from: &str) -> Result<String> {
    let rule = manifest.rule(from).with_context(|| {
        format!("{file} is generated from rule {from}, which the manifest lacks")
    })?;
    match file
        .rsplit_once('/')
        .map_or(("", file), |(dir, name)| (dir, name))
    {
        (dir, "clippy.toml") => clippy_toml(root, manifest, rule, file, dir),
        _ => bail!("no generator renders {file} (from rule {from})"),
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

/// Every clippy rule's scope has its `clippy.toml` among the generated
/// files, so no such rule renders to nothing.
fn every_scope_rendered(manifest: &Manifest) -> Result<()> {
    let files: Vec<&String> = manifest
        .rules
        .iter()
        .flat_map(|r| r.predicate.generated.iter().flatten().map(|(file, _)| file))
        .collect();
    for rule in manifest.rules.iter().filter(|r| renders_to_clippy(r)) {
        for dir in clippy_dirs(rule)? {
            let file = if dir.is_empty() {
                "clippy.toml".to_owned()
            } else {
                format!("{dir}/clippy.toml")
            };
            if !files.contains(&&file) {
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
/// directory, `from` among them, each list a rule renders under its key.
fn clippy_toml(
    root: &Path,
    manifest: &Manifest,
    from: &Rule,
    file: &str,
    dir: &str,
) -> Result<String> {
    every_scope_rendered(manifest)?;
    let mut rules = Vec::new();
    for rule in manifest.rules.iter().filter(|r| renders_to_clippy(r)) {
        if reaches(rule, dir)? {
            rules.push(rule);
        }
    }
    if !rules.iter().any(|r| r.id == from.id) {
        bail!(
            "{file} is generated from rule {}, which renders nothing to clippy.toml or whose scope does not reach {dir}/",
            from.id
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
         # generated.committed. Clippy finds this file by nearest-file search, so it\n\
         # covers every crate under {dir}/ that no nearer clippy.toml covers.\n",
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
