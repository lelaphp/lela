//! The reading and its store (harness-design.md §4.2). Every instrument ends
//! by writing its readings through [`Store::record`], the one writer; every
//! rendering reads them; nothing else touches the store. The store is
//! `target/harness/<instrument>/<scope>.toml`, the scope's last reading,
//! overwritten on each run; the index beside it, one file per key; and the
//! log, `target/harness/log/<instrument>.jsonl`, append-only, one line per
//! reading ([`Store`], revision 83). The clock a reading's `at` is taken
//! from, and the calendar that reads it back, live here too.

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
use crate::sources;

/// Where the store lives, relative to the workspace root.
pub const DIR: &str = "target/harness";
/// The log's directory, inside the store: one append-only file per
/// instrument (revision 83; #2164).
pub const LOG: &str = "log";
/// The one log a store kept before revision 83, split into [`LOG`] and the
/// index by the first record or read that finds it ([`Store::migrate`]).
pub const LEGACY: &str = "log.jsonl";
/// What a split [`LEGACY`] log is kept as, inside the store.
pub const MIGRATED: &str = "log.migrated.jsonl";
/// The file of the log a line that is no reading is kept in, as it was.
pub const UNREAD: &str = "unread";

/// The version of a reading's shape, written as `v` on every reading
/// (harness-design.md §4.2): 2 since the scope's key is spelled `key`
/// (#1719). A reading written before carries no `v` and is version 1,
/// which spells the key `tree`; the store reads both, and refuses a `v` it
/// does not know. A field a reading may omit, whose absence a reader of
/// the earlier shape already handles correctly, is added under the same
/// `v`; a field removed, renamed or retyped, or a meaning changed, is a
/// new `v` (§4.6, revision 76).
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

/// One reading, the one shape every instrument writes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Reading {
    /// The version of the shape, [`V`]; absent on a reading of the first.
    #[serde(default)]
    v: Version,
    /// The subcommands that write one, in the order `cli::Command` has
    /// them: `api-check | permit | done | track-work | ticket | usage |
    /// bench | budget | clippy | test | miri | phpt | port-map |
    /// compile-identity | crosscheck | fpm-compare | soak | unsafe-audit |
    /// steer | divergence-reason | witness | vocab`; `fuzz | mutants` are
    /// named by harness-design.md §4.2 and not built yet.
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
    /// declared dependency closure (#1318); what `done` holds against the
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
    /// The [`Pin::digest`] of the oracle pin the run was taken under, for
    /// an instrument of [`UNDER_RECIPE`]; `None` for every other (#1387).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recipe: Option<String>,
}

/// The instruments whose runs are taken under the oracle's recipe: a
/// reading of one is fresh only under the pin it records (#1387).
pub const UNDER_RECIPE: [&str; 4] = ["witness", "phpt", "crosscheck", "fpm-compare"];

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
    /// index's key of it, pre-commit's (#1143); and, for an instrument of
    /// [`UNDER_RECIPE`], under the pin whose [`Pin::digest`] is `pin` (#1387).
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
    /// One line of the log back into a reading; the inverse of `to_json`,
    /// in one pass: the line is read into [`Line`], whose `at` is the text
    /// `to_json` writes, and only `at` is parsed again (#2164). A field
    /// the reading has not is refused, as the reading's own `Deserialize`
    /// refuses one.
    pub fn from_json(line: &str) -> Result<Reading> {
        let line: Line = serde_json::from_str(line).context("a log line that is not a reading")?;
        let at = line
            .at
            .parse::<Datetime>()
            .context("a log line whose `at` is not a datetime")?;
        Ok(Reading {
            v: line.v,
            instrument: line.instrument,
            scope: line.scope,
            rule: line.rule,
            status: line.status,
            numbers: line.numbers,
            message: line.message,
            adr: line.adr,
            key: line.key,
            commit: line.commit,
            at,
            session: line.session,
            groups: line.groups,
            recipe: line.recipe,
        })
    }
}

