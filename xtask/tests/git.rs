//! The stamp an instrument puts on a reading: tree, commit, time.

use std::path::Path;

use xtask::git::Stamp;
use xtask::manifest::Manifest;
use xtask::reading::{Reading, Status, Store};

fn workspace() -> &'static Path {
    Path::new(env!("CARGO_MANIFEST_DIR")).parent().unwrap()
}

#[test]
fn a_stamp_has_a_tree_hash_a_commit_and_a_time_that_toml_reads() {
    let stamp = Stamp::take(workspace(), Path::new("xtask")).unwrap();
    assert_eq!(stamp.tree.len(), 40, "{}", stamp.tree);
    assert!(stamp.tree.chars().all(|c| c.is_ascii_hexdigit()));
    assert!(!stamp.commit.is_empty());
    assert!(stamp.at.to_string().ends_with('Z'), "{}", stamp.at);
}

#[test]
fn the_tree_hash_follows_the_sources_it_covers() {
    let a = Stamp::take(workspace(), Path::new("xtask")).unwrap();
    let b = Stamp::take(workspace(), Path::new("docs")).unwrap();
    assert_ne!(a.tree, b.tree);
    assert_eq!(
        a.tree,
        Stamp::take(workspace(), Path::new("xtask")).unwrap().tree
    );
}

/// The whole path an instrument takes: manifest, stamp, reading, store.
#[test]
fn a_test_instrument_writes_one_reading_end_to_end() {
    let root = std::env::temp_dir().join(format!("xtask-instrument-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let manifest = Manifest::load(workspace()).unwrap();
    let scope = manifest.scope_path("xtask").unwrap();
    let mut reading = Reading::new("test", "xtask");
    reading.status = Status::Pass;
    reading.stamp(&Stamp::take(workspace(), &scope).unwrap());
    let store = Store::at(&root);
    let path = store.write(&reading).unwrap();
    assert_eq!(path, root.join("test/xtask.toml"));
    assert_eq!(store.read("test", "xtask").unwrap().unwrap(), reading);
}
