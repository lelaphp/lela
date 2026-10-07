//! `xtask bench`, the instrument the measurement protocol's readings come
//! from (harness-design.md §3, §4.2, §7). It measures one bar under
//! `docs/perf/measurement-protocol.md`, the protocol this engine inherits:
//! two arms, twelve paired reps after two discarded, the order alternated
//! inside each rep, the arms' bytes held equal in the rep that produced
//! them, each arm's program pinned at both ends of the run, and the paired
//! delta's quartiles deciding whether the sign is established at all.
//!
//! **Half of a bench number, and which half.** The run and the reading are
//! the agent's; saying what the number means is a person's (§4.2, §11). So
//! this instrument ends where every instrument ends, in one reading per
//! bar, scoped by the bar's name, the measurement in `numbers` and `status`
//! pass or fail against the bar — and its exit is 0 whatever the reading
//! says, recorded through [`Store::record_measurement`], which owes none.
//!
//! **What a bar is today.** Every bar the harness declares is an A/A
//! control: two arms that are one call, whose bar is the protocol's own
//! verdict, that the rig cannot tell the arm from itself (§2). The one
//! declared bar is the judge at the tool call, which is the arm the harness
//! has that runs. A bar whose arms are two different things needs a bound
//! of its own — a geomean ceiling, a ratio — and that is a number a person
//! writes in the manifest, not one this instrument invents.
//!
//! **Why the A/A verdict is a reading and not a refusal.** §4.3 is a
//! quartile rule with no confidence level, so it resolves some unmeasured
//! fraction of the time on a null effect; the second engine measured one
//! resolved row in seventeen. A single resolved A/A row is evidence and a
//! pattern of them is the defect, which is a judgement over runs and not
//! over one, and it is the person's half.
//!
//! **What is not here.** The bar for `permit`'s cost at the tool call stays
//! in `permit --bench`, where harness-design.md §4.4 states it: that bar is
//! an absolute p99 ceiling on one arm, and a per-arm percentile under this
//! protocol is descriptive and never signed (§4.1, §9). What this
//! instrument states about the same call is the other thing — that the
//! paired rig is honest about it.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use serde::Deserialize;

use crate::inputs;
use crate::key::Snapshot;
use crate::reading::{Reading, Status, Store};

/// The instrument's name, the reading's `instrument` field.
pub(crate) const INSTRUMENT: &str = "bench";

/// The protocol every number here is taken under.
pub(crate) const PROTOCOL: &str = "docs/perf/measurement-protocol.md";

/// §3: measured reps per arm. What `--reps` defaults to.
pub const REPS: usize = 12;

/// §3: reps run before the first measured one, and discarded.
const WARMUP: usize = 2;

/// §4.3: below this many paired deltas nothing resolves, whatever the
/// quartiles say. At one rep Q1 and Q3 are the sample itself, so any
/// nonzero delta cleared zero and one binary against a copy of itself came
/// back signed.
const MIN_REPS_TO_RESOLVE: usize = 4;

/// §8: what standing a run's numbers are offered with. `PROVISIONAL` and
/// `BASELINE-OF-RECORD` owe a written reservation, since a standing with
/// none is a word and not a label; which one a run deserves is part of the
/// sign-off, and that is a person's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Label {
    /// The default: a number with no standing claimed.
    Unlabelled,
    /// A run made to see the instrument work.
    Smoke,
    /// A number offered with a written reservation.
    Provisional,
    /// The number later runs are held against.
    BaselineOfRecord,
}

impl Label {
    /// The label as the reading spells it.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Label::Unlabelled => "UNLABELLED",
            Label::Smoke => "SMOKE",
            Label::Provisional => "PROVISIONAL",
            Label::BaselineOfRecord => "BASELINE-OF-RECORD",
        }
    }

    /// Whether the label owes a note (§8).
    fn needs_note(self) -> bool {
        matches!(self, Label::Provisional | Label::BaselineOfRecord)
    }
}

/// One arm of a bar: the program a rep runs once, its arguments, and what
/// it reads on stdin.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Arm {
    name: String,
    program: PathBuf,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    stdin: String,
}

/// One bar: what is measured, and the two arms a rep times against each
/// other. A bar whose arms are one call is an A/A control (§2); the
/// fixture under `xtask/fixtures/bench/` is one whose arms are not, which
/// is what makes it the negative control.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct Bar {
    id: String,
    about: String,
    a: Arm,
    b: Arm,
}

impl Bar {
    /// One bar read from a TOML file: how the negative control is run, and
    /// how a person points the instrument at a pair of their own.
    fn read(path: &Path) -> Result<Bar> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading the bar at {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("{} is not a bar", path.display()))
    }
}

