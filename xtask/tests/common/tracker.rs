//! The tracker a test talks to in place of GitHub: `fixtures/stand-ins/gh`
//! laid in a directory of the test's own, answering what the test wrote
//! down as `xtask::ticket`'s own types, so the answer is what `Gh::view`
//! and `Gh::open` parse, recording what it was asked to change, and
//! failing the test on any call nothing laid answers. One wire-format
//! test in ticket.rs holds the types to a hand-written answer of `gh`'s.
//! Each file of `tests/common` is one topic: an integration test includes
//! the topics it calls by name, as `mod common { pub mod tracker; }`, and
//! the unit tests under `src` include theirs once, in lib.rs, by `#[path]`.
//! A binary takes a topic whole and may call part of it.
#![allow(
    dead_code,
    reason = "a binary includes the topic whole and calls part of it"
)]

use std::fs;
use std::path::{Path, PathBuf};

use xtask::scratch::Tree;
use xtask::ticket::view::Thread;
use xtask::ticket::{BlockedBy, Blocker, Gh, Issue, Label, Open, State, SubIssues};

/// The stand-in `gh` in its directory, which holds what it answers from;
/// the stand-in's own header names each file. Dropped with an unexpected
/// call recorded, it fails the test naming the call, unless the test is
/// failing already.
#[derive(Debug)]
pub struct Tracker {
    dir: Tree,
}

impl Tracker {
    /// A tracker that answers nothing yet: every call is unexpected until
    /// the test lays its answer.
    pub fn empty(name: &str) -> Tracker {
        let dir = Tree::new(&format!("tracker-{name}")).unwrap();
        fs::create_dir_all(dir.join("issues")).unwrap();
        let gh = dir.join("gh");
        fs::copy(xtask::package_dir().join("fixtures/stand-ins/gh"), &gh).unwrap();
        xtask::scratch::executable(&gh).unwrap();
        Tracker { dir }
    }

    /// `issue view <n>` answered with `issue`, in place of what it
    /// answered before.
    pub fn with_issue(&self, issue: &Issue) -> &Tracker {
        self.with_answer(issue.number, &serde_json::to_string(issue).unwrap())
    }

    /// `issue view <n>` answered with `thread`, the issue and its
    /// comments, in place of what it answered before.
    pub fn with_thread(&self, thread: &Thread) -> &Tracker {
        self.with_answer(thread.issue.number, &serde_json::to_string(thread).unwrap())
    }

    /// `issue view <number>` answered with `text` as written: a
    /// hand-written answer of `gh`'s own, or one that is no issue.
    pub fn with_answer(&self, number: u64, text: &str) -> &Tracker {
        self.write(&format!("issues/{number}.json"), text)
    }

    /// `issue list` answered with `open`, in place of what it answered
    /// before.
    pub fn listing(&self, open: &[Open]) -> &Tracker {
        self.listing_answer(&serde_json::to_string(open).unwrap())
    }

    /// `issue list` answered with `text` as written.
    pub fn listing_answer(&self, text: &str) -> &Tracker {
        self.write("list.json", text)
    }

    /// `issue view <number>` answered as the tracker answers a number
    /// that is no issue.
    pub fn none(&self, number: u64) -> &Tracker {
        self.write(&format!("issues/{number}.none"), "")
    }

    /// `issue view <number>` answers `answered` times, then fails `fails`
    /// times as `gh` does when the machine has run out of ports for a
    /// moment, then answers again.
    pub fn flaky(&self, number: u64, answered: u32, fails: u32) -> &Tracker {
        self.write(
            &format!("issues/{number}.flaky"),
            &format!("{answered} {fails}\n"),
        )
    }

    /// `issue <verb>`, `edit` or `close`, refused with `gh`'s text after
    /// it is recorded.
    pub fn refusing(&self, verb: &str) -> &Tracker {
        self.write(&format!("refuse.{verb}"), "")
    }

    /// The file `name` of the stand-in's directory, as the header of
    /// `fixtures/stand-ins/gh` names them: what a session a test scripts
    /// writes to change the tracker in the middle of a run, `down.list`
    /// to take its listing down or `list.json` for a new list.
    pub fn file(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }

    /// The tracker as the instrument calls it.
    pub fn gh(&self) -> Gh {
        Gh::at(&self.dir.join("gh"))
    }

    /// The directory the stand-in stands in, as `gh`: what goes first on
    /// the `PATH` of a binary that finds `gh` there.
    pub fn bin(&self) -> &Path {
        &self.dir
    }

    /// What the tracker was asked to change, `issue edit` and `issue
    /// close`, one call a line as its arguments were given.
    pub fn calls(&self) -> Vec<String> {
        lines(&self.dir.join("calls"))
    }

    fn write(&self, file: &str, text: &str) -> &Tracker {
        fs::write(self.dir.join(file), text).unwrap();
        self
    }
}

impl Drop for Tracker {
    fn drop(&mut self) {
        let unexpected = lines(&self.dir.join("unexpected"));
        assert!(
            unexpected.is_empty() || std::thread::panicking(),
            "the tracker was asked what the test laid no answer for: {}",
            unexpected
                .iter()
                .map(|call| format!("`gh {call}`"))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
}

fn lines(file: &Path) -> Vec<String> {
    fs::read_to_string(file)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

/// Issue `number` as the tracker answers one: open, titled `ticket <n>`,
/// with no body, label or edge. A test sets what it holds the instrument
/// to with the struct's own fields, `Issue { body, ..issue(7) }`.
pub fn issue(number: u64) -> Issue {
    Issue {
        number,
        title: format!("ticket {number}"),
        body: String::new(),
        state: State::Open,
        url: format!("https://example.invalid/issues/{number}"),
        labels: Vec::new(),
        blocking: BlockedBy::default(),
        blocked_by: BlockedBy::default(),
        sub_issues: SubIssues::default(),
    }
}

/// Issue `number` as `issue list` answers it, carrying `labels` and
/// blocked by nothing.
pub fn open(number: u64, labels: &[&str]) -> Open {
    Open {
        number,
        title: format!("ticket {number}"),
        url: format!("https://example.invalid/issues/{number}"),
        labels: self::labels(labels),
        blocked_by: BlockedBy::default(),
    }
}

/// Labels by name, as the tracker spells them.
pub fn labels(names: &[&str]) -> Vec<Label> {
    names
        .iter()
        .map(|name| Label {
            name: (*name).to_owned(),
        })
        .collect()
}

/// The edges of one issue to the numbered ones, each in the state given,
/// as `blocking` and `blockedBy` nest them.
pub fn blocked_by(blockers: &[(u64, State)]) -> BlockedBy {
    BlockedBy {
        nodes: blockers
            .iter()
            .map(|&(number, state)| Blocker { number, state })
            .collect(),
    }
}
