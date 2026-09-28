//! `xtask budget` (design §4.1, §10; harness-design.md §6, §12 item 6):
//! every `[[crate]]` against its ceiling, the workspace against `[budget]`,
//! committed generated code refused unless the manifest declares it, and
//! a declared file held to its regenerated form. The numbers are the
//! manifest's; nothing here states one. One reading per crate, one for the
//! workspace, one for `scripts/**` against `[budget] scripts`, one per
//! generated file.

use std::path::Path;

use anyhow::{Context, Result};

use crate::generated;
use crate::git;
use crate::lines;
use crate::lock::CargoLock;
use crate::manifest::{self, Budget, Crate, Manifest};
use crate::reading::{Number, Reading, Status, Store};
use crate::sources::{self, Standing};

/// Where the design's sentence on ceilings is, for the failure text; the
/// `[[crate]]` row carries no text of its own.
const CEILING_TEXT: &str = "a line ceiling is design §4.1, principle 7: size is a gate; lines are non-blank, non-comment, tests excluded";

/// Measure the workspace; every reading, stamped.
pub fn run(root: &Path, manifest: &Manifest) -> Result<Vec<Reading>> {
    over(root, manifest, &Standing::default())
}

/// [`run`], and with `staged` less the crates whose reading stands for
/// the commit (#1143).
pub(crate) fn measure(
    root: &Path,
    manifest: &Manifest,
    store: &Store,
    staged: bool,
) -> Result<Vec<Reading>> {
    let standing = Standing::of_every_crate(staged, root, manifest, store, "budget")?;
    over(root, manifest, &standing)
}

