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
//! **What a bar is today.** A bar is [`Held`] one of two ways. An A/A
//! control is two arms that are one call, whose bar is the protocol's own
//! verdict, that the rig cannot tell the arm from itself (§2): the judge
//! at the tool call, and makala against itself over each bench workload. A
//! series is two arms that are two things, the production server against
//! php-fpm behind a `FastCGI` client over the same workload (#2351,
//! `makala::bench`). Its reading states the measurement and the distance
//! still to go to its second arm, harness §3's "`FastCGI` ≥ php-fpm",
//! which holds nothing (#1902). A series with a `[[bench]]` tolerance is
//! held by [`series`]'s rule, beside a baseline the store keeps: one
//! established regression warns, two in a row fail the reading and the
//! run (#2659); one without a tolerance passes whatever it measured.
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

pub(crate) mod row;
mod series;

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
    /// A series and not an A/A control: the negative control of the rule a nightly bar is held by (protocol §12).
    #[serde(default)]
    series: bool,
}

impl Bar {
    /// One bar read from a TOML file: how the negative control is run, and
    /// how a person points the instrument at a pair of their own.
    fn read(path: &Path) -> Result<Bar> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading the bar at {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("{} is not a bar", path.display()))
    }

    /// The bar as the run measures it, two processes, an A/A control
    /// unless the file says it is a series.
    fn declared(self) -> Declared {
        let Bar {
            id,
            about,
            a,
            b,
            series,
        } = self;
        Declared {
            id,
            about,
            held: if series { Held::Series } else { Held::Control },
            start: Box::new(move || {
                let spawned = |arm: Arm| -> Result<Box<dyn Side>> {
                    let program = resolve(&arm.program)?;
                    Ok(Box::new(Spawned { arm, program }))
                };
                Ok(Ok([spawned(a)?, spawned(b)?]))
            }),
        }
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
    sign(delta.n, delta.q1, delta.q3)
}

/// [`sign_of`] over a delta's reps and quartiles, as a reading keeps them:
/// the one rule a run and a row of its reading judge by (`row`, #2646).
pub(crate) fn sign(n: usize, q1: f64, q3: f64) -> &'static str {
    if n < MIN_REPS_TO_RESOLVE || (q1 <= 0.0 && q3 >= 0.0) {
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
    /// Both arms wrote the same non-empty bytes on the stream named.
    Bytes(Stream),
    /// Both arms wrote nothing, so nothing was compared: the timings
    /// stand and the agreement proves nothing.
    Vacuous,
}

impl Correctness {
    fn as_str(self) -> &'static str {
        match self {
            Correctness::Bytes(Stream::Stdout) => "stdout-bytes",
            Correctness::Bytes(Stream::Body) => "body-bytes",
            Correctness::Vacuous => "vacuous",
        }
    }
}

/// One run of one arm: what it measured in its arm's [`Unit`], and what it
/// wrote, the bytes the two arms' runs of one rep are held equal by (§5).
pub(crate) struct Sample {
    pub(crate) value: f64,
    pub(crate) wrote: Vec<u8>,
}

/// What a rep of an arm measures (§3): the milliseconds one run took, a
/// cost, or the requests a second its clients were answered at, a rate
/// (#2660). The paired delta is taken in it, `a - b`, so a rate's loss is
/// a negative delta; [`Unit::worse`] turns either into a cost.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Unit {
    Ms,
    Rps,
}

