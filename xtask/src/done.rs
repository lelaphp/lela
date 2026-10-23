//! `xtask done`, the definition of done (harness-design.md §4.5): the
//! conditions unmet for the changes since a base commit, computed from git
//! and the store and nothing else, so its three callers (§4.5: the Stop
//! command hook, pre-push, and CI per commit) read one list. The base is
//! the merge base of `HEAD` with `main` ([`Base`]), or with `--base`; the
//! changes are every file the working tree differs in from it, committed
//! since or not, plus the untracked files.
//!
//! The conditions are [`Condition`]'s variants, in the order the list
//! names what is unmet. Each variant's first paragraph is its sentence, and
//! §4.5 renders those sentences as its list (`status --section
//! conditions`, #2309), so the definition is read from this code and
//! cannot fall behind it. [`judge`] has one arm for each variant.
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

use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::api_check;
use crate::carried::Carried;
use crate::clippy;
use crate::crosscheck;
use crate::divergence::{self, Registry};
use crate::git::{self, Change};
use crate::key::{self, Snapshot};
use crate::manifest::{Crate, Manifest};
use crate::miri;
use crate::permit;
use crate::phpt;
use crate::phpt::against::{self, Against};
use crate::port_map;
use crate::reading::{Number, Reading, Status, Store, plural};
use crate::reply;
use crate::ticket;
use crate::track::{self, MAIN, Ticket, merge_base, short};

/// The instrument name the reading carries.
pub(crate) const INSTRUMENT: &str = "done";
/// The scope of the verdict's reading: the whole tree.
pub(crate) const WORKSPACE: &str = "workspace";
/// The instruments condition 1 knows. Which of them one crate is asked
/// for is [`instruments_of`], since three of the eight are a narrower
/// crate's than "every touched one": `phpt`'s is the reading over the
/// subsets the crate's `[[crate]]` row owns, written under the crate's
/// name, `witness` is no crate's without a kernel or a native, and
/// `port-map` is the one crate `api.lock` lives with alone, and
/// `crosscheck` is an extension's the binary carries, the ones the
/// per-commit lane renders (#269), and `miri` is the memory model's,
/// the crates [`miri::CRATES`] names (#2416). `test` is narrowed for
/// nobody: the harness has tests as every crate has, and it is asked the
/// way `clippy` is (#191).
pub const FRESH: [&str; 10] = [
    "clippy",
    "test",
    "witness",
    "api-check",
    "budget",
    "unsafe-audit",
    "phpt",
    "port-map",
    "crosscheck",
    "miri",
];

/// The harness crate; it has no kernel and no native.
pub(crate) const HARNESS: &str = "xtask";

/// The instruments condition 1 asks of one crate: all of [`FRESH`], less
/// `witness` for the harness, which has no kernel and no native, less
/// `phpt` for a crate the per-commit lane writes no reading for, and less
/// `port-map` for every crate but the one `api.lock` lives with, named by
/// `home_of_the_lock` in the manifest's terms and passed in here. Which
/// crate owns which of the corpus is the `[[crate]]` row's sentence, so a
/// crate that owns none is not asked here rather than asked and answered
/// by the runner's error. The lane runs the subsets of every crate the
/// binary under test carries, `carried`, read from the binary's own
/// declaration: a crate the row marks `feature =` is asked where the
/// binary turns the feature on or the row names an oracle variant, whose
/// subset the lane runs on the engine built with the feature (#2029), and
/// any other is the nightly lane's, which `done`, running per commit, does
/// not ask for a reading only a run by name could write (harness §4.1,
/// §4.5 condition 1, §7, revision 63). `port-map` writes exactly one reading, at
/// the crate the lock lives with, so every other crate would be held to a
/// reading nothing ever writes; that crate is the one asked because the
/// lock is in its tree, and a change to the lock is what makes a row of
/// the join matched or unmatched (harness §4.5 condition 1, #148).
/// `crosscheck` is asked of an `ext/*` crate the binary carries: the lane
/// renders those per commit and every other feature nightly (harness §7),
/// the same line `phpt` stands on, and the reading is the port's progress
/// meter, so an extension whose rendering is not the oracle's is not done
/// (design §7.1, #269). `miri` is asked of the crates the Miri lane runs,
/// [`miri::CRATES`], `php_heap`, the engine's main `unsafe` home, when the
/// change moves its key (#2416). `php_heap` depends on no workspace crate,
/// so its dependency closure is itself and a change that moves its key is
/// a change to it, one that touches it.
pub(crate) fn instruments_of(
    krate: &Crate,
    lock_home: Option<&str>,
    carried: &Carried,
) -> impl Iterator<Item = &'static str> {
    let harness = krate.name == HARNESS;
    let measured_by_the_lane = measured_by_the_lane(krate, carried);
    let carried = carried.carries(krate);
    let home_of_the_lock = lock_home == Some(krate.name.as_str());
    let carried_extension = krate.name.starts_with("ext/") && carried;
    FRESH.into_iter().filter(move |i| match *i {
        "witness" => !harness,
        "phpt" => measured_by_the_lane,
        "port-map" => home_of_the_lock,
        "crosscheck" => carried_extension,
        "miri" => miri::CRATES.contains(&krate.name.as_str()),
        _ => true,
    })
}

/// Whether the per-commit lane runs a subset of `krate`: it owns one, and
/// the binary under test carries it or its row names an oracle variant,
/// whose subset the lane runs on the engine built with the row's feature
/// (#2029). The one rule the lane is composed by and `phpt --base` takes
/// the touched crates' runs by, as condition 1 holds them to those runs.
pub(crate) fn measured_by_the_lane(krate: &Crate, carried: &Carried) -> bool {
    !krate.subsets().is_empty() && (carried.carries(krate) || krate.oracle.is_some())
}

/// The name of the crate `api.lock` lives with, the one crate condition 1
/// asks for a `port-map` reading, or `None` for a manifest that names no
/// crate holding the lock, which is a manifest the instrument itself
/// refuses; `done` is not the place that says so, and asks nobody for the
/// reading instead.
fn lock_home(manifest: &Manifest) -> Option<String> {
    port_map::home_of_the_lock(manifest)
        .ok()
        .map(|krate| krate.name.clone())
}

/// The commit the changes are measured since, and the ref it was taken
/// from: `--base`'s, or `main`, every ticket's branch being cut from it
/// (ADR-0009, harness-design.md §4.5).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Base {
    pub since: String,
    pub commit: String,
}

impl Base {
    /// The merge base of `HEAD` with `since`, `main` for a bare `done`:
    /// the ref first and then `origin`'s, for a checkout that has only the
    /// remote's.
    pub fn of(root: &Path, since: Option<&str>) -> Result<Base> {
        let since = since.unwrap_or(MAIN);
        Ok(Base {
            since: since.to_owned(),
            commit: merge_base(root, "HEAD", since)?,
        })
    }
}

