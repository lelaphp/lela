//! `xtask ticket`, the closer (#28): the judgement held without the
//! network. Every test here runs the instrument's `run` over a `gh` that
//! is a shell file, answering `issue view` from the transcript laid beside
//! it and appending what `issue close` asked to a file, so what the
//! tracker was asked is read back as a fact and nothing here reaches
//! GitHub. The checkbox grammar is held beside the parser.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

use xtask::manifest::Manifest;
use xtask::reading::{Reading, Status, Store};
use xtask::scratch::Tree;
use xtask::ticket::{
    Act, CAP, Gh, MERGED, Reach, Successors, Verdict, merged, parent, run, stranded,
};
use xtask::track::Ticket;

fn workspace() -> &'static Path {
    xtask::package_dir().parent().unwrap()
}

/// The manifest of this workspace, whose `deny = true` rules a ticket an
/// agent may take is held to (#153).
fn manifest() -> Manifest {
    Manifest::load(workspace()).unwrap()
}

/// A recorded `gh`: a shell file in a directory of this test's own that
/// answers `issue view <n>` with `issues/<n>.json` and appends every other
/// call's arguments to `calls`, one line each. The instrument stamps its
/// reading over the workspace, which it only reads.
struct Recorded {
    dir: Tree,
}

impl Recorded {
    fn lay(name: &str) -> Recorded {
        let dir = Tree::new(&format!("ticket-{name}")).unwrap();
        fs::create_dir_all(dir.join("issues")).unwrap();
        let recorded = Recorded { dir };
        recorded.answering(None);
        recorded
    }

