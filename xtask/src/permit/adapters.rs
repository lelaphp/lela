//! The two adapters over one judge and what each records: the command
//! hook, and the function hook above it (harness-design.md §4.4). `run` is
//! the command both are over: the call on stdin, the hook's JSON on stdout,
//! the findings as readings, and the refusal when this build cannot judge.

use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::approval::Evidence;
use super::argv::Line;
use super::audit;
use super::call::Call;
use super::hand_off::hand_off;
use super::ledger;
use super::rules::{Judgement, Kind, deny_output, judge};
use super::shell::Walk;
use super::writes;
use super::{INSTRUMENT, STALE_RULE, UNJUDGED_RULE};
use crate::built::{self, Judge, Refusal};
use crate::key::Snapshot;
use crate::manifest::{self, Manifest};
use crate::reading::{Reading, Status, Store};

/// Which adapter judges the call: the command hook records every
/// finding; the function hook above it records a denial, which ends the
/// call there, and leaves a note to the command hook beneath, which sees
/// the same call and records it once (harness-design.md §4.4). Pre-commit's
/// `--diff` records every finding under its own caller, [`PRE_COMMIT`], so
/// a diff's judgement is never counted as an agent's call (#3619).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Adapter {
    CommandHook,
    AboveTheCommandHook,
    PreCommit,
}

/// The caller a diff judged at pre-commit is recorded under: its readings'
/// session, and the head of their scope.
pub(crate) const PRE_COMMIT: &str = "pre-commit";

impl Adapter {
    fn records(self, kind: Kind) -> bool {
        match self {
            Adapter::CommandHook | Adapter::PreCommit => true,
            Adapter::AboveTheCommandHook => kind == Kind::Deny,
        }
    }
}

/// Record one reading per finding the adapter records: scope and rule are
/// the rule id, a denial fails and a note passes, the stamp is over the
/// file the call touches. The outcome is dropped: the decision is in the
/// hook's JSON and the exit is 0 whatever it was (harness-design.md §4.4).
pub(super) fn record(
    root: &Path,
    call: &Call,
    judgement: &Judgement,
    adapter: Adapter,
) -> Result<()> {
    let touched = call.path(root).unwrap_or_else(|| PathBuf::from("."));
    let mut readings: Vec<Reading> = Vec::new();
    for finding in judgement
        .findings
        .iter()
        .filter(|f| adapter.records(f.kind))
    {
        // One call is one moment and one tree: the first finding's stamp
        // is every finding's, and git is spawned once per call.
        let mut reading = match readings.first() {
            Some(first) => {
                let mut reading = first.clone();
                reading.scope.clone_from(&finding.rule);
                reading
            }
            None => Reading::stamped_over(
                INSTRUMENT,
                &finding.rule,
                &Snapshot::disk_over(root, &touched)?,
                &touched,
            )?,
        };
        reading.rule.clone_from(&finding.rule);
        reading.status = match finding.kind {
            Kind::Deny => Status::Fail,
            Kind::Note => Status::Pass,
        };
        reading.message = format!(
            "{} {}: {}",
            call.tool_name,
            call.subject(root),
            finding.said()
        );
        reading.adr = finding.adr.first().cloned().unwrap_or_default();
        reading.session.clone_from(&call.session_id);
        if adapter == Adapter::PreCommit {
            reading.scope = format!("{PRE_COMMIT}/{}", finding.rule);
            PRE_COMMIT.clone_into(&mut reading.session);
        }
        readings.push(reading);
    }
    Store::of_workspace(root).record(&readings)?;
    Ok(())
}

