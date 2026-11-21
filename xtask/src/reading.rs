//! The reading and its store (harness-design.md §4.2). Every instrument ends
//! by writing its readings through [`Store::record`], the one writer; every
//! rendering reads them; nothing else touches the store. The store is
//! `target/harness/<instrument>/<scope>.toml`, the scope's last reading,
//! overwritten on each run; the index beside it, one file per key; and the
//! log, `target/harness/log/<instrument>.jsonl`, append-only, one line per
//! reading ([`Store`], revision 83). The clock a reading's `at` is taken
//! from, and the calendar that reads it back, live here too.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use toml::value::{Datetime, Offset, Time};

use crate::key::Snapshot;
use crate::manifest::{Crate, Manifest};
use crate::oracle::Pin;
use crate::owner::SessionId;
use crate::sources;

/// Where the store lives, relative to the workspace root.
pub const DIR: &str = "target/harness";
/// The log's directory, inside the store: one append-only file per
/// instrument (revision 83).
pub const LOG: &str = "log";
/// The file of the log a line that is no reading is kept in, as it was.
pub const UNREAD: &str = "unread";

/// The version of a reading's shape, written as `v` on every reading
/// (harness-design.md §4.2): 2 since the scope's key is spelled `key`.
/// A reading written before carries no `v` and is version 1,
/// which spells the key `tree`; the store reads both, and refuses a `v` it
/// does not know. A field it does not know it ignores, in the store's
/// files and the log's lines alike, as a handed-in line and a `--format
/// jsonl` row are read (§4.2). So a field is added under the same
/// `v` only where a reading without it is never read as fresher than one
/// with it, since an older harness reads the newer reading as if the field
/// were absent; a field removed, renamed or retyped, a meaning changed, or
/// a field whose absence would read fresher is a new `v`.
pub const V: u32 = 2;

/// The `v` of a reading: written as [`V`] whatever version it was read
/// at, since a reading read back is held in this version's shape, and
/// read as any version this harness knows, absent for the first.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct Version;

impl Serialize for Version {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_u32(V)
    }
}

impl<'de> Deserialize<'de> for Version {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Version, D::Error> {
        match u32::deserialize(deserializer)? {
            V => Ok(Version),
            v => Err(serde::de::Error::custom(format!(
                "a reading of v = {v}, which this harness does not know: it reads v = {V}, and a reading with no `v`, the first version"
            ))),
        }
    }
}

/// `at` as either side keeps it: toml's datetime in the store, or the
/// text [`Reading::to_json`] writes in the log, whose own `Deserialize`
/// reads only toml's form.
fn either_at<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Datetime, D::Error> {
    struct Either;
    impl<'de> serde::de::Visitor<'de> for Either {
        type Value = Datetime;
        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a datetime, or its RFC 3339 text")
        }
        fn visit_str<E: serde::de::Error>(self, text: &str) -> Result<Datetime, E> {
            text.parse().map_err(E::custom)
        }
        fn visit_map<A: serde::de::MapAccess<'de>>(self, map: A) -> Result<Datetime, A::Error> {
            Datetime::deserialize(serde::de::value::MapAccessDeserializer::new(map))
        }
    }
    deserializer.deserialize_any(Either)
}

/// One reading, the one shape every instrument writes. Read back, a field
/// it does not know is ignored and a `v` it does not know refused ([`V`]).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Reading {
    /// The version of the shape, [`V`]; absent on a reading of the first.
    #[serde(default)]
    v: Version,
    /// The subcommands that write one, in the order `cli::Command` has
    /// them: `api-check | permit | done | track-work | ticket | usage |
    /// review | bench | budget | clippy | test | miri | phpt | port-map |
    /// compile-identity | crosscheck | trace-dump | hostile | fuzz | fold | dispatch-census | fpm-compare | soak | unsafe-audit |
    /// steer | divergence-reason | witness | vocab`; `mutants` is named by
    /// harness-design.md §4.2 and not built yet.
    pub instrument: String,
    /// A crate, a corpus subset, a bar's id such as `G1`, a ticket's
    /// number, or `"workspace"`.
    pub scope: String,
    /// A manifest rule id when one applies, else empty; a `clippy`
    /// reading names each `deny_lints` rule that reaches its crate,
    /// separated by `, `.
    pub rule: String,
    pub status: Status,
    pub numbers: BTreeMap<String, Number>,
    pub message: String,
    /// From the manifest when the rule names one, else empty.
    pub adr: String,
    /// The scope's key at the time, [`crate::key::Snapshot::key`]: the
    /// hash of the scope's files and of every workspace crate in its
    /// declared dependency closure; what `done` holds against the
    /// scope's key now to call the reading fresh (harness-design.md §4.2,
    /// §13 D.18). Spelled `tree` by a reading of the first version, which
    /// is read as this one. Private, and set by the constructors alone: a
    /// reading made through [`Reading::for_crate`] or
    /// [`Reading::for_scope`] is hashed over the manifest's mapping of its
    /// scope, [`Manifest::scope_path`], the path `done` hashes; one made
    /// through [`Reading::stamped_over`] says which path it was hashed over.
    #[serde(alias = "tree")]
    key: String,
    pub commit: String,
    #[serde(deserialize_with = "either_at")]
    pub at: Datetime,
    /// The Claude Code session the process ran in, or the session a hook
    /// or caller named; empty where there was none, which says nothing of
    /// who ran it (harness-design.md §4.2). An instrument names one where
    /// it has one: the call a `permit` reading judged, from the hook's
    /// `session_id`, the session a `ticket` call named or was dispatched
    /// in, the one a `done --hook` verdict was taken in, the one a `usage`
    /// reading measured. A reading handed to the writer naming none is
    /// given the session the writer runs in ([`Store::record`]).
    #[serde(default)]
    pub session: String,
    /// A lane's named groups: one count per key, where the keys are the
    /// run's own and not a fixed set `numbers` could name. The phpt
    /// census is the first, a count per construct the compiler refused,
    /// and the reason this is not `numbers`: `status` tables
    /// `numbers` as one line of `key=value`, which a fold over an open
    /// set of sentences is not. Empty for every instrument that writes
    /// none, and for a reading written before this field existed.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub groups: BTreeMap<String, Number>,
    /// The [`Pin::digest`] of the oracle pin the run was taken under, for
    /// an instrument of [`UNDER_RECIPE`]; `None` for every other.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recipe: Option<String>,
}

/// The line an instrument prints for a reading: its status, instrument,
/// scope and message.
impl std::fmt::Display for Reading {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Reading {
            status,
            instrument,
            scope,
            message,
            ..
        } = self;
        write!(f, "{} {instrument} {scope}: {message}", status.as_str())
    }
}

/// The instruments whose runs are taken under the oracle's recipe: a
/// reading of one is fresh only under the pin it records.
pub const UNDER_RECIPE: [&str; 6] = [
    "witness",
    "phpt",
    "crosscheck",
    "trace-dump",
    "dispatch-census",
    "fpm-compare",
];

impl Reading {
    /// A reading of `instrument` over a crate, stamped over the crate's
    /// sources as `snapshot` holds them: the path the manifest maps the
    /// crate to ([`Crate::path`]), which is the path `done` hashes to call
    /// the reading fresh (harness-design.md §4.5 condition 1). A crate not
    /// yet created is measured by nothing and its reading carries no
    /// stamp, so `done` finds it fresh for nothing.
    pub fn for_crate(instrument: &str, krate: &Crate, snapshot: &Snapshot) -> Result<Reading> {
        if !sources::exists(snapshot.root(), krate) {
            return Ok(Reading::new(instrument, &krate.name));
        }
        Reading::stamped_over(instrument, &krate.name, snapshot, &krate.path())
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
        snapshot: &Snapshot,
    ) -> Result<Reading> {
        let path = manifest.scope_path(scope).with_context(|| {
            format!(
                "{instrument} over {scope:?}: the manifest names no crate, seam or generated file so called and the pinned corpus has no such directory, so there is no path to stamp the reading over"
            )
        })?;
        Reading::stamped_over(instrument, scope, snapshot, &path)
    }

