//! The sensor over `harness.self`'s own files, beside the judge of the
//! call's words (#625, #1516). `permit` reads a `Bash` call by its argv,
//! and a write a program makes from inside an interpreter or a built
//! binary never puts its path on the command line. So before an agent's
//! `Bash` call `permit` takes the git blob id of every tracked file the
//! rule scopes in the one checkout the call runs in, and keeps them in the
//! store under the session and the hook's tool-use id ([`before`]). After
//! the call `permit --after` takes the ids again ([`after`]): a file whose
//! blob moved, and which the call's line can write ([`writes::can_write`]:
//! a redirection into it, a writer handed its path, an interpreter whose
//! program names it), is put back from the blob kept before, and the call
//! is a `harness.self` denial with the rule's text and the path, a permit
//! reading like any other, and the `PostToolUse` hook's block, which the
//! model reads.
//!
//! The rule is one: a file is put back only in the checkout the call ran
//! in, only where its line can write it, never where history carries it,
//! and never where an operation in progress there has staged it; every
//! other file that moved is kept as it stands and named. The checkout is
//! where the call acts, its start ([`Call::start`]) moved by the line's
//! leading `cd`, `pushd` and `popd`, or by git's `-C` where every command
//! past them is git and names one directory, at the top of its worktree
//! ([`Walk::ran_in`], #2607), and not the hook's own: a subagent's shell
//! starts in the main checkout and `cd`s into its worktree, so the hook's
//! checkout is often one the call never touched, and another session's
//! merge may stand there (#2597). The line's words are read where its
//! shell stands, the call's `cwd` moved by those changes. The checkout
//! and the start are taken as the call begins and kept in its record, and
//! `--after` reads both there, the words read from that start, since the
//! hook's `cwd` after the call is where the line left the shell (Round 1,
//! Q18 (c)); a record whose checkout is gone by then is dropped.
//!
//! What it leaves alone, on purpose:
//!
//! - A file that moved while the call ran and that its line cannot write
//!   (#1895). Several sessions share one checkout, so a person's edit made
//!   in an editor lands inside some call's window most of the time, and a
//!   change the line does not account for is as likely theirs as the
//!   call's. The sensor cannot tell whose it is, so it keeps it as it
//!   stands and says so: a passing reading and the hook's context name the
//!   file and the blob it was before, and the change is recorded
//!   ([`super::kept`]). Until a person commits it or restores it to
//!   `HEAD`, it is one of `done`'s unmet conditions, which is where a
//!   binary that hides the path is held; a person's lost edit would be
//!   held by nothing. That holds in main's checkout and in one no session
//!   owns, where persons edit. In a linked worktree the call's session
//!   owns ([`crate::owner`]), the change is the session's whichever of its
//!   calls made it, a background one among them: where it differs from
//!   `HEAD` it is put back and the call denied, saying so (#3644).
//! - A file history carries (#3220): `HEAD` moved during the call, its
//!   blob for the file moved with it, and the file stands at the blob
//!   `HEAD` holds now, as another session's merge or commit in the
//!   checkout leaves it ([`landed_at`]). Putting it back would leave the
//!   checkout behind `HEAD`, and the next commit that stages it would
//!   undo the merge. It is kept and named, and it settles nothing: no
//!   reading, no record. What a commit carries is `done`'s to judge over
//!   the diff. A write back to `HEAD`'s blob where `HEAD`'s blob for the
//!   file did not move, a restore, is no change history made, and nor is
//!   a commit the call's own line makes ([`moves_head`]): each is judged
//!   as any write is.
//! - A call with no record of its own: no tool-use id, a record a build
//!   older than this one never wrote, a record already taken. Nothing is
//!   put back without the blob it was before, and stderr says so.
//! - A file the merge, cherry-pick, revert or rebase in progress in the
//!   checkout has staged, its index entry other than `HEAD`'s
//!   ([`operation::staged`]): the operation's commit carries it, so even a
//!   line that can write it does not put it back. It is kept and named.
//!   A path
//!   the operation left conflicted, unmerged in the index, is not staged:
//!   its markers are guarded as any file is, so the one hand that resolves
//!   it is `steer`'s ([`crate::steer::Ground`], #2880).
//! - What the call wrote: it goes into the object database before the file
//!   is put back, and the denial names its blob, so a person's edit made
//!   in an editor while a long call ran is a `git cat-file` away, not gone.
//!   A blob that cannot be kept leaves the file as the call left it.
//! - An untracked file: `.claude/` holds Claude Code's own state beside
//!   the tracked files (`scheduled_tasks.lock`, an approval written to
//!   `settings.local.json`), written between the two hooks on a person's
//!   act, and putting those back would undo the person.
//! - What `steer` wrote: a call whose words run `xtask steer` is the
//!   sanctioned writer, and a file it moved stands where steer's own
//!   reading of that call says it wrote exactly that ([`steered`]), as
//!   judged by a build compiled from the sources the checkout carries that
//!   reads the manifest it commits ([`Steering`], #3631): another
//!   checkout's own build where this one is not that ([`handed`]), the
//!   next build the hook runs where this one is stale, its record left for
//!   it, and where no build is, every file is kept and the answer says it
//!   could not judge.
//! - A file the rule's `exempt` covers: `.claude/skills/**` is an ordinary
//!   file (#626), so a `Bash` write there stands as an `Edit` there does,
//!   but for what a `!` pattern takes back, a plugin laid in a skill (#3470).
//!
//! Each session keeps its records in a directory of its own, a file per
//! call, so two sessions in one checkout, or two calls of one session side
//! by side, never take each other's.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use super::INSTRUMENT;
use super::approval::Admission;
use super::argv;
use super::audit;
use super::call::Call;
use super::kept;
use super::operation;
use super::shell::{Walk, at};
use super::writes::{self, Place};
use crate::key::Snapshot;
use crate::manifest::OneOrMany;
use crate::owner::{self, SessionId};
use crate::reading::{self, Number, Reading, Status, Store, joined};
use crate::{git, manifest, steer};

