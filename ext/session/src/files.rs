//! `mod_files.c`, the `files` save handler: one file `sess_<id>` per
//! session under `session.save_path`, `N;MODE;DIR` splitting it into `N`
//! levels of one-character directories, opened create-and-read-write
//! under `O_NOFOLLOW` with the path's permissions, held under `flock(LOCK_EX)`
//! from the read to the close, read and written from offset zero with
//! `pread` and `pwrite`, and collected by modification time. The file is
//! php's raw fd, not a stream, so it takes no resource id; its
//! path is read against the request's working directory, as
//! `VCWD_OPEN_MODE` reads it.

use std::fs::{File, OpenOptions, Permissions};
use std::os::unix::fs::{FileExt, MetadataExt, OpenOptionsExt, PermissionsExt};

use php_ext::coerce::is_space;
use php_ext::process;
use php_ext::raising::{Level, Thrown, docref, raise};
use php_ext::request::{MAXPATHLEN, Request};

use crate::{State, session};

/// `FILE_PREFIX`.
const PREFIX: &[u8] = b"sess_";

/// `O_NOFOLLOW`, which the standard library names on no platform: the
/// BSDs' and macOS's bit, and Linux's, which the architectures number
/// two ways.
#[cfg(not(target_os = "linux"))]
const O_NOFOLLOW: i32 = 0x0100;
#[cfg(all(
    target_os = "linux",
    any(
        target_arch = "aarch64",
        target_arch = "arm",
        target_arch = "powerpc64"
    )
))]
const O_NOFOLLOW: i32 = 0o100_000;
#[cfg(all(
    target_os = "linux",
    not(any(
        target_arch = "aarch64",
        target_arch = "arm",
        target_arch = "powerpc64"
    ))
))]
const O_NOFOLLOW: i32 = 0o400_000;

/// `ps_files`: what `PS_OPEN_FUNC(files)` sets up, the file it holds
/// open and the key it was opened for.
pub(crate) struct Files {
    last_key: Option<Vec<u8>>,
    basedir: Vec<u8>,
    dirdepth: usize,
    st_size: i64,
    filemode: i64,
    fd: Option<File>,
}

/// `ps_files_path_create`: `basedir/a/b/sess_ab...` for a depth of two,
/// `None` for a key no longer than the depth or a path past `MAXPATHLEN`.
fn path_create(data: &Files, key: &[u8]) -> Option<Vec<u8>> {
    let needed = data.basedir.len() + 2 * data.dirdepth + key.len() + 5 + PREFIX.len() + 1;
    if key.len() <= data.dirdepth || MAXPATHLEN < needed {
        return None;
    }
    let mut buf = data.basedir.clone();
    buf.push(b'/');
    for &c in &key[..data.dirdepth] {
        buf.push(c);
        buf.push(b'/');
    }
    buf.extend_from_slice(PREFIX);
    buf.extend_from_slice(key);
    Some(buf)
}

/// The files handler's data, `PS_FILES_DATA`.
fn data(req: &mut Request) -> Option<&mut Files> {
    req.state::<State>().files.as_mut()
}

/// `ps_files_close`: the file given up, its `flock` with it.
fn close_fd(req: &mut Request) {
    if let Some(files) = data(req) {
        files.fd = None;
    }
}

/// php's `%s (%d)` of an `errno`, `strerror` and the number.
fn errno_text(req: &Request, error: &std::io::Error) -> String {
    let errno = error.raw_os_error().unwrap_or(0);
    format!("{} ({errno})", req.environment().os_error_text(errno))
}

/// `VCWD_STAT`: the host's `stat(2)` of the path against the working
/// directory, as `mod_files.c` makes it, past every wrapper and so past
/// the `open_basedir` guard, which php asks only of the save path.
fn exists(req: &Request, path: &[u8]) -> Option<std::fs::Metadata> {
    std::fs::metadata(req.environment().absolutize(path)).ok()
}

/// `VCWD_UNLINK`, as [`exists`] states: `false` where it failed, with
/// nothing said.
fn unlink(req: &Request, path: &[u8]) -> bool {
    std::fs::remove_file(req.environment().absolutize(path)).is_ok()
}

