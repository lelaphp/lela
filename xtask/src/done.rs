//! `xtask done`, the definition of done (harness-design.md §4.5): the
//! conditions unmet for the changes since a base commit, computed from git
//! and the store and nothing else, so its three callers (§4.5: the Stop
//! command hook, pre-push, and CI per commit) read one list. The base is
//! the merge base of `HEAD` with the branch's track ([`Base`], taken through
//! `track`, the one statement of the rule), or with `--base`; the changes are every file the working tree differs in from
//! it, committed since or not, plus the untracked files.
//!
//! The six conditions, one sentence each:
//!
//! 1. Every crate with a changed file has a fresh passing reading from each
//!    of [`FRESH`]. Fresh means the reading's `tree` is the content hash over
//!    the crate's sources now (§13 D.18), the hash every instrument stamps;
//!    an instrument that does not exist yet has written no reading, so its
//!    condition is unmet and the list is honest from day one.
//! 2. Every kernel or native added has a witness: the `witness` reading's
//!    `missing` is zero for the crate.
//! 3. A lock that differs from its regenerated form is in the diff, read
//!    from the `api-check` seam readings and the diff's paths.
//! 4. No `open` divergence row touches a changed scope: a touched crate's
//!    name or a changed path in the row's id, or a row this diff proposes.
//! 5. `permit` over the diff denies nothing: every changed file, judged as
//!    the `Write` that would put its content in place.
//! 6. The manifest, the pin and the registry (`seams.toml`, `oracle.toml`,
//!    `docs/divergence.toml`, and the plugin's own files: what the
//!    manifest's `deny = true` rule covers) are unchanged, or changed by a
//!    person; the hook cannot tell, so such a change is a note, never an
//!    unmet condition, and pre-push, run by the person, passes on it.
//!
//! With `--hook` the command reads the Stop hook's stdin (the shape in
//! `data/pre-day-1.md` §4): exit 0 and nothing printed when
//! `stop_hook_active` is true, otherwise `{"decision":"block","reason":…}`
//! with the list while anything is unmet. Every verdict is one reading,
//! `done` over `workspace`.

use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::api_check;
use crate::divergence::{self, Registry};
use crate::git::{self, Change};
use crate::manifest::{Crate, Manifest};
use crate::permit;
use crate::reading::{Number, Outcome, Reading, Status, Store};

/// The instrument name the reading carries.
pub(crate) const INSTRUMENT: &str = "done";
/// The instruments a touched crate needs a fresh passing reading from
/// (condition 1). `phpt`'s is the reading over the subsets the crate's
/// `[[crate]]` row owns, written under the crate's name.
pub const FRESH: [&str; 6] = [
    "clippy",
    "witness",
    "api-check",
    "budget",
    "unsafe-audit",
    "phpt",
];

/// The harness crate; it has no kernel and no native.
pub(crate) const HARNESS: &str = "xtask";

/// The instruments condition 1 asks of one crate: all of [`FRESH`], less
/// `witness` for the harness, which has no kernel and no native, and less
/// `phpt` for a crate the manifest gives no subset. Which crate owns which
/// of the corpus is the `[[crate]]` row's sentence, so a crate that owns
/// none is not asked here rather than asked and answered by the runner's
/// error (harness §4.1, §4.5 condition 1).
pub(crate) fn instruments_of(krate: &Crate) -> impl Iterator<Item = &'static str> {
    let harness = krate.name == HARNESS;
    let measured_by_the_corpus = !krate.subsets().is_empty();
    FRESH.into_iter().filter(move |i| match *i {
        "witness" => !harness,
        "phpt" => measured_by_the_corpus,
        _ => true,
    })
}

pub(crate) use crate::track::Base;

/// What `done` found over one diff.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    /// What the changes are measured since.
    pub base: Base,
    pub changed: Vec<Change>,
    /// The manifest names of the crates with a changed file, in manifest order.
    pub crates: Vec<String>,
    /// One sentence per unmet condition, in the order of the conditions.
    pub unmet: Vec<String>,
    /// What the hook cannot tell: the harness's own files in the diff.
    pub notes: Vec<String>,
}

impl Verdict {
    pub fn met(&self) -> bool {
        self.unmet.is_empty()
    }

