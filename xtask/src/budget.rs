//! `xtask budget` (design §4.1, §10; harness-design.md §6, §12 item 6):
//! every `[[crate]]` against its ceiling, the workspace against `[budget]`,
//! committed generated code refused outside the declared exception, and
//! that exception held to its regenerated form. The numbers are the
//! manifest's; nothing here states one. One reading per crate, one for the
//! workspace, one per generated file.

use std::path::Path;

use anyhow::{Context, Result};

use crate::generated;
use crate::git::{self, Stamp};
use crate::lines;
use crate::lock::CargoLock;
use crate::manifest::{Budget, Crate, Manifest};
use crate::reading::{Reading, Status};
use crate::sources;

/// Where the design's sentence on ceilings is, for the failure text; the
/// `[[crate]]` row carries no text of its own.
const CEILING_TEXT: &str = "a line ceiling is design §4.1, principle 7: size is a gate; lines are non-blank, non-comment, tests excluded";

/// Measure the workspace; every reading, stamped.
pub fn run(root: &Path, manifest: &Manifest) -> Result<Vec<Reading>> {
    let mut readings = Vec::new();
    let mut total = Count::default();
    for krate in &manifest.crates {
        let (reading, count) = crate_reading(root, krate)?;
        total.lines += count.lines;
        total.bindings += count.bindings;
        readings.push(reading);
    }
    readings.push(workspace_reading(root, manifest, total)?);
    readings.extend(generated_readings(root, manifest)?);
    Ok(readings)
}

/// One crate against its ceiling, and what it counted.
fn crate_reading(root: &Path, krate: &Crate) -> Result<(Reading, Count)> {
    let mut reading = Reading::new("budget", &krate.name);
    reading.count("ceiling", krate.budget)?;
    if !sources::exists(root, krate) {
        reading.message = format!("{}: not yet created", krate.name);
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
    reading.stamp(&Stamp::take(root, &krate.path())?);
    Ok((reading, count))
}

/// The workspace against `[budget]`: the pooled counts, every crate
/// declared, no committed generated code outside the exceptions.
fn workspace_reading(root: &Path, manifest: &Manifest, total: Count) -> Result<Reading> {
    let mut reading = Reading::new("budget", "workspace");
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
        ".claude/** hand-written lines",
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
    reading.stamp(&Stamp::take(root, Path::new("."))?);
    Ok(reading)
}

/// Each generated, committed file against its rendering
/// (`generated.committed`). The reading's ADR is the source rule's, which
/// decided the list the file carries, beside the generated rule's own.
fn generated_readings(root: &Path, manifest: &Manifest) -> Result<Vec<Reading>> {
    let mut readings = Vec::new();
    for rule in &manifest.rules {
        for (file, from) in rule.predicate.generated.iter().flatten() {
            let mut reading = Reading::new("budget", file);
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
            reading.stamp(&Stamp::take(root, Path::new(file))?);
            readings.push(reading);
        }
    }
    Ok(readings)
}

/// One crate's count: its own lines, and its bindings pooled apart.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Count {
    pub lines: u64,
    pub bindings: u64,
}

/// Count one crate's sources.
pub fn count(root: &Path, krate: &Crate) -> Result<Count> {
    let mut out = Count::default();
    for path in sources::rust(root, krate)? {
        let text =
            std::fs::read_to_string(root.join(&path)).with_context(|| format!("reading {path}"))?;
        let n = u64::try_from(lines::rust(&text))?;
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

/// `.claude/**` hand-written: generated types (`*.d.ts`) excluded.
fn harness(root: &Path) -> Result<u64> {
    let paths: Vec<String> = git::files(root, Path::new(".claude"))?
        .into_iter()
        .filter(|p| !p.ends_with(".d.ts"))
        .collect();
    sum(root, &paths)
}

fn sum(root: &Path, paths: &[String]) -> Result<u64> {
    let mut n = 0;
    for path in paths {
        let text =
            std::fs::read_to_string(root.join(path)).with_context(|| format!("reading {path}"))?;
        n += u64::try_from(lines::count(path, &text))?;
    }
    Ok(n)
}

/// Committed generated files under the crate trees: a `generated` path
/// component, or an `@generated` marker in the first lines, outside the
/// files the manifest declares as generated and committed. The harness's
/// own negative controls hold generated code on purpose and are left out.
pub fn committed_generated(root: &Path, exceptions: &[&str]) -> Result<Vec<String>> {
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
