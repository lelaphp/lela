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
//! php-fpm behind a `FastCGI` client over the same workload
//! (`makala::bench`). Its reading states the measurement and the distance
//! still to go to its second arm, harness §3's "`FastCGI` ≥ php-fpm",
//! which holds nothing. A series with a `[[bench]]` tolerance is held by
//! [`series`]'s rule, beside a baseline the store keeps: one established
//! regression warns, two in a row fail the reading and the run; one
//! without a tolerance passes whatever it measured.
//!
//! **One pair, or a pair per program.** A bar's [`Arms`] are one pair, or
//! one pair per program of a set, each measured as a pair is and the
//! reading carrying each program's ratio and their geomean, whose sign
//! stands only where every program's does (§10's floor) and is `mixed`
//! where they point both ways: the seven kernels and
//! `Zend/bench.php`, which the G-bars measure ([`kernels`]).
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

pub(crate) mod kernels;
mod pinned;
pub(crate) mod row;
mod series;
pub(crate) mod spent;

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::ValueEnum;
use serde::Deserialize;

use crate::inputs;
use crate::key::Snapshot;
use crate::manifest::Tolerance;
use crate::reading::{Reading, Status, Store};
use spent::Spent;

/// The instrument's name, the reading's `instrument` field.
pub(crate) const INSTRUMENT: &str = "bench";

/// The protocol every number here is taken under.
pub(crate) const PROTOCOL: &str = "docs/perf/measurement-protocol.md";

/// The ini entries a measured php runs under after the recipe's: the two
/// the recipe sets for correctness at a cost to every request, at
/// php.ini-production's values (`makala::bench`, [`kernels`]).
pub(crate) const PRODUCTION: [&str; 2] = ["zend.assertions=-1", "opcache.protect_memory=0"];

/// §3: measured reps per arm. What `--reps` defaults to.
pub const REPS: usize = 12;

/// §3: reps run before the first measured one, and discarded.
const WARMUP: usize = 2;

/// §4.3: below this many paired deltas nothing resolves, whatever the
/// quartiles say. At one rep Q1 and Q3 are the sample itself, so any
/// nonzero delta cleared zero and one binary against a copy of itself came
/// back signed.
const MIN_REPS_TO_RESOLVE: usize = 4;

/// §2: how often an A/A row resolves on a null effect, as the second
/// engine measured it, one row in seventeen.
const AA_RESOLVES: f64 = 1.0 / 17.0;

/// §2: the chance at or below which a count of an A/A set's rows resolving
/// is the pattern that is the defect, and not evidence.
const PATTERN: f64 = 0.01;

/// The fewest of an A/A set's `n` rows whose resolving is §2's pattern:
/// the smallest count whose chance of being reached or passed, each row
/// resolving at [`AA_RESOLVES`], is at most [`PATTERN`], the binomial's
/// upper tail: 3 of 8. None where no count is that unlikely, a set
/// of one, whose one row is evidence alone.
#[allow(clippy::cast_precision_loss)]
fn aa_fails_at(n: usize) -> Option<usize> {
    let (p, q) = (AA_RESOLVES, 1.0 - AA_RESOLVES);
    // At count k, `each` is P(X = k - 1) and `below` becomes P(X < k).
    let (mut each, mut below) = (q.powi(i32::try_from(n).ok()?), 0.0);
    (1..=n).find(|&k| {
        below += each;
        each *= (n + 1 - k) as f64 / k as f64 * p / q;
        1.0 - below <= PATTERN
    })
}

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
    /// The label as the reading spells it: its flag's value in capitals.
    pub(crate) fn spelled(self) -> String {
        (self.to_possible_value()).map_or_else(String::new, |v| v.get_name().to_uppercase())
    }

    /// Whether the label owes a note (§8).
    fn needs_note(self) -> bool {
        matches!(self, Label::Provisional | Label::BaselineOfRecord)
    }
}

/// What a run may assume of the machine it measures on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, ValueEnum)]
pub enum Machine {
    /// Shared with other work: one seat taken, and the load recorded as
    /// it is found and not controlled (§7).
    #[default]
    Shared,
    /// Held for the bench while it runs: every seat taken
    /// ([`crate::seat`]), and each bar measured only once the 1-minute
    /// load is below [`QUIET`], waited for.
    Dedicated,
    /// A hosted runner's machine, the job's alone for as long as it runs
    /// and held as a dedicated one is, on a host whose other guests move
    /// every arm and show in no load average. Its numbers are offered as
    /// `smoke` or `provisional` and no more ([`Machine::offers`]), and each
    /// bar is measured between two runs of its A/A control
    /// ([`between_controls`]), which say what the machine did meanwhile
    /// (§7; ruled by Mitchell 2026-11-19).
    Hosted,
}

/// The 1-minute load below which a dedicated machine measures a bar.
const QUIET: f64 = 2.0;

/// How long a dedicated machine waits for the load to fall below
/// [`QUIET`] before a bar, and how often it looks.
const PATIENCE: Duration = Duration::from_mins(15);
const LOOK: Duration = Duration::from_secs(15);

/// §7's hardware and load where a bar's reps start: the machine as the
/// reading names it, the load averages, and on a dedicated machine what
/// it waited out first, how long and the highest 1-minute load it read.
#[derive(Debug, Clone, PartialEq)]
struct Conditions {
    hardware: String,
    load: Option<[f64; 3]>,
    waited: Option<(Duration, f64)>,
}

impl Conditions {
    /// The conditions as a rendering and a reading say them.
    fn text(&self) -> String {
        let waited = (self.waited).map_or_else(String::new, |(took, highest)| {
            format!(
                ", after {}s waiting for the 1-minute load to fall below {QUIET} from {highest:.2}",
                took.as_secs()
            )
        });
        format!(
            "load {}{waited}; on {}",
            load_text(self.load),
            self.hardware
        )
    }
}

impl Machine {
    /// Whether this machine's numbers may be offered under `label`: a
    /// hosted one's as `smoke` or `provisional` alone, since a reading of
    /// its control says how far the same build ran from itself there and
    /// nothing says it of the next run's machine (§7, §8).
    fn offers(self, label: Label) -> Result<()> {
        if self == Machine::Hosted && !matches!(label, Label::Smoke | Label::Provisional) {
            bail!(
                "--machine hosted offers its numbers as --label smoke or --label provisional and no more, \
                 and {} was asked: a hosted runner shares its host ({PROTOCOL} §7)",
                label.spelled().to_lowercase()
            );
        }
        Ok(())
    }

    /// The conditions a bar starts under on this machine, its load as
    /// `read` finds it: on a dedicated or hosted machine only below [`QUIET`],
    /// looked for again after each `sleep` of [`LOOK`] until [`PATIENCE`]
    /// has passed, and otherwise why the bar is not measured.
    fn settle(
        self,
        hardware: &str,
        mut read: impl FnMut() -> Option<[f64; 3]>,
        mut sleep: impl FnMut(Duration),
    ) -> Result<Conditions, String> {
        let hardware = hardware.to_owned();
        if self == Machine::Shared {
            let load = read();
            return Ok(Conditions {
                hardware,
                load,
                waited: None,
            });
        }
        let (mut took, mut highest) = (Duration::ZERO, None::<f64>);
        loop {
            let Some(load) = read() else {
                return Err(format!(
                    "the machine states no load average, so none is known to be below {QUIET}"
                ));
            };
            if load[0] < QUIET {
                let waited = highest.map(|highest| (took, highest));
                return Ok(Conditions {
                    hardware,
                    load: Some(load),
                    waited,
                });
            }
            highest = Some(highest.map_or(load[0], |h| h.max(load[0])));
            if took >= PATIENCE {
                return Err(format!(
                    "the 1-minute load stayed at {QUIET} or above for {}s, at most {:.2} and last {}, so the machine held for the bench measures nothing",
                    took.as_secs(),
                    highest.unwrap_or(load[0]),
                    load_text(Some(load))
                ));
            }
            sleep(LOOK);
            took += LOOK;
        }
    }
}

/// §7's hardware: the processor, its cores and the kernel.
fn hardware() -> String {
    let said = |program: &str, args: &[&str]| {
        let out = Command::new(program).args(args).output().ok()?;
        let text = String::from_utf8(out.stdout).ok()?;
        Some(text.trim().to_owned()).filter(|t| out.status.success() && !t.is_empty())
    };
    let cpu = std::fs::read_to_string("/proc/cpuinfo")
        .ok()
        .and_then(|info| {
            let named = info.lines().find(|line| line.starts_with("model name"))?;
            Some(named.split_once(':')?.1.trim().to_owned())
        });
    let cpu = cpu.or_else(|| said("sysctl", &["-n", "machdep.cpu.brand_string"]));
    let cores = std::thread::available_parallelism().map_or(1, std::num::NonZeroUsize::get);
    let kernel = said("uname", &["-sr"]);
    format!(
        "{}, {cores} cores, {}",
        cpu.as_deref().unwrap_or("a processor it does not name"),
        kernel.as_deref().unwrap_or("a kernel it does not name")
    )
}

