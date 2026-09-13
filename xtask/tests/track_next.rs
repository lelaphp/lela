//! `xtask track next`, the ticket a track works next (#73): the frontier
//! held without the network. The tracker is a shell file answering `issue
//! list` from a recorded transcript, and the clone is a scratch repository
//! whose branch records are written by hand, so each of the four filters
//! is held against an answer this test wrote down.

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use xtask::scratch::Tree;
use xtask::ticket::Gh;
use xtask::track_next::{Next, next};

/// A recorded tracker beside a scratch repository: `gh issue list`
/// answers with `list.json`, every other call is written to `calls`.
struct Recorded {
    dir: Tree,
}

impl Recorded {
    fn lay(name: &str) -> Recorded {
        let dir = Tree::new(&format!("track-next-{name}")).unwrap();
        fs::create_dir_all(dir.join("repo")).unwrap();
        let script = format!(
            "#!/bin/sh\nif [ \"$1\" = issue ] && [ \"$2\" = list ]; then cat '{0}/list.json'; exit $?; fi\nprintf '%s\\n' \"$*\" >> '{0}/calls'\nexit 0\n",
            dir.display()
        );
        fs::write(dir.join("gh"), script).unwrap();
        fs::set_permissions(dir.join("gh"), fs::Permissions::from_mode(0o755)).unwrap();
        git(&dir.join("repo"), &["init", "-q"]);
        Recorded { dir }
    }

    fn repo(&self) -> PathBuf {
        self.dir.join("repo")
    }

    fn gh(&self) -> Gh {
        Gh::at(&self.dir.join("gh"))
    }

    /// The transcript: one open issue per row, `(number, labels, the
    /// open tickets blocking it)`, oldest last so the order is judged and
    /// not inherited.
    fn open(&self, rows: &[(u64, &[&str], &[u64])]) {
        let issues = rows
            .iter()
            .map(|(number, labels, blockers)| {
                serde_json::json!({
                    "number": number,
                    "title": format!("ticket {number}"),
                    "url": format!("https://example.invalid/issues/{number}"),
                    "labels": labels.iter().map(|name| serde_json::json!({"name": name})).collect::<Vec<_>>(),
                    "blockedBy": {
                        "nodes": blockers.iter().map(|n| serde_json::json!({"number": n, "state": "OPEN"})).collect::<Vec<_>>(),
                        "totalCount": blockers.len(),
                    },
                })
            })
            .collect::<Vec<_>>();
        fs::write(
            self.dir.join("list.json"),
            serde_json::Value::Array(issues).to_string(),
        )
        .unwrap();
    }

    /// A branch of the clone recording `ticket`, the way `worktree add
    /// --ticket` records it.
    fn records(&self, branch: &str, ticket: u64) {
        git(
            &self.repo(),
            &[
                "config",
                &format!("branch.{branch}.ticket"),
                &ticket.to_string(),
            ],
        );
    }

    fn calls(&self) -> Vec<String> {
        fs::read_to_string(self.dir.join("calls"))
            .unwrap_or_default()
            .lines()
            .map(str::to_owned)
            .collect()
    }
}

fn git(dir: &Path, args: &[&str]) {
    let out = Command::new("git")
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {}", args.join(" "));
}

const READY: &[&str] = &["ready-for-agent"];

#[test]
fn the_four_filters_pass_a_ticket_over_and_the_oldest_left_is_next() {
    let recorded = Recorded::lay("filters");
    recorded.open(&[
        (30, &["ready-for-agent", "track:main"], &[]),
        (20, READY, &[]),
        (14, &["ready-for-agent", "track:t-x"], &[]),
        (12, &[], &[]),
        (11, READY, &[12]),
        (10, READY, &[]),
    ]);
    recorded.records("t-y/ten", 10);
    let found = next(&recorded.repo(), &recorded.gh(), "t-y").unwrap();
    assert_eq!(
        found,
        Some(Next {
            number: 20,
            title: "ticket 20".to_owned(),
            url: "https://example.invalid/issues/20".to_owned(),
            labels: vec!["ready-for-agent".to_owned()],
        }),
        "10 is recorded by a branch, 11 is blocked by the open 12, 12 has no label, 14 is t-x's, 30 is main's"
    );
    assert!(
        recorded.calls().is_empty(),
        "next asks the tracker nothing but the list: {:?}",
        recorded.calls()
    );

    let main = next(&recorded.repo(), &recorded.gh(), "main").unwrap();
    assert_eq!(
        main.map(|n| n.number),
        Some(20),
        "an unlabelled ticket is main's too, and 20 is older than 30"
    );

    let t_x = next(&recorded.repo(), &recorded.gh(), "t-x").unwrap();
    assert_eq!(
        t_x.map(|n| n.number),
        Some(14),
        "t-x's own ticket is the oldest it may work"
    );
}

#[test]
fn a_blocker_that_is_closed_does_not_block() {
    let recorded = Recorded::lay("closed-blocker");
    fs::write(
        recorded.dir.join("list.json"),
        serde_json::json!([{
            "number": 5,
            "title": "ticket 5",
            "url": "https://example.invalid/issues/5",
            "labels": [{"name": "ready-for-agent"}],
            "blockedBy": {"nodes": [{"number": 4, "state": "CLOSED"}], "totalCount": 1},
        }])
        .to_string(),
    )
    .unwrap();
    let found = next(&recorded.repo(), &recorded.gh(), "t-y").unwrap();
    assert_eq!(found.map(|n| n.number), Some(5));
}

#[test]
fn an_empty_frontier_is_none_and_not_an_error() {
    let recorded = Recorded::lay("empty");
    recorded.open(&[(12, &[], &[]), (11, READY, &[12])]);
    let found = next(&recorded.repo(), &recorded.gh(), "t-y").unwrap();
    assert_eq!(found, None);
    assert_eq!(
        Next::render(None, "t-y"),
        "none: the frontier of t-y is empty\n"
    );
    assert_eq!(
        Next::to_json(None, "t-y"),
        serde_json::json!({"track": "t-y", "ticket": null, "title": null, "url": null})
    );
}

#[test]
fn a_found_ticket_is_printed_by_number_and_title() {
    let found = Next {
        number: 20,
        title: "ticket 20".to_owned(),
        url: "https://example.invalid/issues/20".to_owned(),
        labels: vec!["ready-for-agent".to_owned()],
    };
    assert_eq!(Next::render(Some(&found), "t-y"), "#20 ticket 20\n");
    assert_eq!(
        Next::to_json(Some(&found), "t-y"),
        serde_json::json!({"track": "t-y", "ticket": 20, "title": "ticket 20", "url": "https://example.invalid/issues/20"})
    );
}
