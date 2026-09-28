//! The reading and its store (harness-design.md §4.2). Every instrument ends
//! by writing its readings through [`Store::record`], the one writer; every
//! rendering reads them; nothing else touches the store. The store is
//! `target/harness/<instrument>/<scope>.toml`, one file per key, overwritten
//! on each run, and `target/harness/log.jsonl`, append-only, one line per
//! reading. The clock a reading's `at` is taken from, and the calendar that
//! reads it back, live here too.

use std::collections::BTreeMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use toml::value::{Datetime, Offset, Time};

use crate::git::Stamp;
use crate::manifest::{Crate, Manifest};
use crate::sources;

/// Where the store lives, relative to the workspace root.
pub const DIR: &str = "target/harness";
/// The append-only log, inside the store.
pub const LOG: &str = "log.jsonl";

/// One reading, the one shape every instrument writes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reading {
    /// The subcommands that write one, in the order `cli::Command` has
    /// them: `api-check | permit | done | track-work | ticket | usage |
    /// budget | clippy | test | phpt | port-map | compile-identity |
    /// crosscheck | unsafe-audit | steer | divergence-reason | witness |
    /// vocab`; `bench | fuzz | mutants` are named by harness-design.md
    /// §4.2 and not built yet.
    pub instrument: String,
    /// A crate, a corpus subset, a bar's id such as `G1`, a ticket's
    /// number, or `"workspace"`.
    pub scope: String,
    /// A manifest rule id when one applies, else empty.
    pub rule: String,
    pub status: Status,
    pub numbers: BTreeMap<String, Number>,
    pub message: String,
    /// From the manifest when the rule names one, else empty.
    pub adr: String,
    /// The git tree hash of the scope's sources at the time: what `done`
    /// holds against the scope's hash now to call the reading fresh
    /// (harness-design.md §4.2, §13 D.18). Private, and set by the
    /// constructors alone: a reading made through [`Reading::for_crate`]
    /// or [`Reading::for_scope`] is hashed over the manifest's mapping of
    /// its scope, [`Manifest::scope_path`], the path `done` hashes; one
    /// made through [`Reading::stamped_over`] says which path it was hashed over.
    tree: String,
    pub commit: String,
    pub at: Datetime,
    /// The session of the call a `permit` reading judged, from the
    /// hook's `session_id`; the session a `ticket` close named, the one a
    /// `done --hook` verdict was taken in, and the one a `usage` reading
    /// measured; empty for every other instrument, and for a call that
    /// carried none.
    #[serde(default)]
    pub session: String,
    /// A lane's named groups: one count per key, where the keys are the
    /// run's own and not a fixed set `numbers` could name. The phpt
    /// census is the first, a count per construct the compiler refused
    /// (#617), and the reason this is not `numbers`: `status` tables
    /// `numbers` as one line of `key=value`, which a fold over an open
    /// set of sentences is not. Empty for every instrument that writes
    /// none, and for a reading written before this field existed.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub groups: BTreeMap<String, Number>,
}

impl Reading {
    /// A reading of `instrument` over a crate, stamped over the crate's
    /// sources now: the path the manifest maps the crate to
    /// ([`Crate::path`]), which is the path `done` hashes to call the
    /// reading fresh (harness-design.md §4.5 condition 1). A crate not yet
    /// created is measured by nothing and its reading carries no stamp,
    /// so `done` finds it fresh for nothing.
    pub fn for_crate(instrument: &str, krate: &Crate, root: &Path) -> Result<Reading> {
        if !sources::exists(root, krate) {
            return Ok(Reading::new(instrument, &krate.name));
        }
        Reading::stamped_over(instrument, &krate.name, root, &krate.path())
    }

