//! `xtask budget` (design §4.1, §10; harness-design.md §6, §12 item 6):
//! every `[[crate]]` against its ceiling, the workspace against `[budget]`,
//! committed generated code refused unless the manifest declares it, a
//! `build.rs` that bakes its crate's directory in refused, and a
//! declared file held to its regenerated form. The numbers are the
//! manifest's; nothing here states one. One reading per crate, one for the
//! workspace, one for `scripts/**` against `[budget] scripts`, one per
//! generated file.

pub(crate) mod row;

use std::path::Path;

use anyhow::{Context, Result};

use crate::generated;
use crate::git;
use crate::key::Snapshot;
use crate::layout::{HARNESS, Layout};
use crate::lines;
use crate::lock::CargoLock;
use crate::manifest::{self, Budget, Crate, Generated, Manifest, OneOrMany};
use crate::reading::{Reading, Status, Store, joined, plural};
use crate::sources::{self, Standing};
use crate::workloads;

/// The instrument whose readings this module writes.
pub(crate) const INSTRUMENT: &str = "budget";

/// The scope of the reading of the whole workspace against `[budget]`.
const WORKSPACE: &str = "workspace";

/// The keys of a budget reading's counts, named once for the instrument
/// that writes them and the rendering that reads them.
pub(crate) mod numbers {
    /// The lines counted: a crate's own, or `scripts/**`'s.
    pub(crate) const LINES: &str = "lines";
    /// The ceiling a crate's, or `scripts/**`'s, lines are held to.
    pub(crate) const CEILING: &str = "ceiling";
    /// A crate's binding lines, `ext/*/src/ffi/**`, pooled apart.
    pub(crate) const BINDINGS: &str = "bindings";
    /// The tracked files `scripts/**` counts.
    pub(crate) const FILES: &str = "files";
    /// The workspace's every counted line, against `[budget] total`.
    pub(crate) const TOTAL: &str = "total";
    /// `docs/**` and the living documents, less the workload programs
    /// where `[budget] workloads` holds them, against `[budget] docs`.
    pub(crate) const DOCS: &str = "docs";
    /// The workload programs, `docs/perf/workloads/**`, against
    /// `[budget] workloads`.
    pub(crate) const WORKLOADS: &str = "workloads";
    /// `.claude/**` and `harness-plugins/**`, less the desk where
    /// `[budget] harness-desk` holds it, against `[budget] harness`.
    pub(crate) const HARNESS: &str = "harness";
    /// `harness-plugins/desk/**`, against `[budget] harness-desk`.
    pub(crate) const HARNESS_DESK: &str = "harness_desk";
    /// Third-party `Cargo.lock` packages, against `[budget] lock_packages`.
    pub(crate) const LOCK_PACKAGES: &str = "lock_packages";
    /// `[budget] new`, stated and not measured.
    pub(crate) const NEW_CEILING: &str = "new_ceiling";

    /// The key of the ceiling a workspace pool `name` is held to.
    pub(crate) fn ceiling_of(name: &str) -> String {
        format!("{name}_ceiling")
    }
}

/// Where the design's sentence on ceilings is, for the failure text; the
/// `[[crate]]` row carries no text of its own.
const CEILING_TEXT: &str = "a line ceiling is design §4.1, principle 7: size is a gate; lines are non-blank, non-comment, tests excluded";

/// Measure the workspace; every reading, stamped.
pub fn run(root: &Path, manifest: &Manifest) -> Result<Vec<Reading>> {
    over(root, manifest, &Standing::default())
}

/// [`run`], and with `staged` less the crates whose reading stands for
/// the commit.
pub(crate) fn measure(
    root: &Path,
    manifest: &Manifest,
    store: &Store,
    staged: bool,
) -> Result<Vec<Reading>> {
    let standing = Standing::of_every_crate(staged, root, manifest, store, INSTRUMENT)?;
    over(root, manifest, &standing)
}