impl std::fmt::Display for Base {
    /// The ref the base was taken from and the commit it resolved to, and
    /// the commit alone when the ref was that commit.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let short = short(&self.commit);
        if self.since == self.commit || self.since == short {
            write!(f, "{short}")
        } else {
            write!(f, "{} ({short})", self.since)
        }
    }
}

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
    /// What the hook cannot tell: the harness's own files in the diff,
    /// and the readings of an instrument this run did not take.
    pub notes: Vec<String>,
    /// The instruments the caller said this run did not take (`--not-run`),
    /// whose missing readings are notes and not unmet (#2477).
    pub not_run: Vec<String>,
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
        if self.met() && self.not_run.is_empty() {
            out.push_str("every condition of done is met\n");
        } else if self.met() {
            out.push_str("every condition of done is met over what ran; ");
            out.push_str(&self.did_not_run());
            out.push('\n');
        } else {
            bullets(&mut out, "unmet", &self.unmet);
        }
        if !self.notes.is_empty() {
            bullets(&mut out, "notes", &self.notes);
        }
        out
    }

    /// The reading: `done` over `workspace`, stamped over the whole tree
    /// as the snapshot the verdict was judged over holds it.
    pub(crate) fn reading(&self, manifest: &Manifest, snapshot: &Snapshot) -> Result<Reading> {
        let mut reading = Reading::for_scope(INSTRUMENT, WORKSPACE, manifest, snapshot)?;
        reading.status = Status::of(self.met());
        reading.count("changed", self.changed.len() as u64)?;
        reading.count("crates", self.crates.len() as u64)?;
        reading.count("unmet", self.unmet.len() as u64)?;
        reading.count("notes", self.notes.len() as u64)?;
        reading.count("not_run", self.not_run.len() as u64)?;
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
        if !self.not_run.is_empty() {
            reading.message = format!("{}; {}", reading.message, self.did_not_run());
        }
        Ok(reading)
    }

    /// What the head line and the reading say of `--not-run`, so a pass
    /// beside it never reads as one over those instruments' readings.
    fn did_not_run(&self) -> String {
        format!("{} did not run in this run", self.not_run.join(" and "))
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

/// The conditions of done (harness-design.md §4.5), in the order the
/// verdict lists what is unmet. A variant's first paragraph is its
/// sentence, the line §4.5's region renders for it (`status --section
/// conditions`); what follows it is this code's account and is not
/// rendered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Condition {
    /// For every crate with a changed file, a fresh passing reading exists
    /// for `clippy`, `test`, `witness`, `api-check`, `budget` and
    /// `unsafe-audit`, for `port-map` where the crate is the one `api.lock`
    /// lives with, and for `crosscheck` in both of its columns where the
    /// crate is an extension the binary carries; `witness` is not asked of
    /// the harness, and fresh means the reading's `key` is the crate's key
    /// now and, under the oracle's recipe, its `recipe` the pin's digest
    /// now. For `phpt`, a touched crate's subset is held to no regression
    /// against the base `done` measures since: every test met at the merge
    /// base is met here, read from the run kept at the merge base's engine
    /// tree and the run kept at this tree's, a comparison a skip does not
    /// meet. The absolute reading is still written and rendered, the port's
    /// meter as the crosscheck is, and the `porting` key's `phpt` half goes
    /// with it, since a partial port no longer fails for the tests it has
    /// not reached. The missing engine binary stays the one excused skip
    /// (#132). The subsets held are those of a crate the binary under test
    /// carries, or one whose row names an oracle variant, which the
    /// per-commit lane runs (revision 63, #2029).
    /// A crate whose library or binary sources name `debug_assertions` is
    /// asked for `clippy` twice, at its name and at `<crate>/release`, the
    /// release profile's lint on its library and binaries (#2392).
    /// For a change that moves `php_heap`'s key (ADR-0011), `done` reads a
    /// fresh `miri` reading of `php_heap` and holds `undefined` to zero
    /// (`unsupported` is reported, not held, under the permissive
    /// provenance #2415 ruled).
    ///
    /// Which instruments one crate is asked for is [`instruments_of`],
    /// over [`FRESH`], `test` among them and asked of every crate, since
    /// every crate has tests and one whose tests do not run is not done
    /// (#191); `port-map` is the only scope that instrument writes a
    /// reading at (#148), and `crosscheck` is the rendering diff the
    /// per-commit lane writes per extension (#269). The key is the hash
    /// over the crate's sources and its declared dependency closure's (§4.2,
    /// §13 D.18), the one every instrument stamps; an instrument that does
    /// not exist yet has written no reading, so its condition is unmet and
    /// the list is honest from day one. `phpt` is [`no_regression`], the
    /// two kept runs diffed by [`against::regressed`], where a test the
    /// compiler no longer reads is a regression and one it never read is
    /// held to nothing, which retires the `unread` rung of #319; where no
    /// run is kept here, a fresh skip the missing engine binary causes is
    /// met, [`no_binary_yet`] (#132). A `witness` reading is held to its
    /// status, the linked engine having served it (#119). A rung is the
    /// manifest's, not a lane's: a crate whose `[[crate]]` row carries
    /// `porting = true` has its `crosscheck` readings read as the port's
    /// meter, [`porting_meter`], fresh and printed but met whatever they
    /// say, while the port is partial (#383). `miri` is asked of the
    /// crates [`miri::CRATES`] names, `php_heap`, when a change touches
    /// it, `php_heap` depending
    /// on no workspace crate; a fail made only of unsupported operations is
    /// met, [`miri_unsupported`], and every other is held (#2416). A crate
    /// whose library or binary sources name `debug_assertions` is asked
    /// for `clippy` twice, at its name and at `<name>/release`, the release
    /// profile's lint ([`scopes_of`], #2392). A
    /// caller's `--not-run` moves an instrument's missing reading off this
    /// list and onto the notes, as CI's does for the oracle job's two while
    /// that job is skipped (#2477).
    Fresh,
    /// Every kernel or native added in the diff has a witness: the
    /// `witness` reading's `missing` is zero for the crate.
    ///
    /// Judged with [`Condition::Fresh`], over the same `witness` reading.
    Witnessed,
    /// If a `.lock` differs from its regenerated form, the lock file is in
    /// the diff, read from the `api-check` seam readings and the diff's
    /// paths.
    LockInDiff,
    /// No `open` divergence touches a changed scope: a touched crate's name
    /// or a changed path in an id the row names or covers, or a row the
    /// diff proposes.
    NoOpenRow,
    /// `permit` over the diff denies nothing: every changed file is judged
    /// as the `Write` that would put its content in place.
    PermitOverTheDiff,
    /// `seams.toml`, `oracle.toml`, `docs/divergence.toml` and the plugin's
    /// own files, what the manifest's `deny = true` rule covers, are
    /// unchanged in the diff, or the diff is by a person (the hook cannot
    /// tell, so the Stop hook reports it as a note, and pre-push, run by
    /// the person, passes).
    ///
    /// Judged with [`Condition::PermitOverTheDiff`]: a denial under a
    /// `deny = true` rule is this note, never unmet.
    APersonsFiles,
    /// The ticket the branch records (`branch.<name>.ticket`, the record
    /// `xtask worktree add --ticket` writes) has no unticked acceptance
    /// box, or was withdrawn, read from the `ticket` reading over its
    /// number and never from the tracker (#31, #221); a branch that
    /// records no ticket, and a detached `HEAD`, are not asked.
    ///
    /// The reading is the one `xtask ticket read` writes (#28), fresh and
    /// passing, and its message names the boxes when it does not; a ticket
    /// with no box at all and not withdrawn is held the same way, since it
    /// cannot be shown done, and a close the tracker refused with every
    /// box ticked is met here, the boxes being the condition and not the
    /// close. A withdrawal that went through is met whatever its boxes
    /// say, the instrument having settled the ticket, and one refused is
    /// held (#221). Nothing here says the word for a branch with no
    /// record, the way `xtask` is not asked for a witness (#31). In
    /// practice this is the Stop hook's and pre-push's condition: the
    /// record is the clone's own configuration, which a push does not
    /// carry, and a pull request's checkout is detached, so CI's `done` is
    /// not asked and has no store to answer from if it were.
    TicketBoxes,
    /// A change `permit --after` kept, one no call's line could write, is
    /// unmet until a person commits it or restores it to `HEAD`, read from
    /// the sensor's record in the store and not from the diff (§4.4,
    /// #1931).
    ///
    /// The record is [`permit::kept_changes`]'s, which a change the person
    /// approved in the session never has.
    KeptChange,
    /// A ticket a merge since the base names, and that `xtask ticket
    /// merged` left open, for want of a ready record or because its
    /// close did not go through (#2804), is unmet until a close replaces
    /// that reading, the merge at `HEAD` being read where
    /// the base is `HEAD`, as on `main` (#2358); a merge that lands part of
    /// a ticket, one with no ready record, a box unticked and an open
    /// ticket blocking it, is a partial landing and is not named (#2394).
    ///
    /// [`ticket::stranded`] reads the `ticket` readings `merged` wrote:
    /// a ticket it left open counts `unclosed` and is named here,
    /// and a partial landing is a `skip` counting `partial`, which is not.
    Stranded,
}