/// The command: judge the call on stdin (or in `--json`), print the hook's
/// JSON, record the findings the adapter records. A call this cannot read
/// is a deny that says so; a manifest it cannot judge by is the refusal,
/// the one error returned, since that is the signal each adapter walks its
/// next candidate on (§4.4, #16). The manifest is read here and not by the
/// caller for that reason: a build that cannot read it must record the
/// refusal before it exits.
pub fn run(root: &Path, json: Option<&str>, adapter: Adapter) -> Result<()> {
    let call = Call::text(json).and_then(|text| Call::parse(&text).map(|call| (text, call)));
    let (text, mut call) = match call {
        Ok(read) => read,
        Err(err) => {
            unjudged(&err);
            return Ok(());
        }
    };
    // Each adapter reads the person's answers where its own caller put
    // them: the command hook from the transcript Claude Code writes, the
    // function hook from the records it read from the engine (#2247).
    call.evidence = match adapter {
        Adapter::CommandHook | Adapter::PreCommit => Evidence::Transcript,
        Adapter::AboveTheCommandHook => Evidence::Engine,
    };
    // The blobs of harness.self's files before a Bash call, for the
    // PostToolUse hook's `--after` to hold the call to. Taken before any
    // build decides whether it judges, since the call may run whoever
    // judges it; one that fails leaves the call to the judge alone.
    if let Err(err) = super::after::before(root, &call) {
        eprintln!("xtask permit: harness.self's files were not recorded before this call: {err:#}");
    }
    // The workspace whose manifest judges the call, and the checkout it is
    // in, whose store takes the readings and whose manifest this build is
    // held to: the two differ for a fixture workspace, whose manifest no
    // build is compiled against (#57).
    let workspace = call.workspace(root);
    let checkout = call.checkout(root);
    // What this build judges by: the manifest in front of it, the sources
    // it was built from, and the manifest readable as rules. Any of the
    // three failing is a refusal to decide, and a refusal decides nothing
    // whichever way the decision would have fallen. A manifest that will
    // not parse is the refusal no rebuild repairs, so it joins the two
    // digests as the kind of refusal that ends the walk (#118). A call in
    // a worktree whose manifest this build was not compiled against is
    // that worktree's build's to judge, and its decision is this one's
    // (#1047).
    let manifest = match built::judges(&checkout, root, &crate::built_root()) {
        Ok(Judge::This) => Manifest::load(&workspace).map_err(Refusal::Unreadable),
        Ok(Judge::Checkout(other)) => match hand_off(&other, &text, adapter) {
            Ok(decision) => {
                return std::io::stdout()
                    .write_all(&decision)
                    .context("printing the decision the worktree's build made");
            }
            Err(refusal) => Err(refusal),
        },
        Err(refusal) => Err(refusal),
    };
    let manifest = match manifest {
        Ok(manifest) => manifest,
        Err(refusal) => {
            record_the_refusal(&checkout, &call, &refusal);
            if refusal.hands_the_call_on() {
                return Err(refusal.into());
            }
            // No rebuild judges it either, so the decision is this one's,
            // and the walk ends here, the call recorded once (#3619).
            println!("{}", unjudged_output(&call, refusal.why()));
            return Ok(());
        }
    };
    match judge(&manifest, &workspace, &call) {
        Ok(judgement) => {
            if let Some(output) = judgement.hook_stdout() {
                println!("{output}");
            }
            record_the_approvals(&checkout, &call, &judgement);
            if adapter == Adapter::CommandHook {
                if let Err(err) = ledger::count(&checkout, &judgement.judged) {
                    eprintln!("xtask permit: the calls judged were not counted: {err:#}");
                }
                record_the_write(&workspace, &call, &judgement);
            }
            if let Err(err) = record(&checkout, &call, &judgement, adapter) {
                eprintln!("xtask permit: the decision stands but was not recorded: {err:#}");
            }
        }
        Err(err) => unjudged(&err),
    }
    Ok(())
}

/// The decision on a call no build judges, with the error's first line:
/// a write no judge saw is denied, and a `Bash` call passes with the note,
/// since an agent repairs a broken harness with `Bash` (#16, harness-design.md
/// §13 item 8).
fn unjudged_output(call: &Call, why: &anyhow::Error) -> serde_json::Value {
    let why = format!("{why:#}");
    let first = why.lines().next().unwrap_or_default();
    let said = format!("permit could not judge this call: {first}");
    if call.judged_as() == "Bash" {
        serde_json::json!({"hookSpecificOutput": {
            "hookEventName": super::rules::HOOK_EVENT, "additionalContext": said}})
    } else {
        deny_output(&said)
    }
}

/// The deny a call this build could not judge is, printed: why, `err`.
fn unjudged(err: &anyhow::Error) {
    println!(
        "{}",
        deny_output(&format!("permit could not judge this call: {err:#}"))
    );
}