    /// A reading of `instrument` over `scope`, stamped over the path the
    /// manifest maps the scope to ([`Manifest::scope_path`]): a crate's
    /// sources, a subset's crate, a lock's home, a generated file, a
    /// directory of the pinned corpus, or the workspace. A scope the
    /// manifest maps to nothing is refused by name rather than written,
    /// since `done` could never find such a reading fresh and would say
    /// only that it is not.
    pub fn for_scope(
        instrument: &str,
        scope: &str,
        manifest: &Manifest,
        root: &Path,
    ) -> Result<Reading> {
        let path = manifest.scope_path(scope).with_context(|| {
            format!(
                "{instrument} over {scope:?}: the manifest names no crate, seam or generated file so called and the pinned corpus has no such directory, so there is no path to stamp the reading over"
            )
        })?;
        Reading::stamped_over(instrument, scope, root, &path)
    }

    /// A reading of `instrument` over `scope`, stamped over `path`, relative
    /// to the root, and timed now: the constructor for a scope the manifest
    /// maps to no sources, which `done` never holds fresh. `permit`'s
    /// readings are one: their scope is the rule that fired and their tree
    /// is the file the call touched, or the manifest for a refusal to judge
    /// (harness-design.md §4.4). The two constructors above come through
    /// here over the manifest's path.
    pub fn stamped_over(
        instrument: &str,
        scope: &str,
        root: &Path,
        path: &Path,
    ) -> Result<Reading> {
        let stamp = Stamp::take(root, path)?;
        let mut reading = Reading::new(instrument, scope);
        reading.tree = stamp.tree;
        reading.commit = stamp.commit;
        reading.at = now()?;
        Ok(reading)
    }

    /// A reading of `instrument` over `scope` with every other field at its
    /// empty value and `at` at the Unix epoch; the instrument fills the
    /// rest. Its `tree` is empty, which is no scope's hash, so `done` finds
    /// it fresh for nothing: the reading of a crate not yet created, which
    /// nothing measured, and the reading a rendering's test lays in a store
    /// that is no git repository.
    pub fn new(instrument: &str, scope: &str) -> Reading {
        Reading {
            instrument: instrument.to_owned(),
            scope: scope.to_owned(),
            rule: String::new(),
            status: Status::Skip,
            numbers: BTreeMap::new(),
            message: String::new(),
            adr: String::new(),
            tree: String::new(),
            commit: String::new(),
            at: EPOCH
                .parse()
                .unwrap_or_else(|_| unreachable!("{EPOCH} is a datetime")),
            session: String::new(),
            groups: BTreeMap::new(),
        }
    }
}

impl Reading {
    /// The tree hash the reading was stamped with; empty when it was not.
    pub fn tree(&self) -> &str {
        &self.tree
    }

    /// Whether the reading was taken over the content `hash` names: the
    /// scope's [`crate::git::tree_hash`] now, `done`'s question, or the
    /// index's hash of it, pre-commit's (#1143). The one statement of
    /// fresh (harness-design.md §4.2, §4.5 condition 1).
    pub fn fresh_for(&self, hash: &str) -> bool {
        self.tree == hash
    }

    /// Record one count under `key`.
    pub fn count(&mut self, key: &str, n: u64) -> Result<()> {
        let n = i64::try_from(n).with_context(|| format!("{key} = {n} overflows a reading"))?;
        self.numbers.insert(key.to_owned(), n.into());
        Ok(())
    }

    /// One group of the reading's fold: a count under a key the run
    /// named. Unlike [`Reading::count`] the key is data, so nothing here
    /// holds it to a spelling.
    pub fn group(&mut self, key: &str, n: u64) -> Result<()> {
        let n = i64::try_from(n).with_context(|| format!("{key} = {n} overflows a reading"))?;
        self.groups.insert(key.to_owned(), n.into());
        Ok(())
    }

    /// The reading as JSON, `at` as an RFC 3339 string: the log line, and
    /// what `status --format json` carries.
    pub fn to_json(&self) -> Result<serde_json::Value> {
        let mut value = serde_json::to_value(self).context("rendering the reading as JSON")?;
        value["at"] = serde_json::Value::String(self.at.to_string());
        Ok(value)
    }
}