/// Every reading, less a crate whose reading stands for the commit:
/// each counts toward the workspace's total with the numbers its standing
/// reading carries, and writes no reading of its own.
fn over(root: &Path, manifest: &Manifest, standing: &Standing) -> Result<Vec<Reading>> {
    let snapshot = Snapshot::disk(root)?;
    let mut readings = Vec::new();
    let mut total = Count::default();
    for krate in &manifest.crates {
        if let Some(stood) = standing.get(&krate.name) {
            let number = |key: &str| stood.int(key).map_or(0, |n| u64::try_from(n).unwrap_or(0));
            total.lines += number(numbers::LINES);
            total.bindings += number(numbers::BINDINGS);
            continue;
        }
        let (reading, count) = crate_reading(&snapshot, krate)?;
        total.lines += count.lines;
        total.bindings += count.bindings;
        readings.push(reading);
    }
    readings.push(workspace_reading(&snapshot, manifest, total)?);
    readings.push(scripts_reading(&snapshot, manifest)?);
    readings.extend(generated_readings(&snapshot, manifest)?);
    Ok(readings)
}

/// What a line under `scripts/` is, for the failure text: the count
/// `lines` makes for a text that is not Rust or TOML.
const SCRIPTS_LINE_TEXT: &str =
    "a line ceiling is design §4.1, principle 7: size is a gate; lines are non-blank";

/// `scripts/**` against `[budget] scripts`: every tracked file under the
/// directory counts except the generators the row names, and the message
/// says what makes the count up, file by file, so a failure names what to
/// shrink. A generator the row names that is no tracked file is a failure,
/// since a row that says one thing while the count does another is worse
/// than no row. Without the row, which a person writes, the count is
/// measured and held to nothing: a skip that says the number.
fn scripts_reading(snapshot: &Snapshot, manifest: &Manifest) -> Result<Reading> {
    let root = snapshot.root();
    let mut reading = Reading::for_scope(INSTRUMENT, manifest::SCRIPTS, manifest, snapshot)?;
    let row = manifest.budget.scripts.as_ref();
    let generators: &[String] = row.map_or(&[], |s| s.generators.as_slice());
    let mut counted = Vec::new();
    let mut excluded = Vec::new();
    let mut lines = 0;
    for path in git::files(root, Path::new(manifest::SCRIPTS))? {
        if generators.contains(&path) {
            excluded.push(path);
            continue;
        }
        let Some(n) = lines_of(root, &path)? else {
            continue;
        };
        lines += n;
        counted.push(format!("{path} {n}"));
    }
    let unmatched: Vec<&str> = generators
        .iter()
        .filter(|g| !excluded.contains(g))
        .map(String::as_str)
        .collect();
    reading.count(numbers::LINES, lines)?;
    reading.count(numbers::FILES, u64::try_from(counted.len())?)?;
    let mut made_up = if counted.is_empty() {
        "made up of nothing".to_owned()
    } else {
        format!("made up of {}", counted.join(", "))
    };
    if !excluded.is_empty() {
        made_up = format!("{made_up}; generators not counted: {}", excluded.join(", "));
    }
    let Some(row) = row else {
        reading.status = Status::Skip;
        reading.message = format!(
            "{}: {lines} lines, {made_up}; no `[budget] scripts` row holds them to a ceiling, and a person writes one",
            manifest::SCRIPTS
        );
        return Ok(reading);
    };
    reading.count(numbers::CEILING, row.ceiling)?;
    let mut failures = Vec::new();
    if lines > row.ceiling {
        failures.push(format!(
            "{}: {lines} lines over the ceiling of {}, {made_up} ({SCRIPTS_LINE_TEXT})",
            manifest::SCRIPTS,
            row.ceiling
        ));
    }
    for name in unmatched {
        failures.push(format!(
            "`[budget] scripts` names {name} as a generator and no tracked file under {}/ is so called",
            manifest::SCRIPTS
        ));
    }
    if failures.is_empty() {
        reading.status = Status::Pass;
        reading.message = format!(
            "{}: {lines} lines under the ceiling of {}, {made_up}",
            manifest::SCRIPTS,
            row.ceiling
        );
    } else {
        reading.status = Status::Fail;
        reading.message = failures.join("; ");
    }
    Ok(reading)
}