/// The rule the sensor holds, whose scope and text it reads.
const RULE: &str = "harness.self";

/// How long a record no `--after` took is kept: the one a denied call
/// leaves, since a call that never ran has no `PostToolUse`. No call runs
/// this long; the `Bash` timeout's ceiling is two hours.
const KEPT: Duration = Duration::from_hours(24);

/// What one call's `before` took.
#[derive(Debug, Serialize, Deserialize)]
struct Record {
    /// When, in seconds since 1970: steer's reading of the call is one
    /// taken at or after it.
    at: i64,
    /// The call's command, whose words say whether it runs steer.
    command: String,
    /// Where the call's line started as the call began ([`Call::start`]),
    /// which `--after` walks it from, since the hook's `cwd` after the
    /// call is where the line left the shell (Round 1, Q18 (c)); `None` in
    /// a record an older build wrote, whose line is walked from the
    /// call's `cwd`.
    #[serde(default)]
    start: Option<PathBuf>,
    /// The checkout the call ran in ([`held`]), the last; a list since a
    /// record an older build wrote holds the hook's own before it, which
    /// `--after` does not compare (#2597).
    checkouts: Vec<Guarded>,
}

impl Record {
    /// Where `--after` walks the call's line from: the start the record
    /// keeps, or the call's `cwd` in a record that keeps none.
    fn walked_from<'a>(&'a self, call: &'a Call) -> Option<&'a Path> {
        self.start.as_deref().or(call.cwd.as_deref().map(Path::new))
    }
}

/// One checkout's files under the rule, as the call found them.
#[derive(Debug, Serialize, Deserialize)]
struct Guarded {
    checkout: PathBuf,
    /// The commit `HEAD` stood at as the call began, which [`landed_at`]
    /// reads history from; `None` for an unborn `HEAD`, or in a record an
    /// older build wrote.
    #[serde(default)]
    head: Option<String>,
    /// The rule's text in this checkout's manifest.
    text: String,
    /// Each tracked file in scope by its path, and its blob id on disk;
    /// `None` for a tracked file that was not on disk.
    blobs: BTreeMap<String, Option<String>>,
}

/// One file that moved across the call, and what the sensor did with it.
struct Moved {
    checkout: PathBuf,
    path: String,
    /// Its blob before the call; `None` where it was not on disk.
    was: Option<String>,
    said: String,
    verdict: Verdict,
    /// The person's approval the write stands on, for a
    /// [`Verdict::Approved`] one; its audit entry is written with the
    /// reading ([`audit`]).
    admission: Option<Admission>,
}

/// What the sensor did with a moved file, as the call's line reads.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Verdict {
    /// The line can write it: put back, and the call denied.
    PutBack,
    /// The line cannot: kept as it stands, and named, since the sensor
    /// cannot tell a person's edit from a write the line does not show.
    Kept,
    /// The line wrote it as the person approved in the session: kept, and
    /// named (#2247).
    Approved,
    /// History carries it, a commit `HEAD` moved to during the call
    /// ([`landed_at`]): kept, named, and recorded nowhere (#3220).
    Landed,
}

impl Verdict {
    /// The reading's status: a denial fails, a kept change is a note, and
    /// a change history carries writes none.
    fn status(self) -> Option<Status> {
        match self {
            Verdict::PutBack => Some(Status::Fail),
            Verdict::Kept | Verdict::Approved => Some(Status::Pass),
            Verdict::Landed => None,
        }
    }
}

/// Before a `Bash` call: the blob id of every tracked file `harness.self`
/// scopes in the checkout the call runs in ([`held`]), kept under the
/// session and the tool-use id. A call that carries neither is not recorded, and one
/// already recorded keeps its first record, which both adapters take
/// before the call runs.
pub(super) fn before(root: &Path, call: &Call) -> Result<()> {
    if call.judged_as() != "Bash" {
        return Ok(());
    }
    let Some(file) = record_path(root, call) else {
        return Ok(());
    };
    if file.exists() {
        return Ok(());
    }
    prune(&records(root));
    let (start, checkout) = held(root, call);
    let Some(guarded) = guard(&checkout)? else {
        return Ok(());
    };
    let record = Record {
        at: reading::epoch_seconds(&reading::now()?)?,
        command: call.command().unwrap_or_default().to_owned(),
        start: Some(start),
        checkouts: vec![guarded],
    };
    let text = serde_json::to_string(&record).context("rendering the record")?;
    reading::replace(&file, text.as_bytes())
}