    /// A reading of `instrument` over `scope`, stamped over `path`, relative
    /// to the root, as `snapshot` holds it, and timed now: the constructor
    /// for a scope the manifest maps to no sources, which `done` never
    /// holds fresh. The snapshot is the one the instrument took before it
    /// measured ([`Snapshot::disk`]), so the key is that of what it
    /// measured and not of what the tree became while it ran. `permit`'s
    /// readings are one: their scope is the rule that fired and their key
    /// is the file's the call touched, or the manifest for a refusal to judge
    /// (harness-design.md §4.4). The two constructors above come through
    /// here over the manifest's path.
    pub fn stamped_over(
        instrument: &str,
        scope: &str,
        snapshot: &Snapshot,
        path: &Path,
    ) -> Result<Reading> {
        let stamp = snapshot.stamp(path)?;
        let mut reading = Reading::new(instrument, scope);
        reading.key = stamp.key;
        reading.commit = stamp.commit;
        reading.at = now()?;
        Ok(reading)
    }

    /// A reading of `instrument` over `scope` whose key is `key`, timed now:
    /// the record `witness --accept` writes of a verdict it captured, whose
    /// scope is the crate's accepted verdicts and whose key is the verdict's
    /// block ([`crate::witness::record_accepted`]), which `permit` asks
    /// for as it asks for a crate's key.
    pub(crate) fn keyed(instrument: &str, scope: &str, key: String) -> Result<Reading> {
        let mut reading = Reading::new(instrument, scope);
        reading.key = key;
        reading.at = now()?;
        Ok(reading)
    }

    /// A reading of `instrument` over `scope` with every other field at its
    /// empty value and `at` at the Unix epoch; the instrument fills the
    /// rest. Its key is empty, which is no scope's hash, so `done` finds
    /// it fresh for nothing: the reading of a crate not yet created, which
    /// nothing measured, and the reading a rendering's test lays in a store
    /// that is no git repository.
    pub fn new(instrument: &str, scope: &str) -> Reading {
        Reading {
            v: Version,
            instrument: instrument.to_owned(),
            scope: scope.to_owned(),
            rule: String::new(),
            status: Status::Skip,
            numbers: BTreeMap::new(),
            message: String::new(),
            adr: String::new(),
            key: String::new(),
            commit: String::new(),
            at: EPOCH
                .parse()
                .unwrap_or_else(|_| unreachable!("{EPOCH} is a datetime")),
            session: String::new(),
            groups: BTreeMap::new(),
            recipe: None,
        }
    }
}

impl Reading {
    /// The key the reading was stamped with; empty when it was not.
    pub fn key(&self) -> &str {
        &self.key
    }

    /// Whether the reading was taken over the content `hash` names: the
    /// scope's key now ([`crate::key::Snapshot::key`]), `done`'s question, or the
    /// index's key of it, pre-commit's; and, for an instrument of
    /// [`UNDER_RECIPE`], under the pin whose [`Pin::digest`] is `pin`.
    /// The one statement of fresh (harness-design.md §4.2, §4.5 condition 1).
    pub fn fresh_for(&self, hash: &str, pin: &str) -> bool {
        self.key == hash
            && (!UNDER_RECIPE.contains(&self.instrument.as_str())
                || self.recipe.as_deref() == Some(pin))
    }

    /// The reading as taken under `pin`'s recipe.
    #[must_use]
    pub fn under(mut self, pin: &Pin) -> Reading {
        self.recipe = Some(pin.digest().to_owned());
        self
    }

    /// The count under `key`: none where the reading has no number there,
    /// or a measurement.
    pub(crate) fn int(&self, key: &str) -> Option<i64> {
        match self.numbers.get(key) {
            Some(Number::Int(n)) => Some(*n),
            _ => None,
        }
    }

    /// Record one count under `key`.
    pub fn count(&mut self, key: &str, n: u64) -> Result<()> {
        let n = i64::try_from(n).with_context(|| format!("{key} = {n} overflows a reading"))?;
        self.set(key, n);
        Ok(())
    }

    /// Record one number under `key`, a measurement or a count that fits.
    pub(crate) fn set(&mut self, key: impl Into<String>, n: impl Into<Number>) {
        self.numbers.insert(key.into(), n.into());
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
    /// One line of the log back into a reading; the inverse of `to_json`,
    /// in one pass: `at` is read from the text `to_json` writes.
    /// A field the reading has not is ignored and a `v` this harness does
    /// not know refused, as in the store ([`V`]).
    pub fn from_json(line: &str) -> Result<Reading> {
        serde_json::from_str(line).context("a log line that is not a reading")
    }
}

/// A line of a log handed in, in either shape the published history has
/// kept a reading in (harness-design.md §4.6).
#[derive(Deserialize)]
#[serde(untagged)]
enum Kept {
    /// A reading whole, as the log writes it; a field this build does
    /// not know is ignored, so a newer harness's line still reads.
    Whole {
        #[serde(default)]
        v: Version,
        instrument: String,
        scope: String,
        status: Status,
        #[serde(deserialize_with = "either_at")]
        at: Datetime,
        #[serde(default)]
        rule: String,
        #[serde(default)]
        numbers: BTreeMap<String, Number>,
        #[serde(default)]
        message: String,
        #[serde(default)]
        adr: String,
        #[serde(default, alias = "tree")]
        key: String,
        #[serde(default)]
        commit: String,
        #[serde(default)]
        session: String,
        #[serde(default)]
        groups: BTreeMap<String, Number>,
        #[serde(default)]
        recipe: Option<String>,
    },
    /// The lean row the history kept before it kept readings whole:
    /// instrument, scope, status, at, commit and numbers.
    Lean {
        i: String,
        s: String,
        t: Status,
        #[serde(deserialize_with = "either_at")]
        a: Datetime,
        #[serde(default)]
        c: String,
        #[serde(default)]
        n: BTreeMap<String, Number>,
    },
}

impl Reading {
    /// One line of a log handed in back into a reading, whichever shape
    /// the history kept it in; the fields a lean row has not are left as
    /// [`Reading::new`] leaves them.
    fn from_kept(line: &str) -> Result<Reading> {
        let kept: Kept = serde_json::from_str(line).context("a line that is no reading")?;
        Ok(match kept {
            Kept::Whole {
                v,
                instrument,
                scope,
                status,
                at,
                rule,
                numbers,
                message,
                adr,
                key,
                commit,
                session,
                groups,
                recipe,
            } => Reading {
                v,
                rule,
                status,
                numbers,
                message,
                adr,
                key,
                commit,
                at,
                session,
                groups,
                recipe,
                ..Reading::new(&instrument, &scope)
            },
            Kept::Lean { i, s, t, a, c, n } => Reading {
                status: t,
                numbers: n,
                commit: c,
                at: a,
                ..Reading::new(&i, &s)
            },
        })
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
    /// A pass where `met`, a fail where not.
    pub(crate) fn of(met: bool) -> Status {
        if met { Status::Pass } else { Status::Fail }
    }

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

/// The store: a directory of readings, their index and their log
/// (harness-design.md §4.2, revision 83).
///
/// - `<instrument>/<scope>.toml`, the last reading of each scope,
///   overwritten on each run.
/// - `<instrument>/<scope>/<key>.toml`, the index: the latest reading of
///   a scope over each key it was taken over, the newest [`KEPT`] keys of
///   a scope kept. A reading taken under an oracle pin is kept per pin
///   too, `<key>-<pin>.toml`. Each file is written whole by a rename, so
///   two writers at once need no lock and a reader sees one or the
///   other. "The latest reading over a key" is one file read.
/// - `log/<instrument>.jsonl`, the log: append-only history, one line per
///   reading, one file per instrument, read by the renderings of history
///   (the permits section, usage, the dashboard) and by no instrument's
///   question about a key.
#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
    /// The Claude Code session the process writes in, which the writer
    /// gives every reading that names none; `None` in a person's terminal.
    session: Option<SessionId>,
}

/// How many keys of one scope the index keeps, newest first: `done`
/// asks for the key of the tree now, which the last run wrote, and a key
/// older than this many runs of the scope is one no tree returns to often
/// enough to keep. A key the index has let go reads as never measured, so
/// the instrument runs again.
pub const KEPT: usize = 32;

impl Store {
    /// The store of the workspace at `root`.
    pub fn of_workspace(root: &Path) -> Store {
        Store::at(root.join(DIR))
    }

    /// A store rooted at `dir`, whether or not it exists yet, written in
    /// the session the process runs in ([`SessionId::current`]).
    pub fn at(dir: impl Into<PathBuf>) -> Store {
        Store {
            root: dir.into(),
            session: SessionId::current(),
        }
    }