impl Reading {
    /// One line of the log back into a reading; the inverse of `to_json`.
    pub fn from_json(line: &str) -> Result<Reading> {
        let mut value: serde_json::Value =
            serde_json::from_str(line).context("parsing a log line")?;
        let at = value["at"]
            .as_str()
            .context("a log line without `at`")?
            .parse::<Datetime>()
            .context("a log line whose `at` is not a datetime")?;
        value["at"] = serde_json::to_value(at).context("the datetime as toml's value")?;
        serde_json::from_value(value).context("a log line that is not a reading")
    }
}

const EPOCH: &str = "1970-01-01T00:00:00Z";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Pass,
    Fail,
    Skip,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Pass => "pass",
            Status::Fail => "fail",
            Status::Skip => "skip",
        }
    }
}

/// One entry of `numbers`: a count or a measurement.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum Number {
    Int(i64),
    Float(f64),
}

impl From<i64> for Number {
    fn from(n: i64) -> Number {
        Number::Int(n)
    }
}

impl Number {
    /// A count as a number; one past `i64::MAX` is not a count this store
    /// will see, and saturates.
    pub fn count(n: impl TryInto<i64>) -> Number {
        Number::Int(n.try_into().unwrap_or(i64::MAX))
    }
}

impl Number {
    /// The number as a float, for a comparison against a bound. A count
    /// past 2^53 is not one this store will see; the cast is exact below it.
    #[allow(clippy::cast_precision_loss)]
    pub fn as_f64(self) -> f64 {
        match self {
            Number::Int(n) => n as f64,
            Number::Float(n) => n,
        }
    }
}

impl From<f64> for Number {
    fn from(n: f64) -> Number {
        Number::Float(n)
    }
}

impl std::fmt::Display for Number {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Number::Int(n) => write!(f, "{n}"),
            Number::Float(n) => write!(f, "{n}"),
        }
    }
}