/// `permit --after`, the `PostToolUse` and `PostToolUseFailure` hooks' call: the call on stdin (or
/// in `json`), its record taken again, each file whose blob moved put
/// back and denied, the denials printed as the hook's block and recorded.
/// The exit is 0 whatever it found; a call it cannot pair with a record
/// is left as it stands, said on stderr.
pub fn after(root: &Path, json: Option<&str>) -> Result<()> {
    let text = Call::text(json)?;
    let call = match Call::parse(&text) {
        Ok(call) => call,
        Err(err) => {
            eprintln!("xtask permit --after: {err:#}; nothing is put back");
            return Ok(());
        }
    };
    if call.judged_as() != "Bash" {
        return Ok(());
    }
    let Some(file) = record_path(root, &call) else {
        eprintln!(
            "xtask permit --after: the call carries no session or tool-use id to pair it with \
             what was taken before it, so nothing is put back"
        );
        return Ok(());
    };
    let Ok(taken) = fs::read_to_string(&file) else {
        eprintln!(
            "xtask permit --after: nothing was taken before this call ({}), so nothing is put back",
            file.display()
        );
        return Ok(());
    };
    let record: Record = serde_json::from_str(&taken)
        .with_context(|| format!("reading the record at {}", file.display()))?;
    // The checkout the call was held in as it began, and not one walked
    // again from where the line left the shell (Round 1, Q18 (c)).
    let Some(checkout) = record.checkouts.last().map(|g| g.checkout.clone()) else {
        fs::remove_file(&file).ok();
        return Ok(());
    };
    if !checkout.is_dir() {
        fs::remove_file(&file).ok();
        eprintln!(
            "xtask permit --after: {} is gone since the call began, so nothing is put back",
            checkout.display()
        );
        return Ok(());
    }
    let steering = runs_steer(&record.command);
    let unread = match steering.then(|| Steering::of(root, &checkout)) {
        Some(Steering::Newer(why)) => anyhow::bail!(
            "{why}, so this build is no judge of steer's write; the record is left for the \
             next build the hook runs, the last of which rebuilds"
        ),
        Some(Steering::Checkout(why)) => {
            fs::remove_file(&file).ok();
            if let Some(decision) = handed(&checkout, &call, &taken, &text) {
                return std::io::Write::write_all(&mut std::io::stdout(), &decision)
                    .context("printing the decision the checkout's build made");
            }
            Some(why)
        }
        Some(Steering::This(unread)) => unread,
        None => None,
    };
    // The record is this call's alone and is spent whatever follows.
    fs::remove_file(&file).ok();
    let moved = sort_moved(&record, &call, &checkout, unread.as_deref());
    if moved.is_empty() {
        return Ok(());
    }
    let said = joined(moved.iter().map(|m| format!("{RULE}: {}", m.said)), "\n");
    // The answer names the event it answers. A failed call's event takes
    // no `decision`: the call has already failed, and what was put back is
    // said as context (#2762).
    let event = call.hook_event_name.as_deref().unwrap_or("PostToolUse");
    let context = serde_json::json!({"hookEventName": event, "additionalContext": said});
    let hook = if event == "PostToolUse" && moved.iter().any(|m| m.verdict == Verdict::PutBack) {
        serde_json::json!({"decision": "block", "reason": said, "hookSpecificOutput": context})
    } else {
        serde_json::json!({"hookSpecificOutput": context})
    };
    println!("{hook}");
    if let Err(err) = record_moved(root, &call, &moved) {
        eprintln!("xtask permit --after: what was found stands but was not recorded: {err:#}");
    }
    Ok(())
}

/// Every file of the record in the checkout `held` whose blob moved and
/// that steer did not write: kept where history carries it ([`landed_at`]),
/// put back where the call's line can write it and no operation in
/// progress staged it, kept otherwise. A word of the line is read where
/// its shell stands ([`shell_at`]), walked from the start the record
/// keeps, or from the call's `cwd` where it keeps none
/// ([`Record::walked_from`]). Where the line runs steer and this build
/// cannot read the manifest steer judges by, `unread` says why, and every
/// file is kept: whether steer wrote it is not this build's to say.
fn sort_moved(record: &Record, call: &Call, held: &Path, unread: Option<&str>) -> Vec<Moved> {
    let steering = runs_steer(&record.command);
    let line = argv::Line::read(&record.command);
    let mut moved = Vec::new();
    for guarded in record.checkouts.iter().filter(|g| g.checkout == held) {
        let cwd = shell_at(record.walked_from(call), &guarded.checkout, &line);
        // A line that moves `HEAD` itself makes the history it would be
        // credited with, so it is credited with none.
        let credited = !moves_head(&line);
        let owned = sessions_own(&guarded.checkout, &call.session_id);
        for (path, was) in &guarded.blobs {
            let file = guarded.checkout.join(path);
            if on_disk(&file) == *was {
                continue;
            }
            if steering && steered(&guarded.checkout, path, was.as_deref(), record.at) {
                continue;
            }
            let (now, tip) = settled(|| operation::head(&guarded.checkout), || on_disk(&file));
            if now == *was {
                continue;
            }
            let tip = tip.filter(|_| credited);
            let landed = landed_at(guarded, tip.as_deref(), path, now.as_deref());
            let place = OnDisk::new(&cwd, &guarded.checkout, path);
            let writes = landed.is_none() && writes::can_write(&line, &place);
            // In a linked worktree the session owns, a change its line
            // cannot be read to write is the session's all the same, where
            // it now differs from `HEAD` (#3644 rulings 1 and 4).
            let sessions = landed.is_none()
                && !writes
                && owned
                && !matches!(kept::at_head(&guarded.checkout, path), Ok(true));
            let answers = writes || sessions;
            let approved = answers
                .then(|| approved(&guarded.checkout, path, was.as_deref(), call))
                .flatten();
            let staged = answers
                .then(|| operation::staged(&guarded.checkout, path))
                .flatten();
            let (verdict, said) = if let Some(why) = unread {
                let why = format!(
                    "the line runs steer, and this build could not judge whether steer wrote \
                     it, since it cannot read the manifest steer judges by ({why}); nothing is \
                     put back and it is kept as it stands"
                );
                let said = kept_words(&guarded.checkout, path, &why, was.as_deref());
                (Verdict::Kept, format!("{path}: {said}"))
            } else if let Some(moved_to) = landed {
                let why = format!(
                    "HEAD moved to {moved_to} during the call and holds it at this blob: history \
                     carries the change, so it is kept as it stands and settles nothing"
                );
                let said = kept_words(&guarded.checkout, path, &why, was.as_deref());
                (Verdict::Landed, format!("{path}: {said}"))
            } else if let Some(admission) = &approved {
                (Verdict::Approved, format!("{path}: {}", admission.said()))
            } else if let Some(operation) = staged {
                let why = format!(
                    "{operation} in progress in this checkout has staged it, so it is kept as \
                     it stands for the operation's commit"
                );
                let said = kept_words(&guarded.checkout, path, &why, was.as_deref());
                (Verdict::Kept, format!("{path}: {said}"))
            } else if writes || sessions {
                let by = if writes {
                    "the call"
                } else {
                    "a call of this session"
                };
                let what = restore(&guarded.checkout, path, was.as_deref(), now.as_deref(), by);
                let what = if writes {
                    what
                } else {
                    format!(
                        "changed while this call, {} `{}`, ran, whose line cannot be read to \
                         write it, and it differs from HEAD; {} is a linked worktree this \
                         session owns, so the change is the session's whichever of its calls \
                         made it, and the write may have come from a background call (#3644); \
                         {what}",
                        call.tool_use_id,
                        call.subject(held),
                        guarded.checkout.display()
                    )
                };
                (Verdict::PutBack, format!("{} {path}: {what}", guarded.text))
            } else {
                let why = "its line cannot write it, so it is kept as it stands: a person's \
                           edit, or a write the line does not show";
                let said = kept_words(&guarded.checkout, path, why, was.as_deref());
                (Verdict::Kept, format!("{path}: {said}"))
            };
            moved.push(Moved {
                checkout: guarded.checkout.clone(),
                path: path.clone(),
                was: was.clone(),
                said,
                verdict,
                admission: approved,
            });
        }
    }
    moved
}

