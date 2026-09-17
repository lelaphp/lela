//! `xtask crosscheck`, the rendering diff (design §7.1, §4.4; harness
//! §7, ledger 41). php prints the whole of an extension itself: `php --re
//! <name>` constructs a `ReflectionExtension` and prints its
//! `__toString()`, every constant, ini entry, function, class, method,
//! property and the tentative-return row, in php's wording and order. The
//! lane runs the oracle's `--re` under the recipe for each extension of
//! the bundle's default set, or the one `--ext` names, keeps the oracle's
//! bytes in the store under the extension's name and the oracle's digest,
//! and holds the engine's rendering to them byte for byte through
//! [`oracle::compare`], one reading per extension scoped `ext/<name>`.
//!
//! **The oracle half.** No engine renders an extension yet: `ext/reflection`
//! and the linked tables are design §11's later days, so the engine's side
//! is [`Rendering::Absent`] and every reading fails saying so, which is the
//! shape `compile-identity` shipped in against a compiler that emitted
//! nothing. What a run leaves behind is the oracle's bytes, so the day the
//! engine renders, the diff is one file against another. A test hands the
//! lane a directory of renderings standing for the engine's
//! ([`Ask::renderings`]), which is how the fixture under
//! `xtask/fixtures/crosscheck/` is measurable: php's own rendering of
//! `ctype` beside one that differs from it by one byte.
//!
//! **The load number.** The first line, `Extension [ <persistent>
//! extension #10 ctype version ... ]`, carries the module's load number,
//! which is the oracle's build order and not the bundle's. A
//! `[recipe.normalise]` row of the pin that replaces it is a person's; the
//! reading compares the number until one is written.

#[cfg(test)]
mod tests;

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::divergence::Registry;
use crate::manifest::Manifest;
use crate::oracle::{self, Invocation, Observation, Oracle, Program, Verdict};
use crate::reading::{Reading, Status, Store};

/// The instrument's name, the reading's `instrument` field.
pub(crate) const INSTRUMENT: &str = "crosscheck";
/// The first word of every key the lane compares under: the registry has
/// no kind of this name yet, so no row accepts a difference here until a
/// person adds one (harness §4.3).
pub(crate) const KIND: &str = "crosscheck";
/// php's flag that prints one extension's rendering and runs nothing.
pub(crate) const FLAG: &str = "--re";
/// The file the oracle's bytes are kept in, under the store: what a run
/// leaves for the day the engine renders.
pub(crate) const KEPT: &str = "re";

/// What one `xtask crosscheck` run was asked for.
#[derive(Debug, Clone, Default)]
pub struct Ask {
    /// The extensions, by their php names (`ctype`): `--ext <name>`,
    /// repeatable; the bundle's default set when none is named.
    pub ext: Vec<String>,
    /// A directory of `<name>.re` files standing for the engine's
    /// renderings, what a test hands the lane while no engine renders;
    /// without it the engine's side is absent and every reading fails.
    pub renderings: Option<PathBuf>,
}

/// The engine's side of one extension.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Rendering {
    /// The engine rendered nothing, and why.
    Absent(String),
    /// What the engine rendered.
    Bytes(Vec<u8>),
}

/// What one extension came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Judgement {
    /// The two renderings are the same bytes.
    Pass,
    /// They differ and an accepted registry row says so.
    Diverged(String),
    /// They differ, or the engine rendered nothing, and the note says
    /// which.
    Fail(String),
}

impl Judgement {
    pub(crate) fn status(&self) -> Status {
        match self {
            Judgement::Pass | Judgement::Diverged(_) => Status::Pass,
            Judgement::Fail(_) => Status::Fail,
        }
    }
}

/// The reading's scope for an extension: the crate's name in the manifest.
pub(crate) fn scope(name: &str) -> String {
    format!("ext/{name}")
}

/// The divergence key of one extension: `crosscheck:ctype`.
pub(crate) fn id(name: &str) -> String {
    format!("{KIND}:{name}")
}

/// Where the oracle's bytes for `name` are kept: under the store, the
/// instrument, the extension's scope and the oracle's digest, so a pin
/// bump leaves the old rendering beside the new.
pub fn kept(store: &Store, digest: &str, name: &str) -> PathBuf {
    store
        .root()
        .join(INSTRUMENT)
        .join(scope(name))
        .join(format!("{digest}.{KEPT}"))
}

/// The verdict on one extension: the engine's rendering held to the
/// oracle's through the one comparison every lane uses.
pub(crate) fn judge(
    registry: &Registry,
    name: &str,
    oracle: &[u8],
    engine: &Rendering,
) -> Judgement {
    match engine {
        Rendering::Absent(why) => Judgement::Fail(format!("the engine rendered nothing: {why}")),
        Rendering::Bytes(bytes) => match oracle::compare(registry, &id(name), oracle, bytes) {
            Verdict::Same => Judgement::Pass,
            Verdict::Diverged(row) => Judgement::Diverged(row.reason.clone()),
            Verdict::Differs(diff) => Judgement::Fail(format!(
                "the engine's rendering differs from the oracle's:\n{diff}"
            )),
        },
    }
}

