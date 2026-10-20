//! `main/php_open_temporary_file.c`: the temporary file a request makes
//! for an upload, for `tempnam()` and for a spilling `php://temp`, and the
//! `open_basedir` guard asked of where it is made.

use std::fs::{File, OpenOptions};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use crate::api::raising::{Level, docref};
use crate::api::request::Request;

impl Request {
    /// `php_open_temporary_fd_ex` (`Z/main/php_open_temporary_file.c:318`):
    /// the file made in `dir`, or where that fails php's notice `file
    /// created in the system's temporary directory` and the file made
    /// there; no `dir`, or an empty one, goes to the temporary directory
    /// with nothing said. The guard is asked, warning, of the directories
    /// `check` names before each is tried ([`TemporaryGuard`]). The file
    /// is open for reading and writing, and `None` is the one that could
    /// not be made anywhere or was refused.
    pub(crate) fn open_temporary_fd(
        &mut self,
        dir: Option<&[u8]>,
        prefix: &[u8],
        check: TemporaryGuard,
    ) -> Option<(File, PathBuf)> {
        if let Some(dir) = dir.filter(|dir| !dir.is_empty()) {
            if check == TemporaryGuard::Always
                && self
                    .environment_mut()
                    .check_open_basedir(dir, true)
                    .is_err()
            {
                return None;
            }
            let dir = self.environment().absolutize(dir);
            if let Some(made) = open_temporary_in(&dir, prefix) {
                return Some(made);
            }
            docref(
                self,
                Level::Notice,
                "file created in the system's temporary directory",
            );
        }
        let temp_dir = self.environment().temp_dir();
        if check != TemporaryGuard::Never
            && self
                .environment_mut()
                .check_open_basedir(temp_dir.as_os_str().as_bytes(), true)
                .is_err()
        {
            return None;
        }
        open_temporary_in(&temp_dir, prefix)
    }
}

/// Which directories `php_open_temporary_fd_ex` asks the `open_basedir`
/// guard of, php's flags (`Z/main/php_open_temporary_file.h:20-28`)
/// decoded once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TemporaryGuard {
    /// `PHP_TMP_FILE_DEFAULT`, what a temporary stream opens with: neither.
    Never,
    /// `PHP_TMP_FILE_OPEN_BASEDIR_CHECK_ON_FALLBACK`, what an upload's
    /// file is made with (`Z/main/rfc1867.c:1006`): the system's temporary
    /// directory only, where `dir` is unset or could not be used.
    OnFallback,
    /// `PHP_TMP_FILE_OPEN_BASEDIR_CHECK_ALWAYS`, what `tempnam()` makes
    /// its file with: `dir` before it is tried, and the fallback.
    Always,
}

/// `php_do_open_temporary_file` (`Z/main/php_open_temporary_file.c:92`):
/// the directory resolved as `realpath(3)` resolves it, the prefix
/// followed by thirteen characters of `base32alphabet` from a random
/// word, and `mkstemp(3)`'s six, a new six tried where the name is
/// taken; the file `0600`, open for reading and writing.
fn open_temporary_in(dir: &Path, prefix: &[u8]) -> Option<(File, PathBuf)> {
    use std::hash::{BuildHasher, Hasher};
    const BASE32: &[u8] = b"0123456789abcdefghijklmnopqrstuv";
    const MKSTEMP: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let random = |salt: u64| {
        let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
        hasher.write_u64(salt);
        hasher.finish()
    };
    let real = std::fs::canonicalize(dir).ok()?;
    let mut name = prefix.to_vec();
    let mut word = random(0);
    for _ in 0..13 {
        name.push(BASE32[usize::try_from(word % 32).unwrap_or(0)]);
        word /= 32;
    }
    for attempt in 1u64..=100 {
        let mut candidate = name.clone();
        let mut word = random(attempt);
        for _ in 0..6 {
            candidate.push(MKSTEMP[usize::try_from(word % 62).unwrap_or(0)]);
            word /= 62;
        }
        let path = real.join(std::ffi::OsStr::from_bytes(&candidate));
        match OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&path)
        {
            Ok(file) => return Some((file, path)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(_) => return None,
        }
    }
    None
}