/// The checkout the call ran in, and the tracked files of `harness.self`'s
/// scope there that the call's line can write, read where it stands: what
/// an approval must cover before the line runs (#2247).
pub(super) fn written(
    root: &Path,
    call: &Call,
    walk: &Walk,
    line: &argv::Line,
) -> (PathBuf, Vec<String>) {
    let checkout = walk.ran_in(root);
    let Ok(Some(guarded)) = guard(&checkout) else {
        return (checkout, Vec::new());
    };
    let cwd = shell_at(call.cwd.as_deref().map(Path::new), root, line);
    let paths = guarded
        .blobs
        .into_keys()
        .filter(|path| writes::can_write(line, &OnDisk::new(&cwd, &checkout, path)))
        .collect();
    (checkout, paths)
}

/// The commit `HEAD` moved to during the call, `tip` now, where history
/// carries the file at `path` as it stands, `now` its blob on disk: the
/// commit `HEAD` stood at as the call began is not `tip`, the two hold
/// different blobs for the file, and the file stands at `tip`'s, absent
/// where `tip` holds none (#3220). `None` where any of it does not hold,
/// where `tip` is `None` since the call's line moves `HEAD` itself
/// ([`moves_head`]), or where git cannot say, so the file is judged as
/// any write is.
fn landed_at(
    guarded: &Guarded,
    tip: Option<&str>,
    path: &str,
    now: Option<&str>,
) -> Option<String> {
    let (began, tip) = (guarded.head.as_deref()?, tip?);
    if began == tip {
        return None;
    }
    let blob = |commit: &str| kept::blob_at(&guarded.checkout, commit, path).ok();
    let holds = blob(tip)?;
    (blob(began)? != holds && holds.as_deref() == now).then(|| tip.to_owned())
}

/// The person's approval of the file the call left at `path`, where it
/// admits exactly the change from the blob `was` to what stands now
/// ([`super::approval`]); `None` where none does.
fn approved(checkout: &Path, path: &str, was: Option<&str>, call: &Call) -> Option<Admission> {
    let before = match was {
        Some(id) => Some(String::from_utf8(cat_blob(checkout, id).ok()?).ok()?),
        None => None,
    };
    let now = match fs::read(checkout.join(path)) {
        Ok(bytes) => Some(String::from_utf8(bytes).ok()?),
        Err(_) => None,
    };
    call.approvals()
        .admits(Path::new(path), before.as_deref(), now.as_deref())
}

/// What a kept file says when it moved: `why` it is kept, where what it
/// was before stands, and, where it does not stand at `HEAD`'s blob and
/// so [`kept::keep`] records it, that `done` lists it; a file a `git
/// switch` left at `HEAD`'s blob is no record (#2995).
fn kept_words(checkout: &Path, path: &str, why: &str, was: Option<&str>) -> String {
    let before = kept::before_the_call(was);
    let listed = match kept::at_head(checkout, path) {
        Ok(true) => "it stands at HEAD's blob, so `done` lists nothing",
        _ => "until a person commits it or restores it to HEAD, `done` lists it as unmet",
    };
    format!("changed while the call ran, and {why}; {before}; {listed}")
}

/// One guarded file where a call stood, as [`writes::can_write`] asks
/// about it.
struct OnDisk {
    cwd: PathBuf,
    file: PathBuf,
    name: String,
}

