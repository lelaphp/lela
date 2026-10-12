//! The two adapters over one judge and what each records: the command
//! hook, and the function hook above it (harness-design.md §4.4). `run` is
//! the command both are over: the call on stdin, the hook's JSON on stdout,
//! the findings as readings, and the refusal when this build cannot judge.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::approval::Evidence;
use super::argv::Line;
use super::audit;
use super::call::Call;
use super::hand_off::hand_off;
use super::operation;
use super::rules::{Judgement, Kind, deny_output, judge};
use super::{INSTRUMENT, STALE_RULE, UNJUDGED_RULE};
use crate::built::{self, Judge, Refusal};
use crate::key::Snapshot;
use crate::manifest::{self, Manifest};
use crate::reading::{Reading, Status, Store};

/// Which adapter judges the call: the command hook records every
/// finding; the function hook above it records a denial, which ends the
/// call there, and leaves a note to the command hook beneath, which sees
/// the same call and records it once (harness-design.md §4.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Adapter {
    CommandHook,
    AboveTheCommandHook,
}

impl Adapter {
    fn records(self, kind: Kind) -> bool {
        match self {
            Adapter::CommandHook => true,
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
            finding.text
        );
        reading.adr = finding.adr.first().cloned().unwrap_or_default();
        reading.session.clone_from(&call.session_id);
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
    let text = json.map_or_else(
        || {
            let mut bytes = Vec::new();
            std::io::stdin()
                .read_to_end(&mut bytes)
                .context("reading the tool call from stdin")
                .map(|_| String::from_utf8_lossy(&bytes).into_owned())
        },
        |json| Ok(json.to_owned()),
    );
    let call = text.and_then(|text| Call::parse(&text).map(|call| (text, call)));
    let (text, mut call) = match call {
        Ok(read) => read,
        Err(err) => {
            println!(
                "{}",
                deny_output(&format!("permit could not judge this call: {err:#}"))
            );
            return Ok(());
        }
    };
    // Each adapter reads the person's answers where its own caller put
    // them: the command hook from the transcript Claude Code writes, the
    // function hook from the records it read from the engine (#2247).
    call.evidence = match adapter {
        Adapter::CommandHook => Evidence::Transcript,
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
            return Err(refusal.into());
        }
    };
    match judge(&manifest, &workspace, &call) {
        Ok(judgement) => {
            if let Some(output) = judgement.hook_stdout() {
                println!("{output}");
            }
            record_the_approvals(&checkout, &call, &judgement);
            record_the_operation(&workspace, &call, &judgement);
            if let Err(err) = record(&checkout, &call, &judgement, adapter) {
                eprintln!("xtask permit: the decision stands but was not recorded: {err:#}");
            }
        }
        Err(err) => {
            println!(
                "{}",
                deny_output(&format!("permit could not judge this call: {err:#}"))
            );
        }
    }
    Ok(())
}