    /// The same store written in `session` instead: what a test names in
    /// place of the session its own process runs in.
    #[cfg(test)]
    pub(crate) fn in_session(self, session: Option<SessionId>) -> Store {
        Store { session, ..self }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The file of one instrument and scope.
    pub fn path(&self, instrument: &str, scope: &str) -> Result<PathBuf> {
        let dir = self.dir_of(instrument, scope)?;
        let mut name = dir.clone().into_os_string();
        name.push(".toml");
        Ok(PathBuf::from(name))
    }

    /// `<instrument>/<scope>` under the store: the index's directory of
    /// the scope, beside the scope's file.
    fn dir_of(&self, instrument: &str, scope: &str) -> Result<PathBuf> {
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
        Ok(self.root.join(instrument).join(scope))
    }

    /// The log of one instrument.
    fn shard(&self, instrument: &str) -> Result<PathBuf> {
        let clean = Path::new(instrument)
            .components()
            .all(|c| matches!(c, Component::Normal(_)));
        if instrument.is_empty() || !clean || instrument.contains('/') {
            bail!("an instrument names a file of the log; {instrument:?} does not");
        }
        Ok(self.root.join(LOG).join(format!("{instrument}.jsonl")))
    }

    /// Where the index keeps `reading`: none for one whose key is empty,
    /// which names no content.
    fn indexed(&self, reading: &Reading) -> Result<Option<PathBuf>> {
        if reading.key.is_empty() {
            return Ok(None);
        }
        let dir = self.dir_of(&reading.instrument, &reading.scope)?;
        Ok(Some(dir.join(format!(
            "{}.toml",
            index_name(&reading.key, reading.recipe.as_deref())
        ))))
    }

    /// Record what an instrument held: every reading written, in order,
    /// as its file, overwritten, its index entry and one line of its log,
    /// and the exit the instrument owes for them. The one writer of the
    /// store (harness-design.md §4.2), and so the one place a reading is
    /// given the session it was taken in: a reading that names none is
    /// written with the store's, the session the process runs in, and one
    /// a hook or caller named keeps it (Round 1, Q7 (a)). An instrument that ignores the
    /// outcome says so, as `permit` does, whose exit is 0 whatever it
    /// decides (§4.4), and a measurement records through
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
    /// (AGENTS.md; harness-design.md §4.4). `permit --bench` records
    /// through here, and so does a `bench` instrument when one exists: a
    /// measurement's exit is not its status.
    pub fn record_measurement(&self, readings: &[Reading]) -> Result<()> {
        self.record(readings).map(|_| ())
    }

    /// One reading, given the store's session where it names none
    /// ([`Store::in_its_session`]): its file, replaced whole through
    /// [`replace`], its index entry, and its line of the log, which goes
    /// through [`Store::append_line`], the one append.
    fn write(&self, reading: &Reading) -> Result<()> {
        let reading = &*self.in_its_session(reading);
        let path = self.path(&reading.instrument, &reading.scope)?;
        let text = toml::to_string(reading).context("rendering the reading")?;
        replace(&path, text.as_bytes())?;
        if let Some(entry) = self.indexed(reading)? {
            replace(&entry, text.as_bytes())?;
            prune(entry.parent().unwrap_or(&self.root))?;
        }
        let mut line = reading.to_json()?.to_string();
        line.push('\n');
        Self::append_line(&self.shard(&reading.instrument)?, line.as_bytes())
    }

    /// `reading` with the session the store writes in, where it names
    /// none and the store has one; as it was handed otherwise.
    fn in_its_session<'a>(&self, reading: &'a Reading) -> Cow<'a, Reading> {
        match &self.session {
            Some(session) if reading.session.is_empty() => {
                let mut taken = reading.clone();
                session.as_str().clone_into(&mut taken.session);
                Cow::Owned(taken)
            }
            _ => Cow::Borrowed(reading),
        }
    }

    /// Every reading another store's log holds, landed in this one's log
    /// and index, and that log emptied. `worktree retire` lands a
    /// worktree's readings in main's store, and `worktree land` lands
    /// `_merge`'s there too; nothing lands a reading in `_merge`'s store.
    /// So a retired worktree's reading is found, by the key it was taken
    /// over, only where main's store is read: by `done`, `status` and
    /// pre-push run in main's checkout, and by `permit`'s `unless`, which
    /// reads every worktree's store. `done` in `_merge`, which
    /// `worktree land` runs before it moves `main`, reads `_merge`'s store
    /// and does not find it. The other store's files of each scope stay where they are: each is
    /// a scope's last run over a tree the store landed in may not have.
    /// Its log is read whole, the file of each instrument, and each
    /// is emptied once landed, so a retire that refuses after this and the
    /// call that follows cannot land a reading twice. A line lands as it
    /// was written, its session the one it was taken in and never the
    /// session that lands it. How many lines were landed.
    pub fn land(&self, from: &Store) -> Result<usize> {
        let mut landed = 0;
        for source in shards(&from.root.join(LOG))? {
            let bytes =
                fs::read(&source).with_context(|| format!("reading {}", source.display()))?;
            landed += self.split(&bytes)?;
            fs::write(&source, "").with_context(|| format!("emptying {}", source.display()))?;
        }
        Ok(landed)
    }

    /// Every line of `lines` added to the log of its instrument as it is,
    /// and the latest reading of each scope over each key put in the
    /// index unless the index holds a later one: what [`Store::land`] does
    /// with each file of another store's log. The index is written once
    /// per key, the newest [`KEPT`] of each scope, after the lines, each
    /// entry the reading with every other field of its line kept beside it
    /// ([`entry_of`]). A line of a `v` this harness
    /// does not know is no reading, and is kept in [`UNREAD`]'s log. How
    /// many lines.
    fn split(&self, lines: &[u8]) -> Result<usize> {
        type Entry<'a> = (String, PathBuf, Reading, &'a str);
        let mut latest: BTreeMap<PathBuf, (Reading, &str)> = BTreeMap::new();
        let mut count = 0;
        for line in lines.split(|b| *b == b'\n').filter(|l| !l.is_empty()) {
            count += 1;
            let mut whole = line.to_vec();
            whole.push(b'\n');
            let read = std::str::from_utf8(line)
                .ok()
                .and_then(|text| Some((Reading::from_json(text).ok()?, text)));
            let Some((reading, text)) = read else {
                Self::append_line(&self.shard(UNREAD)?, &whole)?;
                continue;
            };
            Self::append_line(&self.shard(&reading.instrument)?, &whole)?;
            if let Some(entry) = self.indexed(&reading)? {
                let later =
                    |(held, _): &(Reading, &str)| reading.at.to_string() >= held.at.to_string();
                if latest.get(&entry).is_none_or(later) {
                    latest.insert(entry, (reading, text));
                }
            }
        }
        // The newest of each scope, by the time each reading was taken.
        let mut by_dir: BTreeMap<PathBuf, Vec<Entry>> = BTreeMap::new();
        for (entry, (reading, text)) in latest {
            let dir = entry.parent().unwrap_or(&self.root).to_path_buf();
            by_dir
                .entry(dir)
                .or_default()
                .push((reading.at.to_string(), entry, reading, text));
        }
        for (dir, mut entries) in by_dir {
            entries.sort_by(|a, b| b.0.cmp(&a.0));
            for (_, entry, reading, text) in entries.into_iter().take(KEPT) {
                let held = Self::read_file(&entry).ok();
                if held.is_none_or(|held| reading.at.to_string() >= held.at.to_string()) {
                    replace(&entry, entry_of(text, &reading)?.as_bytes())?;
                }
            }
            prune(&dir)?;
        }
        Ok(count)
    }

