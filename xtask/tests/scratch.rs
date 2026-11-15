//! The scratch tree from outside: what a test's directory is worth
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
//!
//! The killed run is held the same way and needs the child for a second
//! reason: a process cannot watch itself be killed any more than it can
//! watch its own drop, and the point of the test is that no code of ours
//! runs at that moment. So the parent spawns a child, waits until it has a
//! tree, kills it, and then asserts the tree is standing — the leak itself,
//! as a negative control, because a test that only watched the sweep would
//! still pass if `Drop` had quietly been doing the work all along. Then a
//! second child makes a tree of its own, which is all it takes to sweep,
//! and the parent looks again.

use std::path::{Path, PathBuf};
use std::process::Command;

use xtask::scratch::{KEEP, Tree};

/// The variable that puts this binary in the child's role: the file to
/// write the tree's path into, so the parent can look where the child
/// was.
const RECORD: &str = "XTASK_SCRATCH_RECORD";

/// Set beside [`RECORD`] to put the child in the role of a run that is
/// killed: the tree made and written down, and then nothing at all, until
/// the parent takes the process away mid-sleep. Nothing of the child's
/// runs after that, which is the condition under test.
const HANG: &str = "XTASK_SCRATCH_HANG";

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
    if std::env::var_os(HANG).is_some() {
        // The parent kills this process about here. Nothing below runs,
        // `tree` is never dropped, and what it made is still standing: the
        // leak, on purpose. The sleep is only long enough that the kill is
        // what ends this process; if it ever runs out, the child exits and
        // the parent's first assertion is what fails.
        std::thread::sleep(std::time::Duration::from_mins(2));
    }
    assert!(!panicking, "{PANICKED}");
}