/// One crate against its ceiling, and what it counted. A crate not yet
/// created is measured by nothing and its reading carries no stamp.
fn crate_reading(snapshot: &Snapshot, krate: &Crate) -> Result<(Reading, Count)> {
    let root = snapshot.root();
    let mut reading = Reading::for_crate(INSTRUMENT, krate, snapshot)?;
    reading.count(numbers::CEILING, krate.budget)?;
    if !sources::exists(root, krate) {
        reading.message = format!("{}: {}", krate.name, sources::NOT_YET_CREATED);
        return Ok((reading, Count::default()));
    }
    let count = count(root, krate)?;
    reading.count(numbers::LINES, count.lines)?;
    if count.bindings > 0 {
        reading.count(numbers::BINDINGS, count.bindings)?;
    }
    if count.lines > krate.budget {
        reading.status = Status::Fail;
        reading.message = format!(
            "{}: {} lines over the ceiling of {} ({CEILING_TEXT})",
            krate.name, count.lines, krate.budget
        );
    } else {
        reading.status = Status::Pass;
        reading.message = format!(
            "{}: {} lines under the ceiling of {}",
            krate.name, count.lines, krate.budget
        );
    }
    Ok((reading, count))
}

/// The workspace against `[budget]`: the pooled counts, every crate
/// declared, no committed generated code outside the exceptions, no
/// `build.rs` that bakes its crate's directory in, and no file derived
/// from php-src without its licence line or, translated from LGPL C,
/// without its dated change notice.
fn workspace_reading(snapshot: &Snapshot, manifest: &Manifest, total: Count) -> Result<Reading> {
    let root = snapshot.root();
    let mut reading = Reading::for_scope(INSTRUMENT, WORKSPACE, manifest, snapshot)?;
    let budget: &Budget = &manifest.budget;
    let mut failures = Vec::new();
    let mut ceiling = |name: &str, value: u64, limit: u64, what: &str| -> Result<()> {
        reading.count(name, value)?;
        reading.count(&numbers::ceiling_of(name), limit)?;
        if value > limit {
            failures.push(format!("{what}: {value} over the ceiling of {limit}"));
        }
        Ok(())
    };
    ceiling(
        numbers::TOTAL,
        total.lines + total.bindings,
        budget.total,
        "workspace lines",
    )?;
    if let Some(bindings) = budget.bindings {
        ceiling(
            numbers::BINDINGS,
            total.bindings,
            bindings,
            "ext/*/src/ffi/** lines",
        )?;
    }
    ceiling(
        numbers::DOCS,
        docs(root, budget.workloads.is_some())?,
        budget.docs,
        if budget.workloads.is_some() {
            "docs/** and the living documents, less the workload programs"
        } else {
            "docs/** and the living documents"
        },
    )?;
    if let Some(limit) = budget.workloads {
        ceiling(
            numbers::WORKLOADS,
            sum(root, &git::files(root, Path::new(workloads::DIR))?)?,
            limit,
            &format!("{}/** lines", workloads::DIR),
        )?;
    }
    ceiling(
        numbers::HARNESS,
        harness(root, budget.harness_desk.is_some())?,
        budget.harness,
        if budget.harness_desk.is_some() {
            ".claude/** and harness-plugins/** hand-written lines, less the desk"
        } else {
            ".claude/** and harness-plugins/** hand-written lines"
        },
    )?;
    if let Some(desk) = budget.harness_desk {
        ceiling(
            numbers::HARNESS_DESK,
            harness_desk(root)?,
            desk,
            "harness-plugins/desk/** hand-written lines",
        )?;
    }
    ceiling(
        numbers::LOCK_PACKAGES,
        u64::try_from(CargoLock::load(root)?.packages())?,
        budget.lock_packages,
        "third-party Cargo.lock packages",
    )?;
    reading.count(numbers::NEW_CEILING, budget.new)?;

    for name in sources::undeclared(root, manifest) {
        failures.push(format!(
            "{name} has no [[crate]] row in seams.toml, so no ceiling"
        ));
    }
    let layout = manifest.layout();
    for path in committed_generated(root, &layout, &manifest.generated_files())? {
        failures.push(format!(
            "{path} is committed generated code; generated code is generated in build.rs and never committed (design §10)"
        ));
    }
    for path in build_rs_baking_their_dir(root, &layout)? {
        failures.push(format!(
            "{path} reads {MANIFEST_DIR_AT_COMPILE} when it compiles; a build.rs reads std::env::var_os(\"CARGO_MANIFEST_DIR\") when it runs, since cargo runs one compiled build.rs for every checkout sharing a target directory"
        ));
    }
    let (unlicensed, undated) = derived_without_their_lines(root, &layout)?;
    for path in unlicensed {
        failures.push(format!(
            "{path} cites a php-src source as `Z/<path>`, or {THIRD_PARTY} names it, and none of its first {LICENCE_WITHIN} lines is a `{LICENCE_LINE}` line; a file derived from php-src states the licences it is under where it begins"
        ));
    }
    for path in undated {
        failures.push(format!(
            "{path} states an LGPL licence, and none of its first {LICENCE_WITHIN} lines is a `{CHANGE_NOTICE}` line with a date; a file translated from LGPL C says that it was changed and when"
        ));
    }
    if failures.is_empty() {
        reading.status = Status::Pass;
        reading.message = format!(
            "workspace: {} lines under the ceiling of {}; the new-code ceiling of {} is not measured until the manifest says which crates are inherited",
            total.lines + total.bindings,
            budget.total,
            budget.new
        );
    } else {
        reading.status = Status::Fail;
        reading.message = failures.join("; ");
    }
    Ok(reading)
}