impl OnDisk {
    fn new(cwd: &Path, checkout: &Path, path: &str) -> OnDisk {
        let file = checkout.join(path);
        let name = file
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        OnDisk {
            cwd: cwd.to_path_buf(),
            file: resolved(&file),
            name,
        }
    }
}

impl Place for OnDisk {
    fn is(&self, word: &str) -> bool {
        !word.is_empty() && resolved(&at(&self.cwd, word)) == self.file
    }

    fn name(&self) -> &str {
        &self.name
    }

    /// The bytes of a program an interpreter runs or one run by its path;
    /// `None` for no regular file, or one larger than any such program.
    fn read(&self, word: &str) -> Option<Vec<u8>> {
        const LARGEST: u64 = 16 << 20;
        let path = at(&self.cwd, word);
        let meta = fs::metadata(&path).ok()?;
        (meta.is_file() && meta.len() <= LARGEST)
            .then(|| fs::read(&path).ok())
            .flatten()
    }
}

/// `path` with its directory resolved, links and `..` alike, so a word
/// and the guarded file compare as places; lexical where the directory
/// is not on disk.
fn resolved(path: &Path) -> PathBuf {
    match (
        path.parent().and_then(|dir| dir.canonicalize().ok()),
        path.file_name(),
    ) {
        (Some(dir), Some(name)) => dir.join(name),
        _ => path.components().collect(),
    }
}

/// One reading per moved file, in the store of the checkout the file is
/// in, stamped over the file as it stands: a failure for one put back, a
/// note that passes for one kept or approved; a kept one's record, which
/// `done` reads ([`kept::keep`]); and an approved one's audit entry
/// ([`audit::record`]).
fn record_moved(root: &Path, call: &Call, moved: &[Moved]) -> Result<()> {
    let words = format!("{} {}", call.tool_name, call.subject(root));
    for one in moved {
        // After the file was put back or kept: the stamp is over it as it
        // stands now.
        let Some(status) = one.verdict.status() else {
            continue;
        };
        let now = Snapshot::disk_over(&one.checkout, Path::new(&one.path))?;
        let mut reading = Reading::stamped_over(INSTRUMENT, RULE, &now, Path::new(&one.path))?;
        RULE.clone_into(&mut reading.rule);
        reading.status = status;
        reading.message = format!("{words}: {}", one.said);
        reading.session.clone_from(&call.session_id);
        Store::of_workspace(&one.checkout).record(&[reading])?;
        if one.verdict == Verdict::Kept {
            kept::keep(&one.checkout, &one.path, one.was.as_deref(), &words)?;
        }
        if let Some(admission) = &one.admission {
            audit::record(&one.checkout, call, admission)?;
        }
    }
    Ok(())
}

/// Put `path` back as it was before the call, keeping what `by` left
/// first; what was done, in words for the denial.
fn restore(checkout: &Path, path: &str, was: Option<&str>, now: Option<&str>, by: &str) -> String {
    let target = checkout.join(path);
    let kept = match now {
        Some(_) => match hash_objects(checkout, &[path]) {
            Ok(ids) => ids.into_iter().next(),
            Err(err) => {
                return format!(
                    "the call changed it, and what it wrote could not be kept ({err:#}), so it \
                     stands as the call left it; put it back by hand"
                );
            }
        },
        None => None,
    };
    let done = match was {
        Some(id) => cat_blob(checkout, id).and_then(|bytes| {
            let mode = fs::metadata(&target).map(|m| m.permissions()).ok();
            reading::replace(&target, &bytes)?;
            if let Some(mode) = mode {
                fs::set_permissions(&target, mode)
                    .with_context(|| format!("restoring the mode of {}", target.display()))?;
            }
            Ok(format!("put back as it stood before the call, blob {id}"))
        }),
        None => fs::remove_file(&target)
            .with_context(|| format!("removing {}", target.display()))
            .map(|()| "it was not on disk before the call and is removed again".to_owned()),
    };
    let done = done.unwrap_or_else(|err| format!("it could not be put back: {err:#}"));
    match kept {
        Some(id) => format!(
            "{by} wrote it, and it is {done}; what {by} wrote is blob {id} \
             (`git cat-file -p {id}`)"
        ),
        None => format!("{by} removed it, and it is {done}"),
    }
}

/// Whether `checkout` is a linked worktree whose owner record names
/// `session` (#3621): where a change to a guarded file is the session's
/// whichever of its calls was running, and is put back (#3644 rulings 1
/// and 4). Main's checkout, where a person edits, and a checkout no
/// record names, or another's, are not.
fn sessions_own(checkout: &Path, session: &str) -> bool {
    let linked = git::own_dir(checkout)
        .zip(git::common_dir(checkout))
        .is_some_and(|(own, common)| own != common);
    linked
        && SessionId::new(session)
            .is_some_and(|session| owner::of(checkout).ok().flatten() == Some(session))
}

/// Whether the file stands as steer wrote it in this call: the call runs
/// steer (asked by the caller), the file is one steer writes, and steer's
/// reading of the file was taken since the record, passed, wrote, is
/// stamped over the file as it stands now, and names the additions the
/// file now carries over the content the record kept, every one, judged on
/// the ground steer judged it on: while a merge stands with the file
/// conflicted, the merge's blobs and not the markers ([`steer::Ground`]). A write
/// after steer's moves the stamp; a value written before it is an addition
/// the reading does not name, or a change that is none, and the file is
/// put back. What neither can see is a comment written before steer in the
/// same call, which binds nothing.
fn steered(checkout: &Path, path: &str, was: Option<&str>, since: i64) -> bool {
    was.and_then(|was| steered_by(checkout, path, was, since))
        .is_some()
}

