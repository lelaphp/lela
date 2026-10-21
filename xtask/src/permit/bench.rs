//! `permit --bench`, the cost bar of harness-design.md §4.4: this binary
//! timed as the hook calls it, over a scratch copy of the workspace's
//! manifest, and the quantiles as a reading over the harness crate.

use std::fs;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use serde_json::{Value, json};

use super::INSTRUMENT;
use super::hand_off::ask;
use crate::git::git;
use crate::key::Snapshot;
use crate::manifest::{self, Manifest};
use crate::reading::{Reading, Status, Store};

/// The bar of §4.4: p99 latency at the call, in milliseconds. What
/// `--bar` defaults to; a test misses it on purpose by passing a lower one.
pub(crate) const BAR_MS: f64 = 100.0;

/// What `--bench` measured: the quantiles over every call, and over each
/// shape, since a denial pays for its reading and an allow does not.
#[derive(Debug, Clone, PartialEq)]
struct Bench {
    runs: usize,
    all: Quantiles,
    shapes: Vec<(String, Quantiles)>,
}

/// Three points of one distribution of call times, in milliseconds.
#[derive(Debug, Clone, Copy, PartialEq)]
struct Quantiles {
    p50: f64,
    p99: f64,
    max: f64,
}

impl Quantiles {
    fn of(times: &mut [f64]) -> Quantiles {
        times.sort_by(f64::total_cmp);
        Quantiles {
            p50: quantile(times, 50),
            p99: quantile(times, 99),
            max: quantile(times, 100),
        }
    }
}

impl Bench {
    /// The bar is on every call the hook makes, denials included.
    fn meets_the_bar(&self, bar_ms: f64) -> bool {
        self.all.p99 <= bar_ms
    }
}

/// The nearest-rank quantile of sorted samples: the smallest sample that
/// at least `percent` of them do not exceed.
fn quantile(sorted: &[f64], percent: usize) -> f64 {
    let rank = (sorted.len() * percent).div_ceil(100).max(1);
    sorted[rank - 1]
}

