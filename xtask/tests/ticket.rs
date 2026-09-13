//! `xtask ticket`, the closer (#28): the judgement held without the
//! network. Every test here runs the instrument's `run` over a `gh` that
//! is a shell file, answering `issue view` from the transcript laid beside
//! it and appending what `issue close` asked to a file, so what the
//! tracker was asked is read back as a fact and nothing here reaches
//! GitHub. The checkbox grammar is held beside the parser.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use xtask::manifest::Manifest;
use xtask::reading::Status;
use xtask::scratch::Tree;
use xtask::ticket::{Act, Gh, Successors, run};
use xtask::track::Ticket;

fn workspace() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap()
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
            "#!/bin/sh\nif [ \"$1\" = issue ] && [ \"$2\" = view ]; then cat '{0}/issues/'\"$3\".json; exit $?; fi\nprintf '%s\\n' \"$*\" >> '{0}/calls'\n{refusal}exit 0\n",
            self.dir.display()
        );
        fs::write(self.dir.join("gh"), script).unwrap();
        fs::set_permissions(self.dir.join("gh"), fs::Permissions::from_mode(0o755)).unwrap();
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
        let issue = serde_json::json!({
            "number": number,
            "title": format!("ticket {number}"),
            "body": body,
            "state": state,
            "url": format!("https://example.invalid/issues/{number}"),
            "labels": labels.iter().map(|name| serde_json::json!({"name": name})).collect::<Vec<_>>(),
            "blocking": {"nodes": blocking.iter().map(|n| serde_json::json!({"number": n, "state": "OPEN"})).collect::<Vec<_>>()},
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
    let body = fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures/rules/harness.self/ticket.md"),
    )
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
        "main",
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
        "main",
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
    recorded.labelled(152, "OPEN", body, &["ready-for-agent", "track:t-opus-5"]);
    let reading = run(
        workspace(),
        &manifest(),
        &recorded.gh(),
        Act::Read,
        Ticket::new(152),
        None,
        "t-opus-5",
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
        "main",
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
    assert!(!reading.tree().is_empty(), "stamped over the workspace");
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
        "main",
    )
    .unwrap();
    assert_eq!(reading.status, Status::Pass);
    assert_eq!(reading.numbers["unticked"], 0.into());
    assert!(recorded.calls().is_empty());
}

#[test]
fn close_refuses_while_a_box_is_unticked_and_changes_nothing_at_the_tracker() {
    let recorded = Recorded::lay("refuse");
    recorded.issue(7, "OPEN", UNTICKED);
    let reading = run(
        workspace(),
        &manifest(),
        &recorded.gh(),
        Act::Close(Successors::Nothing),
        Ticket::new(7),
        Some("s1"),
        "main",
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
        Act::Close(Successors::Nothing),
        Ticket::new(9),
        None,
        "main",
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
        Act::Close(Successors::Nothing),
        Ticket::new(10),
        None,
        "main",
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
        Act::Close(Successors::Nothing),
        Ticket::new(8),
        Some("session-abc"),
        "main",
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
        Act::Close(Successors::Tickets(vec![Ticket::new(9), Ticket::new(10)])),
        Ticket::new(8),
        Some("s2"),
        "main",
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
        Act::Close(Successors::Tickets(vec![Ticket::new(9), Ticket::new(10)])),
        Ticket::new(8),
        None,
        "main",
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
        Act::Close(Successors::Tickets(vec![Ticket::new(9)])),
        Ticket::new(8),
        None,
        "main",
    )
    .unwrap();
    assert_eq!(reading.status, Status::Pass, "{}", reading.message);
    let calls = recorded.calls();
    assert_eq!(calls.len(), 1, "{calls:?}");
    assert!(calls[0].starts_with("issue close 8 "), "{calls:?}");
}

/// A close running on a track refuses a successor that carries no
/// `track:<name>` label, names the edit that adds it, and asks the tracker
/// for nothing but the two `issue view` calls (#141).
#[test]
fn close_on_a_track_refuses_a_successor_that_carries_no_track_label() {
    let recorded = Recorded::lay("unlabelled");
    recorded.labelled(8, "OPEN", TICKED, &["track:t-opus-5"]);
    recorded.labelled(9, "OPEN", "no criteria yet", &["ready-for-agent"]);
    let reading = run(
        workspace(),
        &manifest(),
        &recorded.gh(),
        Act::Close(Successors::Tickets(vec![Ticket::new(9)])),
        Ticket::new(8),
        None,
        "t-opus-5",
    )
    .unwrap();
    assert_eq!(reading.status, Status::Fail, "{}", reading.message);
    assert!(
        reading
            .message
            .contains("opens #9, which carries no `track:t-opus-5` label")
            && reading
                .message
                .contains("`gh issue edit 9 --add-label track:t-opus-5`")
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
        Act::Close(Successors::Tickets(vec![Ticket::new(9)])),
        Ticket::new(8),
        None,
        "t-opus-5",
    )
    .unwrap();
    assert_eq!(reading.status, Status::Fail, "{}", reading.message);
    assert!(recorded.calls().is_empty(), "{:?}", recorded.calls());
}

/// The successor carrying the track's label closes as it always did, and
/// from the main checkout an unlabelled successor is every track's, so
/// nothing is asked of its labels (#141).
#[test]
fn a_successor_carrying_the_track_label_closes_and_main_asks_nothing() {
    let recorded = Recorded::lay("labelled");
    recorded.labelled(8, "OPEN", TICKED, &["track:t-opus-5"]);
    recorded.labelled(9, "OPEN", "no criteria yet", &["track:t-opus-5"]);
    let reading = run(
        workspace(),
        &manifest(),
        &recorded.gh(),
        Act::Close(Successors::Tickets(vec![Ticket::new(9)])),
        Ticket::new(8),
        None,
        "t-opus-5",
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
        Act::Close(Successors::Tickets(vec![Ticket::new(9)])),
        Ticket::new(8),
        None,
        "main",
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
        Act::Close(Successors::Nothing),
        Ticket::new(8),
        Some("s3"),
        "main",
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
        Act::Close(Successors::Nothing),
        Ticket::new(8),
        None,
        "main",
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
            Act::Close(Successors::Tickets(vec![Ticket::new(successor)])),
            Ticket::new(8),
            None,
            "main",
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
        Act::Close(Successors::Tickets(vec![Ticket::new(9)])),
        Ticket::new(8),
        None,
        "main",
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
        Act::Close(Successors::Tickets(vec![])),
        Ticket::new(8),
        None,
        "main",
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
        Act::Close(Successors::Tickets(vec![Ticket::new(9)])),
        Ticket::new(8),
        None,
        "main",
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
        Act::Close(Successors::Nothing),
        Ticket::new(8),
        None,
        "main",
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
        "main",
    );
    assert!(
        missing.is_err(),
        "an issue the tracker has not is an error, not a reading"
    );
}
