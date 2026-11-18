//! `xtask trace-dump`, the trace-dump diff (`jit-design.md` §4.1;
//! harness §7): a program's trace and exit lines written by the oracle
//! under `[regime.jit-inline]`, kept in the store, and held against the
//! engine's, one reading per program. Every test here goes through the
//! instrument's `run`, with a binary standing for the engine's that
//! writes the fixture's lines, and reads what it wrote.
//!
//! The tests that spawn the oracle say so on stderr and return when the
//! pinned binary is not on this machine, unless the machine promised it;
//! the rest run anywhere.
#![cfg(tracked)]

use std::fs;
use std::path::{Path, PathBuf};

use xtask::divergence::{Digests, Registry, Side};
use xtask::manifest::Manifest;
use xtask::oracle::Promise;
use xtask::reading::{Number, Reading, Status, Store};
use xtask::scratch::Tree;
use xtask::trace_dump::{self, Ask};

mod common {
    pub mod manifest;
    pub mod registry;
    pub mod repo;
}
use common::manifest::{of_rows, pin};
use common::registry::registry;
use common::repo::scratch_workspace;

fn workspace() -> &'static Path {
    xtask::package_dir().parent().unwrap()
}

fn fixtures() -> PathBuf {
    xtask::package_dir().join("fixtures/trace-dump")
}

/// The tracked pin's binary, when it is on this machine; a failure where
/// this machine promised the binary.
fn oracle() -> Option<PathBuf> {
    Promise::Oracle
        .or_skip(pin().oracle.file().map(Path::to_path_buf))
        .unwrap()
}

/// A workspace of this test's own, with a store of its own for the
/// readings and the lines kept, judging by a manifest of no rows under
/// the tracked pin.
struct Scratch {
    root: Tree,
    store: Store,
    manifest: Manifest,
}

impl Scratch {
    fn new(name: &str) -> Scratch {
        let root = Tree::new(&format!("trace-dump-{name}")).unwrap();
        let repo = scratch_workspace(workspace(), &root);
        // The run reads the kernels' names from the declaration, so a
        // program named is never scoped as one.
        let declared =
            "[[workload]]\nstem = \"fib\"\nfirst_read = 1\nabout = \"x\"\nrole = \"kernel\"\n";
        repo.write("docs/perf/workloads.toml", declared);
        repo.write("docs/perf/workloads/fib.php", "<?php\n");
        let store = Store::at(root.join("target/harness"));
        Scratch {
            root,
            store,
            manifest: of_rows([]),
        }
    }

    /// A binary standing for the engine's: it writes the program's sum on
    /// its first stream, as `loop.php` does, and the fixture `lines` on
    /// its second.
    fn engine(&self, lines: &str) -> PathBuf {
        let path = self.root.join(format!("engine-{lines}"));
        let lines = fixtures().join(lines);
        let body = format!("#!/bin/sh\necho 4950\ncat '{}' >&2\n", lines.display());
        fs::write(&path, body).unwrap();
        xtask::scratch::executable(&path).unwrap();
        path
    }

    /// The one reading of `loop.php` with `lines` standing for the
    /// engine's, under `registry`.
    fn one(&self, lines: &str, registry: &Registry) -> Reading {
        let ask = Ask {
            program: vec![fixtures().join("loop.php")],
            engine: Some(self.engine(lines)),
            ..Ask::default()
        };
        let readings =
            trace_dump::run(&self.root, &self.manifest, registry, &self.store, &ask).unwrap();
        self.store.record(&readings).unwrap();
        assert_eq!(readings.len(), 1, "{readings:#?}");
        readings.into_iter().next().unwrap()
    }

    /// What the run kept for `side` of `loop.php`.
    fn kept(&self, side: Side) -> String {
        let digest = self.manifest.pin().unwrap().digest();
        fs::read_to_string(trace_dump::kept(&self.store, digest, "loop", side)).unwrap()
    }
}