/// The store: a directory of readings and its log.
#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    /// The store of the workspace at `root`.
    pub fn of_workspace(root: &Path) -> Store {
        Store::at(root.join(DIR))
    }

    /// A store rooted at `dir`, whether or not it exists yet.
    pub fn at(dir: impl Into<PathBuf>) -> Store {
        Store { root: dir.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The file of one instrument and scope.
    pub fn path(&self, instrument: &str, scope: &str) -> Result<PathBuf> {
        for part in [instrument, scope] {
            let clean = Path::new(part)
                .components()
                .all(|c| matches!(c, Component::Normal(_)));
            if part.is_empty() || !clean {
                bail!(
                    "a reading's instrument and scope name a file under the store; {part:?} does not"
                );
            }
        }
        Ok(self.root.join(instrument).join(format!("{scope}.toml")))
    }

    /// Record what an instrument held: every reading written, in order,
    /// as its file, overwritten, and one line of the log, and the exit
    /// the instrument owes for them. The one writer of the store
    /// (harness-design.md §4.2); an instrument that ignores the outcome
    /// says so, as `permit` does, whose exit is 0 whatever it decides
    /// (§4.4), and a measurement records through
    /// [`Store::record_measurement`], which owes none.
    pub fn record(&self, readings: &[Reading]) -> Result<Outcome> {
        for reading in readings {
            self.write(reading)?;
        }
        Ok(Outcome::of(readings))
    }

    /// Record what a measurement read, every reading written as
    /// [`Store::record`] writes it, and owe no exit for it: the one
    /// case the exit rule leaves out, stated here on the writer. A bench's
    /// reading keeps `pass` or `fail` against its bar, so `status` shows
    /// it, and the command exits 0 whatever the reading says, since a
    /// bench number is a claim a person reads with the A/A control
    /// (AGENTS.md; harness-design.md §4.4; #61). `permit --bench` records
    /// through here, and so does a `bench` instrument when one exists: a
    /// measurement's exit is not its status.
    pub fn record_measurement(&self, readings: &[Reading]) -> Result<()> {
        self.record(readings).map(|_| ())
    }

    /// One reading: its file, overwritten, and its line of the log, which
    /// goes through [`Store::append_line`], the one append.
    fn write(&self, reading: &Reading) -> Result<()> {
        let path = self.path(&reading.instrument, &reading.scope)?;
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let text = toml::to_string(reading).context("rendering the reading")?;
        fs::write(&path, text).with_context(|| format!("writing {}", path.display()))?;

        let mut line = reading.to_json()?.to_string();
        line.push('\n');
        self.append_line(line.as_bytes())
    }

    /// Every line of `lines`, a log another store wrote, appended to this
    /// store's log, each through [`Store::append_line`]: the retire of a
    /// worktree (`xtask worktree retire`) lands its log here, and a retire
    /// during a run cannot tear a line of either. A line is appended as
    /// it is, bytes and not text, so one the worktree tore stays torn and
    /// tears nothing beside it; a last line without its newline is given
    /// one. How many lines were appended.
    pub fn append(&self, lines: &[u8]) -> Result<usize> {
        let mut count = 0;
        for line in lines.split_inclusive(|b| *b == b'\n') {
            if line.ends_with(b"\n") {
                self.append_line(line)?;
            } else {
                let mut whole = line.to_vec();
                whole.push(b'\n');
                self.append_line(&whole)?;
            }
            count += 1;
        }
        Ok(count)
    }

    /// The one append to the log, and the rule that a line reaches it whole
    /// or not at all (#56): `line`, rendered whole with its newline, is
    /// handed to the file in one `write` on a descriptor opened with
    /// `O_APPEND`, so two instruments appending at once, which pre-commit
    /// and a track's worktrees do, cannot interleave inside a line. POSIX
    /// promises a whole write only under `PIPE_BUF` (4096 bytes on Linux,
    /// 512 on macOS), and the platforms the harness runs on land a single
    /// `O_APPEND` write to a regular file whole in practice; a reading
    /// longer than `PIPE_BUF` is the one case not covered on paper: it is
    /// still handed over in one call, and if the kernel splits it the line
    /// tears, which [`Store::log`] counts and names rather than fails on.
    /// A short write is an error, never retried, since a retry could only
    /// tear the line it means to finish.
    fn append_line(&self, line: &[u8]) -> Result<()> {
        let log = self.root.join(LOG);
        if let Some(dir) = log.parent() {
            fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log)
            .with_context(|| format!("opening {}", log.display()))?;
        let written = file
            .write(line)
            .with_context(|| format!("appending to {}", log.display()))?;
        if written != line.len() {
            bail!(
                "appending to {}: {written} of {} bytes were written, so the line is torn",
                log.display(),
                line.len()
            );
        }
        Ok(())
    }

    /// The reading of one instrument and scope, if one has been written.
    pub fn read(&self, instrument: &str, scope: &str) -> Result<Option<Reading>> {
        let path = self.path(instrument, scope)?;
        if !path.exists() {
            return Ok(None);
        }
        Self::read_file(&path).map(Some)
    }

    /// The append-only log, oldest first: what the store held across every
    /// run and every session, and the lines it could not read. A missing
    /// log is empty. A line that is not a reading, which a write the kernel
    /// split leaves behind (see [`Store::append_line`]), is counted by its
    /// number and skipped rather than failing the whole log, so a torn line
    /// degrades one section and never the session.
    pub fn log(&self) -> Result<Log> {
        let path = self.root.join(LOG);
        if !path.exists() {
            return Ok(Log::default());
        }
        let text =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        let mut log = Log::default();
        for (i, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            match Reading::from_json(line) {
                Ok(reading) => log.readings.push(reading),
                Err(_) => log.unreadable.push(i + 1),
            }
        }
        Ok(log)
    }

    /// Every reading in the store, in path order. A missing store is empty.
    pub fn readings(&self) -> Result<Vec<Reading>> {
        let mut files = Vec::new();
        if self.root.is_dir() {
            walk(&self.root, &mut files)?;
        }
        files.sort();
        files
            .iter()
            .filter(|p| p.extension().is_some_and(|e| e == "toml"))
            .map(|p| Self::read_file(p))
            .collect()
    }

    fn read_file(path: &Path) -> Result<Reading> {
        let text =
            fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("{}", path.display()))
    }
}