/// A reading as one line of the log holds it: [`Reading`]'s fields, with
/// `at` the text `to_json` writes in place of toml's datetime, whose own
/// `Deserialize` reads only toml's form of it.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Line {
    #[serde(default)]
    v: Version,
    instrument: String,
    scope: String,
    rule: String,
    status: Status,
    numbers: BTreeMap<String, Number>,
    message: String,
    adr: String,
    #[serde(alias = "tree")]
    key: String,
    commit: String,
    at: String,
    #[serde(default)]
    session: String,
    #[serde(default)]
    groups: BTreeMap<String, Number>,
    #[serde(default)]
    recipe: Option<String>,
}

/// A line of a log handed in, in either shape the published history has
/// kept a reading in (harness-design.md §4.6).
#[derive(Deserialize)]
#[serde(untagged)]
enum Kept {
    /// A reading whole, as `log.jsonl` writes it; a field this build does
    /// not know is ignored, so a newer harness's line still reads.
    Whole {
        #[serde(default)]
        v: Version,
        instrument: String,
        scope: String,
        status: Status,
        at: String,
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
    /// The lean row the history kept before it kept readings whole
    /// (#1747): instrument, scope, status, at, commit and numbers.
    Lean {
        i: String,
        s: String,
        t: Status,
        a: String,
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
        let (mut reading, at) = match kept {
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
            } => (
                Reading {
                    v,
                    rule,
                    status,
                    numbers,
                    message,
                    adr,
                    key,
                    commit,
                    session,
                    groups,
                    recipe,
                    ..Reading::new(&instrument, &scope)
                },
                at,
            ),
            Kept::Lean { i, s, t, a, c, n } => (
                Reading {
                    status: t,
                    numbers: n,
                    commit: c,
                    ..Reading::new(&i, &s)
                },
                a,
            ),
        };
        reading.at = at
            .parse()
            .with_context(|| format!("a line whose time {at:?} is not a datetime"))?;
        Ok(reading)
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

/// The store: a directory of readings, their index and their log
/// (harness-design.md §4.2, revision 83; #2164).
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
///
/// A store written before revision 83 kept one `log.jsonl`; the first
/// record or read that finds one splits it into the log and the index and
/// keeps it as [`MIGRATED`] ([`Store::migrate`]).
#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
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

    /// A store rooted at `dir`, whether or not it exists yet.
    pub fn at(dir: impl Into<PathBuf>) -> Store {
        Store { root: dir.into() }
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
    /// store (harness-design.md §4.2); an instrument that ignores the
    /// outcome says so, as `permit` does, whose exit is 0 whatever it
    /// decides (§4.4), and a measurement records through
    /// [`Store::record_measurement`], which owes none.
    pub fn record(&self, readings: &[Reading]) -> Result<Outcome> {
        self.migrate()?;
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

    /// One reading: its file, replaced whole through [`replace`], its
    /// index entry, and its line of the log, which goes through
    /// [`Store::append_line`], the one append.
    fn write(&self, reading: &Reading) -> Result<()> {
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

    /// Every reading another store's log holds, landed in this one's log
    /// and index, and that log emptied: the retire of a worktree (`xtask
    /// worktree retire`) lands its readings here, so a branch's passes
    /// stand at its merge, found by the key they were taken over (#1252).
    /// The other store's files of each scope stay where they are: each is
    /// a scope's last run over a tree the store landed in may not have
    /// (#35). Its log is read whole, the one file a store kept before
    /// revision 83 and the files of each instrument after it, and each is
    /// emptied once landed, so a retire that refuses after this and the
    /// call that follows cannot land a reading twice. How many lines
    /// were landed.
    pub fn land(&self, from: &Store) -> Result<usize> {
        self.migrate()?;
        let mut sources = vec![from.root.join(LEGACY)];
        sources.extend(shards(&from.root.join(LOG))?);
        let mut landed = 0;
        for source in sources {
            let bytes = match fs::read(&source) {
                Ok(bytes) => bytes,
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
                Err(err) => {
                    return Err(err).with_context(|| format!("reading {}", source.display()));
                }
            };
            landed += self.split(&bytes)?;
            fs::write(&source, "").with_context(|| format!("emptying {}", source.display()))?;
        }
        Ok(landed)
    }

    /// The one log of a store written before revision 83, split into the
    /// log and the index, and kept beside them as [`MIGRATED`]: done by the
    /// first record or read that finds it, and done once, since the rename
    /// that claims the file is what decides which process splits it. A
    /// harness built before revision 83 that writes `log.jsonl` again
    /// leaves one the next record splits the same way, its lines added to
    /// [`MIGRATED`]. A line that is no reading, which a write the kernel
    /// split leaves behind, is kept as it was in [`UNREAD`].
    pub fn migrate(&self) -> Result<()> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let legacy = self.root.join(LEGACY);
        if !legacy.exists() {
            return Ok(());
        }
        let claimed = self.root.join(format!(
            ".{LEGACY}.{}.{}.splitting",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        match fs::rename(&legacy, &claimed) {
            Ok(()) => {}
            // Another process claimed it between the look and the rename.
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(err) => {
                return Err(err).with_context(|| format!("claiming {}", legacy.display()));
            }
        }
        let bytes = fs::read(&claimed).with_context(|| format!("reading {}", claimed.display()))?;
        self.split(&bytes)?;
        let kept = self.root.join(MIGRATED);
        // The first split's file becomes the kept one whole; a later one,
        // a stale harness's, is added to it.
        if fs::hard_link(&claimed, &kept).is_err() {
            let mut file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&kept)
                .with_context(|| format!("opening {}", kept.display()))?;
            file.write_all(&bytes)
                .with_context(|| format!("appending to {}", kept.display()))?;
        }
        fs::remove_file(&claimed).with_context(|| format!("removing {}", claimed.display()))
    }

    /// Every line of `lines` added to the log of its instrument as it is,
    /// and the latest reading of each scope over each key put in the
    /// index unless the index holds a later one: what [`Store::migrate`]
    /// and [`Store::land`] share. The index is written once per key, the
    /// newest [`KEPT`] of each scope, after the lines. How many lines.
    fn split(&self, lines: &[u8]) -> Result<usize> {
        let mut latest: BTreeMap<PathBuf, Reading> = BTreeMap::new();
        let mut count = 0;
        for line in lines.split(|b| *b == b'\n').filter(|l| !l.is_empty()) {
            count += 1;
            let mut whole = line.to_vec();
            whole.push(b'\n');
            let read = std::str::from_utf8(line)
                .ok()
                .and_then(|text| Reading::from_json(text).ok());
            let Some(reading) = read else {
                Self::append_line(&self.shard(UNREAD)?, &whole)?;
                continue;
            };
            Self::append_line(&self.shard(&reading.instrument)?, &whole)?;
            if let Some(entry) = self.indexed(&reading)? {
                let later = |held: &Reading| reading.at.to_string() >= held.at.to_string();
                if latest.get(&entry).is_none_or(later) {
                    latest.insert(entry, reading);
                }
            }
        }
        // The newest of each scope, by the time each reading was taken.
        let mut by_dir: BTreeMap<PathBuf, Vec<(String, PathBuf, Reading)>> = BTreeMap::new();
        for (entry, reading) in latest {
            let dir = entry.parent().unwrap_or(&self.root).to_path_buf();
            by_dir
                .entry(dir)
                .or_default()
                .push((reading.at.to_string(), entry, reading));
        }
        for (dir, mut entries) in by_dir {
            entries.sort_by(|a, b| b.0.cmp(&a.0));
            for (_, entry, reading) in entries.into_iter().take(KEPT) {
                let held = Self::read_file(&entry).ok();
                if held.is_none_or(|held| reading.at.to_string() >= held.at.to_string()) {
                    let text = toml::to_string(&reading).context("rendering the reading")?;
                    replace(&entry, text.as_bytes())?;
                }
            }
            prune(&dir)?;
        }
        Ok(count)
    }

    /// The one append to a log, and the rule that a line reaches it whole
    /// or not at all (#56): `line`, rendered whole with its newline, is
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
        if let Some(dir) = log.parent() {
            fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
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

    /// Of `instrument`, the latest reading of each scope of `keys` taken
    /// over the content its key names, by scope: the scope's file when the
    /// reading there is over it, since that one is the latest of all, and
    /// otherwise the index's entry for the key. The index is where a
    /// retired worktree's readings are: `worktree retire` lands its log
    /// here and leaves its files, so a branch's pass stands at its merge
    /// only when it is found by the key it was taken over (#1252). A scope
    /// with no reading over its key is absent, and so is one whose key is
    /// empty, which names no content.
    pub(crate) fn latest_over(
        &self,
        instrument: &str,
        keys: &BTreeMap<String, String>,
        pin: &str,
    ) -> Result<BTreeMap<String, Reading>> {
        let sought: Vec<(String, String, String)> = keys
            .iter()
            .map(|(scope, key)| (instrument.to_owned(), scope.clone(), key.clone()))
            .collect();
        Ok(self
            .latest_over_each(&sought, pin)?
            .into_iter()
            .map(|((_, scope), reading)| (scope, reading))
            .collect())
    }

    /// [`Store::latest_over`] for several instruments at once, each sought
    /// as `(instrument, scope, key)` and found by instrument and scope:
    /// what `done` asks of a touched crate whose file holds a reading over
    /// another key, such as a refused commit's, while a pass over the key
    /// now is in the index (#1312). A lookup reads the scope's file and the
    /// index's entries for the key, and no log.
    pub(crate) fn latest_over_each(
        &self,
        sought: &[(String, String, String)],
        pin: &str,
    ) -> Result<BTreeMap<(String, String), Reading>> {
        self.migrate()?;
        let mut found = BTreeMap::new();
        for (instrument, scope, key) in sought.iter().filter(|(_, _, key)| !key.is_empty()) {
            // A file that is no reading is no reading over the key, and
            // the index is asked instead, as for a stale one (#1235).
            let reading = match self.read(instrument, scope).ok().flatten() {
                Some(reading) if reading.fresh_for(key, pin) => Some(reading),
                _ => self
                    .entries(instrument, scope, key)?
                    .into_iter()
                    .filter(|r| r.fresh_for(key, pin))
                    .max_by_key(|r| r.at.to_string()),
            };
            if let Some(reading) = reading {
                found.insert((instrument.clone(), scope.clone()), reading);
            }
        }
        Ok(found)
    }

    /// The last reading of `instrument` over `scope` this store holds,
    /// by the time it was taken: the scope's file or, where a retired
    /// worktree's later one was landed in the index and not the file, the
    /// index's latest entry. None where the store holds neither.
    pub(crate) fn last_of(&self, instrument: &str, scope: &str) -> Result<Option<Reading>> {
        self.migrate()?;
        let mut last = self.read(instrument, scope).ok().flatten();
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
        let dir = self.dir_of(instrument, scope)?;
        let listing = match fs::read_dir(&dir) {
            Ok(listing) => listing,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(err).with_context(|| format!("listing {}", dir.display())),
        };
        let mut keys = Vec::new();
        for entry in listing {
            let path = entry
                .with_context(|| format!("listing {}", dir.display()))?
                .path();
            if let Some(key) = index_key(&path) {
                keys.push((key.to_owned(), path));
            }
        }
        Ok(keys)
    }

    /// Which of `keys` a reading of `instrument` over `scope` was taken
    /// over, whatever its status or recipe: the scope's file, then the
    /// index's keys of the scope, and for a key neither holds, the log of
    /// the instrument, where a key the index let go of still stands, a
    /// line of either version ([`V`]). What `permit`'s `unless` reads as
    /// evidence that an instrument measured a commit's content, by the key
    /// a commit's objects give ([`crate::key::Snapshot::commit`], #1448),
    /// however long ago. An empty key names no content and is never found.
    pub(crate) fn measured_over(
        &self,
        instrument: &str,
        scope: &str,
        keys: &BTreeSet<String>,
    ) -> Result<BTreeSet<String>> {
        /// The key's field in a line of the log, as each version spells it.
        const SPELLED: [&str; 2] = ["\"key\":\"", "\"tree\":\""];
        self.migrate()?;
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
        self.migrate()?;
        let path = self.shard(instrument)?;
        if !path.exists() {
            return Ok(Log::default());
        }
        let text =
            fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
        Ok(Log::of(&text, Reading::from_json))
    }

    /// Every instrument's log at once, in the order the readings were
    /// taken: what `status --every` renders. The one reader of the whole
    /// log; every other reads the instruments it renders.
    pub fn history_all(&self) -> Result<Log> {
        self.migrate()?;
        let mut all = Log::default();
        for path in shards(&self.root.join(LOG))? {
            let text =
                fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
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
        self.migrate()?;
        let mut tallies = BTreeMap::new();
        for path in shards(&self.root.join(LOG))? {
            let Some(name) = path.file_stem().map(|s| s.to_string_lossy().into_owned()) else {
                continue;
            };
            if name == UNREAD {
                continue;
            }
            let text =
                fs::read_to_string(&path).with_context(|| format!("reading {}", path.display()))?;
            let lines: Vec<(Status, String)> = text
                .lines()
                .filter_map(|l| serde_json::from_str::<Brief>(l).ok())
                .map(|b| (b.status, b.at))
                .collect();
            tallies.insert(name, lines);
        }
        Ok(tallies)
    }

    /// Every reading in the store, the last of each scope, in path order,
    /// and every file of the store that is not one, by its path; the
    /// index's entries are no scope's last reading and are left out. A
    /// missing store is empty. A file a crash left empty or cut short,
    /// which [`replace`] no longer can (#1235), is named and skipped rather
    /// than failing every rendering of the store, as the log's torn line
    /// is: it costs its own row, and the next run of its instrument writes
    /// it again.
    pub fn readings(&self) -> Result<Files> {
        let mut paths = Vec::new();
        if self.root.is_dir() {
            walk(&self.root, &mut paths)?;
        }
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
        let text =
            fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        toml::from_str(&text).with_context(|| format!("{}", path.display()))
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

/// The index of one scope cut to its newest [`KEPT`] entries, by the time
/// each was written. Another writer pruning at once may remove a file
/// first, which is the same end.
fn prune(dir: &Path) -> Result<()> {
    let Ok(listing) = fs::read_dir(dir) else {
        return Ok(());
    };
    let mut entries: Vec<(SystemTime, PathBuf)> = listing
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| index_key(p).is_some())
        .filter_map(|p| Some((fs::metadata(&p).ok()?.modified().ok()?, p)))
        .collect();
    if entries.len() <= KEPT {
        return Ok(());
    }
    entries.sort_by_key(|(written, _)| std::cmp::Reverse(*written));
    for (_, path) in entries.into_iter().skip(KEPT) {
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err).with_context(|| format!("removing {}", path.display())),
        }
    }
    Ok(())
}

/// The files of the log under `dir`, in name order; none where it is not.
fn shards(dir: &Path) -> Result<Vec<PathBuf>> {
    let listing = match fs::read_dir(dir) {
        Ok(listing) => listing,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err).with_context(|| format!("listing {}", dir.display())),
    };
    let mut paths: Vec<PathBuf> = listing
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
        .collect();
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
        if self.unreadable.is_empty() {
            return None;
        }
        let paths: Vec<String> = self
            .unreadable
            .iter()
            .map(|path| {
                path.strip_prefix(root)
                    .unwrap_or(path)
                    .display()
                    .to_string()
            })
            .collect();
        let names: Vec<&str> = paths.iter().map(String::as_str).collect();
        Some(format!(
            "{DIR}: {} reading{} unreadable at {}; the next run of its instrument writes it again.",
            self.unreadable.len(),
            plural(self.unreadable.len()),
            listed(&names)
        ))
    }
}

/// `bytes` as the whole of the file at `path`, or the file as it stood:
/// written to a file of its own beside `path` and renamed over it, so a
/// reader at the same moment, another worktree's hook or an instrument
/// beside it, sees the old file or the new one and never one cut short,
/// and a disk that fills leaves the old one standing (#1235). The rename is
/// within one directory, which POSIX makes atomic. The file beside is
/// hidden and ends in `.tmp`, so no reader of `.toml` or `.json` files
/// takes it for what it replaces, and it is removed when the write fails.
pub(crate) fn replace(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let dir = path
        .parent()
        .with_context(|| format!("{} names no directory", path.display()))?;
    fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let name = path
        .file_name()
        .with_context(|| format!("{} names no file", path.display()))?
        .to_string_lossy();
    let beside = dir.join(format!(
        ".{name}.{}.{}.tmp",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let written = fs::write(&beside, bytes)
        .with_context(|| format!("writing {}", beside.display()))
        .and_then(|()| {
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
    /// published history, whose older rows are lean (#1747). Any line
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
            "{name}: {} {noun} unreadable at {}.",
            self.unreadable.len(),
            listed(&names)
        ))
    }
}

/// The commits a checkout stands on: the first-parent chain of `HEAD`,
/// each with its distance from the tip. A branch merged into it sits on
/// a second parent and a branch cut behind the tip is off it past the cut,
/// so the readings their worktrees took, which `worktree retire` appends
/// into the main checkout's store, are not taken on it (#787).
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
    use std::collections::BTreeSet;

    use super::{KEPT, Log, Reading, Status, Store, V, civil_from_days, days_from_civil, listed};
    use crate::scratch::Tree;

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

    /// The `n`th key, forty hex digits.
    fn key(n: usize) -> String {
        format!("{n:040x}")
    }

    /// A reading lands in the index under its key, the newest [`KEPT`] of
    /// a scope stay, and the latest reading over a key is found there
    /// while the scope's file has moved on to another (#2164). A key the
    /// index let go reads as never measured to `done`, and is still found
    /// in the instrument's log by `permit`'s look for evidence.
    #[test]
    fn a_reading_is_found_by_its_key_and_the_newest_keys_stay() {
        let dir = Tree::new("store-index").unwrap();
        let store = Store::at(dir.path());
        for n in 0..KEPT + 8 {
            let at = format!("2026-10-06T10:{n:02}:00Z");
            store
                .record(&[over("budget", "ext/pcre", &key(n), &at)])
                .unwrap();
        }
        let kept: BTreeSet<String> = store
            .keys_of("budget", "ext/pcre")
            .unwrap()
            .into_iter()
            .map(|(key, _)| key)
            .collect();
        assert_eq!(kept.len(), KEPT);
        assert!(kept.contains(&key(KEPT + 7)) && !kept.contains(&key(0)));
        let sought = |n: usize| [("budget".to_owned(), "ext/pcre".to_owned(), key(n))];
        let found = store.latest_over_each(&sought(KEPT + 1), "").unwrap();
        assert_eq!(
            found.values().next().map(Reading::key),
            Some(key(KEPT + 1).as_str()),
            "found by its key while the scope's file holds the last"
        );
        assert!(store.latest_over_each(&sought(0), "").unwrap().is_empty());
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
    /// on a line of the first version, which carries no `v` (#1719).
    #[test]
    fn a_key_the_index_let_go_is_found_in_a_line_of_either_version() {
        let dir = Tree::new("store-versions").unwrap();
        let store = Store::at(dir.path());
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