/// Each generated, committed file against its rendering
/// (`generated.committed`), and against what would stand between it and
/// the crates below it: a clippy configuration nearer to one of them, or
/// a cargo configuration that sets `CLIPPY_CONF_DIR`
/// ([`generated::nearer`], [`generated::conf_dir_set`]). The reading's ADRs
/// are the rules the file carries, which decided its lists, beside the
/// generated rule's own.
fn generated_readings(snapshot: &Snapshot, manifest: &Manifest) -> Result<Vec<Reading>> {
    let root = snapshot.root();
    let files = manifest.generated_files();
    let conf_dir = generated::conf_dir_set(root)?;
    let mut readings = Vec::new();
    for rule in &manifest.rules {
        for file in rule.predicate.generated.iter().flat_map(Generated::files) {
            let mut reading = Reading::for_scope(INSTRUMENT, file, manifest, snapshot)?;
            reading.rule.clone_from(&rule.id);
            let carried = generated::carried(manifest, generated::dir_of(file))?;
            let mut adrs: Vec<&str> = carried
                .iter()
                .flat_map(|r| r.adr.iter())
                .chain(rule.adr.iter())
                .collect();
            adrs.sort_unstable();
            adrs.dedup();
            reading.adr = adrs.join(", ");
            let cited = OneOrMany::Many(adrs.iter().map(|n| (*n).to_owned()).collect()).adr_cited();
            let cited = if cited.is_empty() {
                cited
            } else {
                format!(" {cited}")
            };
            let from = format!(
                "rule{} {}",
                plural(carried.len()),
                joined(carried.iter().map(|r| r.id.as_str()), ", ")
            );
            let want = generated::render(root, manifest, file)?;
            let mut failures = match std::fs::read_to_string(root.join(file)) {
                Ok(have) if have == want => Vec::new(),
                Ok(_) => vec![format!(
                    "{file} differs from its rendering from {from}; run `xtask generate`"
                )],
                Err(_) => vec![format!("{file} is not committed; run `xtask generate`")],
            };
            for path in generated::nearer(root, &files, file)? {
                failures.push(format!(
                    "{path} is nearer than {file} to the crates below it, and clippy reads the nearest configuration alone, so it hides {from} from them; remove it"
                ));
            }
            for config in &conf_dir {
                failures.push(format!(
                    "{config} sets {} in [env], which cargo hands to clippy, so no crate reads {file} and none is held to {from}; remove it",
                    generated::CONF_DIR
                ));
            }
            if failures.is_empty() {
                reading.status = Status::Pass;
                reading.message = format!(
                    "{file} matches its rendering from {from}, and no clippy configuration stands nearer to a crate below it"
                );
            } else {
                reading.status = Status::Fail;
                reading.message = format!("{}. {}{cited}", failures.join("; "), rule.text);
            }
            readings.push(reading);
        }
    }
    Ok(readings)
}

