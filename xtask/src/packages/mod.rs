//! `xtask packages`, the packages lane (spec #32, ruled on #69): the test
//! suites of the pinned Composer packages of a list, each run once under
//! the oracle and once under Lela through the package's own runner. The
//! lane's logic is the crate `lanes/packages` ([`php_lanes_packages`]):
//! the steps of one package's run, the reading of its runner's log and the
//! matching of its tests across the two runs. This is its adapter: it
//! reads the list, takes each step the lane plans under xtask's own
//! process limits and the seat, judges through the tally and writes the
//! readings.
//!
//! **The list.** [`LIST`] unless one is named: a `[[package]]` row each,
//! its [`Pinned`] fields and how its suite runs. A run downloads nothing
//! the row does not pin (R9), and a source or lock whose digest is not
//! the row's is refused ([`digest`] is how both are taken). Every install
//! and suite is given the services' addresses of [`SERVICES`], the file
//! the nightly job starts the services by.
//!
//! **What it writes.** One reading per package, scoped by the package's
//! name, and one of the whole, scoped [`WHOLE`] (R10): packages listed,
//! judged, left out and whose every test is met; tests found, met and
//! outside. A package left out, its suite not running under the oracle or
//! the list marking it as needing what the lane does not provide, is a
//! skip that says why, and the whole names it. A package not judged, for
//! want of the oracle or Lela's binary, a step that could not be taken or
//! a digest refused, is a skip the run did not take; the whole is then a
//! skip with `whole = 0` and the run exits as one that could not run to
//! its end (R20). Its readings bind nothing, no release, `done` row or
//! bar (R12), and nothing owes them.

use std::fs;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use php_lanes_packages::{self as lane, Came, Held, Php, Program, Step, Verdict};
use serde::Deserialize;

use crate::key::Snapshot;
use crate::manifest::Manifest;
use crate::oracle::{Ended, Oracle, Runner, Stdin, Streams};
use crate::reading::{Reading, joined, plural};
use crate::sha256::Sha256;
use crate::tally::{Judged, Judgement, Outcome, Tally, Took};

/// The instrument's name, the reading's `instrument` field.
pub const INSTRUMENT: &str = "packages";

/// The list a run takes where none is named, relative to the root.
pub(crate) const LIST: &str = "data/packages.toml";

/// The services' addresses, relative to the root: `KEY=VALUE` lines,
/// which docker compose reads as an env file and a shell can source.
pub(crate) const SERVICES: &str = "ci/packages/services.env";

/// The scope of the whole's reading; a package's is its name, which
/// Composer spells `<vendor>/<name>`, so the two never meet.
pub(crate) const WHOLE: &str = "packages";

/// How long one step may run, a suite's whole run or an install, before
/// it is stopped with its process group.
const LIMIT: Duration = Duration::from_mins(30);

/// What one `xtask packages` run was asked for.
#[derive(Debug, Clone, clap::Args)]
pub struct Ask {
    /// The list of pinned packages; default `data/packages.toml`.
    #[arg(long, value_name = "FILE")]
    pub list: Option<PathBuf>,
    /// Lela's binary; without it, the engine's own where cargo puts it.
    #[arg(long, value_name = "PATH")]
    pub engine: Option<PathBuf>,
}

/// A Composer project as the harness pins one: the one shape the packages
/// lane's rows and the served lane's projects take (R6, R9). Its source is
/// the repository at `commit` (Packagist's `source.reference`), since an
/// archive leaves the suite out, and `digest` is that source's; `lock` is
/// the lock of its development dependencies the proposal resolved, beside
/// the list, and `lock_digest` that lock's.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Pinned {
    pub name: String,
    pub version: String,
    pub repository: String,
    pub commit: String,
    pub digest: Sha256,
    pub lock: PathBuf,
    pub lock_digest: Sha256,
}

/// One row of the list: a pinned package and how its suite runs.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct Row {
    #[serde(flatten)]
    pinned: Pinned,
    /// The runner, a path in the source; `phpunit` unless named.
    #[serde(default = "phpunit")]
    runner: String,
    #[serde(default)]
    args: Vec<String>,
    /// The services its suite needs; one the lane does not provide leaves
    /// it out.
    #[serde(default)]
    needs: Vec<String>,
}

