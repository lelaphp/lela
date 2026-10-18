//! What a test holds among every run on the machine, not only its own
//! (#1313). Upstream's conflict keys name what two tests may not share at
//! once (run-tests.php:1341-1372), and the run holds them apart within
//! itself; but most of what they name is the machine's and not the
//! tree's: `sapi/fpm/tests` names `all` because every `FPM\Tester` takes
//! the ports from 9008 up (`sapi/fpm/tests/tester.inc:1333-1342`),
//! `ext/curl/tests` names `all` for its server's port, and `mysql`,
//! `pgsql` and `server` name a server or a port. Each run stands in a copy
//! of the tree of its own (#1183), and ports and servers are not in the
//! tree, so two runs on one machine, two worktrees or a merge check beside
//! a builder, met on them: a run read 95 of `sapi/fpm`'s 120 for "Address
//! already in use" while another held 127.0.0.1:9008.
//!
//! So a test holds each of its conflict keys as a lock in the temporary
//! directory while it runs ([`machine`]; a test of the harness names a
//! directory of its own, so it meets no real run, #1675), and waits for a key another run holds; the
//! keys are taken in one order, so two runs never wait on each other. A
//! key that names a path of the tree costs a wait only where two runs
//! reach the same test at once. A lock is the operating system's and goes
//! with the process however it ends, so a killed run holds nothing. The
//! subset's reading counts the tests that waited and for how long.
//!
//! `all` is not a key beside the others but upstream's "nothing beside"
//! (run-tests.php:1365-1370), and among runs it means the same (#1559):
//! every test with a key holds the `all` lock shared before its keys, and
//! a test naming `all` holds it exclusive, so it runs beside no keyed test
//! of any run. A test with no key holds nothing, as upstream's `all` test
//! runs once every keyed one has. An `all` test that waits first takes the
//! turnstile, which every keyed test passes through before its shared
//! hold, so a run whose keyed tests overlap one another does not keep it
//! waiting for the whole run. Within one run the queue already holds an
//! `all` test alone, so a run never waits on itself.

use std::collections::BTreeSet;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

/// What a held key's lock file is named, in the temporary directory: not
/// `.lock`, which the scratch sweep reads as a dead run's tree's.
const PREFIX: &str = "xtask-phpt-held-";

/// The locks one test holds, released when it drops.
#[derive(Debug)]
pub(crate) struct Held {
    _locks: Vec<File>,
}

/// The waits of one subset's tests on keys another run held.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct Waited {
    pub tests: u64,
    pub for_ms: u64,
}

impl Waited {
    pub(crate) fn add(&mut self, waited: Duration) {
        if !waited.is_zero() {
            self.tests += 1;
            self.for_ms += crate::reading::millis(waited);
        }
    }
}

/// The key that names upstream's "nothing beside".
const ALL: &str = "all";

/// Where the locks stand that every run on the machine meets in: the
/// temporary directory. A run that names no other directory holds its
/// keys here.
pub(crate) fn machine() -> PathBuf {
    std::env::temp_dir()
}

/// Every key of `keys` held as a lock in `dir`, in order, waiting for one
/// another run holds; how long the wait was, zero when there was none. The
/// `all` lock comes first, shared, or exclusive where `keys` names `all`;
/// no key holds nothing.
pub(crate) fn hold(dir: &Path, keys: &[String]) -> Result<(Held, Duration)> {
    let mut keys: BTreeSet<&str> = keys.iter().map(String::as_str).collect();
    if keys.is_empty() {
        return Ok((Held { _locks: Vec::new() }, Duration::ZERO));
    }
    let alone = keys.remove(ALL);
    let mut locks = Vec::with_capacity(keys.len() + 1);
    let mut waited = Duration::ZERO;
    let turnstile = dir.join(format!("{PREFIX}-turnstile"));
    let (turnstile, wait) = take(&turnstile, Mode::Exclusive)?;
    waited += wait;
    let mode = if alone { Mode::Exclusive } else { Mode::Shared };
    let (all, wait) = take(&lock_of(dir, ALL), mode)?;
    waited += wait;
    drop(turnstile);
    locks.push(all);
    for key in keys {
        let (file, wait) = take(&lock_of(dir, key), Mode::Exclusive)?;
        waited += wait;
        locks.push(file);
    }
    Ok((Held { _locks: locks }, waited))
}

/// How a lock is held: beside other shared holders, or alone.
#[derive(Debug, Clone, Copy)]
enum Mode {
    Shared,
    Exclusive,
}

/// The lock file at `path`, made where it is not there yet, held as
/// `mode` says, waiting where another holder stands in the way; how long
/// the wait was.
fn take(path: &Path, mode: Mode) -> Result<(File, Duration)> {
    let file = File::options()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("opening the lock {}", path.display()))?;
    let free = match mode {
        Mode::Shared => file.try_lock_shared().is_ok(),
        Mode::Exclusive => file.try_lock().is_ok(),
    };
    if free {
        return Ok((file, Duration::ZERO));
    }
    let since = Instant::now();
    match mode {
        Mode::Shared => file.lock_shared(),
        Mode::Exclusive => file.lock(),
    }
    .with_context(|| format!("waiting for the lock {}", path.display()))?;
    Ok((file, since.elapsed()))
}