/// `ps_files_open`: the file for `key` opened and locked, unless it is
/// the one open already; a key php refuses, a path it cannot build and
/// an open that fails each warn and leave no file open.
/// `VCWD_OPEN_MODE(buf, O_CREAT | O_RDWR | O_NOFOLLOW, filemode)` makes
/// a new file with the permissions `session.save_path` names, less the umask,
/// and refuses a symbolic link where the file would be; the fd
/// takes no resource id, so the stream opened after it has the id it
/// would have had. A file another uid owns is refused, closed, with
/// php's warning, unless the process runs as root ([`foreign`]).
///
/// ```php
/// ob_start(); umask(0); session_save_path("0;0640;" . __DIR__); session_id("mode1"); session_start(["use_cookies" => 0, "cache_limiter" => ""]);
/// session_write_close(); printf("%o\n", fileperms(__DIR__ . "/sess_mode1") & 0777);
/// symlink(__DIR__ . "/elsewhere", __DIR__ . "/sess_link1"); session_id("link1"); var_dump(session_start(), file_exists(__DIR__ . "/elsewhere"));
/// session_save_path(__DIR__ . "/nope"); session_id("gone1"); var_dump(session_start());
/// $f = fopen(__FILE__, "r"); var_dump($f);
/// ```
/// ```expect
/// 640
///
/// Warning: session_start(): open(%s/sess_link1, O_RDWR) failed: %s (%d) in %s on line 4
///
/// Warning: session_start(): Failed to read session data: files (path: 0;0640;%s) in %s on line 4
/// bool(false)
/// bool(false)
///
/// Warning: session_start(): open(%s/nope/sess_gone1, O_RDWR) failed: No such file or directory (2) in %s on line 5
///
/// Warning: session_start(): Failed to read session data: files (path: %s/nope) in %s on line 5
/// bool(false)
/// resource(%d) of type (stream)
/// ```
fn open_key(req: &mut Request, key: &[u8]) {
    let Some(files) = data(req) else {
        return;
    };
    if files.fd.is_some() && files.last_key.as_deref() == Some(key) {
        return;
    }
    files.last_key = None;
    close_fd(req);
    if !session::valid_key(key) {
        docref(
            req,
            Level::Warning,
            "Session ID is too long or contains illegal characters. Only the A-Z, a-z, 0-9, \"-\", and \",\" characters are allowed",
        );
        return;
    }
    let Some(path) = data(req).and_then(|d| path_create(d, key)) else {
        docref(
            req,
            Level::Warning,
            &format!(
                "Failed to create session data file path. Too short session ID, invalid save_path or path length exceeds {MAXPATHLEN} characters"
            ),
        );
        return;
    };
    if let Some(files) = data(req) {
        files.last_key = Some(key.to_vec());
    }
    let host = req.environment().absolutize(&path);
    let existed = std::fs::symlink_metadata(&host).is_ok();
    let filemode = data(req).map_or(0o600, |d| d.filemode);
    let mode = u32::try_from(filemode & 0o7777).unwrap_or(0o600);
    let opened = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .mode(mode)
        .custom_flags(O_NOFOLLOW)
        .open(&host);
    let file = match opened {
        Ok(file) => file,
        Err(error) => {
            let text = errno_text(req, &error);
            docref(
                req,
                Level::Warning,
                &format!(
                    "open({}, O_RDWR) failed: {text}",
                    String::from_utf8_lossy(&path)
                ),
            );
            return;
        }
    };
    // `zend_fstat` over the fd: a file neither root's nor the process's
    // is another web application's, and is not taken.
    let owner = file
        .metadata()
        .map(|meta| std::os::unix::fs::MetadataExt::uid(&meta));
    if owner.is_err() || owner.is_ok_and(|owner| foreign(owner, uids())) {
        drop(file);
        docref(
            req,
            Level::Warning,
            "Session data file is not created by your uid",
        );
        return;
    }
    // The mode as php's process has it: `open(2)` takes the process's own
    // umask off, and `umask()` sets the request's.
    if !existed {
        file.set_permissions(Permissions::from_mode(mode & !req.environment().umask()))
            .ok();
    }
    file.lock().ok();
    if let Some(files) = data(req) {
        files.fd = Some(file);
    }
}

/// `ps_files_open`'s ownership rule: a session file whose owner is
/// neither root nor the process's real or effective uid belongs to
/// another uid, unless the process's real uid is root, which reads every
/// session.
fn foreign(owner: u32, uids: Uids) -> bool {
    owner != 0 && owner != uids.real && owner != uids.effective && uids.real != 0
}

/// The process's real and effective user ids.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Uids {
    real: u32,
    effective: u32,
}

/// `getuid()` and `geteuid()`, the engine's (`php_ext::process`).
fn uids() -> Uids {
    Uids {
        real: process::uid(),
        effective: process::euid(),
    }
}

