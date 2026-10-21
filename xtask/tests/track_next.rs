//! `xtask track next`, the ticket a track works next (#73): the frontier
//! held without the network. The tracker is a [`Tracker`] answering
//! `issue list` with the open issues each test lays, and the clone is a
//! scratch repository with one commit on `main` whose branch records are
//! written by hand, so each of the four filters is held against an answer
//! this test wrote down.

use xtask::scratch::Tree;
use xtask::ticket::{Open, State};
use xtask::track_next::{Next, next};

mod common {
    pub mod repo;
    pub mod run;
    pub mod tracker;
}
use common::repo::Repo;
use common::run::Run;
use common::tracker::{Tracker, blocked_by, open};

/// A scratch clone beside the tracker it asks: `main` with one commit,
/// and branch records a test writes by hand.
struct Scratch {
    repo: Tree,
    tracker: Tracker,
}

impl Scratch {
    fn lay(name: &str) -> Scratch {
        let repo = Tree::new(&format!("track-next-{name}")).unwrap();
        Repo::init(&repo).git(&["commit", "-q", "--allow-empty", "-m", "main"]);
        Scratch {
            repo,
            tracker: Tracker::empty(&format!("track-next-{name}")),
        }
    }

    /// The tracker's open list: one issue per row, `(number, labels, the
    /// open tickets blocking it)`, oldest last so the order is judged and
    /// not inherited.
    fn open(&self, rows: &[(u64, &[&str], &[u64])]) {
        let issues: Vec<Open> = rows
            .iter()
            .map(|&(number, labels, blockers)| {
                let blockers: Vec<(u64, State)> =
                    blockers.iter().map(|&n| (n, State::Open)).collect();
                Open {
                    blocked_by: blocked_by(&blockers),
                    ..open(number, labels)
                }
            })
            .collect();
        self.tracker.listing(&issues);
    }

    /// A branch of the clone recording `ticket`, the way `worktree add
    /// --ticket` records it.
    fn records(&self, branch: &str, ticket: u64) {
        Repo::at(&self.repo).git(&[
            "config",
            &format!("branch.{branch}.ticket"),
            &ticket.to_string(),
        ]);
    }
}

const READY: &[&str] = &["ready-for-agent"];

#[test]
fn the_four_filters_pass_a_ticket_over_and_the_oldest_left_is_next() {
    let scratch = Scratch::lay("filters");
    scratch.open(&[
        (30, &["ready-for-agent", "track:main"], &[]),
        (20, READY, &[]),
        (14, &["ready-for-agent", "track:t-x"], &[]),
        (12, &[], &[]),
        (11, READY, &[12]),
        (10, READY, &[]),
    ]);
    scratch.records("t10", 10);
    let found = next(&scratch.repo, &scratch.tracker.gh(), "main").unwrap();
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
        scratch.tracker.calls().is_empty(),
        "next asks the tracker nothing but the list: {:?}",
        scratch.tracker.calls()
    );

    let t_y = next(&scratch.repo, &scratch.tracker.gh(), "t-y").unwrap();
    assert_eq!(
        t_y, None,
        "an unlabelled ticket is main's alone, and no ticket is t-y's"
    );

    let t_x = next(&scratch.repo, &scratch.tracker.gh(), "t-x").unwrap();
    assert_eq!(
        t_x.map(|n| n.number),
        Some(14),
        "t-x's own ticket is the one it may work"
    );
}

#[test]
fn a_blocker_that_is_closed_does_not_block() {
    let scratch = Scratch::lay("closed-blocker");
    scratch.tracker.listing(&[Open {
        blocked_by: blocked_by(&[(4, State::Closed)]),
        ..open(5, READY)
    }]);
    let found = next(&scratch.repo, &scratch.tracker.gh(), "main").unwrap();
    assert_eq!(found.map(|n| n.number), Some(5));
}

/// A ticket with no `track:` label is `main`'s alone (ADR-0009, #1836):
/// `track next --track parity-hosts` passes over the lower-numbered
/// unlabelled ticket to name the one labelled `track:parity-hosts`, and
/// `main`'s run names the unlabelled one, so the two never compete for it.
#[test]
fn a_label_track_passes_over_an_unlabelled_ticket_which_is_mains() {
    let scratch = Scratch::lay("unlabelled");
    scratch.open(&[
        (21, &["ready-for-agent", "track:parity-hosts"], &[]),
        (20, READY, &[]),
    ]);
    let run = |track: &str| {
        Run::at(&scratch.repo)
            .args(["track", "next", "--track", track])
            .env(
                "PATH",
                format!("{}:/usr/bin:/bin", scratch.tracker.bin().display()),
            )
            .env_remove("CARGO")
            .env_remove("CARGO_MANIFEST_DIR")
            .go()
            .ok()
    };
    assert_eq!(run("parity-hosts"), "#21 ticket 21\n");
    assert_eq!(run("main"), "#20 ticket 20\n");
}

#[test]
fn an_empty_frontier_is_none_and_not_an_error() {
    let scratch = Scratch::lay("empty");
    scratch.open(&[(12, &[], &[]), (11, READY, &[12])]);
    let found = next(&scratch.repo, &scratch.tracker.gh(), "t-y").unwrap();
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