/// The lock file of one key in `dir`: its readable characters, and the
/// hash of the whole key, so two keys that read alike are two locks.
fn lock_of(dir: &Path, key: &str) -> PathBuf {
    let readable: String = key
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .take(40)
        .collect();
    let digest = crate::git::blob_id(key.as_bytes());
    dir.join(format!("{PREFIX}{readable}-{}", &digest[..12]))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{Waited, hold, lock_of, machine};
    use crate::scratch::Tree;

    /// The directory one test holds its locks in, its own, so neither
    /// another test nor a real run on the machine holding `all` makes it
    /// wait (#1675).
    fn own() -> Tree {
        Tree::new("phpt-across").unwrap()
    }

    #[test]
    fn a_key_held_by_another_holder_is_waited_for_and_then_held() {
        let dir = own();
        let key = "held-test".to_owned();
        let (first, _) = hold(dir.path(), std::slice::from_ref(&key)).unwrap();
        let path = dir.path().to_path_buf();
        let second = std::thread::spawn(move || hold(&path, &[key]).unwrap().1);
        std::thread::sleep(Duration::from_millis(200));
        drop(first);
        let waited = second.join().unwrap();
        assert!(waited >= Duration::from_millis(100), "{waited:?}");
    }

    /// A test naming `all` and a test of another key never hold at once
    /// (#1559), whichever came first: a keyed holder waits for an `all`
    /// holder, and an `all` holder for a keyed one. A key held under `all`
    /// alone, before #1559, waited for neither.
    #[test]
    fn all_and_another_key_wait_for_each_other_both_ways() {
        let dir = own();
        let key = "all-test".to_owned();
        let all = || vec!["all".to_owned()];
        for all_first in [true, false] {
            let (first, second): (Vec<String>, Vec<String>) = if all_first {
                (all(), vec![key.clone()])
            } else {
                (vec![key.clone()], all())
            };
            let (held, _) = hold(dir.path(), &first).unwrap();
            let path = dir.path().to_path_buf();
            let after = std::thread::spawn(move || hold(&path, &second).unwrap().1);
            std::thread::sleep(Duration::from_millis(300));
            drop(held);
            let waited = after.join().unwrap();
            assert!(
                waited >= Duration::from_millis(150),
                "all first: {all_first}, waited {waited:?}"
            );
        }
    }

    /// Two keyed holders of different keys share the `all` lock and do
    /// not wait on each other, and a test with no key holds nothing.
    #[test]
    fn two_other_keys_hold_at_once_and_no_key_holds_nothing() {
        let dir = own();
        let (one, _) = hold(dir.path(), &["one-test".to_owned()]).unwrap();
        let (sent, received) = std::sync::mpsc::channel();
        let path = dir.path().to_path_buf();
        let two = std::thread::spawn(move || {
            let held = hold(&path, &["two-test".to_owned()]).unwrap();
            sent.send(()).unwrap();
            held
        });
        let both = received.recv_timeout(Duration::from_secs(30));
        drop(one);
        drop(two.join().unwrap());
        assert!(both.is_ok(), "the second key waited for the first");
        let (none, waited) = hold(dir.path(), &[]).unwrap();
        assert_eq!(waited, Duration::ZERO);
        let (sent, received) = std::sync::mpsc::channel();
        let path = dir.path().to_path_buf();
        let all = std::thread::spawn(move || {
            let held = hold(&path, &["all".to_owned()]).unwrap();
            sent.send(()).unwrap();
            held
        });
        let alone = received.recv_timeout(Duration::from_secs(30));
        drop(none);
        drop(all.join().unwrap());
        assert!(alone.is_ok(), "a test with no key held the all lock");
    }

    /// Locks in two directories are two sets of locks (#1675): an `all`
    /// held alone in one, as a run on the machine holds it in the
    /// temporary directory, makes no holder in the other wait, while a
    /// holder in the same directory does. A `hold` that took its locks in
    /// the temporary directory whatever it was handed fails this.
    #[test]
    fn an_all_held_in_another_directory_makes_no_holder_here_wait() {
        let (machine_like, here) = (own(), own());
        let (alone, _) = hold(machine_like.path(), &["all".to_owned()]).unwrap();
        let (sent, received) = std::sync::mpsc::channel();
        let path = here.path().to_path_buf();
        let keyed = std::thread::spawn(move || {
            let held = hold(&path, &["keyed-test".to_owned(), "all".to_owned()]).unwrap();
            sent.send(()).unwrap();
            held
        });
        let free = received.recv_timeout(Duration::from_secs(30));
        drop(keyed.join().unwrap());
        let path = machine_like.path().to_path_buf();
        let beside = std::thread::spawn(move || hold(&path, &["keyed-test".to_owned()]).unwrap().1);
        std::thread::sleep(Duration::from_millis(200));
        drop(alone);
        let waited = beside.join().unwrap();
        assert!(
            free.is_ok(),
            "a holder here waited for an all held elsewhere"
        );
        assert!(waited >= Duration::from_millis(100), "{waited:?}");
    }

    /// A run that names no directory meets every other run on the machine
    /// in the temporary directory, where the locks of #1313 stood.
    #[test]
    fn a_run_naming_no_directory_holds_in_the_temporary_directory() {
        assert_eq!(machine(), std::env::temp_dir());
        assert_eq!(
            lock_of(&machine(), "all").parent(),
            Some(std::env::temp_dir().as_path())
        );
    }

    #[test]
    fn two_keys_that_read_alike_are_two_locks() {
        let dir = machine();
        assert_ne!(lock_of(&dir, "a/b"), lock_of(&dir, "a_b"));
        let name = lock_of(&dir, "all");
        let name = name.file_name().unwrap().to_str().unwrap();
        assert!(
            name.starts_with("xtask-phpt-held-all-")
                && std::path::Path::new(name).extension().is_none(),
            "{name}"
        );
    }

    #[test]
    fn a_wait_counts_one_test_and_no_wait_counts_none() {
        let mut waited = Waited::default();
        waited.add(Duration::ZERO);
        waited.add(Duration::from_millis(1500));
        assert_eq!(
            waited,
            Waited {
                tests: 1,
                for_ms: 1500
            }
        );
    }
}
