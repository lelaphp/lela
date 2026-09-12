//! `xtask done`, the definition of done (harness-design.md §4.5): the
//! conditions unmet for the changes since a base commit, computed from git
//! and the store and nothing else, so its three callers (§4.5: the Stop
//! command hook, pre-push, and CI per commit) read one list. The base is
//! the merge base of `HEAD` with the branch's track ([`Base`], taken through
//! `track`, the one statement of the rule), or with `--base`; the changes are every file the working tree differs in from
//! it, committed since or not, plus the untracked files.
//!
//! The conditions, one sentence each: the six of §4.5, and the seventh
//! #31 proposes for it, a person's sentence still to land there:
//!
//! 1. Every crate with a changed file has a fresh passing reading from each
//!    of [`FRESH`]. Fresh means the reading's `tree` is the content hash over
//!    the crate's sources now (§13 D.18), the hash every instrument stamps;
//!    an instrument that does not exist yet has written no reading, so its
//!    condition is unmet and the list is honest from day one. The two
//!    reading not held to its status is the `phpt` skip the missing engine
//!    binary causes, [`no_binary_yet`] (#132); a `witness` reading is held
//!    to its status, the linked engine having served it (#119).
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
//! 7. The ticket the branch records (`Track::of`'s `ticket`, #30) has no
//!    unticked acceptance box: the `ticket` reading over its number, the
//!    one `xtask ticket read` writes (#28), is fresh and passes, and its
//!    message names the boxes when it does not; a ticket with no box at
//!    all is held the same way, since it cannot be shown done, and a
//!    close the tracker refused with every box ticked is met here, the
//!    boxes being the condition and not the close. A branch that records
//!    no ticket, and a detached `HEAD`, are not asked and nothing here
//!    says the word, the way `xtask` is not asked for a witness (#31).
//!    In practice this is the Stop hook's and pre-push's condition: the
//!    record is the clone's own configuration, which a push does not
//!    carry, and a pull request's checkout is detached, so CI's `done`
//!    is not asked and has no store to answer from if it were.
//!
//! **What makes a `ticket` reading fresh** (#31). A ticket has no tree of
//! its own, so the reading is stamped over the workspace, and it is fresh
//! while the workspace's tree hash still matches: the reading is a claim
//! about a pairing, the boxes as the tracker had them and the work as it
//! stood, and when the work changes the claim is re-asserted by reading
//! again, which fetches the body again. The rule is §13 D.18's applied to
//! the whole tree, and `done` never reaches the tracker: it is computed
//! from git and the store and nothing else, the Stop hook runs it at
//! every turn end, offline or not, and the one tracker call is `xtask
//! ticket read` at the agent's hand after its last edit, outside the
//! hook. What the rule costs, written down: every write after the read
//! stales it, the ones an instrument makes included (`witness --accept`,
//! `regions`, `generate`), so on a branch with a ticket the read is the
//! last act of a turn that wrote anything, one `gh` call per such turn.
//! What it does not catch: a body edited at the tracker while the work
//! stands still is a green `done` until the next edit of the work, after
//! which the reading is stale and the read that refreshes it sees the
//! new box. An age window was the other candidate,
//! and it is weaker in both directions: it lets a reading stand over work
//! that changed under it, and stales one over work that did not.
//!
//! With `--hook` the command reads the Stop hook's stdin (the shape in
//! `data/pre-day-1.md` §4): exit 0 and nothing printed when
//! `stop_hook_active` is true, otherwise `{"decision":"block","reason":…}`
//! with the list while anything is unmet. Every verdict is one reading,
//! `done` over `workspace`. At the Stop, and there alone, the reply is
//! read as well (#32, the `reply` module): a session the store says closed a
//! ticket whose `last_assistant_message` carries the language of not
//! filing is held once with the sentences it matched, a second reading of
//! `done` over the rule; the second Stop passes as it always did, so the
//! hold costs one turn.

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
use crate::reading::{Number, Outcome, Reading, Status, Store, plural};
use crate::reply;
use crate::ticket;
use crate::track::{self, Ticket};