fn count(reading: &Reading, key: &str) -> i64 {
    match reading.numbers.get(key) {
        Some(Number::Int(n)) => *n,
        other => panic!("{key} is {other:?} in {reading:#?}"),
    }
}

/// The negative control (harness §14 item 4): php's own lines standing
/// for the engine's are a pass, and the lines the run keeps for the
/// oracle are the fixture's, the loop's exit line among them; the engine's
/// lines, whose loop root is a line apart and which grow no side trace,
/// are a fail naming both roots.
#[test]
fn the_oracles_lines_are_a_pass_and_the_engines_lines_apart_are_not() {
    if oracle().is_none() {
        return;
    }
    let scratch = Scratch::new("control");
    let oracle_lines = fs::read_to_string(fixtures().join("oracle.lines")).unwrap();
    let reading = scratch.one("oracle.lines", &registry(""));
    assert_eq!(reading.status, Status::Pass, "{reading:#?}");
    assert_eq!(
        scratch.kept(Side::Oracle),
        oracle_lines,
        "php's lines are kept"
    );
    assert_eq!(count(&reading, "oracle_lines"), 9, "{reading:#?}");
    assert!(
        reading
            .message
            .contains("the engine's 9 trace lines are the oracle's")
            && reading
                .message
                .contains("jit.mode=force jit.compile=inline"),
        "{}",
        reading.message
    );

    let reading = scratch.one("engine.lines", &registry(""));
    assert_eq!(reading.status, Status::Fail, "{reading:#?}");
    assert_eq!(scratch.kept(Side::Oracle), oracle_lines);
    let engine_lines = fs::read_to_string(fixtures().join("engine.lines")).unwrap();
    assert_eq!(
        scratch.kept(Side::Engine),
        engine_lines,
        "the engine's lines are kept"
    );
    assert_eq!(count(&reading, "engine_lines"), 5, "{reading:#?}");
    for line in [
        "at line 3",
        "----- TRACE 1 start (loop) $main() %SCRATCH%/loop.php:3",
        "+---- TRACE 1 start (loop) $main() %SCRATCH%/loop.php:4",
    ] {
        assert!(
            reading.message.contains(line),
            "{line}: {}",
            reading.message
        );
    }
}

/// An accepted `trace:dump/<program>` row pinning both sides' lines as
/// the run keeps them is what a difference passes under, naming the row;
/// an `open` one accepts nothing.
#[test]
fn an_accepted_row_passes_the_difference_and_an_open_one_does_not() {
    if oracle().is_none() {
        return;
    }
    let scratch = Scratch::new("row");
    let read = |name: &str| fs::read(fixtures().join(name)).unwrap();
    let pins = Digests::of(&read("oracle.lines"), &read("engine.lines"));
    let row = |status: &str| {
        let row = format!(
            "[[divergence]]\nid = \"trace:dump/loop\"\nkind = \"trace\"\nreason = \"a for loop's root (#3134)\"\nstatus = \"{status}\"\nsince = \"2026-10-21\"\noracle = \"{}\"\nengine = \"{}\"\n",
            pins.oracle, pins.engine
        );
        registry(&row)
    };
    let reading = scratch.one("engine.lines", &row("accepted"));
    assert_eq!(reading.status, Status::Pass, "{reading:#?}");
    assert!(
        reading
            .message
            .contains("trace:dump/loop accepts it: a for loop's root"),
        "{}",
        reading.message
    );
    let reading = scratch.one("engine.lines", &row("open"));
    assert_eq!(reading.status, Status::Fail, "{reading:#?}");
}

/// An absent oracle is a skip that says why, and runs no engine.
#[test]
fn an_absent_oracle_is_a_skip_and_not_a_pass() {
    let mut scratch = Scratch::new("absent");
    scratch.manifest.pin.as_mut().unwrap().oracle.path = scratch.root.join("nowhere/php");
    let reading = scratch.one("engine.lines", &registry(""));
    assert_eq!(reading.status, Status::Skip, "{reading:#?}");
    assert!(
        reading.message.contains("nowhere/php"),
        "{}",
        reading.message
    );
}