    /// The list as text: the head line, the unmet conditions, the notes.
    pub fn render(&self) -> String {
        let mut out = format!(
            "done: {} changed file{} since {} in {}\n",
            self.changed.len(),
            plural(self.changed.len()),
            self.base,
            if self.crates.is_empty() {
                "no crate".to_owned()
            } else {
                self.crates.join(", ")
            }
        );
        if self.met() {
            out.push_str("every condition of done is met\n");
        } else {
            out.push_str("unmet:\n");
            for line in &self.unmet {
                out.push_str("- ");
                out.push_str(line);
                out.push('\n');
            }
        }
        if !self.notes.is_empty() {
            out.push_str("notes:\n");
            for line in &self.notes {
                out.push_str("- ");
                out.push_str(line);
                out.push('\n');
            }
        }
        out
    }

    /// The Stop hook's stdout: the block with the list while anything is
    /// unmet, nothing otherwise (harness-design.md §13 D.16).
    pub(crate) fn hook_stdout(&self) -> Option<Value> {
        (!self.met()).then(|| block(&self.render()))
    }

    /// The reading: `done` over `workspace`, stamped over the whole tree.
    pub(crate) fn reading(&self, manifest: &Manifest, root: &Path) -> Result<Reading> {
        let mut reading = Reading::for_scope(INSTRUMENT, "workspace", manifest, root)?;
        reading.status = if self.met() {
            Status::Pass
        } else {
            Status::Fail
        };
        reading.count("changed", self.changed.len() as u64)?;
        reading.count("crates", self.crates.len() as u64)?;
        reading.count("unmet", self.unmet.len() as u64)?;
        reading.count("notes", self.notes.len() as u64)?;
        reading.message = if self.met() {
            format!("every condition of done is met since {}", self.base)
        } else {
            format!(
                "{} unmet since {}: {}",
                self.unmet.len(),
                self.base,
                self.unmet.join("; ")
            )
        };
        Ok(reading)
    }
}

/// The hold at Stop, the shape recorded in data/pre-day-1.md §4.
fn block(reason: &str) -> Value {
    json!({ "decision": "block", "reason": reason })
}

/// `"s"` past one.
fn plural(n: usize) -> &'static str {
    if n == 1 { "" } else { "s" }
}

/// Whether `id` names `part` as whole path segments: `ext/pcre` is in
/// `phpt:php-src/ext/pcre/tests/basic.phpt` and not in `ext/pcre_ng/…`.
fn names(id: &str, part: &str) -> bool {
    let id = format!("/{}/", id.replace(':', "/"));
    id.contains(&format!("/{}/", part.trim_matches('/')))
}

/// Judge the changes since the merge base with `base`, the branch's
/// track ([`Base::of`]) when none is given.
pub fn judge(
    root: &Path,
    manifest: &Manifest,
    registry: &Registry,
    store: &Store,
    base: Option<&str>,
) -> Result<Verdict> {
    let base = Base::of(root, base)?;
    let changed = git::changed_since(root, &base.commit)?;
    let paths: Vec<&str> = changed.iter().map(|c| c.path.as_str()).collect();
    let crates: Vec<&Crate> = manifest.crates_of(&paths);
    let mut unmet = Vec::new();
    let mut notes = Vec::new();

    for krate in &crates {
        fresh_readings(root, store, krate, &mut unmet)?;
    }
    locks(root, manifest, store, &changed, &mut unmet)?;
    open_rows(root, &base.commit, registry, &crates, &changed, &mut unmet)?;
    over_the_diff(root, manifest, &changed, &mut unmet, &mut notes)?;

    Ok(Verdict {
        base,
        changed,
        crates: crates.iter().map(|c| c.name.clone()).collect(),
        unmet,
        notes,
    })
}

/// Conditions 1 and 2 for one touched crate.
fn fresh_readings(
    root: &Path,
    store: &Store,
    krate: &Crate,
    unmet: &mut Vec<String>,
) -> Result<()> {
    let tree = git::tree_hash(root, &krate.path())?;
    let name = &krate.name;
    for instrument in instruments_of(krate) {
        let Some(reading) = store.read(instrument, name)? else {
            unmet.push(format!("{instrument}: no reading for {name}"));
            continue;
        };
        if reading.tree() != tree {
            unmet.push(format!(
                "{instrument}: the reading for {name} is stale; {name} changed since it was taken"
            ));
            continue;
        }
        if reading.status != Status::Pass {
            unmet.push(format!(
                "{instrument}: {name} {}: {}",
                reading.status.as_str(),
                reading.message
            ));
        }
        if instrument == "witness"
            && let Some(Number::Int(missing)) = reading.numbers.get("missing")
            && *missing > 0
        {
            let s = plural(usize::try_from(*missing).unwrap_or(usize::MAX));
            unmet.push(format!(
                "witness: {missing} kernel{s} or native{s} in {name} without a witness"
            ));
        }
    }
    Ok(())
}