/// The instrument name the reading carries.
pub(crate) const INSTRUMENT: &str = "done";
/// The scope of the verdict's reading: the whole tree.
pub(crate) const WORKSPACE: &str = "workspace";
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
    /// The ticket the branch records, the one condition 7 asked; `None`
    /// for a branch that records none, which is not asked.
    pub ticket: Option<Ticket>,
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
            "done: {} changed file{} since {} in {}{}\n",
            self.changed.len(),
            plural(self.changed.len()),
            self.base,
            if self.crates.is_empty() {
                "no crate".to_owned()
            } else {
                self.crates.join(", ")
            },
            self.ticket
                .map(|ticket| format!(", for {ticket}"))
                .unwrap_or_default()
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

    /// The reading: `done` over `workspace`, stamped over the whole tree.
    pub(crate) fn reading(&self, manifest: &Manifest, root: &Path) -> Result<Reading> {
        let mut reading = Reading::for_scope(INSTRUMENT, WORKSPACE, manifest, root)?;
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
    let ticket = track::head_ticket(root)?;
    let mut unmet = Vec::new();
    let mut notes = Vec::new();

    for krate in &crates {
        fresh_readings(root, store, krate, &mut unmet)?;
    }
    locks(root, manifest, store, &changed, &mut unmet)?;
    open_rows(root, &base.commit, registry, &crates, &changed, &mut unmet)?;
    over_the_diff(root, manifest, &changed, &mut unmet, &mut notes)?;
    if let Some(ticket) = ticket {
        boxes(root, store, ticket, &mut unmet)?;
    }

    Ok(Verdict {
        base,
        changed,
        crates: crates.iter().map(|c| c.name.clone()).collect(),
        ticket,
        unmet,
        notes,
    })
}

/// Condition 7: the branch's ticket has no unticked box, read from the
/// `ticket` reading over its number and never from the tracker. The
/// reading is fresh while the workspace's tree, [`ticket::STAMPED_OVER`],
/// still matches (the rule at the top of this module); a
/// missing or stale one says what to run, and a failing one carries the
/// instrument's message, which names each unticked box.
fn boxes(root: &Path, store: &Store, ticket: Ticket, unmet: &mut Vec<String>) -> Result<()> {
    const READ: &str = "run `cargo xtask ticket read`";
    let Some(reading) = store.read(ticket::INSTRUMENT, &ticket::scope(ticket))? else {
        unmet.push(format!("ticket: no reading for {ticket}; {READ}"));
        return Ok(());
    };
    if reading.tree() != git::tree_hash(root, Path::new(ticket::STAMPED_OVER))? {
        unmet.push(format!(
            "ticket: the reading for {ticket} is stale; the tree changed since it was read; {READ}"
        ));
        return Ok(());
    }
    // The boxes are the condition, read from the numbers: a close the
    // tracker refused with every box ticked fails as a reading and is
    // met here, since the ticket has no unticked box; a reading that
    // counts nothing is held to its status.
    let count = |key: &str| match reading.numbers.get(key) {
        Some(Number::Int(n)) => Some(*n),
        _ => None,
    };
    let held = match (count("criteria"), count("unticked")) {
        (Some(criteria), Some(unticked)) => criteria == 0 || unticked > 0,
        _ => reading.status != Status::Pass,
    };
    if held {
        unmet.push(format!("ticket: {}", reading.message));
    }
    Ok(())
}