/// The log as read: every line that is a reading, in the log's order, and
/// the number of every line that is not, 1-based as an editor counts.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Log {
    pub readings: Vec<Reading>,
    pub unreadable: Vec<usize>,
}

impl Log {
    /// What a rendering says about the lines it could not read, `log.jsonl:
    /// 1 line unreadable at 10019.`, at most five named and the rest
    /// counted; none when every line read. One string, so every section
    /// says it the same way.
    pub fn unreadable_note(&self) -> Option<String> {
        if self.unreadable.is_empty() {
            return None;
        }
        let lines: Vec<String> = self.unreadable.iter().map(usize::to_string).collect();
        let names: Vec<&str> = lines.iter().map(String::as_str).collect();
        let noun = if self.unreadable.len() == 1 {
            "line"
        } else {
            "lines"
        };
        Some(format!(
            "{LOG}: {} {noun} unreadable at {}.",
            self.unreadable.len(),
            listed(&names)
        ))
    }
}

/// The commits a track stands on: the first-parent chain of `HEAD`, each
/// with its distance from the tip. A branch merged into the track sits on
/// a second parent and a branch cut behind the tip is off it past the cut,
/// so the readings their worktrees took, which `worktree retire` appends
/// into the track's store, are not taken on it (#787).
#[derive(Debug, Clone, Default)]
pub struct Chain {
    distance: BTreeMap<String, usize>,
}

impl Chain {
    /// The chain of `HEAD` at `root`; empty where `root` is no repository,
    /// so a store beside none stands on its unstamped readings alone.
    pub fn of_head(root: &Path) -> Result<Chain> {
        if !root.join(".git").exists() {
            return Ok(Chain::default());
        }
        let ids = crate::git::git(root, &["rev-list", "--first-parent", "HEAD"], None)?;
        let distance = ids.lines().enumerate().map(|(n, id)| (id.to_owned(), n));
        Ok(Chain {
            distance: distance.collect(),
        })
    }

    /// How far `commit`, whole or abbreviated as a reading records it, is
    /// from the tip; none when it is off the chain.
    fn distance(&self, commit: &str) -> Option<usize> {
        let (id, n) = self.distance.range(commit.to_owned()..).next()?;
        (!commit.is_empty() && id.starts_with(commit)).then_some(*n)
    }

    /// The reading of each scope of `instrument` that stands for the track,
    /// in scope order: the one taken at the tip or at its nearest ancestor
    /// on the chain, the last written of those taken at one commit. One
    /// taken off the chain stands for nothing, and one taken at no commit,
    /// a scope nothing measured, stands behind every one taken on it.
    pub fn standing<'a>(&self, log: &'a [Reading], instrument: &str) -> Vec<&'a Reading> {
        let mut nearest: BTreeMap<&str, (usize, &Reading)> = BTreeMap::new();
        for reading in log.iter().filter(|r| r.instrument == instrument) {
            let rank = match self.distance(&reading.commit) {
                Some(n) => n,
                None if reading.commit.is_empty() => usize::MAX,
                None => continue,
            };
            if nearest
                .get(reading.scope.as_str())
                .is_none_or(|(held, _)| rank <= *held)
            {
                nearest.insert(&reading.scope, (rank, reading));
            }
        }
        nearest.into_values().map(|(_, r)| r).collect()
    }
}

/// What an instrument's exit says about the readings it wrote: 1 while
/// any reading fails, 0 when every one passes or is skipped. The rule
/// harness-design.md §4.5 states for `done` (exit 1 while anything is
/// unmet), made every instrument's here, so no instrument maps a status
/// to an exit on its own. A measurement alone stands outside it: it
/// records through [`Store::record_measurement`] and owes no exit (§4.4,
/// #61).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// Every reading passed or was skipped: exit 0.
    Met,
    /// `failed` of the `written` readings fail: exit 1.
    Unmet { failed: usize, written: usize },
}

impl Outcome {
    /// The rule over a set of readings.
    pub fn of(readings: &[Reading]) -> Outcome {
        let failed = readings.iter().filter(|r| r.status == Status::Fail).count();
        if failed == 0 {
            Outcome::Met
        } else {
            Outcome::Unmet {
                failed,
                written: readings.len(),
            }
        }
    }