impl Unit {
    /// The suffix of the reading's keys and of each figure printed.
    pub(crate) fn suffix(self) -> &'static str {
        match self {
            Unit::Ms => "ms",
            Unit::Rps => "rps",
        }
    }

    /// The reading's key for the figure `name` in the unit: the one
    /// spelling the run writes and a reading is read back by.
    pub(crate) fn key(self, name: &str) -> String {
        format!("{name}_{}", self.suffix())
    }

    /// The sign that makes a delta in the unit a cost: more is worse of
    /// a time, and less of a rate.
    pub(crate) fn worse(self) -> f64 {
        match self {
            Unit::Ms => 1.0,
            Unit::Rps => -1.0,
        }
    }

    /// The unit a reading's numbers were recorded in: a rate where any
    /// figure is kept under the rate's suffix.
    pub(crate) fn of(reading: &Reading) -> Unit {
        let rate = reading.numbers.keys().any(|key| key.ends_with("_rps"));
        if rate { Unit::Rps } else { Unit::Ms }
    }
}

/// What an arm's run wrote that the rep holds equal (§5): what a process
/// wrote on its first stream, or the body of a server's response, its head being
/// what each host is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stream {
    Stdout,
    Body,
}

/// One arm as a rep runs it: its name, the program pinned for it at both
/// ends of the run (§6), and one run of it, timed as its caller pays for
/// it. A process spawned per rep is one; a request sent to a server kept
/// across the reps is another (`makala::bench`).
pub(crate) trait Side {
    /// The arm's name, as its line of the rendering says it.
    fn name(&self) -> &str;
    /// The program the arm runs, which the run pins.
    fn program(&self) -> &Path;
    /// One run, a nonzero exit or no answer refusing the run, naming the
    /// arm.
    fn once(&mut self) -> Result<Sample>;
    /// What [`Sample::wrote`] holds of the run.
    fn stream(&self) -> Stream {
        Stream::Stdout
    }
    /// What [`Sample::value`] measures.
    fn unit(&self) -> Unit {
        Unit::Ms
    }
}

/// A bar's two arms as a rep runs them, or why this machine cannot run
/// them, which is the bar's skip.
pub(crate) type Sides = Result<[Box<dyn Side>; 2], String>;

/// How a bar's reading is held.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Held {
    /// An A/A control: a pass while the paired delta's sign is swamped
    /// (§2, §4.3).
    Control,
    /// A series: two arms that differ, its reading stating the measurement
    /// and the distance to go to the second arm, and held by [`series`]'s
    /// rule where the manifest gives it a tolerance (#1902, #2336, #2659).
    Series,
}

impl Held {
    /// The reading's status for a paired delta whose sign is `verdict`: a
    /// control fails where the sign resolved, and a series is held to no
    /// bound.
    fn status(self, verdict: &str) -> Status {
        match self {
            Held::Control if verdict == RESOLVED => Status::Fail,
            Held::Control | Held::Series => Status::Pass,
        }
    }
}

/// A bar the run measures: its name, what it measures, how its reading is
/// held, and its arms, started only once it is measured, so a run of one
/// bar starts nothing of another's.
pub(crate) struct Declared {
    pub(crate) id: String,
    pub(crate) about: String,
    pub(crate) held: Held,
    pub(crate) start: Box<dyn FnOnce() -> Result<Sides>>,
}

/// A process arm, its program resolved: spawned once a rep.
struct Spawned {
    arm: Arm,
    program: PathBuf,
}

impl Side for Spawned {
    fn name(&self) -> &str {
        &self.arm.name
    }

    fn program(&self) -> &Path {
        &self.program
    }

    fn once(&mut self) -> Result<Sample> {
        once(&self.arm, &self.program)
    }
}

/// What one bar measured, in its unit: the two arms, their paired delta,
/// the delta as a percentage of the candidate arm's median, and how the
/// bytes compared.
struct Measured {
    unit: Unit,
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
    let value = start.elapsed().as_secs_f64() * 1000.0;
    if !done.status.success() {
        bail!(
            "the arm {} exited {}: {}",
            arm.name,
            done.status,
            String::from_utf8_lossy(&done.stderr).trim()
        );
    }
    Ok(Sample {
        value,
        wrote: done.stdout,
    })
}