/// One arm of a bar: the program a rep runs once, its arguments, what it
/// reads on stdin, and, which a bar's file leaves out, the environment it
/// runs in, the harness's own by default, and the files it runs beside its
/// program, which the run pins with it (§6): a php arm's workload.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct Arm {
    name: String,
    program: PathBuf,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    stdin: String,
    #[serde(skip)]
    env: Environment,
    #[serde(skip)]
    reads: Vec<PathBuf>,
    /// Whether a run states what its process retired over its life
    /// ([`Spent::run`]), read before it is reaped.
    #[serde(skip)]
    retired: bool,
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
        crate::read_toml(path).context("reading a bar")
    }

    /// The bar as the run measures it, two processes, an A/A control
    /// unless the file says it is a series.
    fn declared(self) -> Declared {
        let (a, b, series) = (self.a, self.b, self.series);
        let spawned = |arm: Arm| -> Result<Box<dyn Side>> {
            let program = resolve(&arm.program)?;
            Ok(Box::new(Spawned { arm, program }))
        };
        Declared {
            id: self.id,
            about: self.about,
            held: if series { Held::Series } else { Held::Control },
            start: Box::new(move || Ok(Ok(Arms::Pair([spawned(a)?, spawned(b)?])))),
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

/// Whether a ratio's sign stands, and which way it points: a row's, by
/// §4.3's quartiles, or a set's geomean, by its rows' (§10). Faster is the
/// first arm taking less time, or answering more requests a second.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Sign {
    /// The quartiles straddle zero, or a row of the set's do: published
    /// unsigned (§1.2), and the A/A control's bar for every row (§2).
    Swamped,
    Faster,
    Slower,
    /// A set whose rows all resolved, some each way: published unsigned
    /// with the count each way ([`Took::each_way`]), since a geomean near
    /// 1.00 over +20% and −17% would claim no change while hiding a
    /// regression.
    Mixed,
}

impl Sign {
    /// The verdict as the rendering, the reading and a row spell it.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Sign::Swamped => "NOISE-SWAMPED",
            Sign::Faster | Sign::Slower => "resolved",
            Sign::Mixed => "mixed",
        }
    }
}

/// §4.3 over a delta's reps and quartiles, as a reading keeps them: the
/// one rule a run and a row of its reading judge by (`row`). `<=`
/// and `>=` and not `<` and `>`, since a quartile exactly at zero still
/// straddles it; a delta above zero is the first arm slower in a cost
/// and faster in a rate ([`Unit::worse`]).
pub(crate) fn sign(n: usize, q1: f64, q3: f64, unit: Unit) -> Sign {
    if n < MIN_REPS_TO_RESOLVE || (q1 <= 0.0 && q3 >= 0.0) {
        Sign::Swamped
    } else if (q1 > 0.0) == (unit.worse() > 0.0) {
        Sign::Slower
    } else {
        Sign::Faster
    }
}

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

/// One run of one arm: what it measured in its arm's [`Unit`], what it
/// wrote, the bytes the two arms' runs of one rep are held equal by (§5),
/// and what a server arm's processes spent per request.
pub(crate) struct Sample {
    pub(crate) value: f64,
    pub(crate) wrote: Vec<u8>,
    pub(crate) spent: Spent,
    /// A process arm's peak memory in bytes, as [`spent::Peak::HERE`]
    /// names it, where it asked for what its process retired and the
    /// platform states it.
    pub(crate) peak: Option<u64>,
}

impl Sample {
    /// A run that states its value and what it wrote, and nothing it spent.
    pub(crate) fn of(value: f64, wrote: Vec<u8>) -> Sample {
        let (spent, peak) = (Spent::Unasked, None);
        Sample {
            value,
            wrote,
            spent,
            peak,
        }
    }
}

/// What a rep of an arm measures (§3): the milliseconds one run took, a
/// cost, the nanoseconds per call of a run making a known count of calls,
/// a cost, or the requests a second its clients were answered at,
/// a rate. The paired delta is taken in it, `a - b`, so a rate's
/// loss is a negative delta; [`Unit::worse`] turns each into a cost.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum Unit {
    Ms,
    Ns,
    Rps,
    /// Bytes, a cost: a process's peak footprint, or its traces' code per
    /// opline compiled (G4).
    Bytes,
}

impl Unit {
    /// The suffix of the reading's keys and of each figure printed.
    pub(crate) fn suffix(self) -> &'static str {
        match self {
            Unit::Ms => "ms",
            Unit::Ns => "ns",
            Unit::Rps => "rps",
            Unit::Bytes => "bytes",
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
            Unit::Ms | Unit::Ns | Unit::Bytes => 1.0,
            Unit::Rps => -1.0,
        }
    }

    /// The unit a reading's numbers were recorded in: the one whose
    /// suffix a figure is kept under, milliseconds where none is another's.
    pub(crate) fn of(reading: &Reading) -> Unit {
        let kept = |unit: Unit| (reading.numbers.keys()).any(|k| k.ends_with(&unit.key("")));
        [Unit::Rps, Unit::Ns, Unit::Bytes]
            .into_iter()
            .find(|unit| kept(*unit))
            .unwrap_or(Unit::Ms)
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
    /// What else the run pins of the arm: the files its program reads,
    /// none by default.
    fn reads(&self) -> &[PathBuf] {
        &[]
    }
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

/// Two arms as a rep runs them.
pub(crate) type Pair = [Box<dyn Side>; 2];

/// What a bar measures: one pair, or a pair per program of a set, each
/// row named by its program ([`kernels`]).
pub(crate) enum Arms {
    Pair(Pair),
    Set(Vec<(String, Pair)>),
}

/// A bar's arms, or why this machine cannot run them, which is the bar's
/// skip.
pub(crate) type Sides = Result<Arms, String>;

/// How a bar's reading is held.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Held {
    /// An A/A control: a pair passes while its paired delta has its sign
    /// swamped (§2, §4.3), and a set until its rows resolve in the count
    /// [`aa_fails_at`] makes §2's pattern.
    Control,
    /// A series: two arms that differ, its reading stating the measurement
    /// and the distance to go to the second arm, and held by [`series`]'s
    /// rule where the manifest gives it a tolerance.
    Series,
    /// A series held to a bound, its `[[bench]]` row's tolerance
    /// ([`Shown::tolerance`]), G1's (design §9.2), row by row and never by
    /// a set's geomean, as Mitchell ruled 2026-10-23: a fail where any row
    /// resolves slower (§4.3) with its ratio beyond one plus the tolerance,
    /// or with no finite ratio, and a pass otherwise, or where the bar has
    /// no `[[bench]]` row. A `NOISE-SWAMPED` row is never a
    /// regression (protocol §1.2) and holds nothing; the geomean, signed,
    /// `mixed` or unsigned, is published and holds nothing (§10), since it
    /// averages away a regression concentrated on one row.
    Bound,
}

