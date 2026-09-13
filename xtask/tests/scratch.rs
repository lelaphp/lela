//! The scratch tree from outside (#194): what a test's directory is worth
//! is what happens to it on the run that fails, so the two that matter are
//! held through a child process of this same binary.
//!
//! A test cannot watch its own drop: `#[should_panic]` hands the panic
//! back to the harness with the frame already unwound, and whatever the
//! body asserts afterwards never runs. So the test that panics is this
//! binary run again with one variable set, and the parent reads the path
//! the child wrote down before it panicked and finds nothing there. The
//! same shape holds the other direction: with [`KEEP`] set the child's
//! tree is still there when the parent looks.

use std::path::Path;
use std::process::Command;

use xtask::scratch::{KEEP, Tree};

/// The variable that puts this binary in the child's role: the file to
/// write the tree's path into, so the parent can look where the child
/// was.
const RECORD: &str = "XTASK_SCRATCH_RECORD";

/// The panic the child raises, and what the parent holds its failure to:
/// a child that fell over earlier than the panic under test says so in
/// another voice, and proves nothing about the drop.
const PANICKED: &str = "the tree is left to drop";

/// The child's half: the tree made, a file written into it so it is not
/// an empty directory that removes itself by accident, the path written
/// down, and then whatever the role asks.
fn as_the_child(record: &Path, panicking: bool) {
    let tree = Tree::new("child").unwrap();
    std::fs::write(tree.join("a-file"), "so the tree is not empty").unwrap();
    assert!(tree.is_dir(), "{}", tree.display());
    std::fs::write(record, tree.display().to_string()).unwrap();
    assert!(!panicking, "{PANICKED}");
}

/// This binary run again with `--exact <test>`, the variables set: the
/// child's exit, its two streams as one text, and the path it wrote down.
fn child(test: &str, record: &Path, keep: bool) -> (bool, String, String) {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([test, "--exact", "--nocapture"])
        .env(RECORD, record);
    if keep {
        command.env(KEEP, "1");
    }
    let out = command.output().unwrap();
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    let wrote = std::fs::read_to_string(record).unwrap_or_default();
    (out.status.success(), said, wrote)
}

/// The run that fails is the one whose leavings are least wanted, and it
/// is the run the old shape leaked by: cleanup at the bottom of a test
/// body never happens when the body panics above it. Dropping does.
#[test]
fn a_panicking_test_leaves_no_tree() {
    if let Some(record) = std::env::var_os(RECORD) {
        as_the_child(Path::new(&record), true);
        return;
    }
    let here = Tree::new("panic-record").unwrap();
    let record = here.join("path");
    let (ok, said, wrote) = child("a_panicking_test_leaves_no_tree", &record, false);
    assert!(!ok, "the child is the failing run: {said}");
    assert!(said.contains(PANICKED), "and it failed there: {said}");
    assert!(!wrote.is_empty(), "the child wrote its tree down: {said}");
    assert!(
        !Path::new(&wrote).exists(),
        "the panicking child's tree is gone: {wrote}"
    );
}

/// The other direction, held the same way: with the variable set the tree
/// outlives the process that made it, which is what a session debugging a
/// failure asks for. The parent takes it away, since nothing else will.
#[test]
fn the_variable_keeps_the_tree_and_names_it() {
    if let Some(record) = std::env::var_os(RECORD) {
        as_the_child(Path::new(&record), false);
        return;
    }
    let here = Tree::new("keep-record").unwrap();
    let record = here.join("path");
    let (ok, said, wrote) = child("the_variable_keeps_the_tree_and_names_it", &record, true);
    assert!(ok, "{said}");
    assert!(!wrote.is_empty(), "the child wrote its tree down: {said}");
    let kept = Path::new(&wrote);
    assert!(kept.is_dir(), "the tree is still there: {wrote}");
    assert!(
        said.contains(KEEP) && said.contains(&wrote),
        "named: {said}"
    );
    std::fs::remove_dir_all(kept).unwrap();
}

/// A test that returns is no different: the tree goes when the value does,
/// and the `Tree` a constructor hands back is what holds it open until
/// then.
#[test]
fn a_tree_goes_when_the_value_does() {
    let path = {
        let tree = Tree::new("scope").unwrap();
        std::fs::write(tree.join("a-file"), "written").unwrap();
        assert!(tree.join("a-file").is_file());
        tree.path().to_path_buf()
    };
    assert!(!path.exists(), "{}", path.display());
}

/// Two trees of one name are two directories: the counter in the name is
/// what keeps concurrent tests apart, which the process id alone did not.
#[test]
fn two_trees_of_one_name_are_two_directories() {
    let first = Tree::new("same-name").unwrap();
    let second = Tree::new("same-name").unwrap();
    assert_ne!(first.path(), second.path());
    std::fs::write(first.join("mine"), "the first's").unwrap();
    assert!(!second.join("mine").exists(), "{}", second.display());
    // And the name still says which test made it, for a session reading a
    // kept tree or a process listing.
    let named = first.file_name().unwrap().to_string_lossy().into_owned();
    assert!(named.starts_with("xtask-same-name-"), "{named}");
}
