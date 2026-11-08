//! The negative control of `xtask miri` (harness-design.md §4.1, §7's Miri
//! row; #1916): a crate whose test reads a box after freeing it fails the
//! instrument as undefined behaviour, and the test beside it still runs,
//! each in a Miri process of its own (#4117). Where the pinned nightly or
//! cargo-nextest is not installed, the reading is a skip naming the
//! command that installs it, and the control returns; CI's Miri job
//! installs both.
#![cfg(tracked)]

use xtask::manifest::Manifest;
use xtask::miri;
use xtask::reading::{Number, Reading, Status};

fn count(reading: &Reading, key: &str) -> i64 {
    match reading.numbers.get(key) {
        Some(Number::Int(n)) => *n,
        other => panic!("{key} is {other:?} in {reading:#?}"),
    }
}

#[test]
fn a_read_after_a_free_fails_the_crate_and_the_test_beside_it_runs() {
    let root = xtask::package_dir().join("fixtures/miri/use-after-free");
    let manifest = Manifest::load(&root).unwrap();
    let readings = miri::run(&root, &manifest, &[]).unwrap();
    let [reading] = &readings[..] else {
        panic!("{readings:#?}")
    };
    if reading.status == Status::Skip {
        assert!(
            reading.message.contains(miri::TOOLCHAIN)
                || reading
                    .message
                    .contains(&format!("cargo-nextest@{}", miri::NEXTEST)),
            "{}",
            reading.message
        );
        return;
    }
    assert_eq!(reading.status, Status::Fail, "{reading:#?}");
    assert_eq!(count(reading, "undefined"), 1, "{reading:#?}");
    assert_eq!(count(reading, "failed"), 1, "{reading:#?}");
    assert_eq!(count(reading, "passed"), 1, "{reading:#?}");
    assert_eq!(count(reading, "ran"), 2, "{reading:#?}");
    assert!(count(reading, "jobs") >= 1, "{reading:#?}");
    assert!(
        reading
            .message
            .contains("tests::a_read_after_the_free: Undefined Behavior"),
        "{}",
        reading.message
    );
    assert!(
        reading.message.contains("permissive provenance"),
        "{}",
        reading.message
    );
}

/// CI's Miri job installs the nightly and the cargo-nextest the instrument
/// runs on, named once in `xtask::miri::TOOLCHAIN` and `xtask::miri::NEXTEST`
/// and copied into the workflow, so a pin moved in one and not the other
/// fails here.
#[test]
fn the_ci_job_installs_the_toolchain_the_instrument_runs_on() {
    let workflow = xtask::package_dir().join("../.github/workflows/harness.yml");
    let text = std::fs::read_to_string(workflow).unwrap();
    let install = format!(
        "rustup toolchain install {} --profile minimal --component miri,rust-src\n",
        miri::TOOLCHAIN
    );
    assert!(
        text.contains(&install),
        "the workflow does not run `{install}`"
    );
    let nextest = format!("cargo install --locked cargo-nextest@{}\n", miri::NEXTEST);
    assert!(
        text.contains(&nextest),
        "the workflow does not run `{nextest}`"
    );
    assert!(
        text.contains("- run: cargo xtask miri\n"),
        "no step runs the instrument"
    );
}