impl Row {
    /// The lane's run of this row, its lock and a repository named by a
    /// relative path found beside the list, in a scratch tree of its own.
    fn run(
        &self,
        beside: &Path,
        env: &[(String, String)],
        sides: &mut Sides<'_>,
    ) -> Result<Verdict> {
        let pinned = &self.pinned;
        let work = crate::scratch::Tree::new("packages")?;
        let repository = &pinned.repository;
        let repository = if repository.contains(':') || Path::new(repository).is_absolute() {
            repository.clone()
        } else {
            beside.join(repository).display().to_string()
        };
        let (digest, lock_digest) = (pinned.digest.to_string(), pinned.lock_digest.to_string());
        let package = lane::Package {
            repository: &repository,
            commit: &pinned.commit,
            digest: &digest,
            lock: &beside.join(&pinned.lock),
            lock_digest: &lock_digest,
            runner: &self.runner,
            args: &self.args,
            needs: &self.needs,
            env,
        };
        Ok(lane::run(&package, &work, sides))
    }
}

fn phpunit() -> String {
    "vendor/bin/phpunit".to_owned()
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct List {
    #[serde(default)]
    package: Vec<Row>,
}

/// The list at `path`, refused as [`parse`] refuses it.
fn load(path: &Path) -> Result<Vec<Row>> {
    parse(&crate::read(path)?).with_context(|| format!("decoding the list {}", path.display()))
}

/// The rows of a list's text, refused where it does not decode or names a
/// package twice: what a run reads, and what steer holds a proposal of
/// the list to.
pub(crate) fn parse(text: &str) -> Result<Vec<Row>> {
    let list: List = toml::from_str(text)?;
    let mut names: Vec<&str> = list
        .package
        .iter()
        .map(|r| r.pinned.name.as_str())
        .collect();
    names.sort_unstable();
    if let Some([twice, _]) = names.windows(2).find(|w| w[0] == w[1]) {
        bail!("it names {twice} twice");
    }
    Ok(list.package)
}

/// `xtask packages`: [`run`], its readings recorded, and the exit the
/// tally's rule gives.
pub(crate) fn command(
    root: &Path,
    manifest: &Manifest,
    store: &crate::reading::Store,
    ask: &Ask,
) -> Result<()> {
    run(root, manifest, ask)?.record(store, INSTRUMENT)
}

/// The services' addresses [`SERVICES`] gives, in its order.
fn services(root: &Path) -> Result<Vec<(String, String)>> {
    let text = crate::read(&root.join(SERVICES))?;
    (text.lines().map(str::trim))
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .map(|line| {
            let (key, value) = (line.split_once('='))
                .with_context(|| format!("{SERVICES}: {line:?} is not KEY=VALUE"))?;
            Ok((key.to_owned(), value.to_owned()))
        })
        .collect()
}

/// Run every package of the list, one reading each and one of the whole.
pub(crate) fn run(root: &Path, manifest: &Manifest, ask: &Ask) -> Result<Took> {
    let list = root.join(ask.list.as_deref().unwrap_or(Path::new(LIST)));
    let rows = load(&list)?;
    let env = services(root)?;
    let beside = list.parent().unwrap_or(root);
    let pin = manifest.pin()?;
    let snapshot = Snapshot::disk(root)?;
    let runner = Runner::resolve(root, &manifest.layout(), pin, ask.engine.as_deref())?;
    let oracle = pin.oracle();
    let engine = match (pin.oracle.absent(), &runner) {
        (Some(why), _) => Err(why),
        (None, Runner::Absent(why) | Runner::Stale(why)) => Err(why.clone()),
        (None, runner) => (runner.binary()).ok_or_else(|| "Lela has no binary to run".to_owned()),
    };
    if engine.is_ok() {
        oracle.asserted()?;
    }
    let mut whole = Whole::default();
    let mut took = Took::default();
    for row in &rows {
        let verdict = match engine {
            Ok(engine) => {
                let mut sides = Sides {
                    oracle: &oracle,
                    php: &pin.oracle.path,
                    engine,
                };
                row.run(beside, &env, &mut sides)?
            }
            Err(ref why) => Verdict::Unjudged(format!("nothing ran; {why}")),
        };
        let stamped =
            Reading::stamped_over(INSTRUMENT, &row.pinned.name, &snapshot, Path::new("."))?
                .under(pin);
        took.one(whole.add(&row.pinned, verdict, stamped, &runner)?);
    }
    let stamped = Reading::stamped_over(INSTRUMENT, WHOLE, &snapshot, Path::new("."))?.under(pin);
    took.one(whole.reading(stamped, &runner)?);
    Ok(took)
}

/// How Lela's run held a test the oracle passed, as the tally reads it.
impl Judgement for Held {
    type Miss = std::convert::Infallible;
    const PASS: Held = Held::Met;
    fn as_str(self) -> &'static str {
        match self {
            Held::Met => "met",
            Held::Missed => "missed",
            Held::Unreported => "unreported",
        }
    }
    fn counts_against(self) -> bool {
        self != Held::Met
    }
    fn met(self) -> bool {
        self == Held::Met
    }
}