impl Condition {
    /// Every condition, in the order of the list.
    pub(crate) const ALL: [Condition; 9] = [
        Condition::Fresh,
        Condition::Witnessed,
        Condition::LockInDiff,
        Condition::NoOpenRow,
        Condition::PermitOverTheDiff,
        Condition::APersonsFiles,
        Condition::TicketBoxes,
        Condition::KeptChange,
        Condition::Stranded,
    ];
}

/// Judge the changes since the merge base with `base`, `main`
/// ([`Base::of`]) when none is given, each key held to the one `snapshot`
/// gives: the tree is read once, before anything is judged. A reading of
/// an instrument in `not_run` that the store does not hold is a note and
/// not unmet: the caller says the run that would write it did not run,
/// as CI's `done` says of the oracle job's while that job is skipped
/// (#2477). One the store holds is judged as any other.
pub fn judge(
    snapshot: &Snapshot,
    manifest: &Manifest,
    registry: &Registry,
    store: &Store,
    base: Option<&str>,
    not_run: &[String],
) -> Result<Verdict> {
    let root = snapshot.root();
    let base = Base::of(root, base)?;
    let changed = git::changed_since(root, &base.commit)?;
    let paths: Vec<&str> = changed.iter().map(|c| c.path.as_str()).collect();
    let crates: Vec<&Crate> = manifest.crates_of(&paths);
    let ticket = track::head_ticket(root)?;
    let mut unmet = Vec::new();
    let mut notes = Vec::new();
    let mut not_judged = Vec::new();

    let lock_home = lock_home(manifest);
    let carried = Carried::binary(root)?;
    let asking = Asking {
        snapshot,
        manifest,
        store,
        base: &base.commit,
        lock_home: lock_home.as_deref(),
        carried: &carried,
        not_run,
    };
    for condition in Condition::ALL {
        match condition {
            Condition::Fresh => {
                for krate in &crates {
                    let now = KeyNow::of(snapshot, krate, &crates)?;
                    fresh_readings(
                        &asking,
                        krate,
                        &now,
                        &mut not_judged,
                        &mut unmet,
                        &mut notes,
                    )?;
                }
                if !not_run.is_empty() {
                    notes.push(format!(
                        "{} did not run in this run, so condition 1 judged {}",
                        not_run.join(" and "),
                        if not_judged.is_empty() {
                            "no reading of theirs: no touched crate is asked for one".to_owned()
                        } else {
                            format!(
                                "none of the readings it asks for: {}",
                                not_judged.join(", ")
                            )
                        }
                    ));
                }
            }
            Condition::Witnessed | Condition::APersonsFiles => {}
            Condition::LockInDiff => locks(snapshot, manifest, store, &changed, &mut unmet)?,
            Condition::NoOpenRow => {
                open_rows(root, &base.commit, registry, &crates, &changed, &mut unmet)?;
            }
            Condition::PermitOverTheDiff => over_the_diff(
                root,
                manifest,
                &base.commit,
                &changed,
                &mut unmet,
                &mut notes,
            )?,
            Condition::KeptChange => unmet.extend(
                permit::kept_changes(store, root)?
                    .iter()
                    .map(ToString::to_string),
            ),
            Condition::TicketBoxes => {
                if let Some(ticket) = ticket {
                    boxes(snapshot, store, manifest.pin_digest(), ticket, &mut unmet)?;
                }
            }
            Condition::Stranded => unmet.extend(ticket::stranded(root, store, &base.commit)?),
        }
    }
    slack_porting_notes(snapshot, manifest, store, &mut notes)?;

    Ok(Verdict {
        base,
        changed,
        crates: crates.iter().map(|c| c.name.clone()).collect(),
        ticket,
        unmet,
        notes,
        not_run: not_run.to_vec(),
    })
}