/// `ps_files_write`: the file for `key` reopened where the id changed,
/// cut to nothing where the data is shorter than what was read, and
/// written from offset zero.
#[allow(clippy::unnecessary_wraps)] // a save handler operation's shape, which `user`'s fills
fn write_key(req: &mut Request, key: &[u8], val: &[u8]) -> Result<bool, Thrown> {
    open_key(req, key);
    let Some(files) = data(req) else {
        return Ok(false);
    };
    let Some(file) = files.fd.as_ref() else {
        return Ok(false);
    };
    if i64::try_from(val.len()).unwrap_or(i64::MAX) < files.st_size {
        file.set_len(0).ok();
    }
    let written = file.write_at(val, 0);
    match written {
        Ok(n) if n == val.len() => Ok(true),
        Ok(_) => {
            docref(req, Level::Warning, "Write wrote less bytes than requested");
            Ok(false)
        }
        Err(error) => {
            let text = errno_text(req, &error);
            docref(req, Level::Warning, &format!("Write failed: {text}"));
            Ok(false)
        }
    }
}

/// `PS_OPEN_FUNC(files)`: `session.save_path` read as `[N;[MODE;]]DIR`,
/// the temporary directory for an empty one.
///
/// ```php
/// @mkdir(__DIR__ . "/a"); @mkdir(__DIR__ . "/a/b");
/// session_save_path("1;" . __DIR__ . "/a"); session_id("bcd"); var_dump(session_start(["use_cookies" => 0, "cache_limiter" => ""]));
/// $_SESSION["x"] = 1; session_write_close();
/// var_dump(file_exists(__DIR__ . "/a/b/sess_bcd"));
/// session_save_path("1;600;" . __DIR__ . "/nope"); session_id("bcd"); var_dump(session_start());
/// ```
/// ```expect
/// bool(true)
/// bool(true)
///
/// Warning: session_save_path(): Session save path cannot be changed after headers have already been sent (sent from %s on line 3) in %s on line 6
/// bool(true)
/// ```
pub(super) fn open(req: &mut Request, save_path: &[u8]) -> bool {
    let path = if save_path.is_empty() {
        let dir = req.environment().temp_dir();
        let dir = dir.to_string_lossy();
        let trimmed = dir.trim_end_matches('/');
        let dir = if trimmed.is_empty() {
            b"/".to_vec()
        } else {
            trimmed.as_bytes().to_vec()
        };
        // The temporary directory is held to the guard, which warns.
        if req
            .environment_mut()
            .check_open_basedir(&dir, true)
            .is_err()
        {
            return false;
        }
        dir
    } else {
        save_path.to_vec()
    };
    let mut parts: Vec<&[u8]> = path.splitn(3, |&b| b == b';').collect();
    let dir = parts.pop().unwrap_or(b"");
    let mut dirdepth = 0;
    let mut filemode = 0o600;
    if let Some(first) = parts.first() {
        let Some(n) = strtol(first, 10) else {
            raise(
                req,
                Level::Warning,
                "The first parameter in session.save_path is invalid",
            );
            return false;
        };
        dirdepth = usize::try_from(n).unwrap_or(usize::MAX);
    }
    if let Some(second) = parts.get(1) {
        let Some(mode @ 0..=0o7777) = strtol(second, 8) else {
            raise(
                req,
                Level::Warning,
                "The second parameter in session.save_path is invalid",
            );
            return false;
        };
        filemode = mode;
    }
    close(req);
    req.state::<State>().files = Some(Files {
        last_key: None,
        basedir: dir.to_vec(),
        dirdepth,
        st_size: 0,
        filemode,
        fd: None,
    });
    true
}

/// `ZEND_STRTOL(s, NULL, base)`: the leading number, `None` for
/// `ERANGE`. Kept beside the face's `strtol`, which reads base 10 and
/// saturates where `ps_files_save_path` reads base 8 too and refuses the
/// range error; the space it skips is the face's, C's `isspace` under the
/// thread's `LC_CTYPE`.
fn strtol(s: &[u8], base: u32) -> Option<i64> {
    let text = &s[s.iter().take_while(|b| is_space(**b)).count()..];
    let (negative, body) = match text.first() {
        Some(b'-') => (true, &text[1..]),
        Some(b'+') => (false, &text[1..]),
        _ => (false, text),
    };
    let digits = body
        .iter()
        .take_while(|b| char::from(**b).is_digit(base))
        .count();
    if digits == 0 {
        return Some(0);
    }
    let n = i64::from_str_radix(std::str::from_utf8(&body[..digits]).ok()?, base).ok()?;
    Some(if negative { -n } else { n })
}