/// Condition 3: a lock that differs is in the diff, and a lock in the diff
/// has a fresh `api-check` reading that passes. A seam reading that is
/// stale says nothing here: the crate the lock lives with changed, and
/// condition 1 lists its `api-check` reading as stale.
fn locks(
    root: &Path,
    manifest: &Manifest,
    store: &Store,
    changed: &[Change],
    unmet: &mut Vec<String>,
) -> Result<()> {
    for seam in &manifest.seams {
        // A lock no [[crate]] row names has no home to hash. One that
        // has is hashed where the manifest maps the seam, the one
        // mapping, so the hash here is the one api-check stamped the
        // seam's reading with.
        if manifest.lock_home(&seam.lock).is_none() {
            continue;
        }
        let Some(dir) = manifest.scope_path(&seam.name) else {
            continue;
        };
        let file = dir.join(&seam.lock);
        let shown = file.display().to_string();
        let in_diff = changed.iter().any(|c| Path::new(&c.path) == file);
        let Some(reading) = store.read(api_check::INSTRUMENT, &seam.name)? else {
            if in_diff {
                unmet.push(format!(
                    "{shown} is in the diff and has no api-check reading"
                ));
            }
            continue;
        };
        let fresh = reading.tree() == git::tree_hash(root, &dir)?;
        match reading.status {
            Status::Fail if fresh => unmet.push(format!(
                "{shown}: {}{}",
                reading.message,
                if in_diff {
                    ""
                } else {
                    "; the lock is not in the diff"
                }
            )),
            Status::Pass if in_diff && !fresh => unmet.push(format!(
                "{shown} is in the diff and its api-check reading predates it"
            )),
            _ => {}
        }
    }
    Ok(())
}

/// Condition 4: no `open` row touches a changed scope. A row the diff
/// itself proposes, one the base's registry does not have, touches it by
/// definition.
fn open_rows(
    root: &Path,
    base: &str,
    registry: &Registry,
    crates: &[&Crate],
    changed: &[Change],
    unmet: &mut Vec<String>,
) -> Result<()> {
    let proposed_here: Vec<String> = if changed.iter().any(|c| c.path == divergence::FILE) {
        let before = git::show(root, base, divergence::FILE)?
            .map(|text| Registry::parse(&text))
            .transpose()
            .with_context(|| format!("{}:{}", base, divergence::FILE))?
            .unwrap_or_default();
        registry
            .rows
            .iter()
            .filter(|r| before.row(&r.id).is_none())
            .map(|r| r.id.clone())
            .collect()
    } else {
        Vec::new()
    };
    for row in registry
        .rows
        .iter()
        .filter(|r| r.status == divergence::Status::Open)
    {
        let touched = if proposed_here.contains(&row.id) {
            Some(format!("{}, proposed in this diff", divergence::FILE))
        } else {
            crates
                .iter()
                .find(|c| names(&row.id, &c.name))
                .map(|c| c.name.clone())
                .or_else(|| {
                    changed
                        .iter()
                        .find(|c| names(&row.id, &c.path))
                        .map(|c| c.path.clone())
                })
        };
        if let Some(what) = touched {
            unmet.push(format!(
                "open divergence {} touches {what}: a person writes the reason, or the difference gets fixed (harness §4.3)",
                row.id
            ));
        }
    }
    Ok(())
}

/// Conditions 5 and 6: every changed file judged as the `Write` of its
/// content, by the one judge pre-commit's `permit --diff` calls. A denial
/// under a rule the manifest marks `deny = true` (the harness's own
/// files) is the note of condition 6; any other is unmet.
fn over_the_diff(
    root: &Path,
    manifest: &Manifest,
    changed: &[Change],
    unmet: &mut Vec<String>,
    notes: &mut Vec<String>,
) -> Result<()> {
    let files = permit::working_files(root, changed)?;
    let diff = permit::judge_files(manifest, root, &files)?;
    for (path, finding) in diff.denials() {
        unmet.push(format!("permit denies {path}: {}", finding.reason()));
    }
    for (path, finding) in diff.a_persons() {
        notes.push(format!(
            "{path} is in the diff: changed by a person, or not; the hook cannot tell ({})",
            finding.rule
        ));
    }
    Ok(())
}

