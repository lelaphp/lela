//! `xtask ticket`, the closer (#28): the judgement held without the
//! network. Every test here runs the instrument's `run` over a `gh` that
//! is a shell file, answering `issue view` from the transcript laid beside
//! it and appending what `issue close` asked to a file, so what the
//! tracker was asked is read back as a fact and nothing here reaches
//! GitHub. The checkbox grammar is held beside the parser.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use xtask::reading::Status;
use xtask::ticket::{Act, Gh, run};
use xtask::track::Ticket;

fn workspace() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap()
}

/// A recorded `gh`: a shell file in a directory of this test's own that
/// answers `issue view <n>` with `issues/<n>.json` and appends every other
/// call's arguments to `calls`, one line each. The instrument stamps its
/// reading over the workspace, which it only reads.
struct Recorded {
    dir: PathBuf,
}

impl Recorded {
    fn lay(name: &str) -> Recorded {
        let dir = std::env::temp_dir().join(format!("xtask-ticket-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("issues")).unwrap();
        let script = format!(
            "#!/bin/sh\nif [ \"$1\" = issue ] && [ \"$2\" = view ]; then cat '{}/issues/'\"$3\".json; exit $?; fi\nprintf '%s\\n' \"$*\" >> '{}/calls'\nexit 0\n",
            dir.display(),
            dir.display()
        );
        fs::write(dir.join("gh"), script).unwrap();
        fs::set_permissions(dir.join("gh"), fs::Permissions::from_mode(0o755)).unwrap();
        Recorded { dir }
    }

    fn issue(&self, number: u64, state: &str, body: &str) {
        let issue = serde_json::json!({
            "number": number,
            "title": format!("ticket {number}"),
            "body": body,
            "state": state,
            "url": format!("https://example.invalid/issues/{number}"),
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

const UNTICKED: &str =
    "## What to build\n\nthings\n\n## Acceptance criteria\n\n- [x] one\n- [ ] two\n- [ ] three\n";
const TICKED: &str = "## Acceptance criteria\n\n- [x] one\n- [X] two\n";

#[test]
fn read_counts_the_boxes_and_names_the_unticked() {
    let recorded = Recorded::lay("read");
    recorded.issue(7, "OPEN", UNTICKED);
    let reading = run(workspace(), &recorded.gh(), Act::Read, Ticket::new(7), None).unwrap();
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
    let reading = run(workspace(), &recorded.gh(), Act::Read, Ticket::new(8), None).unwrap();
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
        &recorded.gh(),
        Act::Close,
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
        &recorded.gh(),
        Act::Close,
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
        &recorded.gh(),
        Act::Close,
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
        &recorded.gh(),
        Act::Close,
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

#[test]
fn a_tracker_that_refuses_the_close_is_a_failed_reading_carrying_its_text() {
    let recorded = Recorded::lay("refused");
    recorded.issue(8, "OPEN", TICKED);
    fs::write(
        recorded.dir.join("gh"),
        format!(
            "#!/bin/sh\nif [ \"$2\" = view ]; then cat '{}/issues/'\"$3\".json; exit 0; fi\necho 'GraphQL: no permission' >&2; exit 1\n",
            recorded.dir.display()
        ),
    )
    .unwrap();
    let reading = run(
        workspace(),
        &recorded.gh(),
        Act::Close,
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
