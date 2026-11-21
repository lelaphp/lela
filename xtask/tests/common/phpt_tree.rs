//! A tree standing where the pinned php-src would, for the phpt tests on
//! both sides of the harness.
//! Each file of `tests/common` is one topic: an integration test includes
//! the topics it calls by name, as `mod common { pub mod phpt_tree; }`, and
//! the unit tests under `src` include theirs once, in lib.rs, by `#[path]`.
//! A binary takes a topic whole and may call part of it.
#![allow(
    dead_code,
    reason = "a binary includes the topic whole and calls part of it"
)]

use std::fs;
use std::path::Path;

use xtask::oracle::Pin;
use xtask::scratch::Tree;

/// The tree, with the directories the per-commit lane's rows name and no
/// test in them: what a subset resolves to is held over it, and what one
/// runs to is held over the real corpus by the tests that skip without it.
pub fn fixture_tree() -> Tree {
    let tree = Tree::new("phpt-tree").unwrap();
    for under in ["Zend/tests", "tests", "sapi/cli/tests", "ext/pcre/tests"] {
        fs::create_dir_all(tree.join(under)).unwrap();
    }
    tree
}

/// The `.phpt` files under `tree`, counted as `[source] phpt_files`
/// counts php-src's at the tag.
pub fn phpt_files(tree: &Path) -> u64 {
    let mut found = 0;
    for entry in fs::read_dir(tree).unwrap().flatten() {
        let path = entry.path();
        if path.is_dir() {
            found += phpt_files(&path);
        } else if path.to_string_lossy().ends_with(".phpt") {
            found += 1;
        }
    }
    found
}

/// `pin` counting the tests its tree at `tree` holds now, so a run over a
/// fixture tree is held to its count as one over php-src is to the tag's.
pub fn counting(mut pin: Pin, tree: &Path) -> Pin {
    pin.source.phpt_files = phpt_files(tree);
    pin
}