/// One bar measured: `reps` paired reps after [`WARMUP`] discarded ones,
/// the order alternated so a penalty on whichever process runs second
/// cancels across reps instead of being charged to one arm; the arms' bytes
/// held equal inside every rep; the programs pinned at both ends. A byte
/// disagreement re-reads the pin first, since a program that moved
/// underneath the run explains one (§6).
fn measure(id: &str, [a, b]: &mut [Box<dyn Side>; 2], reps: usize) -> Result<Measured> {
    let programs = [a.program().to_path_buf(), b.program().to_path_buf()];
    let before = [Pin::take(&programs[0])?, Pin::take(&programs[1])?];
    let (mut a_values, mut b_values, mut delta) = (Vec::new(), Vec::new(), Vec::new());
    let mut vacuous = false;
    for r in 1..=(WARMUP + reps) {
        let (sa, sb) = if r % 2 == 0 {
            let sa = a.once()?;
            (sa, b.once()?)
        } else {
            let sb = b.once()?;
            (a.once()?, sb)
        };
        if sa.wrote != sb.wrote {
            holds(&programs, &before)?;
            bail!(
                "{id}: the two arms wrote different bytes at rep {r}; no timing is read from a bar whose arms disagree ({PROTOCOL} §5)"
            );
        }
        vacuous |= sa.wrote.is_empty();
        if r > WARMUP {
            delta.push(sa.value - sb.value);
            a_values.push(sa.value);
            b_values.push(sb.value);
        }
    }
    holds(&programs, &before)?;
    let (stream, unit) = (a.stream(), a.unit());
    let (a, b, delta) = (stats(&a_values), stats(&b_values), stats(&delta));
    Ok(Measured {
        unit,
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
            Correctness::Bytes(stream)
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

/// The bars the harness declares: the A/A control over the judge at the
/// tool call, whose two arms are one call of `xtask permit` over a scratch
/// workspace under `target/`, so the denials a shape provokes land in that
/// store and not this one; and the production server's, makala against
/// php-fpm and against itself over each bench workload (#2351,
/// `makala::bench`). The other perf bars of harness-design.md §3 join them
/// as the engine reaches what they measure.
fn declared(root: &Path) -> Result<Vec<Declared>> {
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
    let permit = Bar {
        id: "permit".to_owned(),
        about: "the judge at the tool call, arm against arm".to_owned(),
        a: arm("permit-a"),
        b: arm("permit-b"),
        series: false,
    };
    let mut bars = vec![permit.declared()];
    bars.extend(crate::makala::bench::declared(root)?);
    bars.extend(
        [
            ("jit-wrapper", ("direct", "direct"), Held::Series),
            ("jit-wrapper-aa", ("wrapper-b", "wrapper"), Held::Control),
        ]
        .map(|(id, b, held)| wrapper(root, id, b, held)),
    );
    Ok(bars)
}

/// `php_jit`'s bars (#2742): `CALLS` calls of its test kernel through the
/// kernel's `extern "C"` wrapper, `catch_unwind` and its `Status`, against
/// the same calls made to the kernel directly, each through a function
/// pointer, a series; and the wrapper against itself, its A/A control.
fn wrapper(root: &Path, id: &str, (b, way): (&'static str, &'static str), held: Held) -> Declared {
    const CALLS: &str = "200000000";
    let root = root.to_path_buf();
    Declared {
        id: id.to_owned(),
        about: format!("{CALLS} calls of php_jit's test kernel through its wrapper, against {b}"),
        held,
        start: Box::new(move || {
            let program =
                crate::oracle::profile::released(&root, "php_jit", "php_jit_bench", Some("bench"))?;
            let arm = |name: &str, way: &str| -> Box<dyn Side> {
                let args = vec![way.to_owned(), CALLS.to_owned()];
                let (name, stdin) = (name.to_owned(), String::new());
                let arm = Arm {
                    name,
                    program: program.clone(),
                    args,
                    stdin,
                };
                Box::new(Spawned {
                    arm,
                    program: program.clone(),
                })
            };
            Ok(Ok([arm("wrapper", "wrapper"), arm(b, way)]))
        }),
    }
}

/// The variable GitHub sets on every runner: `github-hosted` on its own
/// fleet's, `self-hosted` on a machine of ours.
pub(crate) const RUNNER_ENVIRONMENT: &str = "RUNNER_ENVIRONMENT";

/// A bench number needs the quiet machine (protocol §7): a GitHub-hosted
/// runner is a shared VM whose neighbours move every arm, and its reading
/// would be noise a person could mistake for a measurement. So the command
/// refuses one before it measures, whatever workflow asked it to, and
/// writes nothing (#2477, #2259). The library entry is not held to it:
/// the negative control under `xtask/tests/bench.rs` tests the rig, and
/// runs wherever `xtask`'s tests run.
pub(crate) fn refuse_a_hosted_runner(environment: Option<&str>) -> Result<()> {
    if environment == Some("github-hosted") {
        bail!(
            "bench measures on the quiet self-hosted machine and never on a GitHub-hosted runner \
             ({RUNNER_ENVIRONMENT}=github-hosted): a shared VM's numbers are noise a person could \
             read as a measurement (#2477)"
        );
    }
    Ok(())
}

/// `xtask bench`: each bar measured under the protocol, printed with the
/// load beside the number (§7), and written as one reading scoped by the
/// bar's name. A measurement, which owes no exit: the reading carries
/// `pass` or `fail` against the bar and the exit is 0 whatever it says,
/// since a bench number is a claim a person reads with the A/A control
/// (AGENTS.md; harness-design.md §4.2, §4.4).
///
/// A bar held to a tolerance reads its earlier nights and its baseline
/// from `log` where one is handed, the published history, since each
/// nightly job starts from an empty store; and from the store's own
/// otherwise.
pub fn run(
    root: &Path,
    store: &Store,
    bar: Option<&str>,
    from: Option<&Path>,
    reps: usize,
    (label, note): (Label, &str),
    log: Option<&Path>,
) -> Result<()> {
    if label.needs_note() && note.trim().is_empty() {
        bail!(
            "--label {} owes a written reservation; a standing with none is a word and not a label ({PROTOCOL} §8)",
            label.as_str().to_lowercase()
        );
    }
    if reps == 0 {
        bail!("--reps must be at least 1");
    }
    let mut bars = match from {
        Some(path) => vec![Bar::read(path)?.declared()],
        None => declared(root)?,
    };
    if let Some(name) = bar {
        bars.retain(|b| b.id == name);
        if bars.is_empty() {
            bail!("no bar is named {name:?}");
        }
    }
    let snapshot = Snapshot::disk(root)?;
    let tolerances = crate::manifest::Manifest::load(root)?.tolerances;
    let log = match log {
        None => store.history(INSTRUMENT)?.readings,
        Some(path) => {
            let text = std::fs::read_to_string(path)
                .with_context(|| format!("reading the history at {}", path.display()))?;
            let mut log = crate::reading::Log::handed(&text).readings;
            log.retain(|reading| reading.instrument == INSTRUMENT);
            log
        }
    };
    let failed = store.root().join(FAILED);
    std::fs::remove_dir_all(&failed).ok();
    let (mut readings, mut refused) = (Vec::new(), Vec::new());
    for Declared {
        id,
        about,
        held,
        start,
    } in bars
    {
        // A bar whose arms this machine cannot run is a skip, and one whose
        // run refused (§1) is a failing reading that names why; either way
        // the next bar runs, and a refusal is the command's error once all
        // have run, so a refused run never reads as a measurement.
        let measured = start().and_then(|sides| match sides {
            Err(why) => Ok(Err(why)),
            Ok(mut sides) => {
                // The load where the bar's reps start, after its arms were
                // built and started (§7).
                let load = load_average();
                Ok(Ok((measure(&id, &mut sides, reps)?, sides, load)))
            }
        });
        let (measured, sides, load) = match measured {
            Ok(Ok(measured)) => measured,
            Ok(Err(why)) => {
                readings.push(unmeasured(&id, Status::Skip, &why, &snapshot)?);
                continue;
            }
            Err(e) => {
                let why = format!("{e:#}");
                readings.push(unmeasured(&id, Status::Fail, &why, &snapshot)?);
                refused.push(format!("{id}: {why}"));
                continue;
            }
        };
        let bar = Shown {
            id: &id,
            about: &about,
            held,
            names: [sides[0].name(), sides[1].name()],
        };
        print!("{}", rendered(&bar, &measured, reps, label, load));
        let mut reading = recorded(&bar, &measured, (reps, label, note), load, &snapshot)?;
        let held = (tolerances.iter().find(|t| t.bar == id))
            .map(|tolerance| series::hold(tolerance, &log, &mut reading));
        readings.push(reading);
        if let Some((baseline, regressed)) = held {
            readings.extend(baseline);
            if let Some(regressed) = regressed {
                std::fs::create_dir_all(&failed)?;
                let ticket = failed.join(format!("{}.md", id.replace('/', "-")));
                std::fs::write(&ticket, ticket_of(&id, &regressed))?;
                refused.push(regressed);
            }
        }
    }
    store.record_measurement(&readings)?;
    if !refused.is_empty() {
        bail!(
            "{} bar{} refused the run or regressed two nights running, each recorded as a failing reading: {}",
            refused.len(),
            if refused.len() == 1 { "" } else { "s" },
            refused.join("; ")
        );
    }
    Ok(())
}

/// Where a run leaves, under the store, one ticket per bar that regressed
/// two nights running, for the nightly lane to file (#2659).
pub(crate) const FAILED: &str = "bench-regressed";

/// The ticket a bar that regressed two nights running is filed as: its
/// title on the first line, a blank line, its body.
fn ticket_of(bar: &str, regressed: &str) -> String {
    format!(
        "bench: {bar} regressed beyond its tolerance two nights running\n\n## What happened\n\n{regressed} ({PROTOCOL} §12).\n\n## Acceptance criteria\n\n- [ ] The cause is named among those commits, and the bar's nightly reading holds again\n\n## Blocked by\n\nNone\n"
    )
}

/// A measured bar as its rendering and its reading name it.
struct Shown<'a> {
    id: &'a str,
    about: &'a str,
    held: Held,
    names: [&'a str; 2],
}

/// The reading of a bar that was not measured, with why, printed too.
fn unmeasured(id: &str, status: Status, why: &str, snapshot: &Snapshot) -> Result<Reading> {
    println!("bench {id}: {} {why}", status.as_str());
    let mut reading = Reading::stamped_over(INSTRUMENT, id, snapshot, Path::new("."))?;
    reading.status = status;
    reading.message = format!("{id}: not measured: {why}");
    Ok(reading)
}

/// What one measured bar prints: the standing and the knobs, a line per
/// arm, the paired delta with its verdict, and the load beside them all.
fn rendered(
    bar: &Shown<'_>,
    measured: &Measured,
    reps: usize,
    label: Label,
    load: Option<[f64; 3]>,
) -> String {
    let unit = measured.unit.suffix();
    let stat = |name: &str, s: &Stats| {
        format!(
            "  {name}: median {:.3} {unit}, IQR {:.3} (q1 {:.3}, q3 {:.3}, min {:.3}, max {:.3}, n {})\n",
            s.median, s.iqr, s.q1, s.q3, s.min, s.max, s.n
        )
    };
    format!(
        "bench {}: {}; {reps} reps (+{WARMUP} warm-up) per arm, order alternated; load {}\n{}{}  paired delta: median {:.3} {unit} ({:+.2}%), IQR {:.3} (q1 {:.3}, q3 {:.3}); {}\n  correctness {}; {}\n",
        bar.id,
        label.as_str(),
        load_text(load),
        stat(bar.names[0], &measured.a),
        stat(bar.names[1], &measured.b),
        measured.delta.median,
        measured.paired_pct,
        measured.delta.iqr,
        measured.delta.q1,
        measured.delta.q3,
        sign_of(&measured.delta),
        measured.correctness.as_str(),
        held_by(bar, measured),
    )
}

/// What the bar is held to, as its rendering and its reading say it.
fn held_by(bar: &Shown<'_>, measured: &Measured) -> String {
    let series = "a series, held to its tolerance where `[[bench]]` gives one (#2659)";
    match (bar.held, to_go(measured)) {
        (Held::Control, _) => format!("the bar is the A/A control of {PROTOCOL} §2"),
        (Held::Series, Some(to_go)) => format!(
            "{series}; {} is {to_go:.2}% to go to {}",
            bar.names[0], bar.names[1]
        ),
        (Held::Series, None) => format!(
            "{series}; the sign is not established, so no distance to go is stated ({PROTOCOL} §1.2)"
        ),
    }
}

/// How far the first arm's median stands behind the second's, as a
/// percentage of the second's: the paired delta where its sign is
/// established and it is the first arm's loss, a time more or a rate
/// less, nothing where the first arm is ahead, and none where the sign is
/// not established (§1.2) or the second arm measured nothing.
fn to_go(measured: &Measured) -> Option<f64> {
    (sign_of(&measured.delta) == RESOLVED && measured.paired_pct.is_finite())
        .then(|| (measured.paired_pct * measured.unit.worse()).max(0.0))
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
    bar: &Shown<'_>,
    measured: &Measured,
    (reps, label, note): (usize, Label, &str),
    load: Option<[f64; 3]>,
    snapshot: &Snapshot,
) -> Result<Reading> {
    let verdict = sign_of(&measured.delta);
    let mut reading = Reading::stamped_over(INSTRUMENT, bar.id, snapshot, Path::new("."))?;
    reading.status = bar.held.status(verdict);
    if bar.held == Held::Series
        && let Some(to_go) = to_go(measured)
    {
        reading
            .numbers
            .insert("to_go_pct".to_owned(), rounded(to_go).into());
    }
    reading.count("reps", u64::try_from(measured.delta.n)?)?;
    reading.count("warmup_discarded", u64::try_from(WARMUP)?)?;
    let unit = measured.unit.suffix();
    let numbers = [
        ("a_median", measured.a.median),
        ("a_iqr", measured.a.iqr),
        ("b_median", measured.b.median),
        ("b_iqr", measured.b.iqr),
        ("delta_median", measured.delta.median),
        ("delta_q1", measured.delta.q1),
        ("delta_q3", measured.delta.q3),
        ("delta_iqr", measured.delta.iqr),
    ];
    let numbers = (numbers.into_iter()).map(|(key, value)| (measured.unit.key(key), value));
    for (key, value) in numbers.chain([("paired_pct".to_owned(), measured.paired_pct)]) {
        if value.is_finite() {
            reading.numbers.insert(key, rounded(value).into());
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
        "{}: paired delta {:.3} {unit} ({:+.2}%) over {reps} reps, {verdict}; correctness {}; {}; {}{reservation}; load {} ({PROTOCOL} §4.3, §7)",
        bar.about,
        measured.delta.median,
        measured.paired_pct,
        measured.correctness.as_str(),
        held_by(bar, measured),
        label.as_str(),
        load_text(load),
    );
    Ok(reading)
}

#[cfg(test)]
mod tests {
    use super::{
        Correctness, Held, Measured, RESOLVED, SWAMPED, Stats, Status, Stream,
        refuse_a_hosted_runner, sign_of, stats, to_go,
    };

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

    /// A control fails on a resolved sign and passes on a swamped one; a
    /// series passes either way, and states how far its first arm stands
    /// behind its second only where the sign is established, nothing where
    /// it is ahead (#2351, §1.2).
    #[test]
    fn a_control_fails_on_a_resolved_sign_and_a_series_states_the_distance_to_go() {
        assert_eq!(Held::Control.status(RESOLVED), Status::Fail);
        assert_eq!(Held::Control.status(SWAMPED), Status::Pass);
        assert_eq!(Held::Series.status(RESOLVED), Status::Pass);
        let measured = |deltas: &[f64], paired_pct| Measured {
            unit: super::Unit::Ms,
            a: Stats::default(),
            b: Stats::default(),
            delta: stats(deltas),
            paired_pct,
            correctness: Correctness::Bytes(Stream::Body),
        };
        let behind = [4.0, 5.0, 6.0, 7.0];
        assert!(close(to_go(&measured(&behind, 250.0)).unwrap(), 250.0));
        let ahead = [-7.0, -6.0, -5.0, -4.0];
        assert!(close(to_go(&measured(&ahead, -12.5)).unwrap(), 0.0));
        let straddles = [-1.0, -1.0, 1.0, 1.0];
        assert_eq!(to_go(&measured(&straddles, 3.0)), None, "swamped");
        assert_eq!(to_go(&measured(&behind, f64::NAN)), None);
        let rate = |deltas: &[f64], paired_pct| Measured {
            unit: super::Unit::Rps,
            ..measured(deltas, paired_pct)
        };
        assert!(
            close(to_go(&rate(&ahead, -50.0)).unwrap(), 50.0),
            "fewer requests a second is behind"
        );
        assert!(close(to_go(&rate(&behind, 12.5)).unwrap(), 0.0));
        assert_eq!(Correctness::Bytes(Stream::Body).as_str(), "body-bytes");
    }

    /// A rate arm's reps are paired as a time's are, the delta in requests
    /// a second, `a - b`: 50 against 100 is a delta of -50 and 50% to go.
    #[test]
    fn a_rate_bar_pairs_its_requests_a_second_per_rep() {
        struct Rate(f64, std::path::PathBuf);
        impl super::Side for Rate {
            fn name(&self) -> &'static str {
                "rate"
            }
            fn program(&self) -> &std::path::Path {
                &self.1
            }
            fn unit(&self) -> super::Unit {
                super::Unit::Rps
            }
            fn once(&mut self) -> anyhow::Result<super::Sample> {
                self.0 += 0.001;
                Ok(super::Sample {
                    value: self.0,
                    wrote: b"x".to_vec(),
                })
            }
        }
        let exe = std::env::current_exe().unwrap();
        let mut sides: [Box<dyn super::Side>; 2] = [
            Box::new(Rate(50.0, exe.clone())),
            Box::new(Rate(100.0, exe)),
        ];
        let measured = super::measure("c8", &mut sides, 12).unwrap();
        assert_eq!(measured.unit, super::Unit::Rps);
        assert!((measured.a.median - 50.0).abs() < 0.1 && (measured.b.median - 100.0).abs() < 0.1);
        assert!(close(measured.delta.median, -50.0) && close(measured.paired_pct.round(), -50.0));
        assert!(close(to_go(&measured).unwrap().round(), 50.0));
        assert_eq!(measured.correctness, Correctness::Bytes(Stream::Stdout));
    }

    /// A GitHub-hosted runner is refused, and a machine of ours or one
    /// outside CI is not (#2477).
    #[test]
    fn a_github_hosted_runner_is_refused_and_no_other_machine_is() {
        let refused = refuse_a_hosted_runner(Some("github-hosted")).unwrap_err();
        assert!(
            format!("{refused}").contains("quiet self-hosted machine"),
            "{refused}"
        );
        assert!(refuse_a_hosted_runner(Some("self-hosted")).is_ok());
        assert!(refuse_a_hosted_runner(None).is_ok());
    }
}
