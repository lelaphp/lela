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

/// The stamp computed in process is the one git would compute: the tree
/// hash matches `hash-object` over the same listing and the commit matches
/// `rev-parse --short HEAD`.
#[test]
fn the_stamp_agrees_with_git() {
    let root = workspace();
    let out = |args: &[&str], stdin: Option<&str>| {
        let mut child = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        if let Some(text) = stdin {
            std::io::Write::write_all(&mut child.stdin.take().unwrap(), text.as_bytes()).unwrap();
        }
        String::from_utf8(child.wait_with_output().unwrap().stdout).unwrap()
    };
    assert_eq!(
        xtask::git::head_commit(root).unwrap(),
        out(&["rev-parse", "--short", "HEAD"], None).trim()
    );
    let paths = xtask::git::files(root, Path::new("docs")).unwrap();
    let absolute = paths
        .iter()
        .map(|p| root.join(p).display().to_string())
        .collect::<Vec<_>>()
        .join("\n");
    let ids = out(&["hash-object", "--stdin-paths"], Some(&absolute));
    let mut listing = String::new();
    for (p, id) in paths.iter().zip(ids.lines()) {
        listing.push_str(id);
        listing.push(' ');
        listing.push_str(p);
        listing.push('\n');
    }
    assert_eq!(
        xtask::git::tree_hash(root, Path::new("docs")).unwrap(),
        out(&["hash-object", "--stdin"], Some(&listing)).trim()
    );
}

/// The whole path an instrument takes: manifest, stamp, reading, store.
#[test]
fn a_test_instrument_writes_one_reading_end_to_end() {
    let root = std::env::temp_dir().join(format!("xtask-instrument-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let manifest = Manifest::load(workspace()).unwrap();
    let mut reading = Reading::for_scope("test", "xtask", &manifest, workspace()).unwrap();
    reading.status = Status::Pass;
    assert_eq!(
        reading.tree(),
        Stamp::take(workspace(), &manifest.scope_path("xtask").unwrap())
            .unwrap()
            .tree
    );
    let store = Store::at(&root);
    let path = store.write(&reading).unwrap();
    assert_eq!(path, root.join("test/xtask.toml"));
    assert_eq!(store.read("test", "xtask").unwrap().unwrap(), reading);
}