/// [`steered`], `None` at the first thing that says it was not.
fn steered_by(checkout: &Path, path: &str, was: &str, since: i64) -> Option<()> {
    let manifest = steer::binding(checkout).ok()?;
    steer::steerable(&manifest, path).then_some(())?;
    let reading = Store::of_workspace(checkout)
        .read(steer::INSTRUMENT, path)
        .ok()??;
    let taken_since = reading::epoch_seconds(&reading.at).is_ok_and(|at| at >= since);
    (reading.status == Status::Pass
        && taken_since
        && !reading.message.contains("dry run")
        && on_disk(&checkout.join(path))
            .is_some_and(|now| reading.key() == git::file_hash(path, &now)))
    .then_some(())?;
    let before = String::from_utf8_lossy(&cat_blob(checkout, was).ok()?).into_owned();
    let now = fs::read_to_string(checkout.join(path)).ok()?;
    let verdict = steer::Ground::of(checkout, path, before)
        .and_then(|g| g.judge(&now))
        .ok()?;
    let added = verdict.additions();
    (!added.is_empty()
        && reading.numbers.get("added") == Some(&Number::count(added.len()))
        && reading.message.ends_with(&added.join(", ")))
    .then_some(())
}

/// Which build judges a call that runs steer (#3631). Steer's write is
/// judged with steer's own code and by the manifest the checkout commits,
/// so a build compiled from other sources than the checkout carries, or
/// one that cannot read that manifest, is no judge of it.
enum Steering {
    /// This build, with why it cannot read the manifest where it cannot,
    /// and no other build of the checkout would.
    This(Option<String>),
    /// The build of the checkout the call ran in, another than the one
    /// this build was asked about, and why this one is not.
    Checkout(String),
    /// A newer build of this checkout: this one is stale, and why.
    Newer(String),
}

impl Steering {
    fn of(root: &Path, checkout: &Path) -> Steering {
        let sources = crate::inputs::sources_digest(checkout).ok();
        let apart = checkout.join(crate::inputs::SOURCES_DIR).is_dir()
            && sources.as_deref() != Some(crate::built::SOURCES);
        let unread = steer::binding(checkout).err().map(|err| format!("{err:#}"));
        let why = unread.clone().unwrap_or_else(|| {
            let carries = checkout.join(crate::inputs::SOURCES_DIR);
            format!(
                "{} is not what this build was compiled from",
                carries.display()
            )
        });
        match (
            apart,
            unread.is_some(),
            crate::built::same_directory(checkout, root),
        ) {
            (false, false, _) => Steering::This(None),
            (_, _, false) => Steering::Checkout(why),
            (true, _, true) => Steering::Newer(why),
            (false, true, true) => Steering::This(unread),
        }
    }
}

/// The call handed to the build of `held`, the checkout it ran in: the
/// record laid in `held`'s store, where its build looks for it, and what
/// that build printed, whose decision is this one's (#1047, #3631). `None`
/// where no build of it judged the call.
fn handed(held: &Path, call: &Call, taken: &str, text: &str) -> Option<Vec<u8>> {
    reading::replace(&record_path(held, call)?, taken.as_bytes()).ok()?;
    super::hand_off::walk(
        held,
        super::Adapter::CommandHook,
        &["permit", "--after"],
        text,
    )
    .ok()
}

/// Whether one of the command's simple commands runs `xtask steer`,
/// however xtask is run: `cargo xtask steer`, a built `xtask` by path,
/// `cargo run -p xtask -- steer` ([`argv`]).
fn runs_steer(command: &str) -> bool {
    let steer = OneOrMany::One("xtask steer".to_owned());
    argv::commands(command)
        .iter()
        .any(|argv| argv.invokes(&steer))
}

/// The file's blob, read by `blob`, and the commit `HEAD` stood at across
/// that read, read by `head` before it and after it. Another session's
/// merge can land between the reads (#3220), and a blob judged against a
/// `HEAD` it was not read under puts back what the merge wrote, so the
/// file is read again while the two reads of `HEAD` differ, at most
/// three times; where `HEAD` never stands still, the last read and the
/// `HEAD` after it.
fn settled(
    mut head: impl FnMut() -> Option<String>,
    mut blob: impl FnMut() -> Option<String>,
) -> (Option<String>, Option<String>) {
    const TRIES: usize = 3;
    let mut tip = head();
    let mut tries = 0;
    loop {
        let now = blob();
        let after = head();
        tries += 1;
        if after == tip || tries == TRIES {
            return (now, after);
        }
        tip = after;
    }
}

/// Whether one of the line's simple commands runs a git command that
/// makes a commit or moves `HEAD`, `git -C <dir> commit` among them
/// ([`argv::Argv::runs`]). Such a line's history is its own, so a write it
/// commits is judged as the write it is ([`landed_at`]).
fn moves_head(line: &argv::Line) -> bool {
    const MOVES: [&str; 10] = [
        "git commit",
        "git merge",
        "git pull",
        "git rebase",
        "git reset",
        "git switch",
        "git checkout",
        "git cherry-pick",
        "git revert",
        "git am",
    ];
    line.commands()
        .iter()
        .any(|argv| MOVES.iter().any(|phrase| argv.runs(phrase)))
}