/// Condition 7: the branch's ticket has no unticked box, or was withdrawn
/// by the instrument, read from the `ticket` reading over its number and
/// never from the tracker. The
/// reading is fresh while the workspace's tree, [`ticket::STAMPED_OVER`],
/// still matches (the rule at the top of this module); a
/// missing or stale one says what to run, and a failing one carries the
/// instrument's message, which names each unticked box.
fn boxes(
    snapshot: &Snapshot,
    store: &Store,
    pin: &str,
    ticket: Ticket,
    unmet: &mut Vec<String>,
) -> Result<()> {
    const READ: &str = "run `cargo xtask ticket read`";
    let Some(reading) = store.read(ticket::INSTRUMENT, &ticket::scope(ticket))? else {
        unmet.push(format!("ticket: no reading for {ticket}; {READ}"));
        return Ok(());
    };
    let tree = snapshot.key(Path::new(ticket::STAMPED_OVER))?;
    if !reading.fresh_for(&tree, pin) {
        unmet.push(format!(
            "ticket: the reading for {ticket} is stale; the tree changed since it was read; {READ}"
        ));
        return Ok(());
    }
    // The boxes are the condition, read from the numbers: a close the
    // tracker refused with every box ticked fails as a reading and is
    // met here, since the ticket has no unticked box; a reading that
    // counts nothing is held to its status. A withdrawal is read before
    // the boxes (#221): its boxes are unticked by construction and the
    // instrument settled the ticket, so the reading is held to its status
    // alone, met when the close went through and held when it was
    // refused.
    let count = |key: &str| reading.int(key);
    let held = match (count("withdrawn"), count("criteria"), count("unticked")) {
        (Some(withdrawn), ..) if withdrawn > 0 => reading.status != Status::Pass,
        (_, Some(criteria), Some(unticked)) => criteria == 0 || unticked > 0,
        _ => reading.status != Status::Pass,
    };
    if held {
        unmet.push(format!("ticket: {}", reading.message));
    }
    Ok(())
}

/// Whether a `phpt` reading is the skip the missing engine binary causes,
/// the one a subset with no run kept here is excused by (#132, #725).
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
fn no_binary_yet(reading: &Reading) -> bool {
    reading.status == Status::Skip
        && reading.int("engine_absent") == Some(1)
        && reading.int("failed") == Some(0)
        && reading.int("diverged") == Some(0)
        && reading.int("borked") == Some(0)
}

/// Whether a reading that is not a pass is met all the same because the
/// crate's port is not finished and the manifest says so: `porting = true`
/// on its `[[crate]]` row (#383).
///
/// `crosscheck` measures one extension against the oracle's whole
/// rendering, and an extension whose port has forty natives of six hundred
/// fails it from its first commit to its last. Held to its status, it
/// stops every commit that touches such a crate however green its own
/// work is, which is what #325, #340 and #337 measured. A person decided
/// (#383) that while the row carries the key the reading is the port's
/// meter and not a condition of done: the numbers are printed and the row
/// is met. The key's `phpt` half went when `done` came to hold a subset to
/// no regression against the base rather than to its absolute reading
/// (#725), since a partial port no longer fails for the tests it has not
/// reached.
///
/// The rung is held to the key and to the instrument, and to nothing
/// else. The reading must still be there and fresh, so a session that
/// touches the crate still runs the lane and the rendering stands in
/// `xtask status`. Every other instrument holds the crate as it holds any
/// other. A crate the manifest does not mark is held to the reading the
/// day it is created, and the key's deletion, a person's act, is the
/// port's completion certificate. harness-design.md §4.5 condition 1
/// carries the sentence.
fn porting_meter(krate: &Crate, instrument: &str) -> bool {
    krate.porting && instrument == "crosscheck"
}

/// Why a `porting = true` key excuses nothing, `None` while it may excuse
/// something (#2726, as #2717 ruled). Since #725 the key excuses
/// `crosscheck` alone ([`porting_meter`]), so on a crate that is no
/// extension, which no `crosscheck` is asked of, it binds nothing, and on
/// one whose two columns, `columns`, are both fresh over `key` and pass,
/// it excuses nothing; a column failing or never taken is what it is
/// for. What `done` notes so slack keys cannot pile up again, never
/// unmet: the drop is a person's edit of `seams.toml`.
fn slack_porting(
    krate: &Crate,
    columns: &[Option<Reading>],
    key: &str,
    pin: &str,
) -> Option<Slack> {
    if !krate.porting {
        return None;
    }
    if !krate.name.starts_with("ext/") {
        return Some(Slack::NoExtension);
    }
    let passes = |column: &Option<Reading>| {
        column
            .as_ref()
            .is_some_and(|r| r.status == Status::Pass && r.fresh_for(key, pin))
    };
    columns.iter().all(passes).then_some(Slack::Passing)
}

/// Why a `porting = true` key excuses nothing ([`slack_porting`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Slack {
    /// The row is no extension, so no `crosscheck` is asked of it.
    NoExtension,
    /// The extension's two `crosscheck` columns are fresh and pass.
    Passing,
}

impl Slack {
    /// The cause as the note says it.
    fn why(self) -> &'static str {
        match self {
            Slack::NoExtension => "no extension, so no `crosscheck` is asked of it",
            Slack::Passing => "an extension whose two `crosscheck` columns are fresh and pass",
        }
    }
}

/// The note on every row whose `porting = true` excuses nothing
/// ([`slack_porting`]), the rows of one cause named in one line.
fn slack_porting_notes(
    snapshot: &Snapshot,
    manifest: &Manifest,
    store: &Store,
    notes: &mut Vec<String>,
) -> Result<()> {
    let mut slack: BTreeMap<Slack, Vec<&str>> = BTreeMap::new();
    for krate in manifest.crates.iter().filter(|k| k.porting) {
        let mut columns = Vec::new();
        let mut key = String::new();
        if krate.name.starts_with("ext/") {
            for scope in scopes_of(snapshot.root(), "crosscheck", krate)? {
                columns.push(store.read("crosscheck", &scope)?);
            }
            key = snapshot.key(&krate.path())?;
        }
        if let Some(why) = slack_porting(krate, &columns, &key, manifest.pin_digest()) {
            slack.entry(why).or_default().push(&krate.name);
        }
    }
    for (why, rows) in slack {
        notes.push(format!(
            "porting: `porting = true` excuses nothing on {} ({}); dropping the key from seams.toml is a person's edit (harness.self)",
            rows.join(", "),
            why.why()
        ));
    }
    Ok(())
}