/// The audit entry of each approval that admitted an `Edit` or a
/// `Write` ([`audit`]); a `Bash` line's is `permit --after`'s, once what
/// it wrote stands. Best effort, as recording the decision is.
fn record_the_approvals(checkout: &Path, call: &Call, judgement: &Judgement) {
    if call.judged_as() == "Bash" || judgement.denial().is_some() {
        return;
    }
    for admission in judgement
        .findings
        .iter()
        .filter_map(|f| f.admission.as_ref())
    {
        if let Err(err) = audit::record(checkout, call, admission) {
            eprintln!("xtask permit: the approval stands but its entry was not written: {err:#}");
        }
    }
}

/// The checkout an admitted writing call is placed in, kept under its
/// session and agent for the Stop to hold (#3618 ruling 1): a file's by
/// the file, a `Bash` line's where it runs from the call's directory
/// ([`Walk::ran_in`]) unless it only reads ([`writes::reads_only`]);
/// a place in no checkout of the repository is none. Best effort, as
/// recording the decision is.
fn record_the_write(workspace: &Path, call: &Call, judgement: &Judgement) {
    if judgement.denial().is_some() {
        return;
    }
    let checkout = match (call.command(), call.path(workspace)) {
        (Some(command), _) => {
            let line = Line::read(command);
            if writes::reads_only(&line, workspace) {
                return;
            }
            let start = call.cwd.as_deref().map_or(workspace, Path::new);
            Walk::of(start, Some(&line)).ran_in(start)
        }
        (None, Some(path)) => match path.ancestors().find(|dir| dir.is_dir()) {
            Some(dir) => Walk::of(dir, None).ran_in(dir),
            None => return,
        },
        (None, None) => return,
    };
    let ours = crate::track::worktrees(workspace).is_ok_and(|all| {
        all.iter()
            .any(|(dir, _)| dir.canonicalize().ok().as_ref() == Some(&checkout))
    });
    let agent = call.agent_id.as_deref().unwrap_or_default();
    if ours && let Err(err) = ledger::wrote(workspace, &call.session_id, agent, checkout) {
        eprintln!("xtask permit: the checkout this call wrote in was not recorded: {err:#}");
    }
}

/// The refusal as a reading, so the store says what the judge did with
/// this call and this session: through the outage of #16 a build that
/// could not start recorded nothing, and `status --section permits` said
/// the judge was up. Best effort, since the refusal stands whether or not
/// the store takes it.
///
/// The two refusals are two rules and two statuses (#118). A build apart
/// from the tree hands the call to the candidate that rebuilds, which
/// judges it; that is the walk working, not a judgement the harness is
/// missing, so it is a skip under [`STALE_RULE`] that names where the call
/// went. A manifest no build reads ends the walk with the call unjudged,
/// which is the outage itself, so it is a failure under [`UNJUDGED_RULE`];
/// so is a hand-off to a worktree's build that did not judge, since every
/// candidate of the walk hands the call to that same build (#1047).
/// Two rules and not one status apart, since the store keeps one file per
/// rule and a skip would otherwise write over the failure beneath it.
fn record_the_refusal(root: &Path, call: &Call, refusal: &Refusal) {
    let (rule, status, what) = if refusal.hands_the_call_on() {
        (
            STALE_RULE,
            Status::Skip,
            "this build did not judge this call and handed it to the next candidate, which \
             rebuilds from the tree.",
        )
    } else {
        (
            UNJUDGED_RULE,
            Status::Fail,
            "this call was not judged, and the candidate that closes the walk refuses the \
             same way.",
        )
    };
    let mut recorded = refusal_reading(root, call, rule, status, &format!("{what} {refusal}"));
    // A run of calls handed on with none judged is the walk failing open:
    // its last candidate never judged them, and the hook passed them on.
    if refusal.hands_the_call_on()
        && let Ok(within) = ledger::handed(root, &call.tool_use_id)
        && within >= ledger::BURST
    {
        let what = format!(
            "{within} calls were handed on within a minute and none was judged since: no \
             candidate of the walk judges, and the hook passes a Bash call on unjudged."
        );
        recorded =
            recorded.and_then(|()| refusal_reading(root, call, UNJUDGED_RULE, Status::Fail, &what));
    }
    // The refusal is the exit already; the outcome of recording it is not.
    if let Err(err) = recorded {
        eprintln!("xtask permit: the refusal to judge was not recorded: {err:#}");
    }
}