/// One crate's count: its own lines, and its bindings pooled apart.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Count {
    pub lines: u64,
    pub bindings: u64,
}

/// Count one crate's sources.
pub(crate) fn count(root: &Path, krate: &Crate) -> Result<Count> {
    let mut out = Count::default();
    for path in sources::rust(root, krate)? {
        let Some(n) = lines_of(root, &path)? else {
            continue;
        };
        if sources::is_binding(&path) {
            out.bindings += n;
        } else {
            out.lines += n;
        }
    }
    Ok(out)
}

/// `docs/**` and the living documents at the root (`*-design.md`), the
/// workload programs left out where `workloads_apart`, as
/// `[budget] workloads` holds them on their own.
fn docs(root: &Path, workloads_apart: bool) -> Result<u64> {
    let mut paths = git::files(root, Path::new("docs"))?;
    paths.retain(|p| !(workloads_apart && Path::new(p).starts_with(workloads::DIR)));
    paths.extend(
        git::files(root, Path::new("."))?
            .into_iter()
            .filter(|p| !p.contains('/') && p.ends_with("-design.md")),
    );
    sum(root, &paths)
}

/// The desk plugin's tree, which `[budget] harness-desk` holds apart from
/// `harness` where a person has written the row.
const DESK: &str = "harness-plugins/desk";

/// `.claude/**` and `harness-plugins/**` hand-written: generated types
/// (`*.d.ts`) excluded, and the desk's tree with them where `desk_apart`.
fn harness(root: &Path, desk_apart: bool) -> Result<u64> {
    let mut paths: Vec<String> = git::files(root, Path::new(".claude"))?;
    paths.extend(git::files(root, Path::new("harness-plugins"))?);
    paths.retain(|p| !(p.ends_with(".d.ts") || desk_apart && Path::new(p).starts_with(DESK)));
    sum(root, &paths)
}

/// `harness-plugins/desk/**` hand-written, its tests included: generated
/// types excluded, as under `harness`.
fn harness_desk(root: &Path) -> Result<u64> {
    let mut paths = git::files(root, Path::new(DESK))?;
    paths.retain(|p| !p.ends_with(".d.ts"));
    sum(root, &paths)
}

fn sum(root: &Path, paths: &[String]) -> Result<u64> {
    let mut n = 0;
    for path in paths {
        n += lines_of(root, path)?.unwrap_or(0);
    }
    Ok(n)
}

/// The counted lines of one file, or `None` for a file that is not text.
fn lines_of(root: &Path, path: &str) -> Result<Option<u64>> {
    match text_of(root, path)? {
        Some(text) => Ok(Some(u64::try_from(lines::count(path, &text))?)),
        None => Ok(None),
    }
}

/// The text of one file to count, or `None` when the file is not text: a
/// file that is not text has no lines. A `.DS_Store` Finder left under
/// `docs/` took the whole instrument down with a UTF-8 error, and an
/// instrument that falls over measures nothing, which is the shape of the
/// outage harness revision 11 records.
fn text_of(root: &Path, path: &str) -> Result<Option<String>> {
    let bytes = std::fs::read(root.join(path)).with_context(|| format!("reading {path}"))?;
    Ok(String::from_utf8(bytes).ok())
}

/// The crate directory as a `build.rs` reads it when it compiles, the
/// text matched with its whitespace dropped.
const MANIFEST_DIR_AT_COMPILE: &str = "env!(\"CARGO_MANIFEST_DIR\")";

/// The `build.rs` files under the crate trees that bake their crate's
/// directory in: cargo reuses one compiled `build.rs` for every checkout
/// whose crate sits at the same path below its workspace root, so one
/// sharing a target directory with another checkout reads that checkout's
/// files. The `build.rs` held is the one beside a `Cargo.toml`,
/// which cargo runs when the package names no other.
fn build_rs_baking_their_dir(root: &Path, layout: &Layout) -> Result<Vec<String>> {
    let mut out = crate_tree_files(root, layout)?;
    out.retain(|path| {
        let file = Path::new(path);
        file.file_name() == Some("build.rs".as_ref())
            && root.join(file.with_file_name("Cargo.toml")).is_file()
            && std::fs::read_to_string(root.join(file)).is_ok_and(|text| {
                let bare: String = text.chars().filter(|c| !c.is_whitespace()).collect();
                bare.contains(MANIFEST_DIR_AT_COMPILE)
            })
    });
    Ok(out)
}