impl Held {
    /// The reading's status for what a bar `took`: a control fails at
    /// [`Took::fails_at`] resolved rows, a bound where any row's first arm
    /// resolves behind its second by more than `tolerance` percent, in
    /// time or in rate ([`Measured::behind`]), or where it measured no
    /// row; a series is held to no bound.
    fn status(self, took: &Took, tolerance: Option<f64>) -> Status {
        let beyond = |pct| {
            took.rows.is_empty()
                || (took.rows.iter()).any(|(_, row)| row.behind().is_some_and(|b| b > pct))
        };
        let resolved: usize = took.each_way().iter().sum();
        match self {
            Held::Control if took.fails_at().is_some_and(|k| resolved >= k) => Status::Fail,
            Held::Bound if tolerance.is_some_and(beyond) => Status::Fail,
            Held::Control | Held::Series | Held::Bound => Status::Pass,
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

impl From<Arm> for Spawned {
    /// An arm whose program is a path already.
    fn from(arm: Arm) -> Spawned {
        let program = arm.program.clone();
        Spawned { arm, program }
    }
}

/// What an arm's process runs in: the harness's environment, cleared
/// first where `clear` says, and `set` over it. The default is the
/// harness's own; a php arm's is the recipe's (`kernels`).
#[derive(Debug, Clone, Default)]
pub(crate) struct Environment {
    clear: bool,
    set: BTreeMap<String, String>,
}

impl Spawned {
    /// One run of the arm, and what it wrote on its second stream.
    fn ran(&self) -> Result<(Sample, Vec<u8>)> {
        ran(&self.arm, &self.program)
    }
}

impl Side for Spawned {
    fn name(&self) -> &str {
        &self.arm.name
    }

    fn program(&self) -> &Path {
        &self.program
    }

    fn reads(&self) -> &[PathBuf] {
        &self.arm.reads
    }

    fn once(&mut self) -> Result<Sample> {
        once(&self.arm, &self.program)
    }
}

/// What one bar measured: each row, its program's name where the bar is
/// a set, and what it measured.
struct Took {
    rows: Vec<(Option<String>, Measured)>,
}

impl Took {
    /// Whether the bar is a set, its rows named by their programs, and not
    /// one pair.
    fn set(&self) -> bool {
        self.rows.iter().any(|(program, _)| program.is_some())
    }

    /// How many rows resolved faster, and how many slower.
    fn each_way(&self) -> [usize; 2] {
        let signs: Vec<Sign> = self.rows.iter().map(|(_, row)| row.sign()).collect();
        [Sign::Faster, Sign::Slower].map(|way| signs.iter().filter(|s| **s == way).count())
    }

    /// How many resolved rows fail an A/A control over them: a pair's one,
    /// and a set's [`aa_fails_at`].
    fn fails_at(&self) -> Option<usize> {
        if self.set() {
            aa_fails_at(self.rows.len())
        } else {
            Some(1)
        }
    }

    /// The bar's ratio: the geometric mean of its rows' ratios, one row's
    /// its own.
    fn ratio(&self) -> f64 {
        let logs: f64 = self.rows.iter().map(|(_, row)| row.ratio().ln()).sum();
        #[allow(clippy::cast_precision_loss)]
        let rows = self.rows.len() as f64;
        (logs / rows).exp()
    }

    /// The ratio's sign: one row's its own, and a set's established only
    /// where every row's is, the floor protocol §10 holds a geomean over
    /// workloads to, and signed only where every row points one way.
    fn sign(&self) -> Sign {
        match self.each_way() {
            [faster, slower] if faster + slower < self.rows.len().max(1) => Sign::Swamped,
            [_, 0] => Sign::Faster,
            [0, _] => Sign::Slower,
            _ => Sign::Mixed,
        }
    }
}

/// What one pair measured, in its unit: the two arms, their paired delta,
/// the delta as a percentage of the candidate arm's median, how the bytes
/// compared, and what each arm spent per request.
struct Measured {
    unit: Unit,
    a: Stats,
    b: Stats,
    delta: Stats,
    paired_pct: f64,
    correctness: Correctness,
    spent: [Spent; 2],
}

impl Measured {
    /// The first arm over the second, as the paired delta states it:
    /// one plus [`Measured::paired_pct`] as a fraction, not the ratio of
    /// the medians, which carries no verdict (§2, §4.2).
    fn ratio(&self) -> f64 {
        1.0 + self.paired_pct / 100.0
    }

    /// The paired delta's sign (§4.3).
    fn sign(&self) -> Sign {
        sign(self.delta.n, self.delta.q1, self.delta.q3, self.unit)
    }

    /// How far this row's first arm stands behind its second, where it
    /// resolved slower: what a bound holds each row to. A row
    /// resolved slower whose ratio is no finite number, its second arm
    /// measuring nothing, is behind by more than any tolerance.
    fn behind(&self) -> Option<f64> {
        (self.sign() == Sign::Slower)
            .then(|| lost(self.ratio(), self.unit).unwrap_or(f64::INFINITY))
    }
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
    ran(arm, program).map(|(sample, _)| sample)
}

/// [`once`], and what the run wrote on its second stream.
fn ran(arm: &Arm, program: &Path) -> Result<(Sample, Vec<u8>)> {
    let mut command = Command::new(program);
    if arm.env.clear {
        command.env_clear();
    }
    if arm.retired {
        spent::spawning();
    }
    let start = Instant::now();
    let mut child = command
        .envs(&arm.env.set)
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
    let (done, value, (spent, peak)) = if arm.retired {
        let (stdout, stderr) = (drained(child.stdout.take()), drained(child.stderr.take()));
        let (retired, peak) = spent::exited(child.id());
        let value = start.elapsed().as_secs_f64() * 1000.0;
        let status = child.wait();
        let status = status.with_context(|| format!("waiting for the arm {}", arm.name))?;
        let read = |read: std::thread::JoinHandle<Vec<u8>>| read.join().ok();
        let streams = read(stdout).zip(read(stderr));
        let (stdout, stderr) = streams.context("a thread reading the arm's streams panicked")?;
        let done = std::process::Output {
            status,
            stdout,
            stderr,
        };
        (done, value, (Spent::run(retired), peak))
    } else {
        let done = child.wait_with_output();
        let done = done.with_context(|| format!("waiting for the arm {}", arm.name))?;
        (
            done,
            start.elapsed().as_secs_f64() * 1000.0,
            (Spent::Unasked, None),
        )
    };
    if !done.status.success() {
        bail!(
            "the arm {} exited {}: {}",
            arm.name,
            done.status,
            String::from_utf8_lossy(&done.stderr).trim()
        );
    }
    let sample = Sample {
        spent,
        peak,
        ..Sample::of(value, done.stdout)
    };
    Ok((sample, done.stderr))
}

/// What a child's stream writes, read to its end on a thread of its own,
/// so a child that fills one pipe while the other is read never stalls.
fn drained(pipe: Option<impl std::io::Read + Send + 'static>) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut read = Vec::new();
        if let Some(mut pipe) = pipe {
            pipe.read_to_end(&mut read).ok();
        }
        read
    })
}

/// One bar measured: `reps` paired reps after [`WARMUP`] discarded ones,
/// the order alternated so a penalty on whichever process runs second
/// cancels across reps instead of being charged to one arm; the arms' bytes
/// held equal inside every rep; the programs, and what each reads, pinned
/// at both ends. A byte disagreement re-reads the pin first, since a
/// program that moved underneath the run explains one (§6).
fn measure(id: &str, [a, b]: &mut [Box<dyn Side>; 2], reps: usize) -> Result<Measured> {
    let programs: Vec<PathBuf> = [&*a, &*b]
        .iter()
        .flat_map(|side| {
            std::iter::once(side.program()).chain(side.reads().iter().map(PathBuf::as_path))
        })
        .map(Path::to_path_buf)
        .collect();
    let before = programs
        .iter()
        .map(|path| Pin::take(path))
        .collect::<Result<Vec<_>>>()?;
    let (mut kept, mut vacuous) = (Vec::new(), false);
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
            kept.push([sa, sb]);
        }
    }
    holds(&programs, &before)?;
    let (unit, stream) = (a.unit(), a.stream());
    let of = |value: fn(&[Sample; 2]) -> f64| stats(&kept.iter().map(value).collect::<Vec<_>>());
    let (a, b) = (of(|[a, _]| a.value), of(|[_, b]| b.value));
    let delta = of(|[a, b]| a.value - b.value);
    let spent =
        |arm: usize| Spent::median(&kept.iter().map(|rep| rep[arm].spent).collect::<Vec<_>>());
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
        spent: [spent(0), spent(1)],
    })
}

/// A bar's arms measured, pair by pair: what they took and the two arms'
/// names.
fn measure_each(id: &str, arms: Arms, reps: usize) -> Result<(Took, [String; 2])> {
    let mut pairs = match arms {
        Arms::Pair(pair) => vec![(None, pair)],
        Arms::Set(set) => set.into_iter().map(|(p, pair)| (Some(p), pair)).collect(),
    };
    let mut rows = Vec::new();
    for (program, pair) in &mut pairs {
        let row = (program.as_ref()).map_or_else(|| id.to_owned(), |p| format!("{id}/{p}"));
        rows.push((program.clone(), measure(&row, pair, reps)?));
    }
    let names = (pairs.first()).map(|(_, [a, b])| [a.name(), b.name()].map(str::to_owned));
    Ok((Took { rows }, names.unwrap_or_default()))
}