    /// The one append to a log, and the rule that a line reaches it whole
    /// or not at all: `line`, rendered whole with its newline, is
    /// handed to the file in one `write` on a descriptor opened with
    /// `O_APPEND`, so two instruments appending at once, which pre-commit
    /// and the worktrees do, cannot interleave inside a line. POSIX
    /// promises a whole write only under `PIPE_BUF` (4096 bytes on Linux,
    /// 512 on macOS), and the platforms the harness runs on land a single
    /// `O_APPEND` write to a regular file whole in practice; a reading
    /// longer than `PIPE_BUF` is the one case not covered on paper: it is
    /// still handed over in one call, and if the kernel splits it the line
    /// tears, which [`Store::history`] counts and names rather than fails
    /// on. A short write is an error, never retried, since a retry could
    /// only tear the line it means to finish.
    fn append_line(log: &Path, line: &[u8]) -> Result<()> {
        crate::make_parent(log)?;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(log)
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

    /// Which reading of `instrument` stands over `scope` at `key`, under
    /// the pin whose digest is `pin` for an instrument of [`UNDER_RECIPE`]:
    /// the one question `done`, the `--staged` instruments and `witness
    /// --unmeasured` ask of a scope (harness-design.md §4.2). The index's
    /// entry for the key, which is the latest reading over it, since every
    /// record replaces it and a land replaces it only with a later one; and
    /// the scope's file, over the key, only where prune let the entry go.
    /// No `at` is compared between the two. The index is where a retired worktree's
    /// readings are: `worktree retire` lands its log in main's store and
    /// leaves its files with the worktree, so its pass is found here, by
    /// the key it was taken over, only when this is main's store, as it is
    /// for `done`, `status` and pre-push run in main's checkout.
    /// `done` in `_merge`, which `worktree land` runs before it moves
    /// `main`, asks `_merge`'s store, and nothing lands a reading there.
    /// Where neither stands, the scope's last reading judged
    /// against the key ([`Freshness::judging`]). A file that is no reading
    /// is no reading over the key, and the index is asked instead, as for
    /// a stale one. An empty key names no content and nothing
    /// stands over it. A lookup reads the scope's file and the index, and
    /// no log.
    pub(crate) fn over(
        &self,
        instrument: &str,
        scope: &str,
        key: &str,
        pin: &str,
    ) -> Result<Freshness> {
        if !key.is_empty() {
            let entry = (self.entries(instrument, scope, key)?.into_iter())
                .filter(|r| r.fresh_for(key, pin))
                .max_by_key(|r| r.at.to_string());
            if let Some(reading) = entry {
                return Ok(Freshness::Fresh(reading));
            }
            let file = self.read(instrument, scope).ok().flatten();
            if let Some(reading) = file.filter(|r| r.fresh_for(key, pin)) {
                return Ok(Freshness::Fresh(reading));
            }
        }
        Ok(Freshness::judging(
            self.last_of(instrument, scope)?,
            key,
            pin,
        ))
    }

    /// The last reading of `instrument` over `scope` this store holds,
    /// by the time it was taken: the scope's file or, where a retired
    /// worktree's later one was landed in the index and not the file, the
    /// index's latest entry. None where the store holds neither. A file
    /// that is no reading is none.
    pub(crate) fn last_of(&self, instrument: &str, scope: &str) -> Result<Option<Reading>> {
        let file = self.read(instrument, scope).ok().flatten();
        self.last_after(file, instrument, scope)
    }

    /// [`Store::last_of`], save that a scope file that is no reading is
    /// an error: what a question reads whose empty answer would pass it on
    /// no evidence, `done`'s ticket boxes and `ticket::stranded` (harness
    /// §4.5 conditions 7 and 9).
    pub(crate) fn last_of_readable(
        &self,
        instrument: &str,
        scope: &str,
    ) -> Result<Option<Reading>> {
        let file = self.read(instrument, scope)?;
        self.last_after(file, instrument, scope)
    }

    /// `file`, or the index's latest entry of the scope where it was taken
    /// after it.
    fn last_after(
        &self,
        file: Option<Reading>,
        instrument: &str,
        scope: &str,
    ) -> Result<Option<Reading>> {
        let mut last = file;
        for (_, path) in self.keys_of(instrument, scope)? {
            if let Ok(entry) = Self::read_file(&path)
                && last
                    .as_ref()
                    .is_none_or(|held| entry.at.to_string() > held.at.to_string())
            {
                last = Some(entry);
            }
        }
        Ok(last)
    }

    /// The index's readings of `instrument` over `scope` taken over `key`,
    /// one per oracle pin they were taken under.
    fn entries(&self, instrument: &str, scope: &str, key: &str) -> Result<Vec<Reading>> {
        Ok(self
            .keys_of(instrument, scope)?
            .into_iter()
            .filter(|(held, _)| held == key)
            .filter_map(|(_, path)| Self::read_file(&path).ok())
            .collect())
    }

    /// Each entry of the index of one scope: its key and its file.
    fn keys_of(&self, instrument: &str, scope: &str) -> Result<Vec<(String, PathBuf)>> {
        let listed = crate::entries(&self.dir_of(instrument, scope)?)?;
        Ok(listed
            .into_iter()
            .filter_map(|path| Some((index_key(&path)?.to_owned(), path)))
            .collect())
    }

    /// Which of `keys` a reading of `instrument` over `scope` was taken
    /// over, whatever its status or recipe: the scope's file, then the
    /// index's keys of the scope, and for a key neither holds, the log of
    /// the instrument, where a key the index let go of still stands, a
    /// line of either version ([`V`]). What `permit`'s `unless` reads as
    /// evidence that an instrument measured a commit's content, by the key
    /// a commit's objects give ([`crate::key::Snapshot::commit`]),
    /// however long ago. An empty key names no content and is never found.
    pub(crate) fn measured_over(
        &self,
        instrument: &str,
        scope: &str,
        keys: &BTreeSet<String>,
    ) -> Result<BTreeSet<String>> {
        /// The key's field in a line of the log, as each version spells it.
        const SPELLED: [&str; 2] = ["\"key\":\"", "\"tree\":\""];
        let mut found = BTreeSet::new();
        if let Some(reading) = self.read(instrument, scope).ok().flatten()
            && !reading.key.is_empty()
            && keys.contains(&reading.key)
        {
            found.insert(reading.key);
        }
        for (key, _) in self.keys_of(instrument, scope)? {
            if keys.contains(&key) {
                found.insert(key);
            }
        }
        if found.len() == keys.iter().filter(|k| !k.is_empty()).count() {
            return Ok(found);
        }
        let path = self.shard(instrument)?;
        let bytes = match fs::read(&path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(found),
            Err(err) => return Err(err).with_context(|| format!("reading {}", path.display())),
        };
        for line in String::from_utf8_lossy(&bytes).lines() {
            let Some(key) = SPELLED.iter().find_map(|field| {
                let at = line.rfind(field)? + field.len();
                line[at..].split('"').next()
            }) else {
                continue;
            };
            if key.is_empty() || !keys.contains(key) || found.contains(key) {
                continue;
            }
            if let Ok(reading) = Reading::from_json(line)
                && reading.scope == scope
            {
                found.insert(reading.key);
            }
        }
        Ok(found)
    }

    /// The log of one instrument, oldest first: what the store held of it
    /// across every run and every session, and the lines it could not
    /// read. A missing log is empty. A line that is not a reading, which a
    /// write the kernel split leaves behind (see [`Store::append_line`]),
    /// is counted by its number and skipped rather than failing the whole
    /// log, so a torn line degrades one section and never the session.
    pub fn history(&self, instrument: &str) -> Result<Log> {
        let text = crate::read_if(&self.shard(instrument)?)?;
        Ok(text.map_or_else(Log::default, |text| Log::of(&text, Reading::from_json)))
    }

    /// Every instrument's log at once, in the order the readings were
    /// taken: what `status --every` renders. The one reader of the whole
    /// log; every other reads the instruments it renders.
    pub fn history_all(&self) -> Result<Log> {
        let mut all = Log::default();
        for path in shards(&self.root.join(LOG))? {
            let text = crate::read(&path)?;
            let log = Log::of(&text, Reading::from_json);
            all.readings.extend(log.readings);
            all.unreadable.extend(log.unreadable);
        }
        all.readings.sort_by_cached_key(|r| r.at.to_string());
        Ok(all)
    }

    /// Of each instrument's log, each line's status and time, read without
    /// the rest of the line: what the permits section counts per
    /// instrument, which reads every log and needs no reading whole.
    pub(crate) fn tallies(&self) -> Result<BTreeMap<String, Vec<(Status, String)>>> {
        #[derive(Deserialize)]
        struct Brief {
            status: Status,
            at: String,
        }
        let mut tallies = BTreeMap::new();
        for path in shards(&self.root.join(LOG))? {
            let Some(name) = path.file_stem().map(|s| s.to_string_lossy().into_owned()) else {
                continue;
            };
            if name == UNREAD {
                continue;
            }
            let text = crate::read(&path)?;
            let lines: Vec<(Status, String)> = text
                .lines()
                .filter_map(|l| serde_json::from_str::<Brief>(l).ok())
                .map(|b| (b.status, b.at))
                .collect();
            tallies.insert(name, lines);
        }
        Ok(tallies)
    }

    /// Every scope's file in the store, in path order, and every file of
    /// the store that is not a reading, by its path; the index's entries
    /// are left out. A scope's file is the last reading this store wrote
    /// of it, and not always its last reading: a land puts another
    /// checkout's later one in the index alone, which [`Store::last_of`]
    /// reads (c2.i). A
    /// missing store is empty. A file a crash left empty or cut short,
    /// which [`replace`] no longer can, is named and skipped rather
    /// than failing every rendering of the store, as the log's torn line
    /// is: it costs its own row, and the next run of its instrument writes
    /// it again.
    pub fn readings(&self) -> Result<Files> {
        let mut paths = Vec::new();
        crate::tally::files_under(&self.root, &mut paths)?;
        paths.sort();
        let mut files = Files::default();
        for path in paths
            .into_iter()
            .filter(|p| p.extension().is_some_and(|e| e == "toml") && index_key(p).is_none())
        {
            match Self::read_file(&path) {
                Ok(reading) => files.readings.push(reading),
                Err(_) => files.unreadable.push(path),
            }
        }
        Ok(files)
    }

    fn read_file(path: &Path) -> Result<Reading> {
        crate::read_toml(path)
    }
}

/// Which reading stands over a key: the answer [`Store::over`] gives of a
/// scope, and [`Freshness::judging`] of a reading in hand.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Freshness {
    /// A reading taken over the key, and under the pin for an instrument
    /// of [`UNDER_RECIPE`]: it stands, whatever its status says.
    Fresh(Reading),
    /// No reading stands over the key, and `last` is the scope's last,
    /// its status kept as it was taken.
    Stale { last: Reading, moved: Moved },
    /// The store holds no reading of the scope.
    Never,
}