/// Whether a reading that is not a pass is met all the same because the
/// engine has no binary yet (#132).
///
/// `xtask phpt` runs the corpus it measures through the engine's own
/// binary. There is none until `crates/php` reaches `serve`, so the lane
/// writes the skip that says so, and holding that skip to its status stops
/// every crate that owns a subset.
///
/// The skip cannot be held to numbers, because nothing ran at all — there
/// is no oracle standing in for the corpus the way there is for a fence,
/// and no second adapter of the `ScriptExec` seam that serves a `.phpt`
/// with its sections and its own directory the way `witness` has one for a
/// snippet (#119). A person decided (#132) that such a skip is met anyway,
/// and so that a crate is done with no corpus test run, which is the cost.
/// It is held to the cause instead: `engine_absent` says the lane found no
/// binary, which the lane alone can tell from a subset of a crate not yet
/// created or one whose every test skipped, and nothing counted against
/// what it did read. The rung goes away by itself when the binary exists.
/// harness-design.md §4.5 condition 1 is where that decision is written
/// down, with the two ways it was taken over.
///
/// A **witness** had a rung of its own here while the missing binary was
/// why its reading was a skip (#106). It is not any more: the linked
/// engine serves every snippet in this process, so the reading is a pass
/// or a fail on its own merits and the crate is held to it (#119).
fn no_binary_yet(instrument: &str, reading: &Reading) -> bool {
    if reading.status != Status::Skip {
        return false;
    }
    let count = |key: &str| match reading.numbers.get(key) {
        Some(Number::Int(n)) => Some(*n),
        _ => None,
    };
    match instrument {
        "phpt" => {
            count("engine_absent") == Some(1)
                && count("failed") == Some(0)
                && count("diverged") == Some(0)
                && count("borked") == Some(0)
        }
        _ => false,
    }
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
        if reading.status != Status::Pass && !no_binary_yet(instrument, &reading) {
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

/// The Stop hook's stdin: the keys `done` reads (harness-design.md §13
/// D.16). The reply is read only for a session the store says closed a
/// ticket.
#[derive(Debug, Deserialize)]
struct StopStdin {
    #[serde(default)]
    stop_hook_active: bool,
    #[serde(default)]
    session_id: String,
    #[serde(default)]
    last_assistant_message: String,
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
    let verdict = match judge(root, manifest, registry, store, None) {
        Ok(verdict) => verdict,
        Err(err) => return Some(block(&format!("done could not judge the changes: {err:#}"))),
    };
    // The reply, for a session that closed a ticket and no other.
    let unfiled = match reply::closed_by(store, &stop.session_id)
        .and_then(|closed| reply::read(manifest, &closed, &stop.last_assistant_message))
    {
        Ok(unfiled) => unfiled,
        Err(err) => return Some(block(&format!("done could not read the reply: {err:#}"))),
    };
    // The hook's exit is 0 and its verdict is the JSON; the outcome of
    // recording it is neither.
    let recorded = verdict.reading(manifest, root).and_then(|mut r| {
        // The Stop's verdict carries the session: what marks a later
        // turn's close as this turn's and not an earlier one's.
        stop.session_id.clone_into(&mut r.session);
        let mut readings = vec![r];
        if let Some(unfiled) = &unfiled {
            readings.push(unfiled.reading(root, &stop.session_id)?);
        }
        store.record(&readings)
    });
    if let Err(err) = recorded {
        eprintln!("xtask done: the verdict stands but was not recorded: {err:#}");
    }
    match (verdict.met(), unfiled) {
        (true, None) => None,
        (false, None) => Some(block(&verdict.render())),
        (true, Some(unfiled)) => Some(block(&unfiled.render())),
        (false, Some(unfiled)) => Some(block(&format!("{}{}", verdict.render(), unfiled.render()))),
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
    use super::{FRESH, instruments_of, no_binary_yet};
    use crate::manifest::Crate;
    use crate::reading::{Reading, Status};

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

    /// A phpt skip is met only where the lane says the engine had no
    /// binary (#132), which is the cause a person chose to excuse, and
    /// nothing counted against what it read. A subset of a crate not yet
    /// created and one whose every test skipped both write `engine_absent`
    /// zero and stay unmet, so the excuse does not spread to them.
    #[test]
    fn a_phpt_skip_is_met_only_where_the_engine_had_no_binary() {
        let skip = |engine_absent: u64, failed: u64, diverged: u64, borked: u64| {
            let mut reading = Reading::new("phpt", "ext/standard");
            reading.status = Status::Skip;
            reading.count("engine_absent", engine_absent).unwrap();
            reading.count("failed", failed).unwrap();
            reading.count("diverged", diverged).unwrap();
            reading.count("borked", borked).unwrap();
            reading
        };
        // No binary, and nothing counted against the subset.
        assert!(no_binary_yet("phpt", &skip(1, 0, 0, 0)));
        // The crate is not yet created, or every test skipped: not this.
        assert!(!no_binary_yet("phpt", &skip(0, 0, 0, 0)));
        // Something counted against it, so the skip is not a clean one.
        assert!(!no_binary_yet("phpt", &skip(1, 1, 0, 0)));
        assert!(!no_binary_yet("phpt", &skip(1, 0, 1, 0)));
        assert!(!no_binary_yet("phpt", &skip(1, 0, 0, 1)));
        // A phpt reading with no such number is held to its status.
        let mut bare = Reading::new("phpt", "ext/standard");
        bare.status = Status::Skip;
        assert!(!no_binary_yet("phpt", &bare));
    }

    /// A witness skip is held to its status, whatever its numbers say
    /// (#119). The rung #106 gave it was the missing engine binary, and
    /// the binary is no longer why a witness reading is a skip: the linked
    /// engine serves every snippet in this process, so a skip that is left
    /// is the pinned oracle's absence or a control, and neither is met.
    #[test]
    fn a_witness_skip_is_held_to_its_status() {
        let skip = |run: u64, failed: u64, missing: u64| {
            let mut reading = Reading::new("witness", "php_engine");
            reading.status = Status::Skip;
            reading.count("run", run).unwrap();
            reading.count("failed", failed).unwrap();
            reading.count("missing", missing).unwrap();
            reading
        };
        // The numbers a clean run has, and the skip is unmet all the same.
        assert!(!no_binary_yet("witness", &skip(5, 0, 0)));
        assert!(!no_binary_yet("witness", &skip(0, 0, 0)));
        assert!(!no_binary_yet("witness", &skip(5, 1, 0)));
        // The numbers are the phpt rung's own and are not read here.
        assert!(!no_binary_yet("phpt", &skip(5, 0, 0)));
    }
}