/// The file that lists what this tree takes from others, by path.
const THIRD_PARTY: &str = "THIRD-PARTY.md";

/// How a Rust file states the licences it is under, and how far from its
/// first line the statement may stand.
const LICENCE_LINE: &str = "// SPDX-License-Identifier: ";
const LICENCE_WITHIN: usize = 5;

/// How a file translated from LGPL C says that it was changed, which
/// LGPL-2.1 section 2 asks of a changed file beside the date.
const CHANGE_NOTICE: &str = "// Changed from ";

/// A php-src source as a comment cites it, `Z/` and a directory of the
/// tree or its `configure.ac`.
const CITES_PHP_SRC: &str = r"(^|[^A-Za-z0-9_])Z/(Zend|ext|main|sapi|build|configure\.ac)\b";

/// The Rust files under the crate trees and the harness that are derived
/// from php-src and do not say so where they begin, in two lists. The
/// first is the files that cite a php-src source, or that [`THIRD_PARTY`]
/// names by path, with no [`LICENCE_LINE`] among their first lines. The
/// second is the files whose licence line names an LGPL licence with no
/// dated [`CHANGE_NOTICE`] there. Which licences the line names is the
/// file's to state, as [`THIRD_PARTY`] lists them; that the lines stand
/// is what is held.
fn derived_without_their_lines(root: &Path, layout: &Layout) -> Result<(Vec<String>, Vec<String>)> {
    let cites = regex_lite::Regex::new(CITES_PHP_SRC)?;
    let dated = regex_lite::Regex::new(r"\b20[0-9]{2}-[0-9]{2}-[0-9]{2}\b")?;
    let named = std::fs::read_to_string(root.join(THIRD_PARTY)).unwrap_or_default();
    let (mut unlicensed, mut undated) = (Vec::new(), Vec::new());
    for path in crate_tree_files(root, layout)? {
        if Path::new(&path).extension() != Some(std::ffi::OsStr::new("rs")) {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(root.join(&path)) else {
            continue;
        };
        let head: Vec<&str> = text.lines().take(LICENCE_WITHIN).collect();
        let licence = head.iter().find(|line| line.starts_with(LICENCE_LINE));
        match licence {
            None if cites.is_match(&text) || named.contains(&format!("`{path}`")) => {
                unlicensed.push(path);
            }
            Some(line)
                if line.contains("LGPL-")
                    && !(head.iter())
                        .any(|l| l.starts_with(CHANGE_NOTICE) && dated.is_match(l)) =>
            {
                undated.push(path);
            }
            _ => {}
        }
    }
    Ok((unlicensed, undated))
}

/// Committed generated files under the crate trees: a `generated` path
/// component, or an `@generated` marker in the first lines, outside the
/// files the manifest declares as generated and committed.
pub(crate) fn committed_generated(
    root: &Path,
    layout: &Layout,
    exceptions: &[&str],
) -> Result<Vec<String>> {
    let mut out = crate_tree_files(root, layout)?;
    out.retain(|path| {
        !exceptions.contains(&path.as_str())
            && (path.split('/').any(|part| part == "generated")
                || std::fs::read_to_string(root.join(path))
                    .is_ok_and(|text| text.lines().take(5).any(|l| l.contains("@generated"))))
    });
    Ok(out)
}

/// The files under the crate trees and the harness that git does not
/// ignore, less the harness's own negative controls, which break the
/// rules on purpose.
fn crate_tree_files(root: &Path, layout: &Layout) -> Result<Vec<String>> {
    let dirs: Vec<&str> = layout.dirs().chain([HARNESS]).collect();
    let mut out = git::files_under(root, &dirs)?;
    out.retain(|path| !path.starts_with("xtask/fixtures/"));
    Ok(out)
}