/// What moved since a stale reading was taken.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Moved {
    /// The scope's key: its files, or its dependency closure's.
    Key,
    /// The oracle's pin, the reading being over the key now.
    Recipe,
}

impl Freshness {
    /// `last` judged against `key` and the pin's digest `pin`: fresh by
    /// [`Reading::fresh_for`], the one statement of fresh, stale over
    /// another key or another recipe, or never taken. An empty key names
    /// no content, so a reading over it is stale whatever it was taken
    /// over. What a question about one reading asks, where finding a
    /// reading by its key would revive one the question re-asserts by
    /// reading again, as a ticket's.
    pub(crate) fn judging(last: Option<Reading>, key: &str, pin: &str) -> Freshness {
        match last {
            None => Freshness::Never,
            Some(reading) if !key.is_empty() && reading.fresh_for(key, pin) => {
                Freshness::Fresh(reading)
            }
            Some(last) => {
                let moved = if !key.is_empty() && last.key == key {
                    Moved::Recipe
                } else {
                    Moved::Key
                };
                Freshness::Stale { last, moved }
            }
        }
    }

    /// The reading that stands, none where it is stale or was never taken.
    pub(crate) fn fresh(self) -> Option<Reading> {
        match self {
            Freshness::Fresh(reading) => Some(reading),
            Freshness::Stale { .. } | Freshness::Never => None,
        }
    }
}

/// The index's file name of a key, without `.toml`: the key, and the pin
/// a reading was taken under after a dash where it has one. A pin that is
/// not a hex digest is named by its blob id, so the name stays a name.
fn index_name(key: &str, recipe: Option<&str>) -> String {
    match recipe {
        None => key.to_owned(),
        Some(pin) if pin.chars().all(|c| c.is_ascii_hexdigit()) => format!("{key}-{pin}"),
        Some(pin) => format!("{key}-{}", crate::git::blob_id(pin.as_bytes())),
    }
}

/// The key an index file is kept under, or none for a path that is no
/// index entry: a `.toml` whose name is forty hex digits, the key, alone
/// or before a dash and its pin.
fn index_key(path: &Path) -> Option<&str> {
    if path.extension().is_none_or(|e| e != "toml") {
        return None;
    }
    let stem = path.file_stem()?.to_str()?;
    let key = stem.get(..40)?;
    let rest = &stem[40..];
    (key.chars().all(|c| c.is_ascii_hexdigit()) && (rest.is_empty() || rest.starts_with('-')))
        .then_some(key)
}

/// The index entry [`Store::land`] writes of `line`, a line of another
/// store's log that reads as `reading`: `reading` in this harness's shape,
/// `v` and `key` as [`V`] spells them and `at` as TOML's datetime, with
/// every other field of the line kept beside it, so a field this harness
/// does not know is kept and not dropped, and the harness that wrote it
/// finds it in the entry. The first version's `tree`, which
/// `reading` reads as its `key`, is not kept beside it. A line TOML cannot
/// hold whole, a null or an integer past `i64`, which no writer of this
/// store writes, is entered as this harness reads it.
fn entry_of(line: &str, reading: &Reading) -> Result<String> {
    let read = toml::Table::try_from(reading).context("rendering the reading")?;
    let whole = serde_json::from_str::<serde_json::Value>(line)
        .ok()
        .and_then(|value| toml::Value::try_from(value).ok());
    let table = match whole {
        Some(toml::Value::Table(mut table)) => {
            table.remove("tree");
            table.extend(read);
            table
        }
        _ => read,
    };
    toml::to_string(&table).context("rendering the reading")
}

/// The index of one scope cut to its newest [`KEPT`] entries, in the
/// order [`past_kept`] gives. Another writer pruning at once may remove a
/// file first, which is the same end.
fn prune(dir: &Path) -> Result<()> {
    let Ok(listing) = fs::read_dir(dir) else {
        return Ok(());
    };
    let listed = listing.filter_map(|e| e.ok().map(|e| e.path())).collect();
    for path in past_kept(listed) {
        crate::remove(&path)?;
    }
    Ok(())
}

/// The index files among `listed`, one scope's in whatever order its
/// directory gave, that fall past the newest [`KEPT`]: newest by the time
/// each reading was taken, its `at`, the order [`Store::over`] and
/// [`Store::split`] read; two taken in one second by the file's
/// modification time, and two written in one tick of that clock by name.
/// So the order is the files' own and never the directory's, and two
/// writers pruning at once agree on which go. A file whose `at` cannot be
/// read is the oldest. A scope past [`KEPT`], which each record into a
/// full one finds, reads every entry for its `at`.
fn past_kept(listed: Vec<PathBuf>) -> Vec<PathBuf> {
    let listed: Vec<PathBuf> = (listed.into_iter())
        .filter(|p| index_key(p).is_some())
        .collect();
    if listed.len() <= KEPT {
        return Vec::new();
    }
    let mut entries: Vec<(Option<String>, SystemTime, PathBuf)> = (listed.into_iter())
        .filter_map(|p| Some((taken_at(&p), fs::metadata(&p).ok()?.modified().ok()?, p)))
        .collect();
    entries.sort_by(|a, b| b.cmp(a));
    entries.into_iter().skip(KEPT).map(|(.., p)| p).collect()
}

/// When the reading in the file at `path` was taken, as text, which
/// orders as the instant does; read whatever else the file holds, so an
/// entry of a `v` this harness does not know keeps its place. None where
/// the file has no `at`.
fn taken_at(path: &Path) -> Option<String> {
    #[derive(Deserialize)]
    struct Stamp {
        #[serde(deserialize_with = "either_at")]
        at: Datetime,
    }
    let stamp: Stamp = crate::read_toml(path).ok()?;
    Some(stamp.at.to_string())
}

/// The files of the log under `dir`, in name order; none where it is not.
fn shards(dir: &Path) -> Result<Vec<PathBuf>> {
    let mut paths = crate::entries(dir)?;
    paths.retain(|p| p.extension().is_some_and(|e| e == "jsonl"));
    paths.sort();
    Ok(paths)
}

/// The store's files as read: every one that is a reading, in path order,
/// and the path of every one that is not.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Files {
    pub readings: Vec<Reading>,
    pub unreadable: Vec<PathBuf>,
}

impl Files {
    /// What a rendering says about the files it could not read, each by
    /// its path under the store's `root`, `target/harness: 1 reading
    /// unreadable at budget/php_unit.toml.`, at most five named and the
    /// rest counted; none when every file read. The files' counterpart of
    /// [`Log::unreadable_note`].
    pub fn unreadable_note(&self, root: &Path) -> Option<String> {
        let at = self.unreadable.iter();
        let at = at.map(|path| {
            path.strip_prefix(root)
                .unwrap_or(path)
                .display()
                .to_string()
        });
        let again = "; the next run of its instrument writes it again.";
        unreadable(DIR, "reading", &at.collect::<Vec<_>>(), again)
    }
}