/// The command: judge, write the reading, print the list, fail while
/// anything is unmet.
pub fn run(
    root: &Path,
    manifest: &Manifest,
    registry: &Registry,
    store: &Store,
    base: Option<&str>,
) -> Result<()> {
    let verdict = judge(root, manifest, registry, store, base)?;
    let outcome = store.record(&[verdict.reading(manifest, root)?])?;
    print!("{}", verdict.render());
    match outcome {
        Outcome::Met => Ok(()),
        Outcome::Unmet { .. } => bail!(
            "{} condition{} of done unmet",
            verdict.unmet.len(),
            plural(verdict.unmet.len())
        ),
    }
}

/// The Stop hook's stdin: the one key `done` reads.
#[derive(Debug, Deserialize)]
struct StopStdin {
    #[serde(default)]
    stop_hook_active: bool,
}

const UNREAD_STDIN: &str = "done could not read the Stop hook's stdin";

/// `--hook`: judge the Stop hook's stdin text and give the hook's stdout,
/// `None` for a pass. The second Stop (`stop_hook_active`) passes without
/// judging; whatever goes wrong before a verdict is a block that says so,
/// since the second Stop releases a held turn and nothing releases a
/// silent pass.
pub(crate) fn hook(
    root: &Path,
    manifest: &Manifest,
    registry: &Registry,
    store: &Store,
    stdin: &str,
) -> Option<Value> {
    let stop: StopStdin = match serde_json::from_str(stdin) {
        Ok(stop) => stop,
        Err(err) => return Some(block(&format!("{UNREAD_STDIN}: {err}"))),
    };
    if stop.stop_hook_active {
        return None;
    }
    match judge(root, manifest, registry, store, None) {
        Ok(verdict) => {
            // The hook's exit is 0 and its verdict is the JSON; the
            // outcome of recording it is neither.
            let recorded = verdict
                .reading(manifest, root)
                .and_then(|r| store.record(&[r]));
            if let Err(err) = recorded {
                eprintln!("xtask done: the verdict stands but was not recorded: {err:#}");
            }
            verdict.hook_stdout()
        }
        Err(err) => Some(block(&format!("done could not judge the changes: {err:#}"))),
    }
}

/// `--hook` at the command line: stdin in, the hook's JSON out, exit 0.
pub fn run_hook(root: &Path, manifest: &Manifest, registry: &Registry, store: &Store) {
    let mut text = String::new();
    let output = match std::io::stdin().read_to_string(&mut text) {
        Ok(_) => hook(root, manifest, registry, store, &text),
        Err(err) => Some(block(&format!("{UNREAD_STDIN}: {err}"))),
    };
    if let Some(output) = output {
        println!("{output}");
    }
}

#[cfg(test)]
mod tests {
    use super::{FRESH, instruments_of};
    use crate::manifest::Crate;

    #[test]
    fn the_harness_crate_is_asked_for_clippy_api_check_budget_and_unsafe_audit_only() {
        let asked: Vec<&str> = instruments_of(&Crate::named("xtask")).collect();
        assert_eq!(asked, ["clippy", "api-check", "budget", "unsafe-audit"]);
        let asked: Vec<&str> = instruments_of(&Crate::named("ext/pcre")).collect();
        assert_eq!(asked, FRESH);
    }

    /// An engine crate the manifest gives no subset is not asked for a
    /// phpt reading, the way the harness crate is not; the resolver's
    /// error is never what says so. The same crate with a row that names
    /// one is asked.
    #[test]
    fn a_crate_is_asked_for_a_phpt_reading_only_where_the_manifest_gives_it_one() {
        let asked: Vec<&str> = instruments_of(&Crate::named("php_heap")).collect();
        assert_eq!(
            asked,
            ["clippy", "witness", "api-check", "budget", "unsafe-audit"]
        );
        let row = Crate::named("php_heap").with_phpt(&["Zend/tests"]);
        let asked: Vec<&str> = instruments_of(&row).collect();
        assert_eq!(asked, FRESH);
    }
}