/// What the run came to across the list, for the whole's reading.
#[derive(Default)]
struct Whole {
    listed: u64,
    every_met: u64,
    outside: u64,
    tests: Tally<Held>,
    left_out: Vec<String>,
    unjudged: Vec<String>,
}

impl Whole {
    /// One package's reading, `stamped`, from its verdict, and whether the
    /// run took it; its tests folded into the whole's.
    fn add(
        &mut self,
        pinned: &Pinned,
        verdict: Verdict,
        stamped: Reading,
        runner: &Runner,
    ) -> Result<(Reading, bool)> {
        let name = &pinned.name;
        let named = format!("{name} {}", pinned.version);
        self.listed += 1;
        let (why, taken) = match verdict {
            Verdict::Judged(matched) => {
                let tally = Tally::judge(&matched.found, |test| {
                    let id = format!("{INSTRUMENT}:{name}::{}", test.id);
                    Ok(Judged::new(
                        &id,
                        &format!("{name}::{}", test.id),
                        test.held,
                        &test.note,
                    ))
                })?;
                let outcome = tally.outcome(runner);
                self.every_met += u64::from(outcome == Outcome::Passed);
                self.outside += matched.outside;
                let message = format!("{named}: {}", counted(&tally, matched.outside, &outcome));
                let mut reading = tally.reading(stamped, &outcome, message)?;
                reading.profile = runner.profile();
                numbers(&mut reading, &tally, matched.outside)?;
                self.tests.found += tally.found;
                self.tests.judged.extend(tally.judged);
                return Ok((reading, true));
            }
            Verdict::LeftOut(why) => {
                self.left_out.push(format!("{name} ({why})"));
                (format!("left out: {why}"), true)
            }
            Verdict::Unjudged(why) => {
                self.unjudged.push(name.to_owned());
                (format!("not judged: {why}"), false)
            }
        };
        let mut reading = stamped;
        reading.message = format!("{named}: {why}");
        Ok((reading, taken))
    }

    /// The whole's reading, `stamped`, and whether the run took every
    /// package it listed.
    fn reading(self, stamped: Reading, runner: &Runner) -> Result<(Reading, bool)> {
        let judged = self.listed - (self.left_out.len() + self.unjudged.len()) as u64;
        let missing = (!self.unjudged.is_empty()).then(|| {
            let n = self.unjudged.len();
            format!(
                "not whole: {n} package{} not judged: {}",
                plural(n),
                joined(&self.unjudged, ", ")
            )
        });
        let outcome = match &missing {
            Some(missing) => Outcome::Absent(missing),
            None => self.tests.outcome(runner),
        };
        let left = (!self.left_out.is_empty())
            .then(|| format!("left out: {}", joined(&self.left_out, "; ")));
        let message = joined(
            [
                Some(format!(
                    "{WHOLE}: {} listed, {judged} judged, {} with every test met; {}",
                    self.listed,
                    self.every_met,
                    counted(&self.tests, self.outside, &outcome)
                )),
                left,
            ]
            .into_iter()
            .flatten(),
            "; ",
        );
        let mut reading = self.tests.reading(stamped, &outcome, message)?;
        reading.profile = runner.profile();
        numbers(&mut reading, &self.tests, self.outside)?;
        let counts = [
            ("listed", self.listed),
            ("judged", judged),
            ("left_out", self.left_out.len() as u64),
            ("every_met", self.every_met),
            ("whole", u64::from(missing.is_none())),
        ];
        for (key, n) in counts {
            reading.count(key, n)?;
        }
        Ok((reading, missing.is_none()))
    }
}

/// The tests' counts and the rung's words, after the scope.
fn counted(tally: &Tally<Held>, outside: u64, outcome: &Outcome<'_>) -> String {
    let words = match outcome {
        Outcome::Failed => format!("{} missed{}", tally.against().len(), tally.missed()),
        Outcome::Control(binary) => format!(
            "run by {}, the pinned oracle; a control, not a reading of Lela",
            binary.display()
        ),
        Outcome::NothingRan => "none met".to_owned(),
        Outcome::Absent(why) | Outcome::OracleAbsent(why) => (*why).to_owned(),
        Outcome::Passed => "every one met".to_owned(),
    };
    format!(
        "{} tests found, {} met, {outside} outside; {words}",
        tally.found,
        tally.met()
    )
}