/// `bytes` as the whole of the file at `path`, or the file as it stood:
/// written to a file of its own beside `path` and renamed over it, so a
/// reader at the same moment, another worktree's hook or an instrument
/// beside it, sees the old file or the new one and never one cut short,
/// and a disk that fills leaves the old one standing. The rename is
/// within one directory, which POSIX makes atomic. The file beside is
/// hidden and ends in `.tmp`, so no reader of `.toml` or `.json` files
/// takes it for what it replaces, and it is removed when the write fails.
pub(crate) fn replace(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let dir = path
        .parent()
        .with_context(|| format!("{} names no directory", path.display()))?;
    crate::make_dir(dir)?;
    let name = path
        .file_name()
        .with_context(|| format!("{} names no file", path.display()))?
        .to_string_lossy();
    let beside = dir.join(format!(
        ".{name}.{}.{}.tmp",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let written = crate::write(&beside, bytes).and_then(|()| {
        fs::rename(&beside, path)
            .with_context(|| format!("renaming {} to {}", beside.display(), path.display()))
    });
    if written.is_err() {
        fs::remove_file(&beside).ok();
    }
    written
}

/// The log as read: every line that is a reading, in the log's order, and
/// the number of every line that is not, 1-based as an editor counts.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Log {
    pub readings: Vec<Reading>,
    pub unreadable: Vec<usize>,
}

impl Log {
    /// The log `text` is, each line read by `read`: a blank line is
    /// skipped, and one `read` refuses is counted by its number.
    fn of(text: &str, read: impl Fn(&str) -> Result<Reading>) -> Log {
        let mut log = Log::default();
        for (i, line) in text.lines().enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            match read(line) {
                Ok(reading) => log.readings.push(reading),
                Err(_) => log.unreadable.push(i + 1),
            }
        }
        log
    }

    /// A log handed to a rendering rather than read off its store
    /// (harness-design.md §4.6): a file of another store's log, or the
    /// published history, whose older rows are lean. Any line
    /// shaped as a reading reads, a whole line with the fields this build
    /// does not know ignored or a lean row `{i,s,t,a,c,n}`; any other is
    /// counted by its number, as a torn line of the store's own is.
    pub(crate) fn handed(text: &str) -> Log {
        Log::of(text, Reading::from_kept)
    }

    /// What a rendering says about the lines it could not read, `log: 1
    /// line unreadable at 10019.`, at most five named and the rest
    /// counted; none when every line read. One string, so every section
    /// says it the same way.
    pub fn unreadable_note(&self) -> Option<String> {
        self.unreadable_note_of(LOG)
    }

    /// The same note for the log `name` names, a log handed in by its path.
    pub(crate) fn unreadable_note_of(&self, name: &str) -> Option<String> {
        let lines: Vec<String> = self.unreadable.iter().map(usize::to_string).collect();
        unreadable(name, "line", &lines, ".")
    }
}

/// What a rendering says of what it could not read from the store or log
/// `name`: how many of `noun` and where, at most five named and the rest
/// counted, then `after`; none when it read everything.
fn unreadable(name: &str, noun: &str, at: &[String], after: &str) -> Option<String> {
    let (n, listed) = (at.len(), listed(at));
    (n > 0).then(|| {
        format!(
            "{name}: {n} {noun}{} unreadable at {listed}{after}",
            plural(n)
        )
    })
}

/// The commits a checkout stands on: the first-parent chain of `HEAD`,
/// each with its distance from the tip. A branch merged into it sits on
/// a second parent and a branch cut behind the tip is off it past the cut,
/// so the readings their worktrees took, which `worktree retire` appends
/// into the main checkout's store, are not taken on it.
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
        let ids = crate::git::git(root, &["rev-list", "--first-parent", "HEAD"])?;
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

    /// The reading of each scope of `instrument` that stands for the tip,
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
/// records through [`Store::record_measurement`] and owes no exit (§4.4).
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

    /// [`Outcome::exit`] for an instrument whose unmet exit says
    /// `message` rather than the count.
    pub(crate) fn exit_saying(self, message: impl FnOnce() -> String) -> Result<()> {
        match self {
            Outcome::Met => Ok(()),
            Outcome::Unmet { .. } => bail!("{}", message()),
        }
    }
}