/// Where the call starts ([`Call::start`]), and the one checkout it is
/// held in: the one it ran in, where that start moved by its line acts
/// ([`Walk::ran_in`]), and never the hook's own where the call ran
/// elsewhere (#2597, #2607).
fn held(root: &Path, call: &Call) -> (PathBuf, PathBuf) {
    let line = call.command().map(argv::Line::read);
    let workspace = call.workspace(root);
    let start = call.start(&workspace);
    let checkout = Walk::of(&start, line.as_ref()).ran_in(&workspace);
    (start, checkout)
}

/// Where the line's shell stands, which its words are read against:
/// `from`, the call's `cwd` or the start its record keeps, or `checkout`
/// where there is none, moved by the line's leading `cd`, `pushd` and
/// `popd` ([`Walk::runs_in`]).
fn shell_at(from: Option<&Path>, checkout: &Path, line: &argv::Line) -> PathBuf {
    Walk::of(from.unwrap_or(checkout), Some(line)).runs_in()
}

/// The rule's text and the blob of every tracked file it scopes and does
/// not exempt in `checkout`; `None` where the manifest carries no such
/// rule. The exempt is read in the manifest's one path grammar,
/// [`manifest::exempted`], as `permit` reads it at the call. A file
/// whose blob the object database may not hold, one edited and not
/// staged, is written into it, so the record can always put it back.
fn guard(checkout: &Path) -> Result<Option<Guarded>> {
    let Some(Reach {
        text,
        scope,
        exempt,
    }) = rule_of(checkout)?
    else {
        return Ok(None);
    };
    // `HEAD` before the blobs: a commit that lands between the two reads
    // leaves the blobs at its own, which then do not move.
    let head = operation::head(checkout);
    let mut args = vec!["ls-files".to_owned(), "-s".to_owned(), "-z".to_owned()];
    args.push("--".to_owned());
    args.extend(scope.iter().map(|s| format!(":(glob){s}")));
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let mut blobs = BTreeMap::new();
    let mut unstaged = Vec::new();
    for (head, path) in git::entries(checkout, &args)? {
        let path = path.as_str();
        let [mode, staged, stage] = head.split(' ').collect::<Vec<_>>()[..] else {
            continue;
        };
        if mode == "160000"
            || blobs.contains_key(path)
            || manifest::exempted(exempt.iter().map(String::as_str), Path::new(path))
        {
            continue;
        }
        // A conflict's path is listed once per stage and has no staged
        // blob; its markers on disk are what a write is put back to (#2880).
        let blob = on_disk(&checkout.join(path));
        if blob
            .as_deref()
            .is_some_and(|id| stage != "0" || id != staged)
        {
            unstaged.push(path.to_owned());
        }
        blobs.insert(path.to_owned(), blob);
    }
    if !unstaged.is_empty() {
        let paths: Vec<&str> = unstaged.iter().map(String::as_str).collect();
        hash_objects(checkout, &paths)?;
    }
    Ok(Some(Guarded {
        checkout: checkout.to_path_buf(),
        head,
        text,
        blobs,
    }))
}

/// What the sensor reads of `harness.self`'s row: the text a denial
/// prints and the paths the rule reaches, its scope less its exempt.
struct Reach {
    text: String,
    scope: Vec<String>,
    exempt: Vec<String>,
}

/// `harness.self`'s text, scope and exempt in the manifest at `checkout`,
/// read as plain TOML so a build older or newer than the manifest still
/// finds them.
fn rule_of(checkout: &Path) -> Result<Option<Reach>> {
    let path = checkout.join(manifest::FILE);
    let Ok(text) = fs::read_to_string(&path) else {
        return Ok(None);
    };
    let table: toml::Table =
        toml::from_str(&text).with_context(|| format!("reading {}", path.display()))?;
    let rule = table
        .get("rule")
        .and_then(toml::Value::as_array)
        .and_then(|rules| {
            rules
                .iter()
                .find(|r| r.get("id").and_then(toml::Value::as_str) == Some(RULE))
        });
    let Some(rule) = rule else {
        return Ok(None);
    };
    let paths = |key: &str| -> Vec<String> {
        match rule.get(key) {
            Some(toml::Value::String(one)) => vec![one.clone()],
            Some(toml::Value::Array(many)) => many
                .iter()
                .filter_map(toml::Value::as_str)
                .map(str::to_owned)
                .collect(),
            _ => Vec::new(),
        }
    };
    let scope = paths("scope");
    let exempt = paths("exempt");
    let text = rule
        .get("text")
        .and_then(toml::Value::as_str)
        .unwrap_or_default()
        .to_owned();
    Ok((!scope.is_empty()).then_some(Reach {
        text,
        scope,
        exempt,
    }))
}

/// The blob id of the file at `path` as it is on disk, `None` where there
/// is no file.
pub(super) fn on_disk(path: &Path) -> Option<String> {
    fs::read(path).ok().map(|bytes| git::blob_id(&bytes))
}

/// Where the records are kept, under the store of `root`.
fn records(root: &Path) -> PathBuf {
    Store::of_workspace(root)
        .root()
        .join("permit")
        .join("before")
}

/// The record of one call: under its session, named by its tool-use id.
/// `None` for a call without both, or with one that is not a plain name.
fn record_path(root: &Path, call: &Call) -> Option<PathBuf> {
    let plain = |s: &str| {
        !s.is_empty()
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    };
    (plain(&call.session_id) && plain(&call.tool_use_id)).then(|| {
        records(root)
            .join(&call.session_id)
            .join(format!("{}.json", call.tool_use_id))
    })
}