/// The oracle's rendering of `name`: `php --re <name>` under the recipe,
/// through [`Oracle::run_under`] as every lane's oracle side runs. The
/// recipe's program stands beside the flag, since php opens it before it
/// prints, and prints without running it.
pub(crate) fn render(oracle: &Oracle<'_>, name: &str) -> Result<Observation> {
    let flags = [FLAG.to_owned(), name.to_owned()];
    oracle.run_under(&Invocation {
        flags: &flags,
        ..Invocation::new(&oracle.pin().oracle.path, Program::Snippet(""))
    })
}

/// The recipe as the reading names it: the invocation the oracle's bytes
/// came from.
pub(crate) fn recipe(manifest: &Manifest, name: &str) -> String {
    format!("php {} {FLAG} {name}", manifest.pin.recipe.args.join(" "))
}

/// The extensions a run holds: the ones named, or the manifest's `ext/*`
/// rows outside every feature, which is the bundle's default set (harness
/// §7).
pub(crate) fn extensions(manifest: &Manifest, ask: &Ask) -> Vec<String> {
    if !ask.ext.is_empty() {
        return ask.ext.clone();
    }
    manifest
        .crates
        .iter()
        .filter(|krate| !krate.optional())
        .filter_map(|krate| krate.name.strip_prefix("ext/"))
        .map(str::to_owned)
        .collect()
}

/// Run what was asked for: one reading per extension, written by the
/// caller through `record`, and the oracle's bytes kept in the store.
pub fn run(
    root: &Path,
    manifest: &Manifest,
    registry: &Registry,
    store: &Store,
    ask: &Ask,
) -> Result<Vec<Reading>> {
    let oracle = manifest.pin.oracle();
    let pinned = &manifest.pin.oracle.path;
    let oracle_absent = if pinned.is_file() {
        oracle.asserted()?;
        None
    } else {
        Some(format!("the pinned php is not at {}", pinned.display()))
    };
    extensions(manifest, ask)
        .iter()
        .map(|name| {
            let mut reading = reading_of(root, manifest, name)?;
            let recipe = recipe(manifest, name);
            if let Some(why) = &oracle_absent {
                reading.status = Status::Skip;
                reading.message = format!("{}: nothing rendered under `{recipe}`; {why}", reading.scope);
                return Ok(reading);
            }
            let observed = render(&oracle, name)?;
            if observed.exit != Some(0) {
                // php's own words: under the recipe's `display_errors=1`
                // the uncaught exception is on the first stream, and a
                // refusal before that on the second.
                let said = [&observed.stderr, &observed.stdout]
                    .into_iter()
                    .find(|stream| !stream.is_empty())
                    .map(|stream| String::from_utf8_lossy(stream).trim().to_owned())
                    .unwrap_or_default();
                reading.status = Status::Fail;
                reading.message = format!(
                    "{}: the oracle has no such extension under `{recipe}`: {said}",
                    reading.scope
                );
                return Ok(reading);
            }
            let path = kept(store, &manifest.pin.oracle.md5, name);
            if let Some(dir) = path.parent() {
                fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
            }
            fs::write(&path, &observed.stdout)
                .with_context(|| format!("keeping the oracle's rendering at {}", path.display()))?;
            let engine = rendering(manifest, ask, name)?;
            let judgement = judge(registry, name, &observed.stdout, &engine);
            reading.status = judgement.status();
            reading.count("bytes", observed.stdout.len() as u64)?;
            reading.count("lines", observed.stdout.split(|b| *b == b'\n').count() as u64)?;
            let kept = path.strip_prefix(root).unwrap_or(&path).display();
            reading.message = match judgement {
                Judgement::Pass => format!(
                    "{}: the engine's rendering is the oracle's {} bytes under `{recipe}`, kept at {kept}",
                    reading.scope,
                    observed.stdout.len()
                ),
                Judgement::Diverged(reason) => format!(
                    "{}: the engine's rendering differs from the oracle's under `{recipe}` and the registry accepts it: {reason}",
                    reading.scope
                ),
                Judgement::Fail(note) => format!(
                    "{}: the oracle rendered {} bytes under `{recipe}`, kept at {kept}; {note}",
                    reading.scope,
                    observed.stdout.len()
                ),
            };
            Ok(reading)
        })
        .collect()
}

/// The engine's side for `name`: the file a test handed the lane, through
/// the recipe's normalisation as the oracle's stream went, or absent,
/// since no engine renders an extension yet.
fn rendering(manifest: &Manifest, ask: &Ask, name: &str) -> Result<Rendering> {
    let Some(dir) = &ask.renderings else {
        return Ok(Rendering::Absent(
            "no engine renders an extension yet; `ReflectionExtension::__toString()` over the linked tables is ext/reflection's, not built (design §7.1, §11)".to_owned(),
        ));
    };
    let path = dir.join(format!("{name}.{KEPT}"));
    if !path.is_file() {
        return Ok(Rendering::Absent(format!(
            "no rendering at {}",
            path.display()
        )));
    }
    let bytes = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
    Ok(Rendering::Bytes(oracle::normalise(
        &manifest.pin.recipe.normalise,
        bytes,
    )))
}

/// The reading of one extension, over the crate's sources where the
/// manifest has a row for it, and over the workspace for a name it has
/// not, which the oracle may still render or refuse.
fn reading_of(root: &Path, manifest: &Manifest, name: &str) -> Result<Reading> {
    let scope = scope(name);
    match manifest.crate_named(&scope) {
        Some(krate) => Reading::for_crate(INSTRUMENT, krate, root),
        None => Reading::stamped_over(INSTRUMENT, &scope, root, Path::new(".")),
    }
}