/// §6's second end: each program as it is now against the pin taken at the
/// start, and a refusal naming the one that moved.
fn holds(programs: &[PathBuf], before: &[Pin]) -> Result<()> {
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
/// php-fpm and against itself over each bench workload
/// (`makala::bench`). The other perf bars of harness-design.md §3 join them
/// as the engine reaches what they measure.
fn declared(root: &Path) -> Result<Vec<Declared>> {
    const WRAPPER: &str = "wrapper";
    const BARE: &str = "wrapper-bare";
    const DIRECT: &str = "direct";
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
        ..Arm::default()
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
    bars.extend(kernels::oracle(root));
    bars.extend(kernels::compiled(root));
    bars.extend(kernels::door(root));
    bars.extend(kernels::stepping(root));
    bars.extend(kernels::interrupt(root));
    bars.extend(kernels::runtime(root));
    bars.extend(kernels::g4(root));
    bars.extend(
        [
            ("jit-wrapper", [WRAPPER, DIRECT], Held::Series),
            ("jit-wrapper-aa", [WRAPPER, WRAPPER], Held::Control),
            ("jit-wrapper-unwind", [WRAPPER, BARE], Held::Series),
            ("jit-wrapper-frame", [BARE, DIRECT], Held::Series),
            ("jit-wrapper-bare-aa", [BARE, BARE], Held::Control),
        ]
        .map(|(id, ways, held)| wrapper(root, id, ways, held)),
    );
    Ok(bars)
}

/// `php_jit`'s bars: `CALLS` calls of its test kernel, each
/// arm's way and each through a function pointer: `wrapper`, the kernel's
/// `extern "C"` wrapper with its `catch_unwind` and `Status`;
/// `wrapper-bare`, the bench's same frame with no `catch_unwind`; or
/// `direct`, the kernel itself. `wrapper` against `direct` is the whole
/// wrapper, against `wrapper-bare` what `catch_unwind` costs, and
/// `wrapper-bare` against `direct` what the frame costs, each a series;
/// an arm against itself is an A/A control. A run is read in nanoseconds
/// per call, its time over `CALLS`.
fn wrapper(root: &Path, id: &str, [a, b]: [&'static str; 2], held: Held) -> Declared {
    const CALLS: u32 = 200_000_000;
    let root = root.to_path_buf();
    Declared {
        id: id.to_owned(),
        about: format!("{CALLS} calls of php_jit's test kernel through {a}, against {b}"),
        held,
        start: Box::new(move || {
            let program = crate::oracle::profile::released(
                &root,
                ("php_jit", "php_jit_bench"),
                Some("bench"),
                (None, None),
            )?;
            let arm = |name: String, way: &str| -> Box<dyn Side> {
                let args = vec![way.to_owned(), CALLS.to_string()];
                let arm = Arm {
                    name,
                    program: program.clone(),
                    args,
                    ..Arm::default()
                };
                let side = Spawned::from(arm);
                let count = f64::from(CALLS);
                Box::new(Counted { side, count })
            };
            Ok(Ok(Arms::Pair([arm(a.to_owned(), a), arm(second(a, b), b)])))
        }),
    }
}

/// The name of a bar's second arm, `b`: apart from the first's where the
/// bar is an A/A control, whose two arms are one.
pub(crate) fn second(a: &str, b: &str) -> String {
    format!("{b}{}", if a == b { "-b" } else { "" })
}

/// A process arm that makes a known count of calls or executes a known
/// count of oplines, each run, which [`Spawned`] reads in milliseconds,
/// read in nanoseconds per call or per opline, and what its process
/// retired, where it asks, per opline.
struct Counted {
    side: Spawned,
    count: f64,
}

impl Side for Counted {
    fn name(&self) -> &str {
        self.side.name()
    }

    fn program(&self) -> &Path {
        self.side.program()
    }

    fn reads(&self) -> &[PathBuf] {
        self.side.reads()
    }

    fn once(&mut self) -> Result<Sample> {
        let ran = self.side.once()?;
        let value = ran.value * 1e6 / self.count;
        let spent = ran.spent.per_opline(self.count);
        Ok(Sample {
            value,
            spent,
            ..ran
        })
    }

    fn unit(&self) -> Unit {
        Unit::Ns
    }
}

/// The variable GitHub sets on every runner: `github-hosted` on its own
/// fleet's, `self-hosted` on any other, another provider's hosted ones
/// among them.
pub(crate) const RUNNER_ENVIRONMENT: &str = "RUNNER_ENVIRONMENT";

/// A GitHub-hosted runner is a shared VM whose neighbours move every arm
/// (protocol §7), and a reading taken there as a shared or a dedicated
/// machine's would be noise a person could mistake for a measurement. So
/// the command refuses one asked as anything but [`Machine::Hosted`]
/// before it measures, whatever workflow asked it to, and writes nothing.
/// The library entry is not held to it: the negative control under
/// `xtask/tests/bench.rs` tests the rig, and runs wherever `xtask`'s tests
/// run.
pub(crate) fn refuse_a_hosted_runner(environment: Option<&str>, machine: Machine) -> Result<()> {
    if environment == Some("github-hosted") && machine != Machine::Hosted {
        bail!(
            "bench measures on a GitHub-hosted runner ({RUNNER_ENVIRONMENT}=github-hosted) as \
             --machine hosted alone: a shared VM's numbers are noise a person could read as a \
             measurement, so they are offered as provisional, each bar between two runs of its \
             A/A control"
        );
    }
    Ok(())
}

/// The order a hosted machine measures `bars` in: each bar that has an
/// A/A control, the declared control named `<bar>-aa`, between two runs of
/// it, and every other bar where it stood. A control measured around its
/// bar is not measured again on its own. `before` and `after` are the
/// declared set twice more, which a control's two runs are taken from,
/// since a bar's arms start once.
fn between_controls(
    bars: Vec<Declared>,
    [mut before, mut after]: [Vec<Declared>; 2],
) -> Vec<Declared> {
    fn taken(from: &mut Vec<Declared>, id: &str, turn: &str, bar: &str) -> Option<Declared> {
        let at = (from.iter()).position(|c| c.id == id && c.held == Held::Control)?;
        let mut control = from.swap_remove(at);
        control.about = format!("{}; {turn} {bar}, on a hosted machine", control.about);
        Some(control)
    }
    let control_of = |bar: &Declared| format!("{}-aa", bar.id);
    let stands =
        |set: &[Declared], id: &str| (set.iter()).any(|c| c.id == id && c.held == Held::Control);
    let around: Vec<String> = (bars.iter())
        .filter(|bar| bar.held != Held::Control)
        .map(control_of)
        .filter(|id| stands(&before, id) && stands(&after, id))
        .collect();
    let mut ordered = Vec::new();
    for bar in bars {
        let id = control_of(&bar);
        if bar.held == Held::Control {
            if !around.contains(&bar.id) {
                ordered.push(bar);
            }
        } else if around.contains(&id) {
            ordered.extend(taken(&mut before, &id, "before", &bar.id));
            let last = taken(&mut after, &id, "after", &bar.id);
            ordered.push(bar);
            ordered.extend(last);
        } else {
            ordered.push(bar);
        }
    }
    ordered
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
/// otherwise. On a dedicated or a hosted `machine` each bar waits for a
/// quiet load, and on a hosted one it is measured between two runs of its
/// A/A control.
pub fn run(
    root: &Path,
    store: &Store,
    bar: Option<&str>,
    from: Option<&Path>,
    (reps, machine): (usize, Machine),
    (label, note): (Label, &str),
    log: Option<&Path>,
) -> Result<()> {
    machine.offers(label)?;
    if label.needs_note() && note.trim().is_empty() {
        bail!(
            "--label {} owes a written reservation; a standing with none is a word and not a label ({PROTOCOL} §8)",
            label.spelled().to_lowercase()
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
    if machine == Machine::Hosted && from.is_none() {
        bars = between_controls(bars, [declared(root)?, declared(root)?]);
    }
    let snapshot = Snapshot::disk(root)?;
    let tolerances = crate::manifest::Manifest::load(root)?.tolerances;
    let log = match log {
        None => store.history(INSTRUMENT)?.readings,
        Some(path) => {
            let mut log = crate::reading::Log::handed(&crate::read(path)?).readings;
            log.retain(|reading| reading.instrument == INSTRUMENT);
            log
        }
    };
    let hardware = hardware();
    let mut settle = || machine.settle(&hardware, load_average, std::thread::sleep);
    measure_bars(
        store,
        &snapshot,
        bars,
        &tolerances,
        &log,
        (reps, &mut settle),
        (label, note),
    )
}

/// The conditions [`measure_bars`] starts each bar under, or why it is
/// not measured there ([`Machine::settle`]).
type Settle<'a> = dyn FnMut() -> Result<Conditions, String> + 'a;

/// [`run`]'s measurement of `bars`, once read: each measured, held to its
/// row of `tolerances` and its earlier nights in `log`, rendered and
/// recorded, a regressed series's ticket left for the lane. The bars and
/// the machine's load are handed in, so a test feeds the arms' timings
/// and the load from a stub.
fn measure_bars(
    store: &Store,
    snapshot: &Snapshot,
    bars: Vec<Declared>,
    tolerances: &[Tolerance],
    log: &[Reading],
    (reps, settle): (usize, &mut Settle<'_>),
    (label, note): (Label, &str),
) -> Result<()> {
    let failed = store.root().join(FAILED);
    std::fs::remove_dir_all(&failed).ok();
    let (mut readings, mut refused) = (Vec::new(), Vec::new());
    for declared in bars {
        let (id, held, start) = (declared.id, declared.held, declared.start);
        // A bar whose arms this machine cannot run is a skip, and one whose
        // run refused (§1) is a failing reading that names why; either way
        // the next bar runs, and a refusal is the command's error once all
        // have run, so a refused run never reads as a measurement. A
        // machine held for the bench and too loud to measure on is a skip
        // that says so.
        let measured =
            start().and_then(
                |sides| match sides.and_then(|arms| settle().map(|c| (arms, c))) {
                    Err(why) => Ok(Err(why)),
                    Ok((arms, conditions)) => {
                        let (took, names) = measure_each(&id, arms, reps)?;
                        Ok(Ok((took, names, conditions)))
                    }
                },
            );
        let (measured, names, conditions) = match measured {
            Ok(Ok(measured)) => measured,
            Ok(Err(why)) => {
                readings.push(unmeasured(&id, Status::Skip, &why, snapshot)?);
                continue;
            }
            Err(e) => {
                let why = format!("{e:#}");
                readings.push(unmeasured(&id, Status::Fail, &why, snapshot)?);
                refused.push(format!("{id}: {why}"));
                continue;
            }
        };
        let row = tolerances.iter().find(|t| t.bar == id);
        let bar = Shown {
            id: &id,
            about: &declared.about,
            held,
            names: [&names[0], &names[1]],
            row,
        };
        print!("{}", rendered(&bar, &measured, reps, label, &conditions));
        let mut reading = recorded(&bar, &measured, (reps, label, note), &conditions, snapshot)?;
        let held = (row.filter(|_| held == Held::Series))
            .map(|tolerance| series::hold(tolerance, log, &mut reading));
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
            crate::reading::plural(refused.len()),
            refused.join("; ")
        );
    }
    Ok(())
}

/// Where a run leaves, under the store, one ticket per bar that regressed
/// two nights running, for the nightly lane to file.
pub(crate) const FAILED: &str = "bench-regressed";

/// The ticket a bar that regressed two nights running is filed as: its
/// title on the first line, a blank line, its body.
fn ticket_of(bar: &str, regressed: &str) -> String {
    format!(
        "bench: {bar} regressed beyond its tolerance two nights running\n\n## What happened\n\n{regressed} ({PROTOCOL} §12).\n\n## Acceptance criteria\n\n- [ ] The cause is named among those commits, and the bar's nightly reading holds again\n\n## Blocked by\n\nNone\n"
    )
}

/// A measured bar as its rendering and its reading name it, with its
/// `[[bench]]` row where the manifest carries one.
struct Shown<'a> {
    id: &'a str,
    about: &'a str,
    held: Held,
    names: [&'a str; 2],
    row: Option<&'a Tolerance>,
}

impl Shown<'_> {
    /// The tolerance a bound bar is held to, its row's, the one place it
    /// is written (protocol §12.3); none for another bar, and for a
    /// bound without a row, a series held to none.
    fn tolerance(&self) -> Option<f64> {
        Some(self.row.filter(|_| self.held == Held::Bound)?.pct)
    }
}

/// The reading of a bar that was not measured, with why, printed too.
fn unmeasured(id: &str, status: Status, why: &str, snapshot: &Snapshot) -> Result<Reading> {
    println!("bench {id}: {} {why}", status.as_str());
    let mut reading = Reading::stamped_over(INSTRUMENT, id, snapshot, Path::new("."))?;
    reading.status = status;
    reading.message = format!("{id}: not measured: {why}");
    Ok(reading)
}

/// What one measured bar prints: the standing and the knobs, for each row
/// a line per arm and the paired delta with its verdict, a set's geomean,
/// and the load beside them all.
fn rendered(
    bar: &Shown<'_>,
    took: &Took,
    reps: usize,
    label: Label,
    conditions: &Conditions,
) -> String {
    let rows: Vec<String> = (took.rows.iter()).map(|(program, row)| {
        let unit = row.unit.suffix();
        let stat = |name: &str, s: &Stats, spent: &Spent| {
            let spent = (spent.text()).map_or_else(String::new, |t| format!("; {t}"));
            format!(
                "  {name}: median {:.3} {unit}, IQR {:.3} (q1 {:.3}, q3 {:.3}, min {:.3}, max {:.3}, n {}){spent}\n",
                s.median, s.iqr, s.q1, s.q3, s.min, s.max, s.n
            )
        };
        let program = (program.as_ref()).map_or_else(String::new, |p| format!("  {p}: ratio {:.4}\n", row.ratio()));
        let [a, b] = [(0, &row.a), (1, &row.b)].map(|(i, s)| stat(bar.names[i], s, &row.spent[i]));
        format!("{program}{a}{b}  {}\n", delta_text(row))
    }).collect();
    format!(
        "bench {}: {}; {reps} reps (+{WARMUP} warm-up) per arm, order alternated; {}\n{}  {}{}\n",
        bar.id,
        label.spelled(),
        conditions.text(),
        rows.concat(),
        geomean(took),
        held_by(bar, took),
    )
}

/// A row's paired delta, its verdict and how its bytes compared, as the
/// rendering and the reading say them.
fn delta_text(row: &Measured) -> String {
    let (d, unit) = (&row.delta, row.unit.suffix());
    let (verdict, correctness) = (row.sign().as_str(), row.correctness.as_str());
    format!(
        "paired delta: median {:.3} {unit} ({:+.2}%), IQR {:.3} (q1 {:.3}, q3 {:.3}); {verdict}; correctness {correctness}",
        d.median, row.paired_pct, d.iqr, d.q1, d.q3
    )
}

/// What a set's rendering and reading say of its rows taken together: the
/// geomean, its verdict and how many rows resolved each way; nothing for
/// a pair.
fn geomean(took: &Took) -> String {
    if !took.set() {
        return String::new();
    }
    let [faster, slower] = took.each_way();
    let each: Vec<String> = (took.rows.iter())
        .filter_map(|(program, row)| Some(format!("{} {:.4}", program.as_ref()?, row.ratio())))
        .collect();
    format!(
        "geomean ratio {:.4} over {} programs ({}), {}, {} of them resolved, {faster} faster and {slower} slower ({PROTOCOL} §10); ",
        took.ratio(),
        took.rows.len(),
        each.join(", "),
        took.sign().as_str(),
        faster + slower,
    )
}

/// What the bar is held to, as its rendering and its reading say it.
fn held_by(bar: &Shown<'_>, took: &Took) -> String {
    match bar.held {
        Held::Control if !took.set() => format!("the bar is the A/A control of {PROTOCOL} §2"),
        Held::Control => {
            let (rows, rate, chance) = (took.rows.len(), 1.0 / AA_RESOLVES, PATTERN * 100.0);
            let fails = (took.fails_at()).map_or_else(
                || "at no count of them, none's chance".to_owned(),
                |k| format!("where {k} of them resolve, the fewest whose chance"),
            );
            format!(
                "the bar is the A/A control of {PROTOCOL} §2 over {rows} rows, failing {fails} at 1 row in {rate:.0} is at most {chance}%"
            )
        }
        Held::Series => format!(
            "a series, held to its tolerance where `[[bench]]` gives one; {}",
            to_go(took).map_or_else(
                || format!("the sign is not established, so no distance to go is stated ({PROTOCOL} §1.2)"),
                |to_go| format!("{} is {to_go:.2}% to go to {}", bar.names[0], bar.names[1]),
            )
        ),
        Held::Bound => bar.tolerance().map_or_else(
            || format!("a bound with no `[[bench]]` row, so a series held to none ({PROTOCOL} §12.3)"),
            |pct| format!("a bound, {} held on each row to at most {pct}% behind {}, its `[[bench]]` tolerance, and the geomean to nothing ({PROTOCOL} §10, §12.3)", bar.names[0], bar.names[1]),
        ),
    }
}

/// How far the first arm stands behind the second, as a percentage of
/// the second: the bar's ratio where its sign is established and it is
/// the first arm's loss, a time more or a rate less, nothing where the
/// first arm is ahead, and none where the sign is not established (§1.2)
/// or the second arm measured nothing.
fn to_go(took: &Took) -> Option<f64> {
    // Every arm of a bar measures in one unit, the first row's.
    let (_, first) = took.rows.first()?;
    let signed = matches!(took.sign(), Sign::Faster | Sign::Slower);
    signed.then(|| lost(took.ratio(), first.unit)).flatten()
}

/// The first arm's loss at `ratio` as a percentage of the second, a time
/// more or a rate less, nothing where it is ahead, and none where the
/// ratio is no finite number.
fn lost(ratio: f64, unit: Unit) -> Option<f64> {
    ratio
        .is_finite()
        .then(|| ((ratio - 1.0) * 100.0 * unit.worse()).max(0.0))
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

/// One row's numbers: its two arms, the paired delta under the unit's
/// keys, what each arm spent and the delta as a share, and, where the row
/// is a set's, its ratio, each key after its program's name, `fib_`.
fn row_numbers(program: Option<&str>, row: &Measured) -> Vec<(String, f64)> {
    let prefix = program.map_or_else(String::new, |p| format!("{p}_"));
    let numbers = [
        ("a_median", row.a.median),
        ("a_iqr", row.a.iqr),
        ("b_median", row.b.median),
        ("b_iqr", row.b.iqr),
        ("delta_median", row.delta.median),
        ("delta_q1", row.delta.q1),
        ("delta_q3", row.delta.q3),
        ("delta_iqr", row.delta.iqr),
    ];
    let numbers = (numbers.into_iter()).map(|(key, value)| (row.unit.key(key), value));
    let [a, b] = row.spent;
    let delta = a.instructions().zip(b.instructions());
    let delta = delta.map(|(a, b)| ("delta_instructions".to_owned(), a - b));
    let spent = (a.numbers("a").into_iter())
        .chain(b.numbers("b"))
        .chain(delta);
    let ratio = program.map(|_| ("ratio".to_owned(), row.ratio()));
    (numbers.chain(spent))
        .chain([("paired_pct".to_owned(), row.paired_pct)])
        .chain(ratio)
        .map(|(key, value)| (format!("{prefix}{key}"), value))
        .collect()
}

/// One measured bar as one reading: `pass` while the A/A control holds,
/// `fail` when a row's sign resolved, the measurement in `numbers` with the
/// load the protocol requires beside it, and the standing in the message.
/// A set's rows are kept under their programs' names, `fib_ratio`, beside
/// `geomean_ratio`. Stamped over the workspace, a bar having no sources of
/// its own; `done` asks no instrument for a bench reading, which is the
/// other half of the exit rule.
fn recorded(
    bar: &Shown<'_>,
    took: &Took,
    (reps, label, note): (usize, Label, &str),
    conditions: &Conditions,
    snapshot: &Snapshot,
) -> Result<Reading> {
    let mut reading = Reading::stamped_over(INSTRUMENT, bar.id, snapshot, Path::new("."))?;
    // A measured bar's arms of the engine's own are built in the bench
    // lane's profile ([`crate::oracle::profile::released`]).
    reading.profile = Some(crate::oracle::Profile::Release);
    reading.status = bar.held.status(took, bar.tolerance());
    if let Some(pct) = bar.tolerance() {
        reading
            .numbers
            .insert("tolerance_pct".to_owned(), pct.into());
    }
    if bar.held == Held::Series
        && let Some(to_go) = to_go(took)
    {
        reading
            .numbers
            .insert("to_go_pct".to_owned(), rounded(to_go).into());
    }
    let n = took.rows.first().map_or(0, |(_, row)| row.delta.n);
    reading.count("reps", u64::try_from(n)?)?;
    reading.count("warmup_discarded", u64::try_from(WARMUP)?)?;
    let mut numbers = Vec::new();
    for (program, row) in &took.rows {
        numbers.extend(row_numbers(program.as_deref(), row));
    }
    if took.set() {
        numbers.push(("geomean_ratio".to_owned(), took.ratio()));
        let [faster, slower] = took.each_way();
        for (key, rows) in [
            ("resolved", faster + slower),
            ("faster", faster),
            ("slower", slower),
        ] {
            reading.count(key, u64::try_from(rows)?)?;
        }
    }
    for (key, value) in numbers {
        if value.is_finite() {
            reading.numbers.insert(key, rounded(value).into());
        }
    }
    if let Some(l) = conditions.load {
        for (key, value) in [("load_1", l[0]), ("load_5", l[1]), ("load_15", l[2])] {
            reading.numbers.insert((*key).to_owned(), value.into());
        }
    }
    if let Some((took, highest)) = conditions.waited {
        reading.count("waited_s", took.as_secs())?;
        reading
            .numbers
            .insert("waited_load_1".to_owned(), highest.into());
    }
    let reservation = if note.trim().is_empty() {
        String::new()
    } else {
        format!(" ({})", note.trim())
    };
    let measured = match took.rows.as_slice() {
        [(None, row)] => {
            let spent: String = (bar.names.iter().zip(row.spent))
                .filter_map(|(name, spent)| Some(format!("; {name} {}", spent.text()?)))
                .collect();
            format!(
                "{reps} reps, {}; {}{spent}",
                delta_text(row),
                held_by(bar, took)
            )
        }
        _ => format!(
            "{}{reps} reps a program; {}",
            geomean(took),
            held_by(bar, took)
        ),
    };
    reading.message = format!(
        "{}: {measured}; {}{reservation}; {} ({PROTOCOL} §4.3, §7)",
        bar.about,
        label.spelled(),
        conditions.text(),
    );
    Ok(reading)
}

#[cfg(test)]
mod tests {
    use super::Unit::{Ms, Rps};
    use super::{
        Correctness, Held, Measured, Sign, Stats, Status, Stream, Took, Unit,
        refuse_a_hosted_runner, stats, to_go,
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
    /// quartile exactly at zero straddles it; a delta above zero is the
    /// first arm slower in a time and faster in a rate.
    #[test]
    fn the_sign_is_established_only_above_the_floor_and_clear_of_zero() {
        let of = |deltas: &[f64], unit| {
            let s = stats(deltas);
            super::sign(s.n, s.q1, s.q3, unit)
        };
        assert_eq!(of(&[10.0, 10.0, 10.0], Ms), Sign::Swamped);
        assert_eq!(of(&[10.0, 10.0, 10.0, 10.0], Ms), Sign::Slower);
        assert_eq!(of(&[10.0, 10.0, 10.0, 10.0], Rps), Sign::Faster);
        assert_eq!(of(&[0.0, 0.0, 0.0, 3.0], Ms), Sign::Swamped);
        assert_eq!(of(&[-3.0, 0.0, 0.0, 0.0], Ms), Sign::Swamped);
        assert_eq!(of(&[-1.0, -1.0, 1.0, 1.0], Ms), Sign::Swamped);
        assert_eq!(of(&[-4.0, -3.0, -2.0, -1.0], Ms), Sign::Faster);
    }

    /// One pair's row, its delta over `deltas` and its share `paired_pct`.
    fn row(unit: Unit, deltas: &[f64], paired_pct: f64) -> Measured {
        Measured {
            unit,
            a: Stats::default(),
            b: Stats::default(),
            delta: stats(deltas),
            paired_pct,
            correctness: Correctness::Bytes(Stream::Body),
            spent: [super::Spent::Unasked; 2],
        }
    }

    /// A bar of `rows`, each named `p<n>` where `set`, one pair where not.
    fn took(rows: Vec<Measured>, set: bool) -> Took {
        let named = |i: usize| set.then(|| format!("p{i}"));
        Took {
            rows: rows
                .into_iter()
                .enumerate()
                .map(|(i, r)| (named(i), r))
                .collect(),
        }
    }

    const BEHIND: [f64; 4] = [4.0, 5.0, 6.0, 7.0];
    const AHEAD: [f64; 4] = [-7.0, -6.0, -5.0, -4.0];
    const STRADDLES: [f64; 4] = [-1.0, -1.0, 1.0, 1.0];

    /// A control fails on a resolved sign and passes on a swamped one; a
    /// series passes either way, and states how far its first arm stands
    /// behind its second only where the sign is established, nothing where
    /// it is ahead (§1.2).
    #[test]
    fn a_control_fails_on_a_resolved_sign_and_a_series_states_the_distance_to_go() {
        let one = |unit, deltas: &[f64], pct| took(vec![row(unit, deltas, pct)], false);
        assert_eq!(
            Held::Control.status(&one(Ms, &BEHIND, 1.0), None),
            Status::Fail
        );
        assert_eq!(
            Held::Control.status(&one(Ms, &STRADDLES, 1.0), None),
            Status::Pass
        );
        assert_eq!(
            Held::Series.status(&one(Ms, &BEHIND, 1.0), None),
            Status::Pass
        );
        assert!(close(to_go(&one(Ms, &BEHIND, 250.0)).unwrap(), 250.0));
        assert!(close(to_go(&one(Ms, &AHEAD, -12.5)).unwrap(), 0.0));
        assert_eq!(to_go(&one(Ms, &STRADDLES, 3.0)), None, "swamped");
        assert_eq!(to_go(&one(Ms, &BEHIND, f64::NAN)), None);
        assert!(
            close(to_go(&one(Rps, &AHEAD, -50.0)).unwrap(), 50.0),
            "fewer requests a second is behind"
        );
        assert!(close(to_go(&one(Rps, &BEHIND, 12.5)).unwrap(), 0.0));
        assert_eq!(Correctness::Bytes(Stream::Body).as_str(), "body-bytes");
    }

    /// A set's ratio is the geomean of its rows', 0.5 and 2.0 making 1.0,
    /// its sign established only where every row's is (protocol §10) and
    /// signed only where every row points one way, `mixed` with the count
    /// each way where they do not, and no distance to go stated
    /// where it is unsigned.
    #[test]
    fn a_set_s_geomean_is_signed_only_where_every_row_points_one_way() {
        let set = took(vec![row(Ms, &AHEAD, -50.0), row(Ms, &BEHIND, 100.0)], true);
        assert!(close(set.ratio(), 1.0), "{}", set.ratio());
        assert_eq!((set.sign(), set.each_way()), (Sign::Mixed, [1, 1]));
        assert_eq!(to_go(&set), None, "a mixed geomean has no sign");
        let swamped = took(
            vec![row(Ms, &BEHIND, 10.0), row(Ms, &STRADDLES, 10.0)],
            true,
        );
        assert!(close(swamped.ratio(), 1.1));
        assert_eq!(
            (swamped.sign(), swamped.each_way()),
            (Sign::Swamped, [0, 1])
        );
        assert_eq!(
            to_go(&swamped),
            None,
            "one swamped row leaves the geomean unsigned"
        );
        assert_eq!(Held::Control.status(&swamped, None), Status::Pass, "1 of 2");
        let shown = super::geomean(&set);
        assert!(
            shown.contains(
                "geomean ratio 1.0000 over 2 programs (p0 0.5000, p1 2.0000), mixed, 2 of them resolved, 1 faster and 1 slower"
            ),
            "{shown}"
        );
        assert_eq!(
            super::geomean(&took(vec![row(Ms, &BEHIND, 1.0)], false)),
            ""
        );
    }

    /// A bound holds each row and never the geomean, as Mitchell ruled
    /// 2026-10-23: one kernel at +10% beside six at +0.1% is a geomean of
    /// 1.0145, within 2%, and fails on its one row; +20% beside −17%
    /// fails, and still beside a swamped row; six at −3% beside one at
    /// +0.2% pass, `mixed` as their geomean is. A swamped row holds
    /// nothing however far its median, a row at the bound passes, a rate
    /// is behind where it answers fewer requests a second, and a row
    /// resolved slower with no finite ratio fails, as a bar of no row does.
    #[test]
    fn a_bound_holds_each_row_and_never_the_geomean() {
        let bound = |rows: Vec<Measured>, pct| Held::Bound.status(&took(rows, true), Some(pct));
        let concentrated = || {
            let six = (0..6).map(|_| row(Ms, &BEHIND, 0.1));
            std::iter::once(row(Ms, &BEHIND, 10.0))
                .chain(six)
                .collect::<Vec<_>>()
        };
        let set = took(concentrated(), true);
        assert!((set.ratio() - 1.0145).abs() < 1e-4, "{}", set.ratio());
        assert!(
            to_go(&set).is_some_and(|behind| behind < 2.0),
            "the geomean is within 2%"
        );
        assert_eq!(bound(concentrated(), 2.0), Status::Fail, "one row at +10%");
        let opposite = || vec![row(Ms, &BEHIND, 20.0), row(Ms, &AHEAD, -17.0)];
        assert_eq!(took(opposite(), true).sign(), Sign::Mixed);
        assert_eq!(bound(opposite(), 2.0), Status::Fail, "+20% / -17%");
        let beside = opposite()
            .into_iter()
            .chain([row(Ms, &STRADDLES, 0.0)])
            .collect();
        assert_eq!(bound(beside, 2.0), Status::Fail, "+20% / -17% / swamped");
        let faster = || (0..6).map(|_| row(Ms, &AHEAD, -3.0));
        let slight = faster().chain([row(Ms, &BEHIND, 0.2)]).collect::<Vec<_>>();
        assert_eq!(took(slight, true).sign(), Sign::Mixed);
        let slight = faster().chain([row(Ms, &BEHIND, 0.2)]).collect();
        assert_eq!(
            bound(slight, 2.0),
            Status::Pass,
            "six at -3% and one at +0.2%"
        );
        let swamped = vec![row(Ms, &STRADDLES, 50.0), row(Ms, &BEHIND, 1.0)];
        assert_eq!(
            bound(swamped, 2.0),
            Status::Pass,
            "a swamped row holds nothing"
        );
        let at = vec![row(Ms, &BEHIND, 0.0), row(Ms, &BEHIND, 0.0)];
        assert_eq!(bound(at, 0.0), Status::Pass, "at the bound");
        let ahead = vec![row(Ms, &AHEAD, -10.0), row(Ms, &AHEAD, -10.0)];
        assert_eq!(bound(ahead, 2.0), Status::Pass);
        let fewer = vec![row(Rps, &AHEAD, -10.0), row(Rps, &STRADDLES, 0.0)];
        assert_eq!(bound(fewer, 2.0), Status::Fail, "fewer requests a second");
        let more = vec![row(Rps, &BEHIND, 10.0), row(Rps, &BEHIND, 10.0)];
        assert_eq!(bound(more, 2.0), Status::Pass);
        let unmeasured = vec![row(Ms, &STRADDLES, f64::NAN), row(Ms, &AHEAD, -1.0)];
        assert_eq!(
            bound(unmeasured, 2.0),
            Status::Pass,
            "a swamped row with no ratio"
        );
        let unmeasured = vec![row(Ms, &BEHIND, f64::NAN), row(Ms, &AHEAD, -1.0)];
        assert_eq!(
            bound(unmeasured, 2.0),
            Status::Fail,
            "slower, with no ratio"
        );
        let infinite = vec![row(Ms, &BEHIND, f64::INFINITY)];
        assert_eq!(bound(infinite, 2.0), Status::Fail, "slower, past any ratio");
        assert_eq!(bound(Vec::new(), 2.0), Status::Fail, "nothing measured");
        assert_eq!(
            Held::Series.status(&took(concentrated(), true), Some(2.0)),
            Status::Pass
        );
    }

    /// An A/A set fails at the fewest resolved rows whose chance at §2's
    /// one in seventeen is at most 1%: P(>= 2 of 8) is 7.7% and
    /// P(>= 3 of 8) 0.9%; P(>= 4 of 17) is 1.5% and P(>= 5 of 17) 0.24%;
    /// one row resolves 5.9% of the time, so a set of one fails at none,
    /// where a pair fails on its one row (§2).
    #[test]
    fn an_aa_set_fails_at_the_fewest_resolved_rows_whose_chance_is_at_most_one_percent() {
        assert_eq!(
            [0, 1, 2, 7, 8, 17].map(super::aa_fails_at),
            [None, None, Some(2), Some(3), Some(3), Some(5)]
        );
        let set = |resolved: usize, n: usize| {
            let deltas = |i| if i < resolved { &BEHIND } else { &STRADDLES };
            took((0..n).map(|i| row(Ms, deltas(i), 1.0)).collect(), true)
        };
        assert_eq!(Held::Control.status(&set(2, 8), None), Status::Pass);
        assert_eq!(Held::Control.status(&set(3, 8), None), Status::Fail);
        assert_eq!(Held::Control.status(&set(1, 1), None), Status::Pass);
        let shown = super::held_by(
            &super::Shown {
                id: "aa",
                about: "",
                held: Held::Control,
                names: ["a", "b"],
                row: None,
            },
            &set(3, 8),
        );
        assert!(shown.contains("over 8 rows, failing where 3 of them resolve, the fewest whose chance at 1 row in 17 is at most 1%"), "{shown}");
    }

    /// G1's bound is its `[[bench]]` row's: a manifest carrying
    /// `g1` at the 2% floor and no nights holds a bound bar to 2% behind,
    /// a ratio of 1.02 in time and 0.98 in rate, a series to none of it,
    /// and a bound with no row to none.
    #[test]
    fn a_bound_bar_is_held_to_one_plus_its_row_s_tolerance() {
        let text = "[budget]\ntotal = 1\nnew = 1\ndocs = 1\nharness = 1\nlock_packages = 1\n\n[[bench]]\nbar = \"g1\"\ntolerance_pct = 2.0\n";
        let manifest = crate::manifest::Manifest::parse(text).unwrap();
        let [g1] = manifest.tolerances.as_slice() else {
            panic!("{:?}", manifest.tolerances)
        };
        assert_eq!((g1.bar.as_str(), g1.nights), ("g1", None));
        let bar = |held, row| super::Shown {
            id: "g1",
            about: "",
            held,
            names: ["a", "b"],
            row,
        };
        assert_eq!(bar(Held::Bound, Some(g1)).tolerance(), Some(2.0));
        assert_eq!(bar(Held::Series, Some(g1)).tolerance(), None);
        assert_eq!(bar(Held::Bound, None).tolerance(), None);
        let set = |pct| took(vec![row(Ms, &BEHIND, pct), row(Ms, &BEHIND, pct)], true);
        let held = |pct| Held::Bound.status(&set(pct), bar(Held::Bound, Some(g1)).tolerance());
        assert_eq!((held(1.0), held(3.0)), (Status::Pass, Status::Fail));
        let shown = super::held_by(&bar(Held::Bound, Some(g1)), &set(3.0));
        assert!(
            shown.contains("a held on each row to at most 2% behind b, its `[[bench]]` tolerance"),
            "{shown}"
        );
        let missing = "[budget]\ntotal = 1\nnew = 1\ndocs = 1\nharness = 1\nlock_packages = 1\n\n[[bench]]\nbar = \"g1\"\n";
        assert!(
            crate::manifest::Manifest::parse(missing).is_err(),
            "a row names its tolerance"
        );
    }

    /// A rate arm's reps are paired as a time's are, the delta in requests
    /// a second, `a - b`: 50 against 100 is a delta of -50 and 50% to go;
    /// a server arm whose figures are absent still measures its rate.
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
                let spent = super::Spent::over(None, None, 1);
                let wrote = b"x".to_vec();
                Ok(super::Sample {
                    spent,
                    ..super::Sample::of(self.0, wrote)
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
        let took = Took {
            rows: vec![(None, measured)],
        };
        assert!(close(to_go(&took).unwrap().round(), 50.0));
        let measured = &took.rows[0].1;
        assert_eq!(measured.correctness, Correctness::Bytes(Stream::Stdout));
        // Each rep's server read nothing, as on a platform without
        // proc_pid_rusage: the rate stands, and each arm says why its
        // figures are absent.
        let absent = super::Spent::over(None, None, 1);
        assert_eq!(measured.spent, [absent; 2]);
        let bar = super::Shown {
            id: "c8",
            about: "a rate",
            held: Held::Series,
            names: ["a", "b"],
            row: None,
        };
        let shown = super::rendered(&bar, &took, 12, super::Label::Smoke, &shared().unwrap());
        let said = format!("n 12); per request: absent: {}", super::spent::ABSENT);
        assert!(shown.contains(&said), "{shown}");
    }

    /// A label is spelled as its flag's value in capitals (§8).
    #[test]
    fn a_label_is_spelled_in_capitals() {
        use super::Label::{BaselineOfRecord, Provisional, Smoke, Unlabelled};
        let spelled = [Unlabelled, Smoke, Provisional, BaselineOfRecord].map(super::Label::spelled);
        assert_eq!(
            spelled,
            ["UNLABELLED", "SMOKE", "PROVISIONAL", "BASELINE-OF-RECORD"]
        );
    }

    /// A GitHub-hosted runner is refused as a shared or a dedicated
    /// machine and measured as a hosted one; another runner, or a machine
    /// outside CI, is refused nothing.
    #[test]
    fn a_github_hosted_runner_measures_as_a_hosted_machine_alone() {
        use super::Machine::{Dedicated, Hosted, Shared};
        for machine in [Shared, Dedicated] {
            let refused = refuse_a_hosted_runner(Some("github-hosted"), machine).unwrap_err();
            assert!(
                format!("{refused}").contains("--machine hosted alone"),
                "{refused}"
            );
            assert!(refuse_a_hosted_runner(Some("self-hosted"), machine).is_ok());
            assert!(refuse_a_hosted_runner(None, machine).is_ok());
        }
        assert!(refuse_a_hosted_runner(Some("github-hosted"), Hosted).is_ok());
    }

    /// A hosted machine offers its numbers as smoke or provisional and
    /// under no other label; a machine of another kind offers any.
    #[test]
    fn a_hosted_machine_offers_its_numbers_as_smoke_or_provisional_alone() {
        use super::Label::{BaselineOfRecord, Provisional, Smoke, Unlabelled};
        use super::Machine::{Dedicated, Hosted, Shared};
        for label in [Smoke, Provisional] {
            assert!(Hosted.offers(label).is_ok(), "{label:?}");
        }
        for label in [Unlabelled, BaselineOfRecord] {
            let refused = Hosted.offers(label).unwrap_err();
            assert!(
                format!("{refused}").contains("smoke or --label provisional"),
                "{refused}"
            );
            assert!(Shared.offers(label).is_ok() && Dedicated.offers(label).is_ok());
        }
    }

    /// On a hosted machine a bar is measured between two runs of the
    /// declared control named for it, each saying which run it is; the
    /// control is not measured again where it stood, and a bar with no
    /// such control, or a control that is no bar's, stays where it was.
    #[test]
    fn a_hosted_machine_measures_each_bar_between_two_runs_of_its_control() {
        let same: [Stubbed; 2] = [("a", |_| 10.0), ("b", |_| 10.0)];
        let declared = || {
            vec![
                stubbed("lone-aa", Held::Control, same),
                stubbed("door", Held::Bound, same),
                stubbed("door-aa", Held::Control, same),
                stubbed("code", Held::Series, same),
            ]
        };
        let order = |bars: Vec<super::Declared>| -> Vec<String> {
            super::between_controls(bars, [declared(), declared()])
                .iter()
                .map(|bar| match bar.about.split_once("; ") {
                    Some((_, turn)) => format!("{} ({turn})", bar.id),
                    None => bar.id.clone(),
                })
                .collect()
        };
        let around = [
            "door-aa (before door, on a hosted machine)",
            "door",
            "door-aa (after door, on a hosted machine)",
        ];
        assert_eq!(
            order(declared()),
            [&["lone-aa"][..], &around[..], &["code"][..]].concat()
        );
        // One bar asked for by name brings its control with it; the
        // control asked for alone is measured once.
        let only = |id: &str| declared().into_iter().filter(|b| b.id == id).collect();
        assert_eq!(order(only("door")), around);
        assert_eq!(order(only("door-aa")), ["door-aa"]);
        assert_eq!(order(only("code")), ["code"]);
        // Through the run: both runs of the control are measured and the
        // reading kept is the later one's, which says so.
        let (_tree, store, snapshot) = scratch("bench-between");
        let bars = super::between_controls(only("door"), [declared(), declared()]);
        let mut quiet = || super::Machine::Hosted.settle("hw", || Some([0.5; 3]), |_| {});
        let ask = (super::Label::Provisional, "a hosted runner");
        super::measure_bars(&store, &snapshot, bars, &[], &[], (6, &mut quiet), ask).unwrap();
        let control = store.read("bench", "door-aa").unwrap().unwrap();
        assert!(
            control.message.contains("after door, on a hosted machine")
                && control.message.contains("PROVISIONAL"),
            "{}",
            control.message
        );
        assert!(store.read("bench", "door").unwrap().is_some());
    }

    /// A shared machine's conditions, at a load a loaded machine reads.
    fn shared() -> Result<super::Conditions, String> {
        super::Machine::Shared.settle("a stub machine", || Some([9.0, 8.0, 7.0]), |_| {})
    }

    /// A dedicated machine measures a bar only below a 1-minute load of 2:
    /// a load that falls is waited out and named; one that never
    /// falls, or a machine that states none, refuses the bar, which reads
    /// skip naming the load and measures nothing; a shared machine
    /// measures at any load and never waits.
    #[test]
    fn a_dedicated_machine_measures_a_bar_only_below_a_load_of_two() {
        use super::{LOOK, Machine, PATIENCE};
        let mut looks = [7.5, 3.0, 1.5].into_iter();
        let mut slept = std::time::Duration::ZERO;
        let read = || looks.next().map(|l| [l, 1.0, 0.5]);
        let quiet = Machine::Dedicated
            .settle("hw", read, |d| slept += d)
            .unwrap();
        assert_eq!(quiet.load, Some([1.5, 1.0, 0.5]));
        assert_eq!((quiet.waited, slept), (Some((LOOK * 2, 7.5)), LOOK * 2));
        assert!(
            quiet
                .text()
                .contains("after 30s waiting for the 1-minute load to fall below 2 from 7.50"),
            "{}",
            quiet.text()
        );
        let mut slept = std::time::Duration::ZERO;
        let loud = Machine::Dedicated.settle("hw", || Some([2.0, 2.0, 2.0]), |d| slept += d);
        let loud = loud.unwrap_err();
        assert!(
            loud.contains("stayed at 2 or above") && loud.contains("2.00 2.00 2.00"),
            "{loud}"
        );
        assert_eq!(slept, PATIENCE);
        assert!(Machine::Dedicated.settle("hw", || None, |_| {}).is_err());
        let shared = Machine::Shared.settle("hw", || Some([44.0; 3]), |_| unreachable!());
        assert_eq!(shared.unwrap().load, Some([44.0; 3]));
        // Through the run: the refused bar's reading is a skip saying why.
        let (_tree, store, snapshot) = scratch("bench-loud");
        let arms: [Stubbed; 2] = [("a", |_| 10.0), ("b", |_| 10.0)];
        let bars = vec![stubbed("aa-loud", Held::Control, arms)];
        let mut loud = || Machine::Dedicated.settle("hw", || Some([3.0; 3]), |_| {});
        let ask = (super::Label::Smoke, "");
        super::measure_bars(&store, &snapshot, bars, &[], &[], (6, &mut loud), ask).unwrap();
        let reading = store.read("bench", "aa-loud").unwrap().unwrap();
        assert_eq!(reading.status, Status::Skip, "{}", reading.message);
        assert!(
            reading.message.contains("3.00 3.00 3.00"),
            "{}",
            reading.message
        );
        assert!(
            !reading.numbers.contains_key("reps"),
            "{:?}",
            reading.numbers
        );
    }

    /// A stub arm's name and what each of its reps takes, by the rep.
    type Stubbed = (&'static str, fn(u32) -> f64);

    /// An arm whose reps take what `ms` says of each, as a stub states
    /// them: the timings a test feeds the instrument, so its verdict is
    /// the rule's and never the machine's load. It writes nothing
    /// and pins `/bin/sh`, which it never runs.
    struct Stub {
        name: &'static str,
        ms: fn(u32) -> f64,
        rep: u32,
        program: std::path::PathBuf,
    }

    impl super::Side for Stub {
        fn name(&self) -> &str {
            self.name
        }
        fn program(&self) -> &std::path::Path {
            &self.program
        }
        fn once(&mut self) -> anyhow::Result<super::Sample> {
            self.rep += 1;
            Ok(super::Sample::of((self.ms)(self.rep), Vec::new()))
        }
    }

    /// `id` declared with two stub arms, held as `held`.
    fn stubbed(id: &str, held: Held, [a, b]: [Stubbed; 2]) -> super::Declared {
        let stub = |(name, ms)| -> Box<dyn super::Side> {
            let program = "/bin/sh".into();
            Box::new(Stub {
                name,
                ms,
                rep: 0,
                program,
            })
        };
        super::Declared {
            id: id.to_owned(),
            about: format!("{id}, its arms stubbed"),
            held,
            start: Box::new(move || Ok(Ok(super::Arms::Pair([stub(a), stub(b)])))),
        }
    }

    /// A scratch repository, its store and its snapshot, for one test.
    fn scratch(
        name: &str,
    ) -> (
        crate::scratch::Tree,
        crate::reading::Store,
        crate::key::Snapshot,
    ) {
        let tree = crate::scratch::Tree::new(name).unwrap();
        crate::scratch::repository(&crate::built_root(), &tree, &["xtask/fixtures/bench"]).unwrap();
        let store = crate::reading::Store::of_workspace(&tree);
        let snapshot = crate::key::Snapshot::disk(&tree).unwrap();
        (tree, store, snapshot)
    }

    /// The A/A control whose arms differ by a known twenty milliseconds,
    /// with a jitter of its own, reads fail on the resolved sign: the
    /// negative control of the verdict, fed by a stub so a loaded
    /// machine's noise never swamps it. Six measured reps after the
    /// two discarded, the load beside the number, and the arms' empty
    /// streams the vacuous value of §5.
    #[test]
    fn a_control_fed_arms_a_known_amount_apart_reads_fail_whatever_the_load() {
        let (_tree, store, snapshot) = scratch("bench-stub-control");
        let arms: [Stubbed; 2] = [
            ("10ms", |r| 10.0 + f64::from(r % 3) * 0.5),
            ("30ms", |r| 30.0 + f64::from(r % 2)),
        ];
        let bars = vec![stubbed("aa-differs", Held::Control, arms)];
        super::measure_bars(
            &store,
            &snapshot,
            bars,
            &[],
            &[],
            (6, &mut shared),
            (super::Label::Smoke, ""),
        )
        .unwrap();
        let reading = store.read("bench", "aa-differs").unwrap().unwrap();
        assert_eq!(reading.status, Status::Fail, "{}", reading.message);
        for said in ["resolved", "SMOKE", "load ", "vacuous"] {
            assert!(
                reading.message.contains(said),
                "{said}: {}",
                reading.message
            );
        }
        assert_eq!(reading.numbers["reps"], 6.into());
        assert_eq!(reading.numbers["warmup_discarded"], 2.into());
        let delta = reading.numbers["delta_median_ms"].as_f64();
        assert!(delta < -10.0, "{:?}", reading.numbers);
        // The same arms one arm: the sign swamped, the control passes.
        let same: [Stubbed; 2] = [
            ("a", |r| 10.0 + f64::from(r % 2)),
            ("b", |r| 10.0 + f64::from((r + 1) % 2)),
        ];
        let bars = vec![stubbed("aa-same", Held::Control, same)];
        super::measure_bars(
            &store,
            &snapshot,
            bars,
            &[],
            &[],
            (6, &mut shared),
            (super::Label::Smoke, ""),
        )
        .unwrap();
        let reading = store.read("bench", "aa-same").unwrap().unwrap();
        assert_eq!(reading.status, Status::Pass, "{}", reading.message);
    }

    /// A series held to a baseline of 0% within 2 points, its first arm
    /// twenty milliseconds behind its second after a night that already
    /// regressed, both read from a history handed in as the nightly lane
    /// hands the published one, fails its reading and the run and leaves
    /// the ticket the lane files, naming the commits; the arms'
    /// timings fed by a stub. The same night not regressed before
    /// leaves the run standing.
    #[test]
    fn a_series_that_regressed_two_nights_running_fails_and_leaves_its_ticket() {
        use crate::reading::{Number, Reading};
        let (_tree, store, snapshot) = scratch("bench-stub-series");
        let id = "series-regressed";
        let tolerance = crate::manifest::Tolerance {
            bar: id.to_owned(),
            pct: 2.0,
            nights: Some(["1970-01-01".parse().unwrap(), "1970-01-02".parse().unwrap()]),
        };
        let mut baseline = Reading::new("bench", &format!("{id}/baseline"));
        baseline
            .numbers
            .insert("baseline_pct".to_owned(), Number::Float(0.0));
        let night = |delta: f64| {
            let mut night = Reading::new("bench", id);
            night.commit = "c1".to_owned();
            for (key, ms) in [("b_median_ms", 10.0), ("delta_median_ms", delta)] {
                night.numbers.insert(key.to_owned(), Number::Float(ms));
            }
            for (key, ms) in [("delta_q1_ms", delta - 1.0), ("delta_q3_ms", delta + 1.0)] {
                night.numbers.insert(key.to_owned(), Number::Float(ms));
            }
            night.numbers.insert("reps".to_owned(), Number::Int(12));
            night
        };
        let arms: [Stubbed; 2] = [
            ("30ms", |r| 30.0 + f64::from(r % 3) * 0.5),
            ("10ms", |r| 10.0 + f64::from(r % 2)),
        ];
        let run = |log: &[Reading]| {
            let bars = vec![stubbed(id, Held::Series, arms)];
            let ask = (super::Label::Smoke, "");
            super::measure_bars(
                &store,
                &snapshot,
                bars,
                std::slice::from_ref(&tolerance),
                log,
                (6, &mut shared),
                ask,
            )
        };
        run(&[baseline.clone(), night(0.0)]).expect("one night regressed is no failed run");
        let failed =
            run(&[baseline, night(20.0)]).expect_err("a second night in a row fails the run");
        assert!(
            format!("{failed:#}").contains("two nights running"),
            "{failed:#}"
        );
        let reading = store.read("bench", id).unwrap().unwrap();
        assert_eq!(reading.status, Status::Fail, "{}", reading.message);
        let ticket = store.root().join(super::FAILED).join(format!("{id}.md"));
        let ticket = std::fs::read_to_string(ticket).expect("the ticket the lane files");
        assert!(ticket.contains("..c1"), "{ticket}");
    }
}