/// The audit entry of each approval that admitted an `Edit` or a
/// `Write` ([`audit`]); a `Bash` line's is `permit --after`'s, once what
/// it wrote stands. Best effort, as recording the decision is.
fn record_the_approvals(checkout: &Path, call: &Call, judgement: &Judgement) {
    if call.tool_name == "Bash" || judgement.denial().is_some() {
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

/// The session whose `git merge`, `rebase` or kin this admits, as the
/// starter of the operation it begins ([`operation::record`]), whatever
/// the manifest holds: an operation can bring the first `where =
/// "operating"` rule into the working tree it stands in, and a starter
/// recorded only under such a rule left that one foreign to every session
/// (#2694). Best effort, as recording the decision is.
fn record_the_operation(root: &Path, call: &Call, judgement: &Judgement) {
    let Some(command) = call.command() else {
        return;
    };
    if judgement.denial().is_some() {
        return;
    }
    if let Err(err) = operation::record(root, call, &Line::read(command)) {
        eprintln!("xtask permit: the operation's starter was not recorded: {err:#}");
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
    let recorded = Snapshot::disk_over(root, Path::new(manifest::FILE))
        .and_then(|now| Reading::stamped_over(INSTRUMENT, rule, &now, Path::new(manifest::FILE)))
        .and_then(|mut reading| {
            rule.clone_into(&mut reading.rule);
            reading.status = status;
            reading.message = format!(
                "{} {}: {what} {refusal}",
                call.tool_name,
                call.subject(root)
            );
            reading.session.clone_from(&call.session_id);
            Store::of_workspace(root).record(&[reading])
        });
    // The refusal is the exit already; the outcome of recording it is not.
    if let Err(err) = recorded {
        eprintln!("xtask permit: the refusal to judge was not recorded: {err:#}");
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::process::Command;

    use super::{Call, Manifest, judge, record_the_operation};
    use crate::manifest::Where;
    use crate::scratch::Tree;

    /// A repository of one commit on `main`.
    fn repository(name: &str) -> Tree {
        let dir = Tree::new(name).unwrap();
        for args in [
            &["init", "-q", "-b", "main"][..],
            &[
                "-c",
                "user.name=x",
                "-c",
                "user.email=x@x",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "one",
            ],
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
        dir
    }

    fn bash(session: &str, line: &str) -> Call {
        let call = serde_json::json!({"tool_name": "Bash", "session_id": session, "tool_input": {"command": line}});
        Call::parse(&call.to_string()).unwrap()
    }

    /// `session`'s `git merge` admitted and recorded under `manifest`, and
    /// the merge it started then standing, as git leaves one stopped
    /// before its commit.
    fn started(manifest: &Manifest, dir: &Path, session: &str) {
        let start = bash(session, "git merge --no-commit topic");
        let admitted = judge(manifest, dir, &start).unwrap();
        record_the_operation(dir, &start, &admitted);
        std::fs::write(dir.join(".git/MERGE_HEAD"), "0".repeat(40) + "\n").unwrap();
    }

    /// The trap of #2694: a merge started under a manifest with no `where
    /// = "operating"` rule, whose working tree then carries `git.operation`
    /// as the merge that lands it does, is still its starter's to step,
    /// abort and commit, and still another session's to be refused.
    #[test]
    fn a_merge_started_before_the_operating_row_landed_is_still_its_starters() {
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let manifest = Manifest::load(workspace).unwrap();
        let mut without = manifest.clone();
        without.rules.retain(|r| r.within != Where::Operating);
        let dir = repository("permit-operating");
        started(&without, &dir, "s1");

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

    /// `git.operation` as the manifest carries it (#2530, #2617): its
    /// `allow-starter-continue.json`, the starter's own `git merge
    /// --continue`, passes the merge that session started, which the
    /// fixture walks cannot record, and the same line from another session
    /// is refused.
    #[test]
    fn the_git_operation_row_lets_the_starter_conclude_its_own_merge() {
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let manifest = Manifest::load(workspace).unwrap();
        let fixture = "xtask/fixtures/rules/git.operation/allow-starter-continue.json";
        let own = Call::parse(&std::fs::read_to_string(workspace.join(fixture)).unwrap()).unwrap();
        let dir = repository("permit-operating-starter");
        started(&manifest, &dir, &own.session_id);
        let judged = |call: &Call| {
            let judgement = judge(&manifest, &dir, call).unwrap();
            judgement.denial().map(|f| f.rule.clone())
        };
        assert_eq!(judged(&own), None);
        let other = bash("another-hand", own.command().unwrap());
        assert_eq!(judged(&other).as_deref(), Some("git.operation"));
    }

    /// #2720 as it happened: an agent's `git merge` is admitted, and before
    /// the merge writes its marker a sibling agent of the same session has
    /// a line admitted that `cd`s into its own worktree past its first
    /// command and runs `git merge --ff-only` there. The merge stays its
    /// starter's: `allow-starter-commit.json` and the starter's abort pass,
    /// and `deny-sibling-commit.json` and the sibling's abort are refused.
    #[test]
    fn a_sibling_s_line_between_a_start_and_its_marker_leaves_the_merge_its_starters() {
        let workspace = Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap();
        let manifest = Manifest::load(workspace).unwrap();
        let fixture = |name: &str| {
            let path = workspace
                .join("xtask/fixtures/rules/git.operation")
                .join(name);
            Call::parse(&std::fs::read_to_string(path).unwrap()).unwrap()
        };
        let own = fixture("allow-starter-commit.json");
        let sibling = fixture("deny-sibling-commit.json");
        let saying = |call: &Call, line: &str| {
            let mut call = call.clone();
            call.tool_input.command = Some(line.to_owned());
            call
        };
        let dir = repository("permit-operating-sibling");
        let admit = |call: &Call| {
            let admitted = judge(&manifest, &dir, call).unwrap();
            assert!(admitted.denial().is_none(), "{admitted:?}");
            record_the_operation(&dir, call, &admitted);
        };
        admit(&saying(&own, "git merge --no-ff --no-commit t2634"));
        let elsewhere = format!(
            "S=/var/empty; cat $S/owner; cd /var/empty/t2645; git merge --ff-only main; \
             cd {}; git log --oneline -3",
            dir.display()
        );
        admit(&saying(&sibling, &elsewhere));
        std::fs::write(dir.join(".git/MERGE_HEAD"), "0".repeat(40) + "\n").unwrap();

        let judged = |call: &Call| {
            let judgement = judge(&manifest, &dir, call).unwrap();
            judgement.denial().map(|f| f.rule.clone())
        };
        let refused = Some("git.operation");
        assert_eq!(judged(&own), None);
        assert_eq!(judged(&saying(&own, "git merge --abort")), None);
        assert_eq!(judged(&sibling).as_deref(), refused);
        let abort = saying(&sibling, "git merge --abort");
        assert_eq!(judged(&abort).as_deref(), refused);
    }
}