/// `PS_CLOSE_FUNC(files)`: the file and the handler's data given up.
pub(super) fn close(req: &mut Request) {
    close_fd(req);
    req.state::<State>().files = None;
}

/// `PS_READ_FUNC(files)`: the whole file, the empty string for a new
/// one; `None` where it could not be opened or read.
///
/// ```php
/// ob_start(); ini_set("session.save_path", __DIR__); ini_set("session.use_cookies", 0);
/// file_put_contents(__DIR__ . "/sess_abc", 'a|s:1:"b";n|i:3;');
/// session_id("abc"); session_start(); var_dump(session_encode());
/// session_id("bad*id");
/// session_write_close();
/// session_id("xyz"); session_start(); var_dump(session_encode(), file_exists(__DIR__ . "/sess_xyz"));
/// session_write_close(); ob_end_flush();
/// ```
/// ```expect
/// string(16) "a|s:1:"b";n|i:3;"
///
/// Warning: session_id(): Session ID cannot be changed when a session is active (started from %s on line 4) in %s on line 5
/// bool(false)
/// bool(true)
/// ```
#[allow(clippy::unnecessary_wraps)] // a save handler operation's shape, which `user`'s fills
pub(super) fn read(req: &mut Request, key: &[u8]) -> Result<Option<Vec<u8>>, Thrown> {
    open_key(req, key);
    let Some(files) = data(req) else {
        return Ok(None);
    };
    let Some(file) = files.fd.as_ref() else {
        return Ok(None);
    };
    let Ok(stat) = file.metadata() else {
        return Ok(None);
    };
    let size = stat.len();
    files.st_size = i64::try_from(size).unwrap_or(i64::MAX);
    if size == 0 {
        return Ok(Some(Vec::new()));
    }
    let mut bytes = vec![0u8; usize::try_from(size).unwrap_or(0)];
    match file.read_at(&mut bytes, 0) {
        Ok(n) if n == bytes.len() => Ok(Some(bytes)),
        Ok(_) => {
            docref(
                req,
                Level::Warning,
                "Read returned less bytes than requested",
            );
            Ok(None)
        }
        Err(error) => {
            let text = errno_text(req, &error);
            docref(req, Level::Warning, &format!("Read failed: {text}"));
            Ok(None)
        }
    }
}

/// `PS_WRITE_FUNC(files)`: the data written from offset zero, `lseek(fd,
/// 0, SEEK_SET)` after the read left the file at its end, over a file cut
/// to nothing first where the data is shorter than what was read.
///
/// ```php
/// ob_start(); session_save_path(__DIR__); session_id("seek1"); session_start(["use_cookies" => 0, "cache_limiter" => ""]);
/// session_decode('a|i:1;b|i:22;'); session_write_close();
/// session_start(); session_decode('b|i:33;'); session_write_close(); var_dump(file_get_contents(__DIR__ . "/sess_seek1"));
/// session_start(); session_decode('b|i:4;'); session_write_close(); var_dump(file_get_contents(__DIR__ . "/sess_seek1"));
/// ob_end_flush();
/// ```
/// ```expect
/// string(13) "a|i:1;b|i:33;"
/// string(12) "a|i:1;b|i:4;"
/// ```
pub(super) fn write(req: &mut Request, key: &[u8], val: &[u8]) -> Result<bool, Thrown> {
    write_key(req, key, val)
}

/// `PS_UPDATE_TIMESTAMP_FUNC(files)`: the file's modification time set
/// to now, and a file written where there is none yet, a new id's.
pub(super) fn update_timestamp(req: &mut Request, key: &[u8], val: &[u8]) -> Result<bool, Thrown> {
    let Some(path) = data(req).and_then(|d| path_create(d, key)) else {
        return Ok(false);
    };
    // `VCWD_UTIME(buf, newtime)`, and a file written where it fails.
    let now = std::time::SystemTime::now();
    let times = std::fs::FileTimes::new()
        .set_accessed(now)
        .set_modified(now);
    let host = req.environment().absolutize(&path);
    let touched = File::open(&host)
        .or_else(|_| OpenOptions::new().write(true).open(&host))
        .and_then(|file| file.set_times(times));
    if touched.is_err() {
        return write_key(req, key, val);
    }
    Ok(true)
}

/// `PS_DESTROY_FUNC(files)`: the open file closed and removed; a file
/// never written is not an error.
pub(super) fn destroy(req: &mut Request, key: &[u8]) -> bool {
    let Some(path) = data(req).and_then(|d| path_create(d, key)) else {
        return false;
    };
    if data(req).is_some_and(|d| d.fd.is_some()) {
        close_fd(req);
        if exists(req, &path).is_some() && !unlink(req, &path) {
            return exists(req, &path).is_none();
        }
    }
    true
}