/// The `s` a count of one does not carry: every instrument's message
/// counts something, so the word lives beside the shape they all write.
pub(crate) fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// A span in whole milliseconds, as every reading that times something
/// counts it under a key ending `_ms`, or `ms` alone.
pub(crate) fn millis(span: std::time::Duration) -> u64 {
    u64::try_from(span.as_millis()).unwrap_or(u64::MAX)
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

/// The instant `days` days before now, in seconds since 1970: where a
/// window over the log starts.
pub(crate) fn days_ago(days: i64) -> Result<i64> {
    Ok(epoch_seconds(&now()?)? - days * 86_400)
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
pub(crate) fn civil_from_days(z: i64) -> (i64, i64, i64) {
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

/// Each of `items` as it displays, `separator` between.
pub(crate) fn joined<T: std::fmt::Display>(
    items: impl IntoIterator<Item = T>,
    separator: &str,
) -> String {
    let mut out = String::new();
    for (i, item) in items.into_iter().enumerate() {
        if i > 0 {
            out.push_str(separator);
        }
        out.push_str(&item.to_string());
    }
    out
}

/// At most five names, and how many more there are: how a reading's
/// message names what missed, the same way in every lane.
pub(crate) fn listed(names: &[impl AsRef<str>]) -> String {
    let shown = names
        .iter()
        .take(5)
        .map(AsRef::as_ref)
        .collect::<Vec<_>>()
        .join(", ");
    if names.len() > 5 {
        format!("{shown} and {} more", names.len() - 5)
    } else {
        shown
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::path::PathBuf;

    use super::{
        Freshness, KEPT, Log, Moved, Reading, Status, Store, V, civil_from_days, days_from_civil,
        listed, past_kept, prune,
    };
    use crate::owner::SessionId;
    use crate::scratch::Tree;

    /// A store at `dir` written in no session, so a reading read back is
    /// the reading handed in, whatever session the test runs in.
    fn unnamed(dir: &std::path::Path) -> Store {
        Store::at(dir).in_session(None)
    }

    /// `reading` as a run in another checkout leaves it, landed here by
    /// `worktree retire` or `land`: in the index and the log, and not in
    /// the scope's file.
    fn landed(store: &Store, name: &str, reading: &Reading) {
        let other = Tree::new(name).unwrap();
        let other = unnamed(other.path());
        other.record(std::slice::from_ref(reading)).unwrap();
        store.land(&other).unwrap();
    }

    /// The lookup over a key, case by case (harness-design.md §4.2): the
    /// file over the key stands; a file over another key gives way to the
    /// index's entry over the key; an entry landed with no file stands; a
    /// scope never taken is never; an unreadable file is no reading over
    /// the key and the index is asked; nothing stands over an empty key;
    /// a reading over the key under another recipe is stale for the
    /// recipe; and a stale reading keeps the status it was taken with.
    #[test]
    fn the_reading_over_a_key_is_the_file_or_the_index_and_else_the_last_judged() {
        let dir = Tree::new("store-over").unwrap();
        let store = unnamed(dir.path());
        let at = |m: u32| format!("2026-11-07T10:{m:02}:00Z");
        let fresh_key = |f: Freshness| f.fresh().map(|r| r.key().to_owned());

        store.record(&[over("test", "a", &key(1), &at(0))]).unwrap();
        assert_eq!(
            fresh_key(store.over("test", "a", &key(1), "").unwrap()),
            Some(key(1))
        );

        // The file moved on to another key, a refused commit's: the pass
        // over the key now is the index's.
        let mut refused = over("test", "a", &key(2), &at(1));
        refused.status = Status::Fail;
        store.record(&[refused]).unwrap();
        assert_eq!(
            fresh_key(store.over("test", "a", &key(1), "").unwrap()),
            Some(key(1))
        );
        let stale = store.over("test", "a", &key(3), "").unwrap();
        assert!(
            matches!(&stale, Freshness::Stale { last, moved: Moved::Key }
                if last.status == Status::Fail && last.key() == key(2)),
            "the last reading, over another key, keeps its fail: {stale:?}"
        );

        // Landed with no file of its scope.
        landed(
            &store,
            "store-over-landed",
            &over("test", "b", &key(4), &at(2)),
        );
        assert!(!store.path("test", "b").unwrap().exists());
        assert_eq!(
            fresh_key(store.over("test", "b", &key(4), "").unwrap()),
            Some(key(4))
        );

        assert_eq!(
            store.over("test", "never", &key(1), "").unwrap(),
            Freshness::Never
        );

        std::fs::write(store.path("test", "a").unwrap(), "no reading").unwrap();
        assert_eq!(
            fresh_key(store.over("test", "a", &key(1), "").unwrap()),
            Some(key(1))
        );

        let empty = store.over("test", "b", "", "").unwrap();
        assert!(
            matches!(
                empty,
                Freshness::Stale {
                    moved: Moved::Key,
                    ..
                }
            ),
            "{empty:?}"
        );

        let witness = under(over("witness", "a", &key(5), &at(3)), "old-pin");
        store.record(&[witness]).unwrap();
        let moved = store.over("witness", "a", &key(5), "new-pin").unwrap();
        assert!(
            matches!(
                moved,
                Freshness::Stale {
                    moved: Moved::Recipe,
                    ..
                }
            ),
            "{moved:?}"
        );
        assert_eq!(
            fresh_key(store.over("witness", "a", &key(5), "old-pin").unwrap()),
            Some(key(5))
        );
    }

    /// A reading another checkout took later over the same key, landed in
    /// the index, stands before this store's older file over that key:
    /// the index's entry is the latest over its key (harness-design.md
    /// §4.2, revision 83). Here a landed pass over a fail the file holds,
    /// which the file-first order gave.
    #[test]
    fn a_landed_reading_later_than_the_file_over_the_same_key_stands() {
        let dir = Tree::new("store-over-later").unwrap();
        let store = unnamed(dir.path());
        let mut failed = over("test", "xtask", &key(1), "2026-11-07T10:00:00Z");
        failed.status = Status::Fail;
        store.record(&[failed]).unwrap();
        let passed = over("test", "xtask", &key(1), "2026-11-07T11:00:00Z");
        landed(&store, "store-over-later-landed", &passed);
        let file = store.read("test", "xtask").unwrap().unwrap();
        assert_eq!(
            file.status,
            Status::Fail,
            "the land leaves the file as it was"
        );
        assert_eq!(
            store.over("test", "xtask", &key(1), "").unwrap(),
            Freshness::Fresh(passed)
        );
    }

    /// Where prune let a key's entry go, the scope's file over that key
    /// still answers.
    #[test]
    fn an_entry_prune_let_go_is_answered_from_a_file_over_the_key() {
        let dir = Tree::new("store-over-pruned").unwrap();
        let store = unnamed(dir.path());
        let reading = over("test", "xtask", &key(1), "2026-11-07T10:00:00Z");
        store.record(std::slice::from_ref(&reading)).unwrap();
        let entry = store.indexed(&reading).unwrap().unwrap();
        std::fs::remove_file(entry).unwrap();
        assert_eq!(
            store.over("test", "xtask", &key(1), "").unwrap(),
            Freshness::Fresh(reading)
        );
    }

    /// One reading judged against a key: fresh over it, stale over another
    /// key or under another recipe, never where there is none; and an
    /// empty key, which names no content, stales a reading taken over
    /// nothing as well.
    #[test]
    fn a_reading_in_hand_is_judged_fresh_stale_or_never() {
        let reading = over("test", "a", &key(1), "2026-11-07T10:00:00Z");
        let judged = |key: &str, pin: &str| Freshness::judging(Some(reading.clone()), key, pin);
        assert_eq!(judged(&key(1), ""), Freshness::Fresh(reading.clone()));
        assert!(matches!(
            judged(&key(2), ""),
            Freshness::Stale {
                moved: Moved::Key,
                ..
            }
        ));
        assert_eq!(Freshness::judging(None, &key(1), ""), Freshness::Never);
        let nothing = over("test", "a", "", "2026-11-07T10:00:00Z");
        let empty = Freshness::judging(Some(nothing), "", "");
        assert!(
            matches!(
                empty,
                Freshness::Stale {
                    moved: Moved::Key,
                    ..
                }
            ),
            "{empty:?}"
        );
        let witness = under(over("witness", "a", &key(1), "2026-11-07T10:00:00Z"), "p");
        let other = Freshness::judging(Some(witness), &key(1), "q");
        assert!(
            matches!(
                other,
                Freshness::Stale {
                    moved: Moved::Recipe,
                    ..
                }
            ),
            "{other:?}"
        );
    }

    /// The writer fills `session` on every reading it writes that names
    /// none, in the scope's file, the index entry and the line of the log
    /// alike, and keeps the session a hook or caller named; a store with
    /// no session, a person's terminal, writes the blank it was handed
    /// (harness-design.md §4.2; Round 1, Q7 (a)).
    #[test]
    fn the_writer_fills_the_session_of_a_reading_naming_none_and_keeps_a_named_one() {
        let dir = Tree::new("store-session").unwrap();
        let store = Store::at(dir.join("agent")).in_session(SessionId::new("s-writer"));
        let at = "2026-11-09T08:00:00Z";
        let mut named = over("permit", "rule", &key(2), at);
        "s-hook".clone_into(&mut named.session);
        store
            .record(&[over("test", "xtask", &key(1), at), named])
            .unwrap();
        let file = |store: &Store, instrument: &str, scope: &str| {
            store.read(instrument, scope).unwrap().unwrap().session
        };
        let entry = |store: &Store, instrument: &str, scope: &str, key: &str| {
            let found = store.over(instrument, scope, key, "").unwrap();
            found.fresh().unwrap().session
        };
        let logged = |store: &Store, instrument: &str| {
            let log = store.history(instrument).unwrap();
            log.readings
                .iter()
                .map(|r| r.session.clone())
                .collect::<Vec<_>>()
        };
        assert_eq!(file(&store, "test", "xtask"), "s-writer");
        assert_eq!(entry(&store, "test", "xtask", &key(1)), "s-writer");
        assert_eq!(logged(&store, "test"), ["s-writer"]);
        assert_eq!(file(&store, "permit", "rule"), "s-hook");
        assert_eq!(logged(&store, "permit"), ["s-hook"]);

        let person = Store::at(dir.join("person")).in_session(None);
        person
            .record(&[over("test", "xtask", &key(1), at)])
            .unwrap();
        assert_eq!(file(&person, "test", "xtask"), "");
        assert_eq!(logged(&person, "test"), [""]);
    }

    /// A passing reading of `instrument` over `scope`, taken over `key`
    /// at `at`.
    fn over(instrument: &str, scope: &str, key: &str, at: &str) -> Reading {
        let mut reading = Reading::new(instrument, scope);
        key.clone_into(&mut reading.key);
        reading.status = Status::Pass;
        reading.commit = "0000000".into();
        reading.at = at.parse().unwrap();
        reading
    }

    /// `reading` as taken under the pin whose digest is `pin`.
    fn under(mut reading: Reading, pin: &str) -> Reading {
        reading.recipe = Some(pin.to_owned());
        reading
    }

    /// The `n`th key, forty hex digits.
    fn key(n: usize) -> String {
        format!("{n:040x}")
    }

    /// A reading lands in the index under its key, the newest [`KEPT`] of
    /// a scope stay, and the latest reading over a key is found there
    /// while the scope's file has moved on to another. A key the
    /// index let go reads as never measured to `done`, and is still found
    /// in the instrument's log by `permit`'s look for evidence. Which keys
    /// are newest is the readings' own: each entry's modification time is
    /// set to one instant past the clock once it is written, so an order
    /// by modification time would let the newest go each time.
    #[test]
    fn a_reading_is_found_by_its_key_and_the_newest_keys_stay() {
        let dir = Tree::new("store-index").unwrap();
        let store = unnamed(dir.path());
        for n in 0..KEPT + 8 {
            let at = format!("2026-10-06T10:{n:02}:00Z");
            let reading = over("budget", "ext/pcre", &key(n), &at);
            store.record(std::slice::from_ref(&reading)).unwrap();
            let entry = store.indexed(&reading).unwrap().unwrap();
            written_at(&entry, PAST_THE_CLOCK);
        }
        let kept: BTreeSet<String> = store
            .keys_of("budget", "ext/pcre")
            .unwrap()
            .into_iter()
            .map(|(key, _)| key)
            .collect();
        assert_eq!(kept.len(), KEPT);
        assert!(kept.contains(&key(KEPT + 7)) && !kept.contains(&key(0)));
        let over = |n: usize| store.over("budget", "ext/pcre", &key(n), "").unwrap();
        assert_eq!(
            over(KEPT + 1).fresh().as_ref().map(Reading::key),
            Some(key(KEPT + 1).as_str()),
            "found by its key while the scope's file holds the last"
        );
        assert!(
            matches!(
                over(0),
                Freshness::Stale {
                    moved: Moved::Key,
                    ..
                }
            ),
            "a key the index let go"
        );
        let let_go: BTreeSet<String> = [key(0)].into();
        assert_eq!(
            store.measured_over("budget", "ext/pcre", &let_go).unwrap(),
            let_go
        );
        assert_eq!(
            store.readings().unwrap().readings.len(),
            1,
            "the index's entries are no scope's last reading"
        );
    }

    /// An instant past any clock a test runs under: 2100-01-01.
    const PAST_THE_CLOCK: u64 = 4_102_444_800;

    /// The file at `path` given `secs` past 1970 as its modification time.
    fn written_at(path: &std::path::Path, secs: u64) {
        let file = std::fs::File::options().write(true).open(path).unwrap();
        let instant = std::time::UNIX_EPOCH + std::time::Duration::from_secs(secs);
        file.set_modified(instant).unwrap();
    }

    /// The index of one scope is cut to the newest [`KEPT`] by when each
    /// reading was taken, whatever order the directory lists its files
    /// in: more than [`KEPT`] entries written in one tick, their
    /// modification times equal, keep the newest listed oldest first and
    /// newest first alike, and so does a prune of the directory.
    #[test]
    fn entries_written_in_one_tick_keep_the_newest_whatever_the_listing_order() {
        let dir = Tree::new("store-index-tick").unwrap();
        let store = unnamed(dir.path());
        let mut listed = Vec::new();
        for n in 0..KEPT + 8 {
            let at = format!("2026-10-06T10:{n:02}:00Z");
            let reading = over("budget", "ext/pcre", &key(n), &at);
            let entry = store.indexed(&reading).unwrap().unwrap();
            crate::make_parent(&entry).unwrap();
            std::fs::write(&entry, toml::to_string(&reading).unwrap()).unwrap();
            written_at(&entry, PAST_THE_CLOCK);
            listed.push(entry);
        }
        let let_go: BTreeSet<PathBuf> = listed[..8].iter().cloned().collect();
        let oldest_first: BTreeSet<PathBuf> = past_kept(listed.clone()).into_iter().collect();
        assert_eq!(oldest_first, let_go, "listed oldest first");
        listed.reverse();
        let newest_first: BTreeSet<PathBuf> = past_kept(listed).into_iter().collect();
        assert_eq!(newest_first, let_go, "listed newest first");

        prune(&store.dir_of("budget", "ext/pcre").unwrap()).unwrap();
        let kept: BTreeSet<String> = (store.keys_of("budget", "ext/pcre").unwrap())
            .into_iter()
            .map(|(key, _)| key)
            .collect();
        assert_eq!(kept, (8..KEPT + 8).map(key).collect());
    }

    /// `reading` as a line of the log of the first version: no `v`, and
    /// its key spelled `tree`.
    fn first_version(reading: &Reading) -> serde_json::Value {
        let mut line = reading.to_json().unwrap();
        let object = line.as_object_mut().unwrap();
        assert_eq!(object.remove("v"), Some(V.into()));
        let key = object.remove("key").unwrap();
        object.insert("tree".into(), key);
        line
    }

    /// `permit`'s look for evidence reads a key the index let go of from
    /// a line of the log of either version: `key` under `v = 2`, `tree`
    /// on a line of the first version, which carries no `v`.
    #[test]
    fn a_key_the_index_let_go_is_found_in_a_line_of_either_version() {
        let dir = Tree::new("store-versions").unwrap();
        let store = unnamed(dir.path());
        let v2 = over("budget", "xtask", &key(2), "2026-10-08T01:00:00Z");
        let v1 = first_version(&over("budget", "xtask", &key(1), "2026-10-08T00:00:00Z"));
        let log = format!("{v1}\n{}\n", v2.to_json().unwrap());
        std::fs::create_dir_all(store.root().join("log")).unwrap();
        std::fs::write(store.root().join("log/budget.jsonl"), log).unwrap();
        let sought: BTreeSet<String> = [key(1), key(2), key(3)].into();
        assert_eq!(
            store.measured_over("budget", "xtask", &sought).unwrap(),
            [key(1), key(2)].into()
        );
    }

    /// A log handed to a rendering reads a whole line of either version,
    /// each with its key, and counts one of a `v` it does not know as a
    /// line it could not read (harness-design.md §4.6).
    #[test]
    fn a_handed_log_reads_both_versions_and_refuses_an_unknown_one() {
        let v2 = over("budget", "xtask", &key(2), "2026-10-08T01:00:00Z");
        let mut unknown = v2.to_json().unwrap();
        unknown["v"] = (V + 1).into();
        let log = Log::handed(&format!(
            "{}\n{}\n{unknown}\n",
            v2.to_json().unwrap(),
            first_version(&v2)
        ));
        assert_eq!(log.readings, [v2.clone(), v2]);
        assert_eq!(log.unreadable, [3]);
    }

    /// A reading carrying a field this harness does not know reads, from
    /// the store's file and from a line of the log, as the reading without
    /// it; a land indexes such a line, its entry keeping the field, and
    /// keeps nothing of it in `unread`. A reading of a `v` this harness
    /// does not know is refused in either, and a land keeps its line in
    /// `unread` and indexes nothing.
    #[test]
    fn a_field_it_does_not_know_is_ignored_and_a_v_it_does_not_know_refused() {
        let dir = Tree::new("store-unknown").unwrap();
        let store = unnamed(&dir.join("main"));
        let reading = over("test", "xtask", &key(1), "2026-11-10T09:00:00Z");
        let mut newer = reading.to_json().unwrap();
        newer["later"] = serde_json::json!({ "count": 1, "said": "kept" });
        newer["future"] = "kept".into();
        let mut unknown = reading.to_json().unwrap();
        unknown["v"] = (V + 1).into();
        unknown["scope"] = "unknown".into();
        let old = over("test", "first", &key(2), "2026-11-10T08:00:00Z");
        let first = first_version(&old);

        assert_eq!(Reading::from_json(&newer.to_string()).unwrap(), reading);
        assert!(Reading::from_json(&unknown.to_string()).is_err());

        let file = store.path("test", "xtask").unwrap();
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        let text = toml::to_string(&reading).unwrap();
        std::fs::write(
            &file,
            format!("future = \"kept\"\n{text}\n[later]\ncount = 1\n"),
        )
        .unwrap();
        assert_eq!(store.read("test", "xtask").unwrap(), Some(reading.clone()));
        std::fs::write(
            &file,
            text.replace(&format!("v = {V}"), &format!("v = {}", V + 1)),
        )
        .unwrap();
        assert!(store.read("test", "xtask").is_err());
        std::fs::remove_file(&file).unwrap();

        let other = unnamed(&dir.join("other"));
        std::fs::create_dir_all(other.root().join("log")).unwrap();
        std::fs::write(
            other.root().join("log/test.jsonl"),
            format!("{newer}\n{unknown}\n{first}\n"),
        )
        .unwrap();
        assert_eq!(store.land(&other).unwrap(), 3);
        assert_eq!(
            store.over("test", "xtask", &key(1), "").unwrap(),
            Freshness::Fresh(reading.clone())
        );
        assert_eq!(
            store.over("test", "first", &key(2), "").unwrap(),
            Freshness::Fresh(old),
            "a line of the first version is entered in this version's shape and read back"
        );
        let entry = store.indexed(&reading).unwrap().unwrap();
        let kept: toml::Table = toml::from_str(&std::fs::read_to_string(entry).unwrap()).unwrap();
        assert_eq!(kept["future"].as_str(), Some("kept"));
        assert_eq!(kept["later"]["count"].as_integer(), Some(1));
        assert_eq!(
            kept["at"].as_datetime().map(ToString::to_string),
            Some(reading.at.to_string())
        );
        let unread = std::fs::read_to_string(store.root().join("log/unread.jsonl")).unwrap();
        assert_eq!(unread, format!("{unknown}\n"));
        assert_eq!(
            store.over("test", "unknown", &key(1), "").unwrap(),
            Freshness::Never
        );
    }

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