    /// Lay the `gh` down: `issue view` from the transcript, every other
    /// call written to `calls`, and the verb named, `edit` or `close`,
    /// refused with `gh`'s text after being written down.
    fn answering(&self, refusing: Option<&str>) {
        let refusal = refusing
            .map(|verb| {
                format!(
                    "if [ \"$2\" = {verb} ]; then echo 'GraphQL: no permission' >&2; exit 1; fi\n"
                )
            })
            .unwrap_or_default();
        let script = format!(
            "#!/bin/sh\nif [ \"$1\" = issue ] && [ \"$2\" = view ]; then f='{0}/issues/'\"$3\"; if [ -e \"$f.none\" ]; then echo \"GraphQL: Could not resolve to an issue or pull request with the number of $3. (repository.issue)\" >&2; exit 1; fi; if [ -e \"$f.flaky\" ]; then read a n < \"$f.flaky\"; if [ \"$a\" -gt 0 ]; then echo \"$((a-1)) $n\" > \"$f.flaky\"; elif [ \"$n\" -gt 0 ]; then echo \"0 $((n-1))\" > \"$f.flaky\"; echo \"Post: read: can't assign requested address\" >&2; exit 1; fi; fi; cat \"$f.json\"; exit $?; fi\nprintf '%s\\n' \"$*\" >> '{0}/calls'\n{refusal}exit 0\n",
            self.dir.display()
        );
        fs::write(self.dir.join("gh"), script).unwrap();
        fs::set_permissions(self.dir.join("gh"), fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// `issue view <number>` answers `answered` times, then fails `fails`
    /// times as `gh` does when the machine has run out of ports for a
    /// moment, then answers again (#2804).
    fn flaky(&self, number: u64, answered: u32, fails: u32) {
        let at = self.dir.join(format!("issues/{number}.flaky"));
        fs::write(at, format!("{answered} {fails}\n")).unwrap();
    }

    /// `issue view <number>` answered as the tracker answers a number that
    /// is no issue.
    fn none(&self, number: u64) {
        fs::write(self.dir.join(format!("issues/{number}.none")), "").unwrap();
    }

    fn issue(&self, number: u64, state: &str, body: &str) {
        self.labelled(number, state, body, &[]);
    }

    /// An issue with labels, as the tracker answers them, which a close on
    /// a track holds a successor to (#141).
    fn labelled(&self, number: u64, state: &str, body: &str, labels: &[&str]) {
        self.blocking(number, state, body, labels, &[]);
    }

    /// An issue that already blocks the numbered ones at the tracker, as
    /// `gh issue view --json blocking` nests them, each open.
    fn blocking(&self, number: u64, state: &str, body: &str, labels: &[&str], blocking: &[u64]) {
        self.laid(number, state, body, labels, blocking, &[]);
    }

    /// Any issue: the ones it blocks, each open, and those blocking it,
    /// each in the state given.
    fn laid(
        &self,
        number: u64,
        state: &str,
        body: &str,
        labels: &[&str],
        blocking: &[u64],
        blocked_by: &[(u64, &str)],
    ) {
        let issue = serde_json::json!({
            "number": number,
            "title": format!("ticket {number}"),
            "body": body,
            "state": state,
            "url": format!("https://example.invalid/issues/{number}"),
            "labels": labels.iter().map(|name| serde_json::json!({"name": name})).collect::<Vec<_>>(),
            "blocking": {"nodes": blocking.iter().map(|n| serde_json::json!({"number": n, "state": "OPEN"})).collect::<Vec<_>>()},
            "blockedBy": {"nodes": blocked_by.iter().map(|(n, state)| serde_json::json!({"number": n, "state": state})).collect::<Vec<_>>()},
        });
        fs::write(
            self.dir.join(format!("issues/{number}.json")),
            issue.to_string(),
        )
        .unwrap();
    }

    /// An open issue with `body` blocked at the tracker by the numbered
    /// ones, each in the state given, as `gh issue view --json blockedBy`
    /// nests them.
    fn blocked(&self, number: u64, body: &str, blockers: &[(u64, &str)]) {
        self.laid(number, "OPEN", body, &[], &[], blockers);
    }

    /// A chart sheet as the tracker answers it: an issue holding `total`
    /// sub-issues, the count the cap is measured against (#448).
    fn sheet(&self, number: u64, state: &str, total: u64) {
        let issue = serde_json::json!({
            "number": number,
            "title": format!("Chart: a track at a row, sheet {number}"),
            "body": "## Destination\n\na row\n",
            "state": state,
            "url": format!("https://example.invalid/issues/{number}"),
            "labels": [{"name": "frontier:chart"}],
            "blocking": {"nodes": []},
            "subIssuesSummary": {"total": total, "completed": 0, "percentCompleted": 0},
        });
        fs::write(
            self.dir.join(format!("issues/{number}.json")),
            issue.to_string(),
        )
        .unwrap();
    }

    fn gh(&self) -> Gh {
        Gh::at(&self.dir.join("gh"))
    }

    /// What the tracker was asked besides `issue view`.
    fn calls(&self) -> Vec<String> {
        fs::read_to_string(self.dir.join("calls"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

/// The negative control of `harness.self` at the read (#153): the ticket
/// laid down under `xtask/fixtures/rules/harness.self/`, which carries
/// `ready-for-agent` and asks in two criteria for a write into the rule's
/// scope, one the pin that `xtask steer` writes for an agent and one the
/// hook settings that nothing writes for one. The manifest is this
/// workspace's, so the scope the reading names is the one `permit` denies
/// the write by.
#[test]
fn a_ready_for_agent_ticket_asking_for_a_write_the_manifest_denies_fails_naming_path_and_rule() {
    let body =
        fs::read_to_string(xtask::package_dir().join("fixtures/rules/harness.self/ticket.md"))
            .unwrap();
    let recorded = Recorded::lay("denied");
    recorded.labelled(111, "OPEN", &body, &["ready-for-agent", "track:main"]);
    let reading = run(
        workspace(),
        &manifest(),
        &recorded.gh(),
        Act::Read,
        Ticket::new(111),
        None,
    )
    .unwrap();
    assert_eq!(reading.status, Status::Fail, "{reading:#?}");
    assert_eq!(reading.rule, "harness.self");
    assert_eq!(reading.numbers["denied"], 1.into(), "{reading:#?}");
    assert_eq!(reading.numbers["steerable"], 1.into(), "{reading:#?}");
    assert!(
        reading
            .message
            .contains("`.claude/settings.json` gains the hook")
            && reading.message.contains("asks for .claude/settings.json"),
        "the criterion and the path: {}",
        reading.message
    );
    assert!(
        reading.message.contains("harness.self denies an agent"),
        "the rule: {}",
        reading.message
    );
    assert!(
        reading
            .message
            .contains("gh issue edit 111 --add-label for-a-person"),
        "what a person does with it: {}",
        reading.message
    );
    // The criterion whose object is the pin is said and not refused: the
    // write is an agent's through the instrument sanctioned to make it
    // (#152).
    assert!(
        reading
            .message
            .contains("`cargo xtask steer oracle.toml --from <proposal>`"),
        "the instrument that writes it: {}",
        reading.message
    );
    // The same criteria under the label that says the act is a person's
    // are held to the boxes and to nothing else.
    recorded.labelled(111, "OPEN", &body, &["for-a-person", "track:main"]);
    let person = run(
        workspace(),
        &manifest(),
        &recorded.gh(),
        Act::Read,
        Ticket::new(111),
        None,
    )
    .unwrap();
    assert_eq!(person.numbers["denied"], 0.into(), "{person:#?}");
    assert_eq!(person.numbers["steerable"], 0.into(), "{person:#?}");
    assert!(person.rule.is_empty(), "{person:#?}");
    assert!(
        person.message.contains("0 of 4 criteria ticked"),
        "{}",
        person.message
    );
}

/// The narrowing #152 measured (#153): a ticket the maintainer handed to
/// an agent, every box ticked, whose first criterion's object is the pin.
/// `xtask steer` is the sanctioned writer of that file, so the label is
/// not wrong; the read passes and the message still says what it matched.
#[test]
fn a_criterion_whose_object_is_a_file_xtask_steer_writes_is_said_and_not_refused() {
    let body = "## Acceptance criteria\n\n- [x] `oracle.toml` carries a `[regime.dump]` with the two ini entries above, with a comment that says what the regime is\n";
    let recorded = Recorded::lay("steerable");
    recorded.labelled(
        152,
        "OPEN",
        body,
        &["ready-for-agent", "track:parity-hosts"],
    );
    let reading = run(
        workspace(),
        &manifest(),
        &recorded.gh(),
        Act::Read,
        Ticket::new(152),
        None,
    )
    .unwrap();
    assert_eq!(reading.status, Status::Pass, "{reading:#?}");
    assert_eq!(reading.numbers["denied"], 0.into(), "{reading:#?}");
    assert_eq!(reading.numbers["steerable"], 1.into(), "{reading:#?}");
    assert!(reading.rule.is_empty(), "{reading:#?}");
    assert!(
        reading.message.contains("asks for oracle.toml")
            && reading
                .message
                .contains("`cargo xtask steer oracle.toml --from <proposal>`"),
        "what it matched, and the call that writes it: {}",
        reading.message
    );
    assert!(
        !reading.message.contains("--add-label for-a-person"),
        "the label is not wrong: {}",
        reading.message
    );
}

/// A criterion whose denied subject stays as it is asks for no write
/// (#2391): #630's third box asks that the live numbers still render, and
/// the read passes it, while #111's "`oracle.toml` carries" beside it is
/// still the write, said as the steer that makes it.
#[test]
fn a_criterion_saying_a_denied_path_still_behaves_as_before_asks_for_no_write() {
    let body = "## Acceptance criteria\n\n- [x] `.claude/harness.local.md` still renders this machine's live numbers, and `cargo xtask status --section rules` still prints them\n- [x] `seams.toml` is unchanged\n- [x] `oracle.toml` carries a `[regime.dump]` with the ini above\n";
    let recorded = Recorded::lay("kept");
    recorded.labelled(630, "OPEN", body, &["ready-for-agent"]);
    let reading = run(
        workspace(),
        &manifest(),
        &recorded.gh(),
        Act::Read,
        Ticket::new(630),
        None,
    )
    .unwrap();
    assert_eq!(reading.status, Status::Pass, "{reading:#?}");
    assert_eq!(reading.numbers["denied"], 0.into(), "{reading:#?}");
    assert_eq!(reading.numbers["steerable"], 1.into(), "{reading:#?}");
    assert!(
        reading.message.contains("asks for oracle.toml")
            && !reading.message.contains("harness.local.md")
            && !reading.message.contains("asks for seams.toml"),
        "{}",
        reading.message
    );
}

const UNTICKED: &str =
    "## What to build\n\nthings\n\n## Acceptance criteria\n\n- [x] one\n- [ ] two\n- [ ] three\n";
const TICKED: &str = "## Acceptance criteria\n\n- [x] one\n- [X] two\n";

#[test]
fn read_counts_the_boxes_and_names_the_unticked() {
    let recorded = Recorded::lay("read");
    recorded.issue(7, "OPEN", UNTICKED);
    let reading = run(
        workspace(),
        &manifest(),
        &recorded.gh(),
        Act::Read,
        Ticket::new(7),
        None,
    )
    .unwrap();
    assert_eq!(reading.instrument, "ticket");
    assert_eq!(reading.scope, "7");
    assert_eq!(reading.status, Status::Fail);
    assert_eq!(reading.numbers["criteria"], 3.into());
    assert_eq!(reading.numbers["ticked"], 1.into());
    assert_eq!(reading.numbers["unticked"], 2.into());
    assert!(
        reading.message.contains("(1) two; (2) three"),
        "{}",
        reading.message
    );
    assert!(!reading.key().is_empty(), "stamped over the workspace");
    assert!(
        recorded.calls().is_empty(),
        "read asks the tracker nothing else"
    );

    recorded.issue(8, "OPEN", TICKED);
    let reading = run(
        workspace(),
        &manifest(),
        &recorded.gh(),
        Act::Read,
        Ticket::new(8),
        None,
    )
    .unwrap();
    assert_eq!(reading.status, Status::Pass);
    assert_eq!(reading.numbers["unticked"], 0.into());
    assert!(recorded.calls().is_empty());
}

/// A close of work done at a commit `main` contains, naming `successors`.
fn done(successors: Successors) -> Act {
    Act::Close(Verdict::Done, successors, Reach::Landed)
}

/// `git` in `root` under a fixed name, its stdout trimmed.
fn git(root: &Path, args: &[&str]) -> String {
    let out = Command::new("git")
        .args([
            "-c",
            "user.name=x",
            "-c",
            "user.email=x@x",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .current_dir(root)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// A repository of this test's own on `main` with one empty commit.
fn repository(name: &str) -> Tree {
    let root = Tree::new(name).unwrap();
    git(&root, &["init", "-q", "-b", "main"]);
    git(&root, &["commit", "-q", "--allow-empty", "-m", "base"]);
    root
}

/// `ticket ready` holds the boxes and the successors as a close does,
/// writes a ready record of the ticket on the branch and closes nothing
/// (#2311); a second ticket readied on the same branch keeps the first's
/// record, and readying a ticket again replaces its own (#2358).
#[test]
fn ready_records_each_ticket_of_the_branch_with_its_successors_and_closes_nothing() {
    let root = repository("ticket-ready");
    git(&root, &["checkout", "-q", "-b", "t7"]);
    let recorded = Recorded::lay("ready");
    recorded.issue(7, "OPEN", UNTICKED);
    recorded.issue(8, "OPEN", TICKED);
    recorded.issue(9, "OPEN", TICKED);
    let ready = |ticket: u64, successors: Successors| {
        run(
            &root,
            &manifest(),
            &recorded.gh(),
            Act::Ready(successors),
            Ticket::new(ticket),
            None,
        )
        .unwrap()
    };
    let opens_nine = || Successors::Tickets(vec![Ticket::new(9)]);
    let refused = ready(7, opens_nine());
    assert_eq!(refused.status, Status::Fail, "{}", refused.message);
    assert!(
        refused.message.ends_with("not ready, nothing recorded"),
        "{}",
        refused.message
    );

    recorded.issue(7, "OPEN", TICKED);
    let reading = ready(7, Successors::Nothing);
    assert_eq!(reading.status, Status::Pass, "{}", reading.message);
    assert!(
        reading
            .message
            .contains("recorded as branch.t7.ready-7; the merge into main closes it"),
        "{}",
        reading.message
    );
    let second = ready(8, Successors::Nothing);
    assert_eq!(second.status, Status::Pass, "{}", second.message);
    let again = ready(7, opens_nine());
    assert_eq!(again.status, Status::Pass, "{}", again.message);
    assert_eq!(git(&root, &["config", "branch.t7.ready-7"]), "9");
    assert_eq!(git(&root, &["config", "branch.t7.ready-8"]), "nothing");
    assert!(recorded.calls().is_empty(), "{:?}", recorded.calls());
}

/// The merge into `main` closes every open ticket its subject names with
/// the successors that ticket's ready record names (#2311), a bundle's
/// three tickets readied on one branch all three (#2358); one with no
/// record fails naming it, is left open and is what `done` names after
/// the merge, one closed already is passed over, and off `main` nothing
/// is closed.
#[test]
fn a_merge_into_main_closes_every_ready_ticket_its_subject_names() {
    let root = repository("ticket-merged");
    git(&root, &["checkout", "-q", "-b", "t7"]);
    git(
        &root,
        &["commit", "-q", "--allow-empty", "-m", "the bundle"],
    );
    git(&root, &["config", "branch.t7.ticket", "7"]);
    for (n, successors) in [("7", "nothing"), ("8", "12"), ("9", "nothing")] {
        git(
            &root,
            &["config", &format!("branch.t7.ready-{n}"), successors],
        );
    }
    let recorded = Recorded::lay("merged");
    for n in [7, 8, 9, 11, 12] {
        recorded.issue(n, "OPEN", TICKED);
    }
    recorded.issue(10, "CLOSED", TICKED);
    assert!(
        merged(&root, &manifest(), &recorded.gh(), "main")
            .unwrap()
            .is_empty(),
        "off main nothing is closed"
    );

    git(&root, &["checkout", "-q", "main"]);
    git(
        &root,
        &[
            "merge",
            "-q",
            "--no-ff",
            "t7",
            "-m",
            "Merge t7: the bundle (#7, #8, #9, #10, #11)",
        ],
    );
    let (readings, phases) = tickets_merged(&root, &recorded);
    let verdicts: Vec<(&str, Status)> = readings
        .iter()
        .map(|r| (r.scope.as_str(), r.status))
        .collect();
    assert_eq!(
        verdicts,
        [
            ("7", Status::Pass),
            ("8", Status::Pass),
            ("9", Status::Pass),
            ("11", Status::Fail)
        ],
        "{readings:#?}"
    );
    assert!(
        readings[3]
            .message
            .contains("#11 is named by a merge into main and no branch has a ready record of it"),
        "{}",
        readings[3].message
    );
    phases_named(&phases);
    let calls = recorded.calls();
    for n in [7, 8, 9] {
        assert!(
            calls
                .iter()
                .any(|c| c.starts_with(&format!("issue close {n} "))),
            "#{n} not closed: {calls:?}"
        );
    }
    assert!(
        !calls
            .iter()
            .any(|c| c.starts_with("issue close 11") || c.starts_with("issue close 10")),
        "{calls:?}"
    );

    // What `done` says on `main` after the merge: the ticket the merge
    // named and did not close, until a close replaces its reading.
    let dir = Tree::new("ticket-merged-store").unwrap();
    let store = Store::at(dir.to_path_buf());
    store.record(&readings).unwrap();
    let head = git(&root, &["rev-parse", "HEAD"]);
    let unmet = stranded(&root, &store, &head).unwrap();
    assert_eq!(unmet.len(), 1, "{unmet:?}");
    assert!(
        unmet[0].contains("#11 is named by a merge into main"),
        "{unmet:?}"
    );
    let closed = run(
        &root,
        &manifest(),
        &recorded.gh(),
        done(Successors::Nothing),
        Ticket::new(11),
        None,
    )
    .unwrap();
    assert_eq!(closed.status, Status::Pass, "{}", closed.message);
    store.record(&[closed]).unwrap();
    assert!(stranded(&root, &store, &head).unwrap().is_empty());
}

/// The run's reading of the merge of #7 to #11 names each phase with its
/// time (#2813): every close, the tracker by verb, each ticket read once
/// but #11, which as unready is read again after the closes, and #12 as
/// #8's successor, and the run whole.
fn phases_named(phases: &Reading) {
    for said in [
        "name 5 ticket(s), 3 closed here",
        "closed #7 in ",
        "closed #9 in ",
        "gh close 3 time(s) in ",
        "gh edit 1 time(s) in ",
        "gh view 7 time(s) in ",
        "the workspace read in ",
        " s in all",
    ] {
        assert!(
            phases.message.contains(said),
            "{said:?}: {}",
            phases.message
        );
    }
    assert_eq!(phases.numbers["closed"], 3.into());
    assert_eq!(phases.numbers["gh_calls"], 11.into());
    for key in ["gh_ms", "workspace_ms", "ms"] {
        assert!(phases.numbers.contains_key(key), "{key}: {phases:#?}");
    }
}

/// A merge that lands part of a ticket (#2394): the ticket it names with
/// no ready record has a box unticked and stands blocked by an open one,
/// as the build skill leaves a ticket whose last box waits on another, so
/// the merge skips its close, says it is left open and what holds it,
/// closes nothing, and `done` does not name it. A ticket named with no
/// record whose blockers are all closed, and one every box of which is
/// ticked, are still the merge's refusal, named by `done` as before.
#[test]
fn a_merge_landing_part_of_a_blocked_ticket_leaves_it_open_and_done_does_not_name_it() {
    let root = repository("ticket-partial");
    git(&root, &["checkout", "-q", "-b", "t13"]);
    git(&root, &["commit", "-q", "--allow-empty", "-m", "part"]);
    git(&root, &["checkout", "-q", "main"]);
    git(
        &root,
        &[
            "merge",
            "-q",
            "--no-ff",
            "t13",
            "-m",
            "Merge t13: part of the port (#13, #15, #17)",
        ],
    );
    let recorded = Recorded::lay("partial");
    recorded.blocked(13, UNTICKED, &[(14, "OPEN"), (16, "CLOSED")]);
    recorded.blocked(15, UNTICKED, &[(16, "CLOSED")]);
    recorded.blocked(17, TICKED, &[(14, "OPEN")]);
    let readings = tickets_merged(&root, &recorded).0;
    let verdicts: Vec<(&str, Status)> = readings
        .iter()
        .map(|r| (r.scope.as_str(), r.status))
        .collect();
    assert_eq!(
        verdicts,
        [
            ("13", Status::Skip),
            ("15", Status::Fail),
            ("17", Status::Fail)
        ],
        "{readings:#?}"
    );
    assert!(
        readings[0].message.contains("waits on #14;")
            && readings[0].message.contains("left open")
            && !readings[0].message.contains("#16"),
        "{}",
        readings[0].message
    );
    let parent = git(&root, &["rev-parse", "--short", "HEAD^1"]);
    assert!(
        readings[1]
            .message
            .contains("#15 is named by a merge into main and no branch has a ready record of it")
            && readings[1]
                .message
                .contains(&format!("xtask ticket merged --since {parent}")),
        "{}",
        readings[1].message
    );
    assert!(recorded.calls().is_empty(), "{:?}", recorded.calls());

    let dir = Tree::new("ticket-partial-store").unwrap();
    let store = Store::at(dir.to_path_buf());
    store.record(&readings).unwrap();
    let head = git(&root, &["rev-parse", "HEAD"]);
    let unmet = stranded(&root, &store, &head).unwrap();
    assert_eq!(unmet.len(), 2, "{unmet:?}");
    assert!(
        unmet[0].contains("#15 is named") && unmet[1].contains("#17 is named"),
        "{unmet:?}"
    );
}

/// [`merged`] on `main` over the merge at `HEAD`: the readings of the
/// tickets, and the run's own, the last, set apart once it is held to be
/// the run's (#2813).
fn tickets_merged(root: &Path, recorded: &Recorded) -> (Vec<Reading>, Reading) {
    let mut readings = merged(root, &manifest(), &recorded.gh(), "HEAD^1").unwrap();
    let run = readings.pop().expect("the run's own reading");
    assert_eq!(
        (run.scope.as_str(), run.status),
        (MERGED, Status::Pass),
        "{run:#?}"
    );
    (readings, run)
}

/// A merge of `branch` into `main` under `subject`, each ticket of
/// `ready` readied on the branch with the successors it names, every
/// ticket and successor open with its boxes ticked at `recorded`.
fn merged_with_ready(
    name: &str,
    branch: &str,
    subject: &str,
    ready: &[(u64, &str)],
) -> (Tree, Recorded) {
    let root = repository(name);
    git(&root, &["checkout", "-q", "-b", branch]);
    git(&root, &["commit", "-q", "--allow-empty", "-m", "the work"]);
    let recorded = Recorded::lay(name);
    for &(n, successors) in ready {
        git(
            &root,
            &["config", &format!("branch.{branch}.ready-{n}"), successors],
        );
        recorded.issue(n, "OPEN", TICKED);
        for successor in successors.split(' ').filter_map(|s| s.parse().ok()) {
            recorded.issue(successor, "OPEN", TICKED);
        }
    }
    git(&root, &["checkout", "-q", "main"]);
    git(&root, &["merge", "-q", "--no-ff", branch, "-m", subject]);
    (root, recorded)
}

/// The two merges post-commit left half closed (#2804), replayed with the
/// fault that left them so: under load `gh` fails a read for a moment
/// ("can't assign requested address", the ports run out). In fb2e2b1c
/// that passed #2536 over as no issue at all, its first read failing; in
/// 86edf384, #2631's read in the close failing after #2754 closed took the
/// whole run down before one reading was written, so #2631, #2156 and
/// #2621 stayed open and #2754's close went unrecorded. The close reads
/// no ticket again since #2813, so #2631's one read, after #2754 closed,
/// is the one that fails here. A read is asked again, so every ticket the
/// merge names closes, each with its own successors.
#[test]
fn a_merge_whose_tracker_fails_a_read_for_a_moment_still_closes_every_ticket_it_names() {
    for (branch, subject, ready, (flaky, answered)) in [
        (
            "t2754",
            "Merge t2754: a user call enters and leaves inside the loop (#2754, #2631, #2156, #2621)",
            &[
                (2754, "2801"),
                (2631, "nothing"),
                (2156, "2802"),
                (2621, "nothing"),
            ][..],
            (2631, 0),
        ),
        (
            "t2536",
            "Merge t2536: a resource's destructor is served as a release step (#2536)",
            &[(2536, "2654")][..],
            (2536, 0),
        ),
    ] {
        let (root, recorded) =
            merged_with_ready(&format!("replay-{branch}"), branch, subject, ready);
        recorded.flaky(flaky, answered, 1);
        let readings = tickets_merged(&root, &recorded).0;
        let closed: Vec<(&str, Status)> = readings
            .iter()
            .map(|r| (r.scope.as_str(), r.status))
            .collect();
        let named: Vec<(String, Status)> = ready
            .iter()
            .map(|(n, _)| (n.to_string(), Status::Pass))
            .collect();
        assert_eq!(
            closed,
            named
                .iter()
                .map(|(n, s)| (n.as_str(), *s))
                .collect::<Vec<_>>(),
            "{readings:#?}"
        );
        let calls = recorded.calls();
        for (n, _) in ready {
            assert!(
                calls
                    .iter()
                    .any(|c| c.starts_with(&format!("issue close {n} "))),
                "#{n} not closed: {calls:?}"
            );
        }
    }
}

/// A ticket the tracker will not answer for, every read asked again, is
/// not passed over and does not take the run down (#2804): the tickets
/// around it close, each close is recorded, and it is a failed reading
/// saying what to run again, which `done` names until the run, asked
/// again, closes it. A number the tracker answers is no issue at all is
/// passed over, as before.
#[test]
fn a_ticket_the_tracker_cannot_read_is_named_and_the_rest_of_the_merge_closes() {
    let (root, recorded) = merged_with_ready(
        "unreadable",
        "t20",
        "Merge t20: the bundle (#20, #21, #22, #23)",
        &[(20, "nothing"), (21, "nothing"), (22, "nothing")],
    );
    recorded.none(23);
    recorded.flaky(21, 0, 3);
    let readings = tickets_merged(&root, &recorded).0;
    let verdicts: Vec<(&str, Status)> = readings
        .iter()
        .map(|r| (r.scope.as_str(), r.status))
        .collect();
    assert_eq!(
        verdicts,
        [
            ("20", Status::Pass),
            ("21", Status::Fail),
            ("22", Status::Pass)
        ],
        "{readings:#?}"
    );
    let parent = git(&root, &["rev-parse", "--short", "HEAD^1"]);
    assert!(
        readings[1]
            .message
            .contains("#21 could not be read from the tracker")
            && readings[1]
                .message
                .contains(&format!("xtask ticket merged --since {parent}")),
        "{}",
        readings[1].message
    );

    let dir = Tree::new("ticket-unreadable-store").unwrap();
    let store = Store::at(dir.to_path_buf());
    store.record(&readings).unwrap();
    let head = git(&root, &["rev-parse", "HEAD"]);
    let unmet = stranded(&root, &store, &head).unwrap();
    assert_eq!(unmet.len(), 1, "{unmet:?}");
    assert!(unmet[0].contains("#21 could not be read"), "{unmet:?}");

    recorded.issue(20, "CLOSED", TICKED);
    recorded.issue(22, "CLOSED", TICKED);
    let again = tickets_merged(&root, &recorded).0;
    assert_eq!(again.len(), 1, "{again:#?}");
    assert_eq!(
        (again[0].scope.as_str(), again[0].status),
        ("21", Status::Pass)
    );
    store.record(&again).unwrap();
    assert!(stranded(&root, &store, &head).unwrap().is_empty());
}

/// Where a close's commit stands against `main` (#2249): `main`'s own
/// tip is landed, a branch's commit `main` does not contain is unmerged
/// and named with its branch, and the same commit is landed once `main`
/// has merged it.
#[test]
fn a_commit_is_landed_once_main_contains_it_and_unmerged_before() {
    let root = Tree::new("ticket-reach").unwrap();
    git(&root, &["init", "-q", "-b", "main"]);
    git(&root, &["commit", "-q", "--allow-empty", "-m", "base"]);
    assert_eq!(Reach::of(&root).unwrap(), Reach::Landed);

    git(&root, &["checkout", "-q", "-b", "t7"]);
    git(&root, &["commit", "-q", "--allow-empty", "-m", "the work"]);
    let tip = git(&root, &["rev-parse", "--short=7", "HEAD"]);
    assert_eq!(
        Reach::of(&root).unwrap(),
        Reach::Unmerged {
            commit: tip,
            branch: String::from("t7"),
        }
    );

    git(&root, &["checkout", "-q", "main"]);
    git(&root, &["merge", "-q", "--no-ff", "--no-edit", "t7"]);
    git(&root, &["checkout", "-q", "t7"]);
    assert_eq!(Reach::of(&root).unwrap(), Reach::Landed);
}

/// A close of work done at a commit `main` does not contain is refused
/// with every box ticked, naming the commit and the branch and the two
/// ways on, and asks the tracker nothing (#2249); a withdrawal there is
/// not held to it, since it says nothing of the work is in the tree.
#[test]
fn close_refuses_a_commit_main_does_not_contain_and_changes_nothing_at_the_tracker() {
    let recorded = Recorded::lay("unmerged");
    recorded.issue(7, "OPEN", TICKED);
    let unmerged = Reach::Unmerged {
        commit: String::from("6e20eaa"),
        branch: String::from("t2166"),
    };
    let reading = run(
        workspace(),
        &manifest(),
        &recorded.gh(),
        Act::Close(Verdict::Done, Successors::Nothing, unmerged.clone()),
        Ticket::new(7),
        None,
    )
    .unwrap();
    assert_eq!(reading.status, Status::Fail, "{}", reading.message);
    assert!(
        reading
            .message
            .contains("6e20eaa on t2166 is not in main: merge it first, or close with --withdrawn"),
        "{}",
        reading.message
    );
    assert!(
        reading
            .message
            .ends_with("not closed, nothing changed at the tracker"),
        "{}",
        reading.message
    );
    assert!(recorded.calls().is_empty(), "{:?}", recorded.calls());

    recorded.issue(8, "OPEN", "## Acceptance criteria\n\n- [ ] one\n");
    let reading = run(
        workspace(),
        &manifest(),
        &recorded.gh(),
        Act::Close(
            Verdict::Withdrawn(String::from("measured: nothing of it is in the tree")),
            Successors::Nothing,
            unmerged,
        ),
        Ticket::new(8),
        None,
    )
    .unwrap();
    assert_eq!(reading.status, Status::Pass, "{}", reading.message);
    assert_eq!(recorded.calls().len(), 1, "{:?}", recorded.calls());
}

#[test]
fn close_refuses_while_a_box_is_unticked_and_changes_nothing_at_the_tracker() {
    let recorded = Recorded::lay("refuse");
    recorded.issue(7, "OPEN", UNTICKED);
    let reading = run(
        workspace(),
        &manifest(),
        &recorded.gh(),
        done(Successors::Nothing),
        Ticket::new(7),
        Some("s1"),
    )
    .unwrap();
    assert_eq!(reading.status, Status::Fail);
    assert!(
        reading.message.contains("not closed") && reading.message.contains("two"),
        "{}",
        reading.message
    );
    assert_eq!(reading.session, "s1");
    assert!(recorded.calls().is_empty(), "{:?}", recorded.calls());

    recorded.issue(9, "OPEN", "## Acceptance criteria\n\nnone yet\n");
    let reading = run(
        workspace(),
        &manifest(),
        &recorded.gh(),
        done(Successors::Nothing),
        Ticket::new(9),
        None,
    )
    .unwrap();
    assert_eq!(reading.status, Status::Fail);
    assert_eq!(reading.numbers["criteria"], 0.into());
    assert!(
        recorded.calls().is_empty(),
        "a ticket with no box is not closed"
    );

    recorded.issue(10, "CLOSED", TICKED);
    let reading = run(
        workspace(),
        &manifest(),
        &recorded.gh(),
        done(Successors::Nothing),
        Ticket::new(10),
        None,
    )
    .unwrap();
    assert_eq!(reading.status, Status::Fail);
    assert!(
        reading.message.contains("closed already"),
        "{}",
        reading.message
    );
    assert!(
        recorded.calls().is_empty(),
        "a closed ticket is not closed twice"
    );
}

#[test]
fn close_with_every_box_ticked_closes_and_records_what_closed_it_and_the_session() {
    let recorded = Recorded::lay("close");
    recorded.issue(8, "OPEN", TICKED);
    let reading = run(
        workspace(),
        &manifest(),
        &recorded.gh(),
        done(Successors::Nothing),
        Ticket::new(8),
        Some("session-abc"),
    )
    .unwrap();
    assert_eq!(reading.status, Status::Pass, "{}", reading.message);
    assert_eq!(reading.numbers["criteria"], 2.into());
    assert_eq!(reading.session, "session-abc");
    assert!(!reading.commit.is_empty());
    assert!(
        reading.message.contains(&reading.commit) && reading.message.contains("session-abc"),
        "{}",
        reading.message
    );
    let calls = recorded.calls();
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert!(
        calls[0].starts_with("issue close 8 --comment ") && calls[0].contains(&reading.commit),
        "{calls:?}"
    );
}

/// The successors are fetched, each must be open, and the edge is the
/// tracker's own blocking relation set before the close; the reading
/// carries them by number (#29). Whose a successor is is not held:
/// one `for-a-person`, one `ready-for-agent`.
#[test]
fn close_names_what_it_opened_through_the_tracker_s_blocking_relation() {
    let recorded = Recorded::lay("opens");
    recorded.issue(8, "OPEN", TICKED);
    recorded.labelled(
        9,
        "OPEN",
        "## Acceptance criteria\n\n- [ ] later\n",
        &["for-a-person"],
    );
    recorded.labelled(10, "OPEN", "no criteria yet", &["ready-for-agent"]);
    let reading = run(
        workspace(),
        &manifest(),
        &recorded.gh(),
        done(Successors::Tickets(vec![Ticket::new(9), Ticket::new(10)])),
        Ticket::new(8),
        Some("s2"),
    )
    .unwrap();
    assert_eq!(reading.status, Status::Pass, "{}", reading.message);
    assert_eq!(reading.numbers["opens"], 2.into());
    assert!(
        reading.message.contains("opens #9, #10"),
        "{}",
        reading.message
    );
    let calls = recorded.calls();
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert_eq!(calls[0], "issue edit 8 --add-blocking 9,10");
    assert!(
        calls[1].starts_with("issue close 8 --comment ") && calls[1].contains("opens #9, #10"),
        "the comment names them too: {calls:?}"
    );
}

/// An edge the tracker already has is not set again: the tracker refuses
/// a second `--add-blocking` of the same ticket, and a close retried after
/// one that set the edge and failed later would otherwise never land.
/// Only the missing edge is asked for; with none missing, none is.
#[test]
fn close_sets_only_the_edges_the_tracker_does_not_have_yet() {
    let recorded = Recorded::lay("edges");
    recorded.blocking(8, "OPEN", TICKED, &[], &[9]);
    recorded.issue(9, "OPEN", "no criteria yet");
    recorded.issue(10, "OPEN", "no criteria yet");
    let reading = run(
        workspace(),
        &manifest(),
        &recorded.gh(),
        done(Successors::Tickets(vec![Ticket::new(9), Ticket::new(10)])),
        Ticket::new(8),
        None,
    )
    .unwrap();
    assert_eq!(reading.status, Status::Pass, "{}", reading.message);
    let calls = recorded.calls();
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert_eq!(calls[0], "issue edit 8 --add-blocking 10");
    assert!(calls[1].starts_with("issue close 8 "), "{calls:?}");

    let recorded = Recorded::lay("edges-all");
    recorded.blocking(8, "OPEN", TICKED, &[], &[9]);
    recorded.issue(9, "OPEN", "no criteria yet");
    let reading = run(
        workspace(),
        &manifest(),
        &recorded.gh(),
        done(Successors::Tickets(vec![Ticket::new(9)])),
        Ticket::new(8),
        None,
    )
    .unwrap();
    assert_eq!(reading.status, Status::Pass, "{}", reading.message);
    let calls = recorded.calls();
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert!(calls[0].starts_with("issue close 8 "), "{calls:?}");
}

/// The close of a ticket labelled `track:<name>` refuses a successor that
/// carries no such label, which would be `main`'s (ADR-0009), names the
/// edit that adds it, and asks the tracker for nothing but the two `issue
/// view` calls (#141). The track is the closed ticket's own label, not
/// where the close runs from.
#[test]
fn closing_a_tracks_ticket_refuses_a_successor_that_carries_no_track_label() {
    let recorded = Recorded::lay("unlabelled");
    recorded.labelled(8, "OPEN", TICKED, &["track:parity-hosts"]);
    recorded.labelled(9, "OPEN", "no criteria yet", &["ready-for-agent"]);
    let reading = run(
        workspace(),
        &manifest(),
        &recorded.gh(),
        done(Successors::Tickets(vec![Ticket::new(9)])),
        Ticket::new(8),
        None,
    )
    .unwrap();
    assert_eq!(reading.status, Status::Fail, "{}", reading.message);
    assert!(
        reading
            .message
            .contains("opens #9, which carries no `track:parity-hosts` label")
            && reading
                .message
                .contains("`gh issue edit 9 --add-label track:parity-hosts`")
            && reading
                .message
                .contains("not closed, nothing changed at the tracker"),
        "{}",
        reading.message
    );
    assert!(
        recorded.calls().is_empty(),
        "no edge and no close: {:?}",
        recorded.calls()
    );

    // Another track's label is no more this track's than none at all.
    recorded.labelled(9, "OPEN", "no criteria yet", &["track:main"]);
    let reading = run(
        workspace(),
        &manifest(),
        &recorded.gh(),
        done(Successors::Tickets(vec![Ticket::new(9)])),
        Ticket::new(8),
        None,
    )
    .unwrap();
    assert_eq!(reading.status, Status::Fail, "{}", reading.message);
    assert!(recorded.calls().is_empty(), "{:?}", recorded.calls());
}

/// The successor carrying the closed ticket's track label closes as it
/// always did, and the close of one of `main`'s tickets, unlabelled or
/// labelled `track:main`, asks nothing of a successor's labels, since
/// work on `main` may file for any track (#141).
#[test]
fn a_successor_carrying_the_track_label_closes_and_main_asks_nothing() {
    let recorded = Recorded::lay("labelled");
    recorded.labelled(8, "OPEN", TICKED, &["track:parity-hosts"]);
    recorded.labelled(9, "OPEN", "no criteria yet", &["track:parity-hosts"]);
    let reading = run(
        workspace(),
        &manifest(),
        &recorded.gh(),
        done(Successors::Tickets(vec![Ticket::new(9)])),
        Ticket::new(8),
        None,
    )
    .unwrap();
    assert_eq!(reading.status, Status::Pass, "{}", reading.message);
    let calls = recorded.calls();
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert_eq!(calls[0], "issue edit 8 --add-blocking 9");
    assert!(calls[1].starts_with("issue close 8 "), "{calls:?}");

    let recorded = Recorded::lay("unlabelled-main");
    recorded.issue(8, "OPEN", TICKED);
    recorded.issue(9, "OPEN", "no criteria yet");
    let reading = run(
        workspace(),
        &manifest(),
        &recorded.gh(),
        done(Successors::Tickets(vec![Ticket::new(9)])),
        Ticket::new(8),
        None,
    )
    .unwrap();
    assert_eq!(reading.status, Status::Pass, "{}", reading.message);
    assert_eq!(recorded.calls().len(), 2, "{:?}", recorded.calls());

    // A ticket on two tracks holds a successor to share either of them.
    let recorded = Recorded::lay("labelled-two");
    recorded.labelled(
        8,
        "OPEN",
        TICKED,
        &["track:parity-hosts", "track:parity-streams"],
    );
    recorded.labelled(9, "OPEN", "no criteria yet", &["track:parity-streams"]);
    let reading = run(
        workspace(),
        &manifest(),
        &recorded.gh(),
        done(Successors::Tickets(vec![Ticket::new(9)])),
        Ticket::new(8),
        None,
    )
    .unwrap();
    assert_eq!(reading.status, Status::Pass, "{}", reading.message);
    let recorded = Recorded::lay("labelled-two-none");
    recorded.labelled(
        8,
        "OPEN",
        TICKED,
        &["track:parity-hosts", "track:parity-streams"],
    );
    recorded.labelled(9, "OPEN", "no criteria yet", &["ready-for-agent"]);
    let reading = run(
        workspace(),
        &manifest(),
        &recorded.gh(),
        done(Successors::Tickets(vec![Ticket::new(9)])),
        Ticket::new(8),
        None,
    )
    .unwrap();
    assert_eq!(reading.status, Status::Fail, "{}", reading.message);
    assert!(
        reading
            .message
            .contains("which carries no `track:parity-hosts` or `track:parity-streams` label")
            && reading
                .message
                .contains("`gh issue edit 9 --add-label track:parity-hosts`"),
        "{}",
        reading.message
    );
    assert!(recorded.calls().is_empty(), "{:?}", recorded.calls());

    let recorded = Recorded::lay("labelled-main");
    recorded.labelled(8, "OPEN", TICKED, &["track:main"]);
    recorded.labelled(9, "OPEN", "no criteria yet", &["track:parity-hosts"]);
    let reading = run(
        workspace(),
        &manifest(),
        &recorded.gh(),
        done(Successors::Tickets(vec![Ticket::new(9)])),
        Ticket::new(8),
        None,
    )
    .unwrap();
    assert_eq!(reading.status, Status::Pass, "{}", reading.message);
    assert_eq!(recorded.calls().len(), 2, "{:?}", recorded.calls());
}

/// `--opens-nothing` is an assertion: no edge, the close alone, and the
/// reading says who asserted it (#29).
#[test]
fn opens_nothing_is_an_assertion_the_reading_carries_with_its_session() {
    let recorded = Recorded::lay("nothing");
    recorded.issue(8, "OPEN", TICKED);
    let reading = run(
        workspace(),
        &manifest(),
        &recorded.gh(),
        done(Successors::Nothing),
        Ticket::new(8),
        Some("s3"),
    )
    .unwrap();
    assert_eq!(reading.status, Status::Pass, "{}", reading.message);
    assert_eq!(reading.numbers["opens"], 0.into());
    assert_eq!(reading.session, "s3");
    assert!(
        reading
            .message
            .contains("opens nothing, asserted in session s3"),
        "{}",
        reading.message
    );
    let calls = recorded.calls();
    assert_eq!(calls.len(), 1, "no edge for nothing: {calls:?}");
    assert!(
        calls[0].starts_with("issue close 8 --comment ") && calls[0].contains("opens nothing"),
        "{calls:?}"
    );

    let unnamed = run(
        workspace(),
        &manifest(),
        &recorded.gh(),
        done(Successors::Nothing),
        Ticket::new(8),
        None,
    )
    .unwrap();
    assert!(
        unnamed
            .message
            .contains("opens nothing, asserted with no session named"),
        "{}",
        unnamed.message
    );
}

/// A successor that is closed, missing or unreadable refuses the close,
/// and the tracker is asked nothing but `issue view` (#29).
#[test]
fn close_refuses_a_successor_that_is_closed_missing_or_unreadable_and_changes_nothing() {
    let recorded = Recorded::lay("successor");
    recorded.issue(8, "OPEN", TICKED);
    recorded.issue(9, "CLOSED", "done already");
    fs::write(recorded.dir.join("issues/11.json"), "not an issue").unwrap();
    for (successor, expected) in [
        (9, "#9, which is closed"),
        (404, "#404 could not be read from the tracker"),
        (11, "#11: gh's answer is not an issue"),
    ] {
        let reading = run(
            workspace(),
            &manifest(),
            &recorded.gh(),
            done(Successors::Tickets(vec![Ticket::new(successor)])),
            Ticket::new(8),
            None,
        )
        .unwrap();
        assert_eq!(reading.status, Status::Fail, "{}", reading.message);
        assert!(
            reading.message.contains(expected)
                && reading
                    .message
                    .contains("not closed, nothing changed at the tracker"),
            "{}",
            reading.message
        );
        assert_eq!(
            reading.numbers["opens"],
            1.into(),
            "the call is recorded as made"
        );
    }
    assert!(recorded.calls().is_empty(), "{:?}", recorded.calls());
}

/// The edge is set before the close: a tracker that refuses the edge
/// leaves the ticket open and unchanged (#29).
#[test]
fn a_tracker_that_refuses_the_edge_leaves_the_ticket_open() {
    let recorded = Recorded::lay("edge");
    recorded.issue(8, "OPEN", TICKED);
    recorded.issue(9, "OPEN", "later");
    recorded.answering(Some("edit"));
    let reading = run(
        workspace(),
        &manifest(),
        &recorded.gh(),
        done(Successors::Tickets(vec![Ticket::new(9)])),
        Ticket::new(8),
        None,
    )
    .unwrap();
    assert_eq!(reading.status, Status::Fail);
    assert!(
        reading.message.contains("no permission")
            && reading
                .message
                .contains("not closed, nothing changed at the tracker"),
        "{}",
        reading.message
    );
    assert_eq!(
        recorded.calls(),
        ["issue edit 8 --add-blocking 9"],
        "the edge was asked and refused; the close was not asked"
    );

    // A `Tickets` built empty by hand names nothing and is refused the
    // same way, before the tracker is asked.
    let empty = run(
        workspace(),
        &manifest(),
        &recorded.gh(),
        done(Successors::Tickets(vec![])),
        Ticket::new(8),
        None,
    )
    .unwrap();
    assert_eq!(empty.status, Status::Fail);
    assert!(
        empty.message.contains("opens no ticket") && empty.message.contains("nothing changed"),
        "{}",
        empty.message
    );
    assert_eq!(recorded.calls().len(), 1, "{:?}", recorded.calls());
}

/// The edge is set and then the close is refused: the reading says the
/// edge stands, since the tracker did change (#29).
#[test]
fn a_tracker_that_refuses_the_close_after_the_edge_says_the_edge_stands() {
    let recorded = Recorded::lay("edge-stands");
    recorded.issue(8, "OPEN", TICKED);
    recorded.issue(9, "OPEN", "later");
    recorded.answering(Some("close"));
    let reading = run(
        workspace(),
        &manifest(),
        &recorded.gh(),
        done(Successors::Tickets(vec![Ticket::new(9)])),
        Ticket::new(8),
        None,
    )
    .unwrap();
    assert_eq!(reading.status, Status::Fail);
    assert!(
        reading
            .message
            .contains("opens #9; the edge to what it opened stands")
            && reading.message.contains("no permission")
            && !reading.message.contains("nothing changed"),
        "{}",
        reading.message
    );
    let calls = recorded.calls();
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert_eq!(calls[0], "issue edit 8 --add-blocking 9");
}

#[test]
fn a_tracker_that_refuses_the_close_is_a_failed_reading_carrying_its_text() {
    let recorded = Recorded::lay("refused");
    recorded.issue(8, "OPEN", TICKED);
    recorded.answering(Some("close"));
    let reading = run(
        workspace(),
        &manifest(),
        &recorded.gh(),
        done(Successors::Nothing),
        Ticket::new(8),
        None,
    )
    .unwrap();
    assert_eq!(reading.status, Status::Fail);
    assert!(
        reading.message.contains("no permission"),
        "{}",
        reading.message
    );

    let missing = run(
        workspace(),
        &manifest(),
        &recorded.gh(),
        Act::Read,
        Ticket::new(404),
        None,
    );
    assert!(
        missing.is_err(),
        "an issue the tracker has not is an error, not a reading"
    );
}

/// A ticket withdrawn rather than done (#185): #167's shape, boxes all
/// unticked because there is nothing in the tree to build, closed under
/// `--withdrawn` with the measurement as the reason. The reading passes,
/// counts `withdrawn` so `status --section ticket` tells it from a close
/// of work, carries the reason and the session, and the closing comment
/// at the tracker says withdrawn and carries the reason too. The edge to
/// what it opened is set the way a close of work sets it. A ticket with
/// no `## Acceptance criteria` heading at all is withdrawn the same way,
/// since no box of it is ticked, and so is one carrying `for-a-person`:
/// the label says whose the work is, and there is none.
#[test]
fn a_withdrawal_closes_a_ticket_with_no_box_ticked_and_records_the_measurement() {
    let recorded = Recorded::lay("withdrawn");
    recorded.labelled(
        167,
        "OPEN",
        "## What happened\n\nnot in the tree\n\n## Acceptance criteria\n\n- [ ] *A person decides* which of the two\n- [ ] the decision is implemented\n",
        &["for-a-person"],
    );
    recorded.issue(9, "OPEN", "## Acceptance criteria\n\n- [ ] later\n");
    let reason = "measured on 7d6f1e8: `cargo xtask permit --diff` exit=0 with the captured status staged; #135's `unless` sensor permits it";
    let reading = run(
        workspace(),
        &manifest(),
        &recorded.gh(),
        Act::Close(
            Verdict::Withdrawn(reason.to_owned()),
            Successors::Tickets(vec![Ticket::new(9)]),
            Reach::Landed,
        ),
        Ticket::new(167),
        Some("s3"),
    )
    .unwrap();
    assert_eq!(reading.status, Status::Pass, "{}", reading.message);
    assert_eq!(reading.numbers["withdrawn"], 1.into(), "{reading:#?}");
    assert_eq!(reading.numbers["unticked"], 2.into());
    assert_eq!(reading.numbers["opens"], 1.into());
    assert_eq!(reading.session, "s3");
    assert!(
        reading.message.contains(&format!("withdrawn: {reason}"))
            && reading.message.contains("opens #9")
            && reading.message.contains(&reading.commit)
            && reading.message.contains("session s3"),
        "{}",
        reading.message
    );
    let calls = recorded.calls();
    assert_eq!(calls.len(), 2, "{calls:?}");
    assert_eq!(calls[0], "issue edit 167 --add-blocking 9");
    assert!(
        calls[1].starts_with("issue close 167 --comment Withdrawn by `xtask ticket close` at ")
            && calls[1].contains(reason)
            && calls[1].contains("opens #9"),
        "the comment says withdrawn and carries the reason: {calls:?}"
    );

    recorded.issue(
        11,
        "OPEN",
        "## What happened\n\nfiled, then found already in the tree\n",
    );
    let reading = run(
        workspace(),
        &manifest(),
        &recorded.gh(),
        Act::Close(
            Verdict::Withdrawn("landed by #135".to_owned()),
            Successors::Nothing,
            Reach::Landed,
        ),
        Ticket::new(11),
        None,
    )
    .unwrap();
    assert_eq!(reading.status, Status::Pass, "{}", reading.message);
    assert_eq!(reading.numbers["criteria"], 0.into());
    assert_eq!(reading.numbers["withdrawn"], 1.into());
    assert!(
        recorded.calls()[2].starts_with("issue close 11 --comment Withdrawn"),
        "{:?}",
        recorded.calls()
    );

    // A close of work done says so in the same count, so the section
    // tells the two apart from the store alone.
    recorded.issue(8, "OPEN", TICKED);
    let reading = run(
        workspace(),
        &manifest(),
        &recorded.gh(),
        done(Successors::Nothing),
        Ticket::new(8),
        None,
    )
    .unwrap();
    assert_eq!(reading.status, Status::Pass, "{}", reading.message);
    assert_eq!(reading.numbers["withdrawn"], 0.into(), "{reading:#?}");
}

/// The negative control of a withdrawal (#185): the ticket laid down
/// under `xtask/fixtures/ticket/half-done.md`, one box ticked. Work half
/// done is not withdrawn, so the close fails naming the ticked box and
/// asks the tracker nothing; and a ticket closed already is not withdrawn
/// twice.
#[test]
fn a_withdrawal_of_a_ticket_with_a_box_ticked_is_refused_and_changes_nothing() {
    let body =
        fs::read_to_string(xtask::package_dir().join("fixtures/ticket/half-done.md")).unwrap();
    let recorded = Recorded::lay("half-done");
    recorded.issue(12, "OPEN", &body);
    let reading = run(
        workspace(),
        &manifest(),
        &recorded.gh(),
        Act::Close(
            Verdict::Withdrawn("nothing here".to_owned()),
            Successors::Nothing,
            Reach::Landed,
        ),
        Ticket::new(12),
        Some("s4"),
    )
    .unwrap();
    assert_eq!(reading.status, Status::Fail, "{}", reading.message);
    assert_eq!(reading.numbers["withdrawn"], 1.into());
    assert_eq!(reading.numbers["ticked"], 1.into());
    assert!(
        reading.message.contains("1 of 2 criteria ticked")
            && reading.message.contains("(1) the thing is measured")
            && reading
                .message
                .contains("not closed, nothing changed at the tracker"),
        "{}",
        reading.message
    );
    assert!(recorded.calls().is_empty(), "{:?}", recorded.calls());

    recorded.issue(13, "CLOSED", "## Acceptance criteria\n\n- [ ] one\n");
    let reading = run(
        workspace(),
        &manifest(),
        &recorded.gh(),
        Act::Close(
            Verdict::Withdrawn("nothing here".to_owned()),
            Successors::Nothing,
            Reach::Landed,
        ),
        Ticket::new(13),
        None,
    )
    .unwrap();
    assert_eq!(reading.status, Status::Fail, "{}", reading.message);
    assert!(
        reading.message.contains("closed already"),
        "{}",
        reading.message
    );
    assert!(recorded.calls().is_empty(), "{:?}", recorded.calls());
}

/// The cap before the ticket is filed and not the tracker's refusal after
/// (#448): a sheet at 100 sub-issues fails naming what
/// `gh issue create --parent` would answer, one under it passes naming the
/// room, and a closed sheet fails whatever it holds. The scope is the
/// sheet's own, `parent/<n>`, so a reading here never stands over the one
/// `read` wrote of that issue's criteria, and nothing at the tracker is
/// asked to change.
#[test]
fn a_sheet_at_the_cap_fails_before_a_ticket_is_filed_against_it_and_one_with_room_passes() {
    let recorded = Recorded::lay("parent");
    recorded.sheet(447, "OPEN", CAP);
    recorded.sheet(460, "OPEN", 91);
    recorded.sheet(97, "CLOSED", 12);

    let full = parent(workspace(), &recorded.gh(), Ticket::new(447), None).unwrap();
    assert_eq!(full.status, Status::Fail, "{}", full.message);
    assert_eq!(full.scope, "parent/447");
    assert_eq!(full.numbers["subissues"], 100.into(), "{full:#?}");
    assert_eq!(full.numbers["cap"], 100.into(), "{full:#?}");
    assert_eq!(full.numbers["room"], 0.into(), "{full:#?}");
    assert!(
        full.message
            .contains("#447 holds 100 of GitHub's cap of 100 sub-issues"),
        "{}",
        full.message
    );
    assert!(
        full.message.contains("`gh issue create --parent 447`")
            && full
                .message
                .contains("Parent cannot have more than 100 sub-issues"),
        "what the tracker would refuse: {}",
        full.message
    );
    assert!(
        full.message
            .contains("continued by a sheet at the same row"),
        "what is done instead: {}",
        full.message
    );

    let room = parent(workspace(), &recorded.gh(), Ticket::new(460), None).unwrap();
    assert_eq!(room.status, Status::Pass, "{}", room.message);
    assert_eq!(room.scope, "parent/460");
    assert_eq!(room.numbers["room"], 9.into(), "{room:#?}");
    assert!(
        room.message
            .contains("#460 holds 91 of GitHub's cap of 100 sub-issues, room for 9 more"),
        "{}",
        room.message
    );

    let closed = parent(workspace(), &recorded.gh(), Ticket::new(97), None).unwrap();
    assert_eq!(closed.status, Status::Fail, "{}", closed.message);
    assert_eq!(closed.numbers["room"], 88.into(), "{closed:#?}");
    assert!(
        closed.message.contains("is closed"),
        "a closed sheet indexes nothing filed under it: {}",
        closed.message
    );

    assert!(recorded.calls().is_empty(), "{:?}", recorded.calls());
}

/// An issue the tracker answered without the count, which every recorded
/// transcript beside this one is, reads as holding none: the field is the
/// one the fetch asks for now, and a reading is not owed a panic for an
/// answer that predates it.
#[test]
fn an_issue_answered_without_the_count_holds_no_sub_issues() {
    let recorded = Recorded::lay("parent-absent");
    recorded.issue(13, "OPEN", "## Acceptance criteria\n\n- [x] one\n");
    let reading = parent(workspace(), &recorded.gh(), Ticket::new(13), Some("s1")).unwrap();
    assert_eq!(reading.status, Status::Pass, "{}", reading.message);
    assert_eq!(reading.numbers["subissues"], 0.into(), "{reading:#?}");
    assert_eq!(reading.session, "s1");
}