/// Seven numbers over one sample, §4.1: the quantiles by linear
/// interpolation on nearest ranks, the spread between the outer two, and
/// the ends.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Stats {
    median: f64,
    q1: f64,
    q3: f64,
    iqr: f64,
    min: f64,
    max: f64,
    n: usize,
}

/// §4.1's quantiles over `samples`, which are not sorted here first; an
/// empty sample is every number at zero. Median rather than mean and the
/// inter-quartile range rather than a deviation, because the failure mode
/// on a shared machine is a few very slow reps.
fn stats(samples: &[f64]) -> Stats {
    if samples.is_empty() {
        return Stats::default();
    }
    let mut a = samples.to_vec();
    a.sort_by(f64::total_cmp);
    let n = a.len();
    // The protocol's rank, `p * (n - 1) + 1`, in quarters and in whole
    // numbers: every `p` it names is a quarter, so the rank is an index and
    // a remainder out of four, and the index never leaves the integers.
    let q = |quarters: usize| -> f64 {
        let rank = (n - 1) * quarters;
        let lo = rank / 4;
        let fraction = [0.0, 0.25, 0.5, 0.75][rank % 4];
        if lo + 1 >= n {
            a[n - 1]
        } else {
            a[lo] + fraction * (a[lo + 1] - a[lo])
        }
    };
    let (median, q1, q3) = (q(2), q(1), q(3));
    Stats {
        median,
        q1,
        q3,
        iqr: q3 - q1,
        min: a[0],
        max: a[n - 1],
        n,
    }
}

/// §4.3: whether the paired delta's sign is established. `<=` and `>=` and
/// not `<` and `>`, since a quartile exactly at zero still straddles it.
fn sign_of(delta: &Stats) -> &'static str {
    if delta.n < MIN_REPS_TO_RESOLVE || (delta.q1 <= 0.0 && delta.q3 >= 0.0) {
        SWAMPED
    } else {
        RESOLVED
    }
}

/// The verdict of a row whose sign the quartiles do not establish; the
/// A/A control's bar is that every row reads this.
const SWAMPED: &str = "NOISE-SWAMPED";
/// The verdict of a row whose sign they do.
const RESOLVED: &str = "resolved";

/// §5: how the two arms' bytes compared inside the rep that produced them.
/// A disagreement is not a value here: it stops the run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Correctness {
    /// Both arms wrote the same non-empty bytes.
    Bytes,
    /// Both arms wrote nothing, so nothing was compared: the timings
    /// stand and the agreement proves nothing.
    Vacuous,
}

impl Correctness {
    fn as_str(self) -> &'static str {
        match self {
            Correctness::Bytes => "stdout-bytes",
            Correctness::Vacuous => "vacuous",
        }
    }
}

/// One run of one arm: how long it took, and what it wrote.
struct Sample {
    ms: f64,
    wrote: Vec<u8>,
}

/// What one bar measured: the two arms, their paired delta, the delta as a
/// percentage of the candidate arm's median, and how the bytes compared.
struct Measured {
    a: Stats,
    b: Stats,
    delta: Stats,
    paired_pct: f64,
    correctness: Correctness,
}

/// §6: one program as one read of it, digested with the byte count that
/// read gave, and an independent `metadata` agreeing with the count, so
/// the two cannot describe different files by construction.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Pin {
    digest: String,
    bytes: u64,
}

impl Pin {
    fn take(path: &Path) -> Result<Pin> {
        let data = std::fs::read(path).with_context(|| format!("pinning {}", path.display()))?;
        let bytes = u64::try_from(data.len()).context("a program past the count")?;
        let stat =
            std::fs::metadata(path).with_context(|| format!("pinning {}", path.display()))?;
        if stat.len() != bytes {
            bail!(
                "{} is {bytes} bytes as read and {} as stated; the pin does not hold",
                path.display(),
                stat.len()
            );
        }
        Ok(Pin {
            digest: inputs::digest(&data),
            bytes,
        })
    }
}

/// The program an arm names, as a path that can be pinned: the name as
/// given when it is a path that exists, and otherwise the first entry of
/// `PATH` that holds it. §6 admits no unpinnable arm, so a program neither
/// place has refuses the run.
fn resolve(program: &Path) -> Result<PathBuf> {
    if program.components().count() > 1 || program.exists() {
        return Ok(program.to_path_buf());
    }
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
        .with_context(|| format!("{} is on no PATH entry", program.display()))
}

