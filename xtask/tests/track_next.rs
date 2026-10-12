//! `xtask track next`, the ticket a track works next (#73): the frontier
//! held without the network. The tracker is a shell file answering `issue
//! list` from a recorded transcript, and the clone is a scratch repository
//! with one commit on `main` whose branch records are written by hand, so
//! each of the four filters is held against an answer this test wrote
//! down.

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
        git(&dir.join("repo"), &["init", "-q", "-b", "main"]);
        git(
            &dir.join("repo"),
            &["commit", "-q", "--allow-empty", "-m", "main"],
        );
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
        .args(["-c", "user.name=x", "-c", "user.email=x@x"])
        .args([
            "-c",
            "commit.gpgsign=false",
            "-c",
            "core.hooksPath=/var/empty",
        ])
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
    recorded.records("t10", 10);
    let found = next(&recorded.repo(), &recorded.gh(), "main").unwrap();
    assert_eq!(
        found,
        Some(Next {
            number: 20,
            title: "ticket 20".to_owned(),
            url: "https://example.invalid/issues/20".to_owned(),
            labels: vec!["ready-for-agent".to_owned()],
            weight: 0,
        }),
        "10 is recorded by a branch, 11 is blocked by the open 12, 12 has no ready label, 14 is t-x's; 20 and 30 are main's, and no census weighs any of them, so the order is the number's"
    );
    assert!(
        recorded.calls().is_empty(),
        "next asks the tracker nothing but the list: {:?}",
        recorded.calls()
    );

    let t_y = next(&recorded.repo(), &recorded.gh(), "t-y").unwrap();
    assert_eq!(
        t_y, None,
        "an unlabelled ticket is main's alone, and no ticket is t-y's"
    );

    let t_x = next(&recorded.repo(), &recorded.gh(), "t-x").unwrap();
    assert_eq!(
        t_x.map(|n| n.number),
        Some(14),
        "t-x's own ticket is the one it may work"
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
    let found = next(&recorded.repo(), &recorded.gh(), "main").unwrap();
    assert_eq!(found.map(|n| n.number), Some(5));
}

/// A ticket with no `track:` label is `main`'s alone (ADR-0009, #1836):
/// `track next --track parity-hosts` passes over the lower-numbered
/// unlabelled ticket to name the one labelled `track:parity-hosts`, and
/// `main`'s run names the unlabelled one, so the two never compete for it.
#[test]
fn a_label_track_passes_over_an_unlabelled_ticket_which_is_mains() {
    let recorded = Recorded::lay("unlabelled");
    recorded.open(&[
        (21, &["ready-for-agent", "track:parity-hosts"], &[]),
        (20, READY, &[]),
    ]);
    let run = |track: &str| {
        let out = Command::new(xtask::scratch::xtask(env!("CARGO_BIN_EXE_xtask")))
            .arg("--root")
            .arg(recorded.repo())
            .args(["track", "next", "--track", track])
            .env("PATH", format!("{}:/usr/bin:/bin", recorded.dir.display()))
            .env_remove("CARGO")
            .env_remove("CARGO_MANIFEST_DIR")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8(out.stdout).unwrap()
    };
    assert_eq!(run("parity-hosts"), "#21 ticket 21\n");
    assert_eq!(run("main"), "#20 ticket 20\n");
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
        Next::to_json(None, "t-y", None),
        serde_json::json!({"track": "t-y", "ticket": null, "title": null, "url": null, "weight": null, "census_at": null})
    );
}

#[test]
fn a_found_ticket_is_printed_by_number_and_title() {
    let found = Next {
        number: 20,
        title: "ticket 20".to_owned(),
        url: "https://example.invalid/issues/20".to_owned(),
        labels: vec!["ready-for-agent".to_owned()],
        weight: 67,
    };
    assert_eq!(
        Next::render(Some(&found), "t-y"),
        "#20 ticket 20\n",
        "the weight is not on the line a caller reads a number out of"
    );
    assert_eq!(
        Next::to_json(Some(&found), "t-y", None),
        serde_json::json!({"track": "t-y", "ticket": 20, "title": "ticket 20", "url": "https://example.invalid/issues/20", "weight": 67, "census_at": null})
    );

    // The census's own timestamp travels with the weight, so a ranking
    // made on a stale reading is seen to be one (#618).
    let at: toml::value::Datetime = "2026-09-23T09:25:00Z".parse().unwrap();
    assert_eq!(
        Next::to_json(Some(&found), "t-y", Some(&at))["census_at"],
        serde_json::json!("2026-09-23T09:25:00Z")
    );
}