/// `PS_GC_FUNC(files)` over `ps_files_cleanup_dir`: every `sess_` file
/// of the directory not modified for `maxlifetime` seconds removed, and
/// their count; `-1` for a split directory, which php leaves to cron.
///
/// ```php
/// ob_start(); ini_set("session.save_path", __DIR__); ini_set("session.use_cookies", 0); ini_set("session.gc_probability", 0);
/// touch(__DIR__ . "/sess_old", 1000); touch(__DIR__ . "/sess_new"); touch(__DIR__ . "/other", 1000);
/// session_start(); var_dump(session_gc(), file_exists(__DIR__ . "/sess_old"), file_exists(__DIR__ . "/sess_new"), file_exists(__DIR__ . "/other"));
/// session_write_close(); var_dump(session_gc()); ob_end_flush();
/// ```
/// ```expect
/// int(1)
/// bool(false)
/// bool(true)
/// bool(true)
///
/// Warning: session_gc(): Session cannot be garbage collected when there is no active session in %s on line 5
/// bool(false)
/// ```
pub(super) fn gc(req: &mut Request, maxlifetime: i64) -> i64 {
    let Some((dir, depth)) = data(req).map(|d| (d.basedir.clone(), d.dirdepth)) else {
        return -1;
    };
    if depth != 0 {
        return -1;
    }
    if exists(req, &dir).is_none() {
        docref(
            req,
            Level::Notice,
            &format!(
                "ps_files_cleanup_dir: opendir({}) failed: {} (2)",
                String::from_utf8_lossy(&dir),
                req.environment().os_error_text(2)
            ),
        );
        return -1;
    }
    let Ok(listing) = std::fs::read_dir(req.environment().absolutize(&dir)) else {
        return -1;
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
    let mut deleted = 0;
    for entry in listing.flatten() {
        let name = entry.file_name().into_encoded_bytes();
        if !name.starts_with(PREFIX) || name.len() + dir.len() + 2 >= MAXPATHLEN {
            continue;
        }
        let mut path = dir.clone();
        path.push(b'/');
        path.extend_from_slice(&name);
        if let Some(stat) = exists(req, &path)
            && now - stat.mtime() > maxlifetime
        {
            unlink(req, &path);
            deleted += 1;
        }
    }
    deleted
}

/// `ps_files_key_exists`.
fn key_exists(req: &mut Request, key: &[u8]) -> bool {
    let Some(path) = data(req).and_then(|d| path_create(d, key)) else {
        return false;
    };
    exists(req, &path).is_some()
}

/// `PS_CREATE_SID_FUNC(files)`: a new id with no file of its name, three
/// tries.
pub(super) fn create_sid(req: &mut Request) -> Option<Vec<u8>> {
    for _ in 0..4 {
        let Some(sid) = session::create_id(req) else {
            continue;
        };
        if data(req).is_none() || !key_exists(req, &sid) {
            return Some(sid);
        }
    }
    None
}

/// `PS_VALIDATE_SID_FUNC(files)`: whether the id's file exists.
pub(super) fn validate_sid(req: &mut Request, key: &[u8]) -> bool {
    key_exists(req, key)
}

#[cfg(test)]
mod tests {
    use super::{Uids, foreign, uids};
    use std::os::unix::fs::MetadataExt;

    /// `ps_files_open`'s rule over the owners it tells apart: root's, the
    /// real uid's and the effective uid's taken, another's refused, and
    /// every owner taken by a process whose real uid is root.
    #[test]
    fn a_file_another_uid_owns_is_refused_unless_root_reads_it() {
        let user = Uids {
            real: 501,
            effective: 502,
        };
        assert!(!foreign(0, user));
        assert!(!foreign(501, user));
        assert!(!foreign(502, user));
        assert!(foreign(503, user));
        let root = Uids {
            real: 0,
            effective: 0,
        };
        assert!(!foreign(503, root));
    }

    /// A file this process creates is its own. A file another uid owns
    /// cannot be built from here: a process that is not root cannot give
    /// one away, and one that is root reads every session, so the rule
    /// above holds the refusal.
    #[test]
    fn the_process_owns_what_it_creates() -> std::io::Result<()> {
        let files = php_ext::test::Files::new("session-owner", &[("created", "")]);
        let owner = std::fs::metadata(files.path().join("created"))?.uid();
        assert!(!foreign(owner, uids()));
        Ok(())
    }
}