/// §7: the 1-, 5- and 15-minute load averages at the start of the run.
/// Linux states them in `/proc/loadavg`; macOS in `vm.loadavg`, braced.
/// A machine that states neither is none, which the reading says.
fn load_average() -> Option<[f64; 3]> {
    let text = std::fs::read_to_string("/proc/loadavg").ok().or_else(|| {
        let sysctl = Command::new("sysctl")
            .args(["-n", "vm.loadavg"])
            .output()
            .ok()?;
        String::from_utf8(sysctl.stdout).ok()
    })?;
    let mut fields = text
        .split_whitespace()
        .filter_map(|f| f.trim_matches(|c| c == '{' || c == '}').parse::<f64>().ok());
    Some([fields.next()?, fields.next()?, fields.next()?])
}

/// One rep of one arm, timed as the caller of the arm pays for it: the
/// spawn, the stdin it reads and the wait. A nonzero exit refuses the run,
/// naming the arm, since an arm that failed timed nothing.
fn once(arm: &Arm, program: &Path) -> Result<Sample> {
    let start = Instant::now();
    let mut child = Command::new(program)
        .args(&arm.args)
        .stdin(if arm.stdin.is_empty() {
            Stdio::null()
        } else {
            Stdio::piped()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawning the arm {}", arm.name))?;
    if let Some(mut pipe) = child.stdin.take() {
        pipe.write_all(arm.stdin.as_bytes())
            .with_context(|| format!("feeding the arm {}", arm.name))?;
    }
    let done = child
        .wait_with_output()
        .with_context(|| format!("waiting for the arm {}", arm.name))?;
    let ms = start.elapsed().as_secs_f64() * 1000.0;
    if !done.status.success() {
        bail!(
            "the arm {} exited {}: {}",
            arm.name,
            done.status,
            String::from_utf8_lossy(&done.stderr).trim()
        );
    }
    Ok(Sample {
        ms,
        wrote: done.stdout,
    })
}

/// One bar measured: `reps` paired reps after [`WARMUP`] discarded ones,
/// the order alternated so a penalty on whichever process runs second
/// cancels across reps instead of being charged to one arm; the arms' bytes
/// held equal inside every rep; the programs pinned at both ends. A byte
/// disagreement re-reads the pin first, since a program that moved
/// underneath the run explains one (§6).
fn measure(bar: &Bar, reps: usize) -> Result<Measured> {
    if reps == 0 {
        bail!("--reps must be at least 1");
    }
    let programs = [resolve(&bar.a.program)?, resolve(&bar.b.program)?];
    let before = [Pin::take(&programs[0])?, Pin::take(&programs[1])?];
    let (mut a_ms, mut b_ms, mut delta) = (Vec::new(), Vec::new(), Vec::new());
    let mut vacuous = false;
    for r in 1..=(WARMUP + reps) {
        let (sa, sb) = if r % 2 == 0 {
            let sa = once(&bar.a, &programs[0])?;
            let sb = once(&bar.b, &programs[1])?;
            (sa, sb)
        } else {
            let sb = once(&bar.b, &programs[1])?;
            let sa = once(&bar.a, &programs[0])?;
            (sa, sb)
        };
        if sa.wrote != sb.wrote {
            holds(&programs, &before)?;
            bail!(
                "{}: the two arms wrote different bytes at rep {r}; no timing is read from a bar whose arms disagree ({PROTOCOL} §5)",
                bar.id
            );
        }
        vacuous |= sa.wrote.is_empty();
        if r > WARMUP {
            delta.push(sa.ms - sb.ms);
            a_ms.push(sa.ms);
            b_ms.push(sb.ms);
        }
    }
    holds(&programs, &before)?;
    let (a, b, delta) = (stats(&a_ms), stats(&b_ms), stats(&delta));
    Ok(Measured {
        a,
        b,
        delta,
        paired_pct: if b.median > 0.0 {
            delta.median / b.median * 100.0
        } else {
            f64::NAN
        },
        correctness: if vacuous {
            Correctness::Vacuous
        } else {
            Correctness::Bytes
        },
    })
}

/// §6's second end: each program as it is now against the pin taken at the
/// start, and a refusal naming the one that moved.
fn holds(programs: &[PathBuf; 2], before: &[Pin; 2]) -> Result<()> {
    for (path, pinned) in programs.iter().zip(before) {
        if &Pin::take(path)? != pinned {
            bail!(
                "{} moved underneath the run: it was {} at {} bytes; no number is stated over two programs ({PROTOCOL} §6)",
                path.display(),
                pinned.digest,
                pinned.bytes
            );
        }
    }
    Ok(())
}

/// The hook's call the A/A control times: an `Edit` of the manifest, which
/// the judge denies. The denied shape and not an allowed one, because a
/// denial is the call that writes its own reading and prints its decision,
/// so the two arms have bytes to hold equal (§5) and the arm is the
/// expensive half of what the hook pays for.
const CALL: &str = r#"{"tool_name":"Edit","tool_input":{"file_path":"seams.toml","old_string":"a","new_string":"b"}}"#;

/// The bars the harness declares: one, the A/A control over the judge at
/// the tool call, whose two arms are one call of `xtask permit` over a
/// scratch workspace under `target/`, so the denials a shape provokes land
/// in that store and not this one. It is the arm the harness has that runs;
/// the gates and the perf bars of harness-design.md §3 join it as the
/// engine reaches what they measure.
fn declared(root: &Path) -> Result<Vec<Bar>> {
    let exe = std::env::current_exe().context("locating this binary")?;
    let scratch = root.join("target").join("harness-bench-aa");
    crate::permit::scratch_workspace(root, &scratch)?;
    let arm = |name: &str| Arm {
        name: name.to_owned(),
        program: exe.clone(),
        args: vec![
            "--root".to_owned(),
            scratch.display().to_string(),
            "permit".to_owned(),
        ],
        stdin: CALL.to_owned(),
    };
    Ok(vec![Bar {
        id: "permit".to_owned(),
        about: "the judge at the tool call, arm against arm".to_owned(),
        a: arm("permit-a"),
        b: arm("permit-b"),
    }])
}

/// `xtask bench`: each bar measured under the protocol, printed with the
/// load beside the number (§7), and written as one reading scoped by the
/// bar's name. A measurement, which owes no exit: the reading carries
/// `pass` or `fail` against the bar and the exit is 0 whatever it says,
/// since a bench number is a claim a person reads with the A/A control
/// (AGENTS.md; harness-design.md §4.2, §4.4).
pub fn run(
    root: &Path,
    store: &Store,
    bar: Option<&str>,
    from: Option<&Path>,
    reps: usize,
    label: Label,
    note: &str,
) -> Result<()> {
    if label.needs_note() && note.trim().is_empty() {
        bail!(
            "--label {} owes a written reservation; a standing with none is a word and not a label ({PROTOCOL} §8)",
            label.as_str().to_lowercase()
        );
    }
    let mut bars = match from {
        Some(path) => vec![Bar::read(path)?],
        None => declared(root)?,
    };
    if let Some(name) = bar {
        bars.retain(|b| b.id == name);
        if bars.is_empty() {
            bail!("no bar is named {name:?}");
        }
    }
    let load = load_average();
    let snapshot = Snapshot::disk(root)?;
    let mut readings = Vec::new();
    for bar in &bars {
        let measured = measure(bar, reps)?;
        print!("{}", rendered(bar, &measured, reps, label, load));
        readings.push(recorded(
            bar, &measured, reps, label, note, load, &snapshot,
        )?);
    }
    store.record_measurement(&readings)
}

/// What one measured bar prints: the standing and the knobs, a line per
/// arm, the paired delta with its verdict, and the load beside them all.
fn rendered(
    bar: &Bar,
    measured: &Measured,
    reps: usize,
    label: Label,
    load: Option<[f64; 3]>,
) -> String {
    let stat = |name: &str, s: &Stats| {
        format!(
            "  {name}: median {:.3} ms, IQR {:.3} (q1 {:.3}, q3 {:.3}, min {:.3}, max {:.3}, n {})\n",
            s.median, s.iqr, s.q1, s.q3, s.min, s.max, s.n
        )
    };
    format!(
        "bench {}: {}; {reps} reps (+{WARMUP} warm-up) per arm, order alternated; load {}\n{}{}  paired delta: median {:.3} ms ({:+.2}%), IQR {:.3} (q1 {:.3}, q3 {:.3}); {}\n  correctness {}; the bar is the A/A control of {PROTOCOL} §2\n",
        bar.id,
        label.as_str(),
        load_text(load),
        stat(&bar.a.name, &measured.a),
        stat(&bar.b.name, &measured.b),
        measured.delta.median,
        measured.paired_pct,
        measured.delta.iqr,
        measured.delta.q1,
        measured.delta.q3,
        sign_of(&measured.delta),
        measured.correctness.as_str(),
    )
}

/// A measured number as the store keeps it: to the microsecond, which is
/// the ancestor's six places and finer than any rep this instrument times.
fn rounded(value: f64) -> f64 {
    (value * 1e6).round() / 1e6
}

/// The load as a rendering prints it, and what it says when the machine
/// states none.
fn load_text(load: Option<[f64; 3]>) -> String {
    load.map_or_else(
        || "unknown".to_owned(),
        |l| format!("{:.2} {:.2} {:.2}", l[0], l[1], l[2]),
    )
}

/// One measured bar as one reading: `pass` while the A/A control holds,
/// `fail` when the sign resolved, the measurement in `numbers` with the
/// load the protocol requires beside it, and the standing in the message.
/// Stamped over the workspace, a bar having no sources of its own; `done`
/// asks no instrument for a bench reading, which is the other half of the
/// exit rule.
fn recorded(
    bar: &Bar,
    measured: &Measured,
    reps: usize,
    label: Label,
    note: &str,
    load: Option<[f64; 3]>,
    snapshot: &Snapshot,
) -> Result<Reading> {
    let verdict = sign_of(&measured.delta);
    let mut reading = Reading::stamped_over(INSTRUMENT, &bar.id, snapshot, Path::new("."))?;
    reading.status = if verdict == SWAMPED {
        Status::Pass
    } else {
        Status::Fail
    };
    reading.count("reps", u64::try_from(measured.delta.n)?)?;
    reading.count("warmup_discarded", u64::try_from(WARMUP)?)?;
    let numbers = [
        ("a_median_ms", measured.a.median),
        ("a_iqr_ms", measured.a.iqr),
        ("b_median_ms", measured.b.median),
        ("b_iqr_ms", measured.b.iqr),
        ("delta_median_ms", measured.delta.median),
        ("delta_q1_ms", measured.delta.q1),
        ("delta_q3_ms", measured.delta.q3),
        ("delta_iqr_ms", measured.delta.iqr),
        ("paired_pct", measured.paired_pct),
    ];
    for (key, value) in numbers {
        if value.is_finite() {
            reading
                .numbers
                .insert((*key).to_owned(), rounded(value).into());
        }
    }
    if let Some(l) = load {
        for (key, value) in [("load_1", l[0]), ("load_5", l[1]), ("load_15", l[2])] {
            reading.numbers.insert((*key).to_owned(), value.into());
        }
    }
    let reservation = if note.trim().is_empty() {
        String::new()
    } else {
        format!(" ({})", note.trim())
    };
    reading.message = format!(
        "{}: paired delta {:.3} ms ({:+.2}%) over {reps} reps, {verdict}; correctness {}; {}{reservation}; load {} ({PROTOCOL} §4.3, §7)",
        bar.about,
        measured.delta.median,
        measured.paired_pct,
        measured.correctness.as_str(),
        label.as_str(),
        load_text(load),
    );
    Ok(reading)
}

#[cfg(test)]
mod tests {
    use super::{RESOLVED, SWAMPED, Stats, sign_of, stats};

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    /// §4.1 worked by hand over 1..=4: Q1 sits at index 1.75, so it is
    /// 1 + 0.75 * (2 - 1); the median at 2.5; Q3 at 3.25.
    #[test]
    fn quantiles_interpolate_between_nearest_ranks() {
        let s = stats(&[4.0, 1.0, 3.0, 2.0]);
        assert!(close(s.q1, 1.75), "{s:?}");
        assert!(close(s.median, 2.5), "{s:?}");
        assert!(close(s.q3, 3.25), "{s:?}");
        assert!(close(s.iqr, 1.5), "{s:?}");
        assert!(close(s.min, 1.0) && close(s.max, 4.0), "{s:?}");
        assert_eq!(s.n, 4);
        assert_eq!(stats(&[]), Stats::default());
    }

    /// §4.3: three reps resolve nothing however far apart they are, and a
    /// quartile exactly at zero straddles it.
    #[test]
    fn the_sign_is_established_only_above_the_floor_and_clear_of_zero() {
        assert_eq!(sign_of(&stats(&[10.0, 10.0, 10.0])), SWAMPED);
        assert_eq!(sign_of(&stats(&[10.0, 10.0, 10.0, 10.0])), RESOLVED);
        assert_eq!(sign_of(&stats(&[0.0, 0.0, 0.0, 3.0])), SWAMPED);
        assert_eq!(sign_of(&stats(&[-3.0, 0.0, 0.0, 0.0])), SWAMPED);
        assert_eq!(sign_of(&stats(&[-1.0, -1.0, 1.0, 1.0])), SWAMPED);
        assert_eq!(sign_of(&stats(&[-4.0, -3.0, -2.0, -1.0])), RESOLVED);
    }
}