/// Whether a `miri` fail is met all the same because every test that
/// failed ended in an unsupported operation (#2416).
///
/// A person ruled what `done` holds of the Miri lane: `undefined` at
/// zero, and `unsupported` reported, not held, since under permissive
/// provenance (#2415) an unsupported operation is a call Miri cannot
/// run, a foreign function it has no shim for, and says nothing of the
/// heap's soundness. So a fail whose `failed` is all `unsupported` is met,
/// its message printed with the reading. Everything else a fail can carry
/// is held: undefined behaviour, a test libtest failed, a run Miri ended
/// some other way, and a fail with nothing failed, which is what the
/// instrument writes where the tests did not build or Miri spoke after
/// the last of them (`miri::Suite::after`). A skip, the nightly not
/// installed, is held too: it measured nothing.
fn miri_unsupported(instrument: &str, reading: &Reading) -> bool {
    if instrument != "miri" || reading.status != Status::Fail {
        return false;
    }
    let count = |key: &str| reading.int(key);
    let failed = count("failed");
    failed.is_some_and(|n| n > 0) && failed == count("unsupported") && count("undefined") == Some(0)
}

/// Whether `reading` is a skip naming a rule the manifest does not carry:
/// the rule absent, which a crate owes nothing under, and not a condition
/// unmet (#3182). With the rule written, the same skip is unmet.
fn rule_absent(manifest: &Manifest, reading: &Reading) -> bool {
    reading.status == Status::Skip
        && !reading.rule.is_empty()
        && manifest.rule(&reading.rule).is_none()
}

/// Condition 1's rungs for a reading that is not a pass: the causes a
/// person excused, and no other.
fn met_without_passing(krate: &Crate, instrument: &str, reading: &Reading) -> bool {
    porting_meter(krate, instrument) || miri_unsupported(instrument, reading)
}

/// What condition 1 reads every touched crate against: the tree, the
/// manifest and the store, the base's commit, and what narrows the ask.
struct Asking<'a> {
    snapshot: &'a Snapshot,
    manifest: &'a Manifest,
    store: &'a Store,
    base: &'a str,
    lock_home: Option<&'a str>,
    carried: &'a Carried,
    /// The instruments the caller's run did not take (#2477).
    not_run: &'a [String],
}

/// Condition 1's `phpt` for one touched crate (harness §4.5, #725): its
/// subset held to no regression against the base, from the two kept runs
/// [`against::regressed`] diffs, and never from a `phpt` reading but the
/// one excused skip: where no run is kept here, a fresh skip the missing
/// engine binary causes is met ([`no_binary_yet`], #132), and a caller
/// that names `phpt` not run has it noted. An oracle variant the pin does
/// not carry is unmet, naming it, as `Oracles::of` refuses one.
fn no_regression(
    asking: &Asking,
    krate: &Crate,
    key: &str,
    not_judged: &mut Vec<String>,
    unmet: &mut Vec<String>,
) -> Result<()> {
    let (scope, pin) = (&krate.name, asking.manifest.pin()?);
    let pin = match krate.oracle.as_deref().map(|v| pin.variant(v)) {
        Some(Err(err)) => {
            unmet.push(format!("phpt: {scope}: {err:#}"));
            return Ok(());
        }
        Some(Ok(variant)) => variant,
        None => pin.clone(),
    };
    match against::regressed(asking.snapshot, asking.base, &pin, scope)? {
        Against::Held => {}
        // An extension with no tests of its own keeps no run (#3143) and
        // holds nothing against any base (#725).
        Against::Unrun(_) if phpt::testless(asking.manifest, scope) => {}
        Against::Unrun(why) => {
            let sought = [("phpt".to_owned(), scope.clone(), key.to_owned())];
            let digest = asking.manifest.pin_digest();
            let read = asking.store.latest_over_each(&sought, digest)?;
            let excused = read.values().any(no_binary_yet);
            if asking.not_run.iter().any(|n| n == "phpt") {
                not_judged.push(format!("phpt for {scope}"));
            } else if !excused {
                unmet.push(format!("phpt: {why}"));
            }
        }
        Against::Unmet(why) => unmet.push(format!("phpt: {why}")),
    }
    Ok(())
}

/// A touched crate's key now ([`Snapshot::key`], its tree and its dependency
/// closure's), and what a reading taken over another key names as changed
/// since: the crate and each other touched crate in that closure, the
/// crates whose change moves the key, `a or b` where there are several
/// (#1318).
struct KeyNow {
    key: String,
    changed: String,
}

impl KeyNow {
    fn of(snapshot: &Snapshot, krate: &Crate, touched: &[&Crate]) -> Result<KeyNow> {
        let closure = key::Graph::of(snapshot.root())?.paths(&krate.path());
        let mut names = vec![krate.name.as_str()];
        names.extend(
            touched
                .iter()
                .filter(|t| t.name != krate.name && closure.contains(&t.path()))
                .map(|t| t.name.as_str()),
        );
        let changed = match names.split_last() {
            Some((last, rest)) if !rest.is_empty() => format!("{} or {last}", rest.join(", ")),
            _ => krate.name.clone(),
        };
        Ok(KeyNow {
            key: snapshot.key(&krate.path())?,
            changed,
        })
    }
}