/// This binary run again with `--exact <test>`, the variables set: the
/// child's exit, its two streams as one text, and the path it wrote down.
fn child(test: &str, record: &Path, keep: bool) -> (bool, String, String) {
    let (ok, said) = run_child(test, None, record, keep);
    let wrote = std::fs::read_to_string(record).unwrap_or_default();
    (ok, said, wrote)
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

/// What a tree's lock is named on disk. The module keeps this to itself;
/// a test of what a killed run leaves behind is a test of the shape on
/// disk, so it is written out here rather than read from the code it is
/// checking.
const LOCK: &str = ".lock";

/// This binary run again with `--exact <test>`, the variables set: the
/// child's exit and its two streams as one text.
///
/// `tmp` is the child's whole world for scratch trees — `temp_dir` reads
/// `TMPDIR` — so what such a child makes, and everything its sweep can
/// reach, is out of the way of the tests running beside this one. Without
/// it a sweep is loose in the real temporary directory, which is what the
/// live-run test wants and what the killed-run test cannot have.
fn run_child(test: &str, tmp: Option<&Path>, record: &Path, keep: bool) -> (bool, String) {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([test, "--exact", "--nocapture"])
        .env(RECORD, record);
    if let Some(tmp) = tmp {
        command.env("TMPDIR", tmp);
    }
    if keep {
        command.env(KEEP, "1");
    }
    let out = command.output().unwrap();
    let said = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    (out.status.success(), said)
}

/// The path the child wrote down, once it names a directory that is there.
///
/// The parent waits for this and no longer, so the kill lands on a process
/// that has a tree to leave behind. A half-written path is not a
/// directory, so the same question covers the torn read.
fn wait_for_tree(record: &Path) -> PathBuf {
    for _ in 0..600 {
        if let Ok(text) = std::fs::read_to_string(record) {
            let path = PathBuf::from(text.trim());
            if path.is_dir() {
                return path;
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    panic!("the child never wrote down a tree that was there");
}

/// The ending `Drop` does not have. A killed process unwinds
/// nothing and runs no destructor, so its tree stands — which this asserts
/// first, because a test that only watched the sweep would pass just as
/// well if `Drop` had quietly been doing the work all along. Then any
/// other run making any tree sweeps before it makes one, finds the lock
/// beside the dead run's tree free, and takes both.
///
/// Both children are given a temporary directory of the parent's own, so
/// the tree standing in the middle of this test is not something another
/// test's sweep could carry off first.
#[test]
fn a_killed_run_leaves_a_tree_that_the_next_run_sweeps() {
    if let Some(record) = std::env::var_os(RECORD) {
        as_the_child(Path::new(&record), false);
        return;
    }
    let here = Tree::new("kill-record").unwrap();
    let tmp = here.join("tmp");
    std::fs::create_dir_all(&tmp).unwrap();
    let record = here.join("path");

    let mut killed = Command::new(std::env::current_exe().unwrap())
        .args([
            "a_killed_run_leaves_a_tree_that_the_next_run_sweeps",
            "--exact",
            "--nocapture",
        ])
        .env(RECORD, &record)
        .env("TMPDIR", &tmp)
        .env(HANG, "1")
        .spawn()
        .unwrap();
    let left = wait_for_tree(&record);
    killed.kill().unwrap();
    killed.wait().unwrap();

    let mut lock = left.clone().into_os_string();
    lock.push(LOCK);
    let lock = PathBuf::from(lock);
    assert!(
        left.is_dir(),
        "the killed run left its tree: {}",
        left.display()
    );
    assert!(lock.is_file(), "and the lock beside it: {}", lock.display());

    // Any run at all, making any tree, in that same temporary directory.
    let (ok, said) = run_child(
        "a_tree_goes_when_the_value_does",
        Some(&tmp),
        &record,
        false,
    );
    assert!(ok, "{said}");
    assert!(!left.exists(), "the next run swept it: {}", left.display());
    assert!(!lock.exists(), "and its lock: {}", lock.display());
}

/// The other side of the same lock: a tree whose run is still going is not
/// swept, however many other runs sweep around it. This one deliberately
/// lets the child loose in the real temporary directory, because that is
/// the arrangement that would go wrong — inside one test binary it is also
/// what keeps every test running beside this one from losing its tree to
/// the child this one spawns.
#[test]
fn a_live_runs_tree_survives_another_runs_sweep() {
    let mine = Tree::new("live").unwrap();
    std::fs::write(mine.join("a-file"), "the live run's").unwrap();
    let path = mine.path().to_path_buf();

    let (ok, said) = run_child(
        "a_tree_goes_when_the_value_does",
        None,
        &path.join("no-record"),
        false,
    );
    assert!(ok, "{said}");
    assert!(path.is_dir(), "the sweep left it: {}", path.display());
    assert!(
        mine.join("a-file").is_file(),
        "and left what was in it: {}",
        mine.display()
    );
}

/// A kept tree is made with no lock beside it, and that is what puts it
/// out of the sweep's reach: the sweep reads locks, so a tree with none is
/// never a candidate. Outliving the process that asked for it is the whole
/// of what [`KEEP`] is for, and a sweep that took it would undo that at
/// the first run afterwards — which, for a session that set the variable
/// and went to look, is every run.
#[test]
fn a_kept_tree_has_no_lock_and_no_sweep_takes_it() {
    if let Some(record) = std::env::var_os(RECORD) {
        as_the_child(Path::new(&record), false);
        return;
    }
    let here = Tree::new("keep-lock-record").unwrap();
    let tmp = here.join("tmp");
    std::fs::create_dir_all(&tmp).unwrap();
    let record = here.join("path");

    let (ok, said) = run_child(
        "a_kept_tree_has_no_lock_and_no_sweep_takes_it",
        Some(&tmp),
        &record,
        true,
    );
    assert!(ok, "{said}");
    let kept = PathBuf::from(std::fs::read_to_string(&record).unwrap().trim());
    assert!(kept.is_dir(), "the child's tree is still there: {said}");

    let mut lock = kept.clone().into_os_string();
    lock.push(LOCK);
    assert!(
        !Path::new(&lock).exists(),
        "and nothing locked it: {}",
        Path::new(&lock).display()
    );

    // The process that made it is gone, so a sweep has every reason to
    // take it, and does not.
    let (ok, said) = run_child(
        "a_tree_goes_when_the_value_does",
        Some(&tmp),
        &record,
        false,
    );
    assert!(ok, "{said}");
    assert!(kept.is_dir(), "the sweep left it: {}", kept.display());
    // Dropping `here` takes the kept tree with it: it was made under the
    // temporary directory this test owns, which is inside `here`.
}