/// The shapes `--bench` cycles: an allowed Edit that reads a file for the
/// region-body rule, an allowed Bash call, and a denied Edit that writes a
/// reading.
fn bench_shapes() -> [(&'static str, Value); 3] {
    [
        (
            "allow-edit",
            json!({"tool_name": "Edit", "tool_input": {"file_path": "README.md", "old_string": "#", "new_string": "##"}}),
        ),
        (
            "allow-bash",
            json!({"tool_name": "Bash", "tool_input": {"command": "cargo test -p xtask"}}),
        ),
        (
            "deny-edit",
            json!({"tool_name": "Edit", "tool_input": {"file_path": "seams.toml", "old_string": "a", "new_string": "b"}}),
        ),
    ]
}

/// Time `runs` calls of this binary as the hook would make them, over a
/// scratch copy of the workspace's manifest under `target/`, so the denials
/// the bench provokes are stamped and stored there and not here.
fn bench(root: &Path, runs: usize) -> Result<Bench> {
    if runs == 0 {
        bail!("--runs must be at least 1");
    }
    let scratch = root.join("target").join("harness-bench");
    scratch_workspace(root, &scratch)?;
    let exe = std::env::current_exe().context("locating this binary")?;
    let shapes = bench_shapes();
    let mut times: Vec<Vec<f64>> = vec![Vec::new(); shapes.len()];
    for i in 0..runs {
        let which = i % shapes.len();
        let stdin = shapes[which].1.to_string();
        let start = Instant::now();
        let mut permit = Command::new(&exe);
        permit.arg("--root").arg(&scratch).arg("permit");
        permit
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let out = ask(permit, &stdin).context("running xtask permit")?;
        times[which].push(start.elapsed().as_secs_f64() * 1000.0);
        if !out.status.success() {
            bail!(
                "xtask permit exited {}: {}",
                out.status,
                String::from_utf8_lossy(&out.stderr)
            );
        }
    }
    let mut all: Vec<f64> = times.iter().flatten().copied().collect();
    Ok(Bench {
        runs,
        all: Quantiles::of(&mut all),
        shapes: shapes
            .iter()
            .zip(&mut times)
            .filter(|(_, t)| !t.is_empty())
            .map(|((name, _), t)| ((*name).to_owned(), Quantiles::of(t)))
            .collect(),
    })
}

/// A git repository at `dir`, rebuilt from scratch, holding the
/// workspace's manifest, its pin, the divergence registry and a README:
/// where `--bench` provokes denials, so the readings land in its store and
/// not the workspace's. The tests build the same repository through
/// `tests/common`, which cannot be reached from here, with a registry of
/// no row in place of the tracked one, whose rows no test of theirs is
/// about (#3091); the recipe is four files and one commit, stated in both. `bench`'s A/A control over the
/// judge takes its scratch workspace from here too, at a directory of its
/// own, so the one recipe stands in one place.
pub(crate) fn scratch_workspace(root: &Path, dir: &Path) -> Result<()> {
    fs::remove_dir_all(dir).ok();
    fs::create_dir_all(dir.join("docs")).with_context(|| format!("creating {}", dir.display()))?;
    let manifest = Manifest::load(root)?;
    for file in [
        manifest::FILE,
        manifest.oracle.pin.as_str(),
        "docs/divergence.toml",
        "README.md",
    ] {
        fs::copy(root.join(file), dir.join(file)).with_context(|| format!("copying {file}"))?;
    }
    git(dir, &["init", "-q", "-b", "main"], None)?;
    git(dir, &["add", "."], None)?;
    let commit = [
        &crate::scratch::UNHOOKED[..],
        &["commit", "-q", "-m", "scratch"],
    ]
    .concat();
    git(dir, &commit, None)?;
    Ok(())
}

/// `permit --bench`: measure, print one line per shape and the verdict,
/// record the reading, over the harness crate whose sources the judge is.
/// A measurement, which owes no exit (§4.4, #61): the reading carries `pass`
/// or `fail` against `bar_ms`, and the exit is 0 whatever it says, since
/// a bench number is a claim a person reads with the A/A control.
pub(crate) fn run_bench(
    root: &Path,
    manifest: &Manifest,
    store: &Store,
    runs: usize,
    bar_ms: f64,
) -> Result<()> {
    let snapshot = Snapshot::disk(root)?;
    let measured = bench(root, runs)?;
    let verdict = if measured.meets_the_bar(bar_ms) {
        "pass"
    } else {
        "fail"
    };
    let line = |name: &str, q: &Quantiles| {
        format!(
            "{name}: p50 {:.1} ms, p99 {:.1} ms, max {:.1} ms",
            q.p50, q.p99, q.max
        )
    };
    println!(
        "permit --bench: {} runs; {}; bar p99 ≤ {bar_ms:.0} ms: {verdict}",
        measured.runs,
        line("all", &measured.all)
    );
    for (name, q) in &measured.shapes {
        println!("  {}", line(name, q));
    }
    let mut reading = Reading::for_scope(INSTRUMENT, crate::done::HARNESS, manifest, &snapshot)?;
    reading.status = Status::of(measured.meets_the_bar(bar_ms));
    reading.set("runs", i64::try_from(measured.runs)?);
    reading.set("bar_ms", bar_ms);
    reading.set("p50_ms", measured.all.p50);
    reading.set("p99_ms", measured.all.p99);
    reading.set("max_ms", measured.all.max);
    for (name, q) in &measured.shapes {
        let key = name.replace('-', "_");
        reading.set(format!("{key}_p50_ms"), q.p50);
        reading.set(format!("{key}_p99_ms"), q.p99);
    }
    reading.message = format!(
        "permit at the tool call: p99 {:.1} ms against the bar of {bar_ms:.0} ms (harness-design.md §4.4)",
        measured.all.p99
    );
    store.record_measurement(&[reading])
}