/// Conditions 1 and 2 for one touched crate, held to its key now, so a
/// reading taken before a crate it is built from changed is stale. A
/// missing reading of an instrument in `not_run` goes to `not_judged`,
/// and a fresh passing `test` reading whose tests skipped for a fact this
/// machine did not promise is a note naming how many (#3085).
fn fresh_readings(
    asking: &Asking,
    krate: &Crate,
    now: &KeyNow,
    not_judged: &mut Vec<String>,
    unmet: &mut Vec<String>,
    notes: &mut Vec<String>,
) -> Result<()> {
    let (root, store, not_run) = (asking.snapshot.root(), asking.store, asking.not_run);
    let pin = asking.manifest.pin_digest();
    let key = &now.key;
    let name = &krate.name;
    // The scope's file holds the last reading written, which a refused
    // commit's can be, taken over another key; where it is, the latest
    // one over the key now stands before it, found in the index (#1312).
    // A scope with no file has no reading, whatever the log holds.
    let mut asked = Vec::new();
    let mut phpt = false;
    for instrument in instruments_of(krate, asking.lock_home, asking.carried) {
        // `phpt` is held by its kept runs, not by a reading, after them.
        if instrument == "phpt" {
            phpt = true;
            continue;
        }
        for scope in scopes_of(root, instrument, krate)? {
            asked.push((instrument, scope));
        }
    }
    let mut read = BTreeMap::new();
    for (instrument, scope) in &asked {
        if let Some(reading) = store.read(instrument, scope)? {
            read.insert(((*instrument).to_owned(), scope.clone()), reading);
        }
    }
    let stale: Vec<(String, String, String)> = read
        .iter()
        .filter(|(_, reading)| !reading.fresh_for(key, pin))
        .map(|((instrument, scope), _)| (instrument.clone(), scope.clone(), key.clone()))
        .collect();
    read.extend(store.latest_over_each(&stale, pin)?);
    for (instrument, scope) in asked {
        let Some(reading) = read.remove(&(instrument.to_owned(), scope.clone())) else {
            if not_run.iter().any(|n| n == instrument) {
                not_judged.push(format!("{instrument} for {scope}"));
            } else {
                unmet.push(format!("{instrument}: no reading for {scope}"));
            }
            continue;
        };
        if !reading.fresh_for(key, pin) {
            // Over the key now, a reading is stale for its recipe alone.
            let changed = if reading.key() == key {
                "oracle.toml".to_owned()
            } else {
                now.changed.clone()
            };
            unmet.push(format!(
                "{instrument}: the reading for {scope} is stale; {changed} changed since it was taken"
            ));
            continue;
        }
        if reading.status != Status::Pass
            && !met_without_passing(krate, instrument, &reading)
            && !rule_absent(asking.manifest, &reading)
        {
            unmet.push(format!(
                "{instrument}: {scope} {}: {}",
                reading.status.as_str(),
                reading.message
            ));
        }
        if instrument == "test"
            && reading.status == Status::Pass
            && let Some(Number::Int(skipped)) = reading.numbers.get("skipped_unpromised")
            && *skipped > 0
        {
            let s = plural(usize::try_from(*skipped).unwrap_or(usize::MAX));
            notes.push(format!(
                "test: {scope} passed with {skipped} test{s} skipped for a fact this machine did not promise"
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
    if phpt {
        no_regression(asking, krate, key, not_judged, unmet)?;
    }
    Ok(())
}

/// The scopes condition 1 reads for one instrument of a crate: the
/// crate's name, and for `crosscheck` the `--ri` column's `ext/<name>/ri`
/// beside it (`crosscheck::Column::scope`), since the lane renders a
/// default extension twice and both renderings are one sensor held to the
/// oracle's bytes, so a default extension whose MINFO table is not php's
/// is not done any more than one whose declarations are not (harness
/// §4.5 condition 1, ledger 46; #361). For `clippy` they are the scope of
/// each profile the crate is linted under ([`clippy::Profile::of`]), its
/// name and, where its sources name `debug_assertions`, `<name>/release`
/// (#2392).
pub fn scopes_of(root: &Path, instrument: &str, krate: &Crate) -> Result<Vec<String>> {
    let name = &krate.name;
    if instrument == clippy::INSTRUMENT {
        let profiles = clippy::Profile::of(root, krate)?;
        return Ok(profiles.into_iter().map(|p| p.scope(name)).collect());
    }
    let mut scopes = vec![name.clone()];
    if instrument == "crosscheck"
        && let Some(extension) = name.strip_prefix("ext/")
    {
        scopes.push(crosscheck::Column::Ri.scope(extension));
    }
    Ok(scopes)
}

/// Condition 3: a lock that differs is in the diff, and a lock in the diff
/// has a fresh `api-check` reading that passes. A seam reading that is
/// stale says nothing here: the crate the lock lives with changed, and
/// condition 1 lists its `api-check` reading as stale.
fn locks(
    snapshot: &Snapshot,
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
        let tree = snapshot.key(&dir)?;
        let fresh = reading.fresh_for(&tree, manifest.pin_digest());
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

/// Condition 4: no `open` row touches a changed scope, through its own id
/// or any id it covers. A row the diff itself proposes, one the base's
/// registry does not have, touches it by definition.
fn open_rows(
    root: &Path,
    base: &str,
    registry: &Registry,
    crates: &[&Crate],
    changed: &[Change],
    unmet: &mut Vec<String>,
) -> Result<()> {
    let proposed_here: Vec<divergence::Id> = if changed.iter().any(|c| c.path == divergence::FILE) {
        let before = git::show(root, base, divergence::FILE)?
            .map(|text| divergence::named_ids(&text))
            .transpose()
            .with_context(|| format!("{}:{}", base, divergence::FILE))?
            .unwrap_or_default();
        registry
            .rows()
            .iter()
            .filter(|r| !before.contains(r.id.as_str()))
            .map(|r| r.id.clone())
            .collect()
    } else {
        Vec::new()
    };
    for row in registry
        .rows()
        .iter()
        .filter(|r| r.status == divergence::Status::Open)
    {
        let any_id_names = |part: &str| row.ids().any(|id| names(id.as_str(), part));
        let touched = if proposed_here.contains(&row.id) {
            Some(format!("{}, proposed in this diff", divergence::FILE))
        } else {
            crates
                .iter()
                .find(|c| any_id_names(&c.name))
                .map(|c| c.name.clone())
                .or_else(|| {
                    changed
                        .iter()
                        .find(|c| any_id_names(&c.path))
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
    base: &str,
    changed: &[Change],
    unmet: &mut Vec<String>,
    notes: &mut Vec<String>,
) -> Result<()> {
    let files = permit::working_files(root, base, changed)?;
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

/// `head` and a colon on a line, then each of `lines` as a bullet.
fn bullets(out: &mut String, head: &str, lines: &[String]) {
    out.push_str(head);
    out.push_str(":\n");
    for line in lines {
        out.push_str("- ");
        out.push_str(line);
        out.push('\n');
    }
}

/// The command: judge, write the reading, print the list, fail while
/// anything is unmet.
pub fn run(
    root: &Path,
    manifest: &Manifest,
    registry: &Registry,
    store: &Store,
    base: Option<&str>,
    not_run: &[String],
) -> Result<()> {
    let snapshot = Snapshot::disk(root)?;
    let verdict = judge(&snapshot, manifest, registry, store, base, not_run)?;
    let outcome = store.record(&[verdict.reading(manifest, &snapshot)?])?;
    print!("{}", verdict.render());
    let unmet = verdict.unmet.len();
    outcome.exit_saying(|| format!("{unmet} condition{} of done unmet", plural(unmet)))
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
    let verdict = match Snapshot::disk(root).and_then(|snapshot| {
        Ok((
            judge(&snapshot, manifest, registry, store, None, &[])?,
            snapshot,
        ))
    }) {
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
    let (verdict, snapshot) = verdict;
    let recorded = verdict.reading(manifest, &snapshot).and_then(|mut r| {
        // The Stop's verdict carries the session: what marks a later
        // turn's close as this turn's and not an earlier one's.
        stop.session_id.clone_into(&mut r.session);
        let mut readings = vec![r];
        if let Some(unfiled) = &unfiled {
            readings.push(unfiled.reading(&snapshot, &stop.session_id)?);
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
    use super::{
        FRESH, Slack, instruments_of, met_without_passing, miri_unsupported, no_binary_yet,
        porting_meter, rule_absent, slack_porting,
    };
    use crate::carried::Carried;
    use crate::manifest::{Crate, Manifest, Rule};
    use crate::reading::{Reading, Status};

    /// The crate `api.lock` lives with in the shipped manifest, which is
    /// what narrows `port-map`; every crate here but that one is asked
    /// for `ASKED_OF_EVERY_OTHER_CRATE` at most.
    const HOME: Option<&str> = Some("php_ext");

    /// What the crate asks as the binary under test that turns on the
    /// `openssl` feature and no other carries it.
    fn asked_of(krate: &Crate, lock_home: Option<&str>) -> Vec<&'static str> {
        instruments_of(krate, lock_home, &Carried::turning_on(&["openssl"])).collect()
    }

    /// [`FRESH`] less `port-map`, `crosscheck` and `miri`: what an engine
    /// crate that is not the lock's home nor the Miri lane's is asked,
    /// before the two narrowings of its own. That it is [`FRESH`] less the
    /// three rows is asserted below and not trusted here.
    const ASKED_OF_EVERY_OTHER_CRATE: [&str; 7] = [
        "clippy",
        "test",
        "witness",
        "api-check",
        "budget",
        "unsafe-audit",
        "phpt",
    ];

    /// What a default extension is asked: the seven and `crosscheck`.
    const ASKED_OF_A_DEFAULT_EXTENSION: [&str; 8] = [
        "clippy",
        "test",
        "witness",
        "api-check",
        "budget",
        "unsafe-audit",
        "phpt",
        "crosscheck",
    ];

    #[test]
    fn the_harness_crate_is_asked_for_clippy_test_api_check_budget_and_unsafe_audit_only() {
        let asked = asked_of(&Crate::named("xtask"), HOME);
        assert_eq!(
            asked,
            ["clippy", "test", "api-check", "budget", "unsafe-audit"]
        );
        let asked = asked_of(&Crate::named("ext/pcre"), HOME);
        assert_eq!(asked, ASKED_OF_A_DEFAULT_EXTENSION);
    }

    /// `crosscheck` renders every extension the binary carries per commit
    /// (harness §7, revision 63), so an `ext/*` crate outside every
    /// feature or behind one the binary turns on is asked for its reading,
    /// one behind a feature the binary does not turn on is not, and no
    /// engine crate is (#269).
    #[test]
    fn a_default_extension_is_asked_for_a_crosscheck_reading_and_nobody_else_is() {
        let asked = asked_of(&Crate::named("ext/ctype"), HOME);
        assert!(asked.contains(&"crosscheck"), "{asked:?}");
        let row = Crate::named("ext/mbstring").with_feature("mbstring");
        let asked = asked_of(&row, HOME);
        assert!(!asked.contains(&"crosscheck"), "{asked:?}");
        let row = Crate::named("ext/openssl").with_feature("openssl");
        assert!(asked_of(&row, HOME).contains(&"crosscheck"));
        for other in ["php_engine", "php_ext", "xtask"] {
            let asked = asked_of(&Crate::named(other), HOME);
            assert!(!asked.contains(&"crosscheck"), "{other}: {asked:?}");
        }
    }

    /// An engine crate the manifest gives no subset is not asked for a
    /// phpt reading, the way the harness crate is not; the resolver's
    /// error is never what says so. The same crate with a row that names
    /// one is asked.
    #[test]
    fn a_crate_is_asked_for_a_phpt_reading_only_where_the_manifest_gives_it_one() {
        let asked = asked_of(&Crate::named("php_engine"), HOME);
        assert_eq!(
            asked,
            [
                "clippy",
                "test",
                "witness",
                "api-check",
                "budget",
                "unsafe-audit"
            ]
        );
        let row = Crate::named("php_engine").with_phpt(&["Zend/tests"]);
        let asked = asked_of(&row, HOME);
        assert_eq!(asked, ASKED_OF_EVERY_OTHER_CRATE);
    }

    /// `miri` is asked of the crates the Miri lane runs, `php_heap`, and
    /// of no other crate (#2416).
    #[test]
    fn the_memory_model_is_the_one_crate_asked_for_a_miri_reading() {
        let asked = asked_of(&Crate::named("php_heap"), HOME);
        assert_eq!(
            asked,
            [
                "clippy",
                "test",
                "witness",
                "api-check",
                "budget",
                "unsafe-audit",
                "miri"
            ]
        );
        for other in ["php_engine", "php_ext", "ext/pcre", "xtask"] {
            let asked = asked_of(&Crate::named(other), HOME);
            assert!(!asked.contains(&"miri"), "{other}: {asked:?}");
        }
    }

    /// A `miri` fail is met where every test that failed ended in an
    /// unsupported operation, and held where one is undefined behaviour,
    /// a test libtest failed, or nothing failed at all, which is a build
    /// that did not finish or Miri speaking after the last test (#2416).
    /// A skip is held: it measured nothing.
    #[test]
    fn a_miri_reading_is_held_to_its_undefined_behaviour_and_not_to_what_miri_cannot_run() {
        let reading = |status: Status, failed: u64, undefined: u64, unsupported: u64| {
            let mut reading = Reading::new("miri", "php_heap");
            reading.status = status;
            reading.count("failed", failed).unwrap();
            reading.count("undefined", undefined).unwrap();
            reading.count("unsupported", unsupported).unwrap();
            reading
        };
        let heap = Crate::named("php_heap");
        let met = |r: &Reading| met_without_passing(&heap, "miri", r);
        assert!(met(&reading(Status::Fail, 2, 0, 2)));
        assert!(!met(&reading(Status::Fail, 2, 1, 1)), "undefined behaviour");
        assert!(!met(&reading(Status::Fail, 3, 0, 2)), "a test that failed");
        assert!(
            !met(&reading(Status::Fail, 0, 0, 0)),
            "nothing ran to its end"
        );
        assert!(!met(&reading(Status::Skip, 0, 0, 0)), "nothing measured");
        // The rung reads the miri reading's numbers and no other's.
        assert!(!miri_unsupported("test", &reading(Status::Fail, 2, 0, 2)));
    }

    /// A crate behind a feature the binary under test does not turn on is
    /// outside the per-commit lane, so `done` does not ask it for a
    /// reading no lane run writes (#133); its corpus is the nightly
    /// lane's. The same row without the feature is asked, and so is one
    /// behind a feature the binary turns on, since the lane runs every
    /// extension the binary carries (revision 63, #1252), and so is one
    /// whose row names an oracle variant beside its feature, which the
    /// lane runs on the engine built with it (#2029): `ext/zend_test` in
    /// the shipped manifest, `phpt` and not `crosscheck`.
    #[cfg(tracked)]
    #[test]
    fn an_optional_crate_is_not_asked_for_a_phpt_reading_the_lane_never_writes() {
        let varied = Crate::named("ext/mbstring")
            .with_feature("mbstring")
            .with_oracle("zend-test");
        assert_eq!(asked_of(&varied, HOME), ASKED_OF_EVERY_OTHER_CRATE);
        let root = crate::package_dir().parent().unwrap();
        let manifest = crate::manifest::Manifest::load(root).unwrap();
        let zend_test = manifest.crate_named("ext/zend_test").unwrap();
        let carried = Carried::binary(root).unwrap();
        assert!(!carried.carries(zend_test), "{carried:?}");
        let asked: Vec<_> = instruments_of(zend_test, HOME, &carried).collect();
        assert_eq!(asked, ASKED_OF_EVERY_OTHER_CRATE);

        let row = Crate::named("ext/mbstring").with_feature("mbstring");
        let asked = asked_of(&row, HOME);
        assert_eq!(
            asked,
            [
                "clippy",
                "test",
                "witness",
                "api-check",
                "budget",
                "unsafe-audit"
            ]
        );
        let asked = asked_of(&Crate::named("ext/mbstring"), HOME);
        assert_eq!(asked, ASKED_OF_A_DEFAULT_EXTENSION);
        let row = Crate::named("ext/openssl").with_feature("openssl");
        assert_eq!(asked_of(&row, HOME), ASKED_OF_A_DEFAULT_EXTENSION);
    }

    /// `port-map` writes exactly one reading, at the crate `api.lock`
    /// lives with, so condition 1 asks that crate for it and no other
    /// (harness §4.5 condition 1, #148). A manifest naming no home for
    /// the lock asks nobody: the instrument is where that refusal is
    /// said, not `done`.
    #[test]
    fn the_crate_the_lock_lives_with_is_the_one_asked_for_a_port_map_reading() {
        let rest: Vec<&str> = FRESH
            .into_iter()
            .filter(|i| !matches!(*i, "port-map" | "crosscheck" | "miri"))
            .collect();
        assert_eq!(rest, ASKED_OF_EVERY_OTHER_CRATE);
        let asked = asked_of(&Crate::named("php_ext"), HOME);
        assert_eq!(
            asked,
            [
                "clippy",
                "test",
                "witness",
                "api-check",
                "budget",
                "unsafe-audit",
                "port-map"
            ]
        );
        for other in ["php_engine", "ext/pcre", "xtask"] {
            let asked = asked_of(&Crate::named(other), HOME);
            assert!(!asked.contains(&"port-map"), "{other}: {asked:?}");
        }
        let asked = asked_of(&Crate::named("php_ext"), None);
        assert!(!asked.contains(&"port-map"), "{asked:?}");
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
        assert!(no_binary_yet(&skip(1, 0, 0, 0)));
        // The crate is not yet created, or every test skipped: not this.
        assert!(!no_binary_yet(&skip(0, 0, 0, 0)));
        // Something counted against it, so the skip is not a clean one.
        assert!(!no_binary_yet(&skip(1, 1, 0, 0)));
        assert!(!no_binary_yet(&skip(1, 0, 1, 0)));
        assert!(!no_binary_yet(&skip(1, 0, 0, 1)));
        // A phpt reading with no such number is held to its status.
        let mut bare = Reading::new("phpt", "ext/standard");
        bare.status = Status::Skip;
        assert!(!no_binary_yet(&bare));
    }

    /// A crate the manifest marks `porting = true` has its `crosscheck`
    /// readings read as the port's meter and not as a condition of done
    /// (#383): whatever they say, the row is met. Every other instrument
    /// of the marked crate, `phpt` among them since #725, and the same
    /// reading of a crate the manifest does not mark, are held.
    #[test]
    fn a_porting_crates_crosscheck_readings_are_the_ports_meter() {
        let mut porting = Crate::named("ext/standard");
        porting.porting = true;
        let held = Crate::named("ext/standard");
        let red = |instrument: &str, status: Status| {
            let mut reading = Reading::new(instrument, "ext/standard");
            reading.status = status;
            reading
        };
        for status in [Status::Fail, Status::Skip] {
            let reading = red("crosscheck", status);
            assert!(met_without_passing(&porting, "crosscheck", &reading));
            assert!(!met_without_passing(&held, "crosscheck", &reading));
        }
        for instrument in FRESH.into_iter().filter(|i| *i != "crosscheck") {
            assert!(!porting_meter(&porting, instrument), "{instrument}");
            assert!(
                !met_without_passing(&porting, instrument, &red(instrument, Status::Fail)),
                "{instrument}"
            );
        }
    }

    /// #2726: a `porting = true` key that excuses nothing is said, under
    /// #2717's two rules. A row that is no extension binds nothing; an
    /// extension whose two columns are fresh and pass needs no excuse; one
    /// whose column fails, is stale or was never taken is what the key is
    /// for; and a row without the key is never said.
    #[test]
    fn a_porting_key_that_excuses_nothing_is_said_and_one_excusing_a_failure_is_not() {
        let porting = |name: &str| {
            let mut krate = Crate::named(name);
            krate.porting = true;
            krate
        };
        // A reading's key is the empty one, the key the column is fresh
        // over; any other key now is one it is stale for.
        let column = |status: Status| {
            let mut reading = Reading::new("crosscheck", "ext/zip");
            reading.status = status;
            reading.recipe = Some("pin".to_owned());
            Some(reading)
        };
        let said = |krate: &Crate, columns: &[Option<Reading>], key: &str| {
            slack_porting(krate, columns, key, "pin")
        };
        assert_eq!(
            said(&porting("sapi/cli"), &[], ""),
            Some(Slack::NoExtension)
        );
        let passing = [column(Status::Pass), column(Status::Pass)];
        assert_eq!(
            said(&porting("ext/zip"), &passing, ""),
            Some(Slack::Passing)
        );
        assert_eq!(said(&Crate::named("ext/zip"), &passing, ""), None);
        assert_eq!(said(&porting("ext/zip"), &passing, "moved"), None);
        for columns in [
            [column(Status::Pass), column(Status::Fail)],
            [column(Status::Pass), None],
        ] {
            assert_eq!(said(&porting("ext/zip"), &columns, ""), None, "{columns:?}");
        }
    }

    /// #3182: a skip naming a rule the manifest does not carry is the rule
    /// absent, owed by nobody; with the rule written it is unmet.
    #[test]
    fn a_skip_naming_an_absent_rule_is_the_rule_absent_and_not_unmet() {
        let mut reading = Reading::new("unsafe-audit", "php_heap");
        reading.rule = "unsafe.homes".to_owned();
        let without = Manifest::named("php_heap");
        assert!(rule_absent(&without, &reading));
        let mut with = without.clone();
        with.rules.push(Rule::named("unsafe.homes", "**"));
        assert!(!rule_absent(&with, &reading));
        reading.status = Status::Fail;
        assert!(!rule_absent(&without, &reading), "a fail is held");
    }
}