/// Records older than [`KEPT`] removed, and a session's directory left
/// empty; best effort, since a record left is one more file and no harm.
fn prune(dir: &Path) {
    let Ok(sessions) = fs::read_dir(dir) else {
        return;
    };
    let now = SystemTime::now();
    for session in sessions.flatten() {
        let Ok(calls) = fs::read_dir(session.path()) else {
            continue;
        };
        for call in calls.flatten() {
            let old = call
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|at| now.duration_since(at).ok())
                .is_some_and(|age| age > KEPT);
            if old {
                fs::remove_file(call.path()).ok();
            }
        }
        // Removes only a directory that is empty.
        fs::remove_dir(session.path()).ok();
    }
}

/// `git hash-object -w` over `paths` as they are on disk, unfiltered, so
/// the ids are the ones [`git::blob_id`] takes and the objects are in the
/// database; the ids, in order.
fn hash_objects(checkout: &Path, paths: &[&str]) -> Result<Vec<String>> {
    let mut stdin = paths.join("\n");
    stdin.push('\n');
    let ids = git::fed(
        checkout,
        &["hash-object", "-w", "--no-filters", "--stdin-paths"],
        &stdin,
    )?;
    Ok(ids.lines().map(str::to_owned).collect())
}

/// The bytes of the blob `id`, which need not be text.
fn cat_blob(checkout: &Path, id: &str) -> Result<Vec<u8>> {
    git::run(
        Command::new("git"),
        checkout,
        &["cat-file", "blob", id],
        None,
    )
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use super::{runs_steer, settled, steered};
    use crate::key::Snapshot;
    use crate::reading::{Reading, Status, Store};
    use crate::{git, steer};

    /// #3183: `steered` stands on a passing steer reading over the write
    /// only for a file `harness.self` names, the list steer reads; a
    /// reading over the same addition to a file the rule does not name,
    /// one left behind or written under another manifest, lets nothing
    /// through.
    #[test]
    fn a_steer_reading_over_a_file_harness_self_does_not_name_is_no_steer_write() {
        let root = crate::scratch::Tree::new("after-steered").unwrap();
        let manifest = "[budget]\ntotal = 1\nnew = 1\ndocs = 1\nharness = 1\nlock_packages = 1\nclean_build_release_s = 1\napi_items = 1\nbindings = 1\n[[rule]]\nid = \"harness.self\"\ntool = [\"Edit\", \"Write\"]\nscope = [\"seams.toml\", \"oracle.toml\"]\ndeny = true\ntext = \"harness.self\"\nremedy = \"steer\"\n";
        let was = "[a]\nb = 1\n";
        fs::write(root.join("seams.toml"), manifest).unwrap();
        for file in ["oracle.toml", "notes.toml"] {
            fs::write(root.join(file), was).unwrap();
        }
        git::git(&root, &["init", "-q"]).unwrap();
        git::git(&root, &["add", "."]).unwrap();
        git::git(
            &root,
            &[&crate::scratch::UNHOOKED[..], &["commit", "-qm", "x"]].concat(),
        )
        .unwrap();
        let steered_over = |file: &str| {
            fs::write(root.join(file), "[a]\nb = 1\nc = 2\n").unwrap();
            let over = Path::new(file);
            let snapshot = Snapshot::disk_over(&root, over).unwrap();
            let mut reading =
                Reading::stamped_over(steer::INSTRUMENT, file, &snapshot, over).unwrap();
            reading.status = Status::Pass;
            reading.count("added", 1).unwrap();
            reading.message = format!("{file}: 1 added: a.c");
            Store::of_workspace(&root).record(&[reading]).unwrap();
            steered(&root, file, Some(&git::blob_id(was.as_bytes())), 0)
        };
        assert!(
            steered_over("oracle.toml"),
            "the control: a named file's steer write stands"
        );
        assert!(!steered_over("notes.toml"));
    }

    /// #3220: a merge lands between the first read of `HEAD` and the
    /// file's, so the file is read again under the `HEAD` it stands at;
    /// a `HEAD` that never stands still is read three times and no more.
    #[test]
    fn the_file_is_read_again_while_head_moves_across_the_read() {
        let reads = |ids: &[&str]| {
            let mut ids: Vec<Option<String>> =
                ids.iter().rev().map(|id| Some((*id).to_owned())).collect();
            move || ids.pop().flatten()
        };
        let (now, tip) = settled(reads(&["a", "b", "b"]), reads(&["old", "new"]));
        assert_eq!((now.as_deref(), tip.as_deref()), (Some("new"), Some("b")));
        let (now, tip) = settled(reads(&["a", "a"]), reads(&["one"]));
        assert_eq!((now.as_deref(), tip.as_deref()), (Some("one"), Some("a")));
        let (now, tip) = settled(
            reads(&["a", "b", "c", "d", "e"]),
            reads(&["1", "2", "3", "4"]),
        );
        assert_eq!((now.as_deref(), tip.as_deref()), (Some("3"), Some("d")));
    }

    #[test]
    fn steer_is_read_from_the_argv_and_not_from_the_text() {
        assert!(runs_steer("cargo xtask steer seams.toml --from p.toml"));
        assert!(runs_steer(
            "cd x && target/debug/xtask steer oracle.toml --from p"
        ));
        assert!(runs_steer(
            "cargo run -q -p xtask -- steer seams.toml --from p"
        ));
        assert!(runs_steer(
            "timeout 1800 cargo xtask steer seams.toml --from p"
        ));
        assert!(!runs_steer("echo 'cargo xtask steer' > seams.toml"));
        assert!(!runs_steer("python3 steer.py"));
        assert!(!runs_steer("grep steer xtask/src/steer.rs"));
    }
}
