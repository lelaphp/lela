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
//! So a test holds each of its conflict keys, `all` among them, as a lock
//! in the temporary directory while it runs, and waits for a key another
//! run holds; the keys are taken in one order, so two runs never wait on
//! each other. A key that names a path of the tree costs a wait only where
//! two runs reach the same test at once. A lock is the operating system's
//! and goes with the process however it ends, so a killed run holds
//! nothing. The subset's reading counts the tests that waited and for how
//! long.

use std::collections::BTreeSet;
use std::fs::File;
use std::path::PathBuf;
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
            self.for_ms += u64::try_from(waited.as_millis()).unwrap_or(u64::MAX);
        }
    }
}

/// Every key of `keys` held, in order, waiting for one another run holds;
/// how long the wait was, zero when there was none.
pub(crate) fn hold(keys: &[String]) -> Result<(Held, Duration)> {
    let keys: BTreeSet<&str> = keys.iter().map(String::as_str).collect();
    let mut locks = Vec::with_capacity(keys.len());
    let mut waited = Duration::ZERO;
    for key in keys {
        let path = lock_of(key);
        let file = File::options()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)
            .with_context(|| format!("opening the lock {}", path.display()))?;
        if file.try_lock().is_err() {
            let since = Instant::now();
            file.lock()
                .with_context(|| format!("waiting for the lock {}", path.display()))?;
            waited += since.elapsed();
        }
        locks.push(file);
    }
    Ok((Held { _locks: locks }, waited))
}

/// The lock file of one key: its readable characters, and the hash of the
/// whole key, so two keys that read alike are two locks.
fn lock_of(key: &str) -> PathBuf {
    let readable: String = key
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .take(40)
        .collect();
    let digest = crate::git::blob_id(key.as_bytes());
    std::env::temp_dir().join(format!("{PREFIX}{readable}-{}", &digest[..12]))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{Waited, hold, lock_of};

    #[test]
    fn a_key_held_by_another_holder_is_waited_for_and_then_held() {
        let key = format!("held-test-{}", std::process::id());
        let (first, waited) = hold(std::slice::from_ref(&key)).unwrap();
        assert_eq!(waited, Duration::ZERO);
        let second = std::thread::spawn(move || hold(&[key]).unwrap().1);
        std::thread::sleep(Duration::from_millis(200));
        drop(first);
        let waited = second.join().unwrap();
        assert!(waited >= Duration::from_millis(100), "{waited:?}");
    }

    #[test]
    fn two_keys_that_read_alike_are_two_locks() {
        assert_ne!(lock_of("a/b"), lock_of("a_b"));
        let name = lock_of("all");
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