/// Every reading, less a crate whose reading stands for the commit (#1143):
/// each counts toward the workspace's total with the numbers its standing
/// reading carries, and writes no reading of its own.
fn over(root: &Path, manifest: &Manifest, standing: &Standing) -> Result<Vec<Reading>> {
    let mut readings = Vec::new();
    let mut total = Count::default();
    for krate in &manifest.crates {
        if let Some(stood) = standing.get(&krate.name) {
            let number = |key: &str| match stood.numbers.get(key) {
                Some(Number::Int(n)) => u64::try_from(*n).unwrap_or(0),
                _ => 0,
            };
            total.lines += number("lines");
            total.bindings += number("bindings");
            continue;
        }
        let (reading, count) = crate_reading(root, krate)?;
        total.lines += count.lines;
        total.bindings += count.bindings;
        readings.push(reading);
    }
    readings.push(workspace_reading(root, manifest, total)?);
    readings.push(scripts_reading(root, manifest)?);
    readings.extend(generated_readings(root, manifest)?);
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
fn scripts_reading(root: &Path, manifest: &Manifest) -> Result<Reading> {
    let mut reading = Reading::for_scope("budget", manifest::SCRIPTS, manifest, root)?;
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
    reading.count("lines", lines)?;
    reading.count("files", u64::try_from(counted.len())?)?;
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
    reading.count("ceiling", row.ceiling)?;
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
fn crate_reading(root: &Path, krate: &Crate) -> Result<(Reading, Count)> {
    let mut reading = Reading::for_crate("budget", krate, root)?;
    reading.count("ceiling", krate.budget)?;
    if !sources::exists(root, krate) {
        reading.message = format!("{}: {}", krate.name, sources::NOT_YET_CREATED);
        return Ok((reading, Count::default()));
    }
    let count = count(root, krate)?;
    reading.count("lines", count.lines)?;
    if count.bindings > 0 {
        reading.count("bindings", count.bindings)?;
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
/// declared, no committed generated code outside the exceptions.
fn workspace_reading(root: &Path, manifest: &Manifest, total: Count) -> Result<Reading> {
    let mut reading = Reading::for_scope("budget", "workspace", manifest, root)?;
    let budget: &Budget = &manifest.budget;
    let mut failures = Vec::new();
    let mut ceiling = |name: &str, value: u64, limit: u64, what: &str| -> Result<()> {
        reading.count(name, value)?;
        reading.count(&format!("{name}_ceiling"), limit)?;
        if value > limit {
            failures.push(format!("{what}: {value} over the ceiling of {limit}"));
        }
        Ok(())
    };
    ceiling(
        "total",
        total.lines + total.bindings,
        budget.total,
        "workspace lines",
    )?;
    ceiling(
        "bindings",
        total.bindings,
        budget.bindings,
        "ext/*/src/ffi/** lines",
    )?;
    ceiling(
        "docs",
        docs(root)?,
        budget.docs,
        "docs/** and the living documents",
    )?;
    ceiling(
        "harness",
        harness(root)?,
        budget.harness,
        ".claude/** and harness-plugins/** hand-written lines",
    )?;
    ceiling(
        "lock_packages",
        u64::try_from(CargoLock::load(root)?.packages())?,
        budget.lock_packages,
        "Cargo.lock packages",
    )?;
    reading.count("new_ceiling", budget.new)?;

    for name in sources::undeclared(root, manifest) {
        failures.push(format!(
            "{name} has no [[crate]] row in seams.toml, so no ceiling"
        ));
    }
    let exceptions: Vec<&str> = manifest
        .rules
        .iter()
        .flat_map(|r| r.predicate.generated.iter().flatten())
        .map(|(file, _)| file.as_str())
        .collect();
    for path in committed_generated(root, &exceptions)? {
        failures.push(format!(
            "{path} is committed generated code; generated code is generated in build.rs and never committed (design §10)"
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
/// (`generated.committed`). The reading's ADR is the source rule's, which
/// decided the list the file carries, beside the generated rule's own.
fn generated_readings(root: &Path, manifest: &Manifest) -> Result<Vec<Reading>> {
    let mut readings = Vec::new();
    for rule in &manifest.rules {
        for (file, from) in rule.predicate.generated.iter().flatten() {
            let mut reading = Reading::for_scope("budget", file, manifest, root)?;
            reading.rule.clone_from(&rule.id);
            let source = manifest.rule(from);
            let adrs: Vec<&str> = source
                .into_iter()
                .flat_map(|r| r.adr.iter())
                .chain(rule.adr.iter())
                .collect();
            reading.adr = adrs.join(", ");
            let cited = if adrs.is_empty() {
                String::new()
            } else {
                format!(
                    " ({})",
                    adrs.iter()
                        .map(|n| format!("ADR-{n}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            };
            let want = generated::render(root, manifest, file, from)?;
            let have = std::fs::read_to_string(root.join(file)).ok();
            let stale = match have {
                Some(have) if have == want => None,
                Some(_) => Some(format!(
                    "{file} differs from its rendering from rule {from}"
                )),
                None => Some(format!("{file} is not committed")),
            };
            match stale {
                None => {
                    reading.status = Status::Pass;
                    reading.message = format!("{file} matches its rendering from rule {from}");
                }
                Some(what) => {
                    reading.status = Status::Fail;
                    reading.message = format!("{what}; run `xtask generate`. {}{cited}", rule.text);
                }
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
        let Some(text) = text_of(root, &path)? else {
            continue;
        };
        let n = u64::try_from(lines::count(&path, &text))?;
        if sources::is_binding(&path) {
            out.bindings += n;
        } else {
            out.lines += n;
        }
    }
    Ok(out)
}

/// `docs/**` and the living documents at the root (`*-design.md`).
fn docs(root: &Path) -> Result<u64> {
    let mut paths = git::files(root, Path::new("docs"))?;
    paths.extend(
        git::files(root, Path::new("."))?
            .into_iter()
            .filter(|p| !p.contains('/') && p.ends_with("-design.md")),
    );
    sum(root, &paths)
}

/// `.claude/**` and `harness-plugins/**` hand-written: generated types
/// (`*.d.ts`) excluded.
fn harness(root: &Path) -> Result<u64> {
    let mut paths: Vec<String> = git::files(root, Path::new(".claude"))?;
    paths.extend(git::files(root, Path::new("harness-plugins"))?);
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
/// bug #16 names.
fn text_of(root: &Path, path: &str) -> Result<Option<String>> {
    let bytes = std::fs::read(root.join(path)).with_context(|| format!("reading {path}"))?;
    Ok(String::from_utf8(bytes).ok())
}

/// Committed generated files under the crate trees: a `generated` path
/// component, or an `@generated` marker in the first lines, outside the
/// files the manifest declares as generated and committed. The harness's
/// own negative controls hold generated code on purpose and are left out.
pub(crate) fn committed_generated(root: &Path, exceptions: &[&str]) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for dir in sources::CRATE_DIRS {
        for path in git::files(root, Path::new(dir))? {
            if exceptions.contains(&path.as_str()) || path.starts_with("xtask/fixtures/") {
                continue;
            }
            let by_path = path.split('/').any(|part| part == "generated");
            let by_marker = by_path
                || std::fs::read_to_string(root.join(&path))
                    .is_ok_and(|text| text.lines().take(5).any(|l| l.contains("@generated")));
            if by_marker {
                out.push(path);
            }
        }
    }
    Ok(out)
}