/// One reading of permit's own `rule` over the manifest, for `call`.
fn refusal_reading(root: &Path, call: &Call, rule: &str, status: Status, what: &str) -> Result<()> {
    let now = Snapshot::disk_over(root, Path::new(manifest::FILE))?;
    let mut reading = Reading::stamped_over(INSTRUMENT, rule, &now, Path::new(manifest::FILE))?;
    rule.clone_into(&mut reading.rule);
    reading.status = status;
    reading.message = format!("{} {}: {what}", call.tool_name, call.subject(root));
    reading.session.clone_from(&call.session_id);
    Store::of_workspace(root).record(&[reading])?;
    Ok(())
}

#[cfg(test)]
#[cfg(tracked)]
mod tests {
    use std::process::Command;

    use super::{Call, Manifest, judge};
    use crate::owner::{self, SessionId};
    use crate::scratch::Tree;

    /// A repository of one commit on `main`, owned by `owner`, with a
    /// merge standing in it as git leaves one stopped before its commit.
    fn merging(name: &str, owner: &str) -> Tree {
        let dir = Tree::new(name).unwrap();
        for args in [
            &["init", "-q", "-b", "main"][..],
            &[
                &crate::scratch::UNHOOKED[..],
                &["commit", "-q", "--allow-empty", "-m", "one"],
            ]
            .concat(),
        ] {
            assert!(
                Command::new("git")
                    .args(args)
                    .current_dir(&dir)
                    .status()
                    .unwrap()
                    .success()
            );
        }
        owner::record(&dir, &SessionId::new(owner).unwrap()).unwrap();
        std::fs::write(dir.join(".git/MERGE_HEAD"), "0".repeat(40) + "\n").unwrap();
        dir
    }

    fn bash(session: &str, line: &str) -> Call {
        let call = serde_json::json!({"tool_name": "Bash", "session_id": session, "tool_input": {"command": line}});
        Call::parse(&call.to_string()).unwrap()
    }

    /// #3621: a merge standing in a checkout is its owner's to step, abort
    /// and commit, under the manifest as it stands, and every other
    /// session's to be refused, by `git.operation`'s more specific text
    /// first; who started it is recorded nowhere, so no admission and no
    /// adapter can leave it foreign to its owner (#2694, #2734).
    #[test]
    fn a_merge_standing_in_an_owned_checkout_is_its_owners_to_end() {
        let workspace = crate::package_dir().parent().unwrap();
        let manifest = Manifest::load(workspace).unwrap();
        let dir = merging("permit-operating", "s1");
        let judged = |session: &str, line: &str| {
            let judgement = judge(&manifest, &dir, &bash(session, line)).unwrap();
            judgement.denial().map(|f| f.rule.clone())
        };
        for line in [
            "git merge --continue",
            "git merge --abort",
            "git commit -F m -- a",
            "git add -- a",
        ] {
            assert_eq!(judged("s1", line), None, "{line}");
            assert_eq!(
                judged("s2", line).as_deref(),
                Some("git.operation"),
                "{line}"
            );
        }
    }

    /// `git.operation` as the manifest carries it: its
    /// `allow-owner-continue.json`, the owner's own `git merge
    /// --continue`, passes in the checkout that session owns, which the
    /// fixture walks cannot record, and the same line from another session
    /// is refused.
    #[test]
    fn the_git_operation_row_lets_the_owner_conclude_its_merge() {
        let workspace = crate::package_dir().parent().unwrap();
        let manifest = Manifest::load(workspace).unwrap();
        let fixture = "xtask/fixtures/rules/git.operation/allow-owner-continue.json";
        let own = Call::parse(&std::fs::read_to_string(workspace.join(fixture)).unwrap()).unwrap();
        let dir = merging("permit-operating-owner", &own.session_id);
        let judged = |call: &Call| {
            let judgement = judge(&manifest, &dir, call).unwrap();
            judgement.denial().map(|f| f.rule.clone())
        };
        assert_eq!(judged(&own), None);
        let other = bash("another-hand", own.command().unwrap());
        assert_eq!(judged(&other).as_deref(), Some("git.operation"));
    }
}
