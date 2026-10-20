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