    /// The outcome as the instrument's return: `main` exits 1 on an error
    /// and 0 otherwise, so an unmet outcome is an error naming the count.
    pub fn exit(self) -> Result<()> {
        match self {
            Outcome::Met => Ok(()),
            Outcome::Unmet { failed, written } => bail!("{failed} of {written} readings fail"),
        }
    }
}

/// The `s` a count of one does not carry: every instrument's message
/// counts something, so the word lives beside the shape they all write.
pub(crate) fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// The current time in UTC as a TOML datetime, to the second: what a
/// reading's `at` is, in one format, so its text orders as its instant
/// does.
pub fn now() -> Result<Datetime> {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("the clock is before 1970")?
        .as_secs();
    let (date, time) = (secs / 86_400, secs % 86_400);
    let (y, m, d) = civil_from_days(i64::try_from(date).context("the clock is past the calendar")?);
    let text = format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        time / 3600,
        time / 60 % 60,
        time % 60
    );
    text.parse()
        .with_context(|| format!("{text} is not a datetime"))
}

/// Seconds since 1970-01-01 of a reading's `at`, which `now` writes in UTC
/// to the second; what a window such as the fourteen days of the silent
/// query (harness-design.md §7) is measured in.
pub fn epoch_seconds(at: &Datetime) -> Result<i64> {
    let date = at.date.with_context(|| format!("{at} has no date"))?;
    let time = at.time.unwrap_or(Time {
        hour: 0,
        minute: 0,
        second: 0,
        nanosecond: 0,
    });
    let days = days_from_civil(
        i64::from(date.year),
        i64::from(date.month),
        i64::from(date.day),
    );
    let seconds =
        i64::from(time.hour) * 3600 + i64::from(time.minute) * 60 + i64::from(time.second);
    let offset = at.offset.map_or(0, |o| match o {
        Offset::Z => 0,
        Offset::Custom { minutes } => i64::from(minutes) * 60,
    });
    Ok(days * 86_400 + seconds - offset)
}

/// A proleptic Gregorian date to days since 1970-01-01 (Howard Hinnant's
/// `days_from_civil`), the inverse of `civil_from_days`.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = y - i64::from(m <= 2);
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Days since 1970-01-01 to a proleptic Gregorian date (Howard Hinnant's
/// `civil_from_days`).
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    (y, m, d)
}

/// Every file under `dir`, recursively, in directory order.
pub(crate) fn walk(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in fs::read_dir(dir).with_context(|| format!("listing {}", dir.display()))? {
        let path = entry?.path();
        if path.is_dir() {
            walk(&path, out)?;
        } else {
            out.push(path);
        }
    }
    Ok(())
}

/// At most five names, and how many more there are: how a reading's
/// message names what missed, the same way in every lane.
pub(crate) fn listed(names: &[&str]) -> String {
    let shown = names.iter().take(5).copied().collect::<Vec<_>>().join(", ");
    if names.len() > 5 {
        format!("{shown} and {} more", names.len() - 5)
    } else {
        shown
    }
}

#[cfg(test)]
mod tests {
    use super::{civil_from_days, days_from_civil, listed};

    #[test]
    fn at_most_five_names_are_listed_and_the_rest_counted() {
        assert_eq!(listed(&["a", "b"]), "a, b");
        assert_eq!(
            listed(&["a", "b", "c", "d", "e", "f"]),
            "a, b, c, d, e and 1 more"
        );
    }

    #[test]
    fn civil_dates() {
        assert_eq!(civil_from_days(0), (1970, 1, 1));
        assert_eq!(civil_from_days(19_782), (2024, 2, 29));
        assert_eq!(civil_from_days(20_703), (2026, 9, 7));
        for day in [0, 19_782, 20_703, -1, 1_000_000] {
            let (y, m, d) = civil_from_days(day);
            assert_eq!(days_from_civil(y, m, d), day);
        }
    }
}