/// The tests' counts a reading carries.
fn numbers(reading: &mut Reading, tally: &Tally<Held>, outside: u64) -> Result<()> {
    reading.count("found", tally.found as u64)?;
    reading.count("met", tally.met())?;
    reading.count("outside", outside)?;
    reading.count("missed", tally.against().len() as u64)?;
    reading.count("unreported", tally.count(Held::Unreported))
}

/// The lane's shell on this machine: each program under xtask's process
/// limits, the oracle's php and Lela's where the lane names them.
struct Sides<'a> {
    oracle: &'a Oracle<'a>,
    php: &'a Path,
    engine: &'a Path,
}

impl lane::Shell for Sides<'_> {
    fn run(&mut self, step: &Step) -> Came {
        let came = match step {
            Step::Run(command) => self.ran(command),
            Step::Digest(path) => digest(path).map(|d| Came::Digest(d.to_string())),
            Step::Copy { from, to } => fs::copy(from, to)
                .map(|_| Came::Exited(Some(0), String::new()))
                .with_context(|| format!("copying {} to {}", from.display(), to.display())),
            Step::Read(path) => Ok(Came::Text(
                (fs::read(path).ok()).map(|b| String::from_utf8_lossy(&b).into_owned()),
            )),
        };
        came.unwrap_or_else(|why| Came::Failed(format!("{why:#}")))
    }
}

impl Sides<'_> {
    /// `command` run to its end or [`LIMIT`], in a process group of its
    /// own, with an environment of PATH, HOME and TMPDIR and the lane's.
    fn ran(&self, command: &lane::Command) -> Result<Came> {
        let recipe = &self.oracle.pin().recipe;
        let mut cmd = match command.program {
            Program::Git => Command::new("git"),
            Program::Composer => {
                let mut cmd = Command::new(self.php);
                cmd.arg(on_path("composer")?);
                cmd
            }
            Program::Php(php) => {
                let mut cmd = Command::new(if php == Php::Oracle {
                    self.php
                } else {
                    self.engine
                });
                cmd.args(&recipe.args);
                for ini in &recipe.ini {
                    cmd.arg("-d").arg(ini);
                }
                cmd
            }
        };
        cmd.args(&command.args)
            .current_dir(&command.dir)
            .env_clear();
        for kept in ["PATH", "HOME", "TMPDIR"] {
            if let Some(value) = std::env::var_os(kept) {
                cmd.env(kept, value);
            }
        }
        cmd.envs(command.env.iter().map(|(k, v)| (k, v)));
        let ended = self
            .oracle
            .to_end(cmd, Streams::Files, Stdin::Null, LIMIT, |_| ())?;
        let code = match ended.exit {
            Some(Ended::Status(code)) => Some(code),
            Some(Ended::Signal(_)) | None => None,
        };
        let said = [ended.stdout, ended.stderr].concat();
        Ok(Came::Exited(
            code,
            String::from_utf8_lossy(&said).into_owned(),
        ))
    }
}

/// The first file named `program` on PATH.
fn on_path(program: &str) -> Result<PathBuf> {
    let path = std::env::var_os("PATH").unwrap_or_default();
    std::env::split_paths(&path)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
        .with_context(|| format!("{program} is on no PATH entry"))
}

/// The digest a list pins a source or a lock by: a file's bytes, or a
/// tree's files in their paths' order, `.git` left out, each its path,
/// whether it is executable or a link, its length and its bytes, a link's
/// being its target. So two checkouts of one commit have one digest
/// whatever git wrote beside them.
pub(crate) fn digest(path: &Path) -> Result<Sha256> {
    if path.is_file() {
        return Ok(Sha256::of(&fs::read(path)?));
    }
    let mut files = Vec::new();
    crate::tally::files_under(path, &mut files)?;
    let mut relative: Vec<&Path> = (files.iter())
        .filter_map(|f| f.strip_prefix(path).ok())
        .filter(|f| !f.starts_with(".git"))
        .collect();
    relative.sort();
    let mut bytes = Vec::new();
    for file in relative {
        let full = path.join(file);
        let meta = fs::symlink_metadata(&full)?;
        let (kind, body) = if meta.is_symlink() {
            (
                b'l',
                fs::read_link(&full)?.into_os_string().into_encoded_bytes(),
            )
        } else {
            let executable = meta.permissions().mode() & 0o111 != 0;
            (if executable { b'x' } else { b'f' }, fs::read(&full)?)
        };
        bytes.push(kind);
        bytes.extend(file.as_os_str().as_encoded_bytes());
        bytes.push(0);
        bytes.extend((body.len() as u64).to_be_bytes());
        bytes.extend(body);
    }
    Ok(Sha256::of(&bytes))
}
