//! `mod_files.c`, the `files` save handler: one file `sess_<id>` per
//! session under `session.save_path`, `N;MODE;DIR` splitting it into `N`
//! levels of one-character directories, opened create-and-read-write,
//! held under `flock(LOCK_EX)` from the read to the close, read and
//! written from offset zero, and collected by modification time. The
//! file is opened through the streams facet's plain wrapper, so the
//! request's working directory and `open_basedir` are the engine's;
//! php's `O_NOFOLLOW` and its owner check are the plain wrapper's to
//! offer (#746).

use php_ext::raising::{Level, Thrown, docref, raise};
use php_ext::request::{Request, Stream, WrapperOp};

use crate::{State, session};

/// `FILE_PREFIX`.
const PREFIX: &[u8] = b"sess_";

/// `MAXPATHLEN` on the host.
#[cfg(target_os = "macos")]
const MAXPATHLEN: usize = 1024;
#[cfg(not(target_os = "macos"))]
const MAXPATHLEN: usize = 4096;

/// `LOCK_EX`.
const LOCK_EX: i64 = 2;

/// `ps_files`: what `PS_OPEN_FUNC(files)` sets up, the file it holds
/// open and the key it was opened for.
pub(crate) struct Files {
    last_key: Option<Vec<u8>>,
    basedir: Vec<u8>,
    dirdepth: usize,
    st_size: i64,
    filemode: i64,
    fd: Option<Stream>,
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
    if let Some(stream) = data(req).and_then(|d| d.fd.take()) {
        stream.close(req);
        req.release(stream.as_value());
    }
}

/// Whether `path` names something, `stat(2)` without the cache.
fn exists(req: &mut Request, path: &[u8]) -> Result<Option<[i64; 13]>, Thrown> {
    req.url_stat(path, false, false)
}

/// `ps_files_open`: the file for `key` opened and locked, unless it is
/// the one open already; a key php refuses, a path it cannot build and
/// an open that fails each warn and leave no file open.
fn open_key(req: &mut Request, key: &[u8]) -> Result<(), Thrown> {
    let Some(files) = data(req) else {
        return Ok(());
    };
    if files.fd.is_some() && files.last_key.as_deref() == Some(key) {
        return Ok(());
    }
    files.last_key = None;
    close_fd(req);
    if !session::valid_key(key) {
        docref(
            req,
            Level::Warning,
            "Session ID is too long or contains illegal characters. Only the A-Z, a-z, 0-9, \"-\", and \",\" characters are allowed",
        );
        return Ok(());
    }
    let Some(path) = data(req).and_then(|d| path_create(d, key)) else {
        docref(
            req,
            Level::Warning,
            &format!(
                "Failed to create session data file path. Too short session ID, invalid save_path or path length exceeds {MAXPATHLEN} characters"
            ),
        );
        return Ok(());
    };
    if let Some(files) = data(req) {
        files.last_key = Some(key.to_vec());
    }
    let dir = parent(&path);
    if exists(req, dir)?.is_none() {
        docref(
            req,
            Level::Warning,
            &format!(
                "open({}, O_RDWR) failed: {} (2)",
                String::from_utf8_lossy(&path),
                req.os_error_text(2)
            ),
        );
        return Ok(());
    }
    let created = exists(req, &path)?.is_none();
    let Some(stream) = req.open_stream(&path, b"c+b", false)? else {
        return Ok(());
    };
    if created {
        let mode = data(req).map_or(0o600, |d| d.filemode) & !i64::from(req.umask());
        req.wrapper_op(&path, WrapperOp::Chmod(mode))?;
    }
    stream.flock(req, LOCK_EX);
    if let Some(files) = data(req) {
        files.fd = Some(stream);
    }
    Ok(())
}

/// The directory part of a path, what `open(2)` needs to exist.
fn parent(path: &[u8]) -> &[u8] {
    match path.iter().rposition(|&b| b == b'/') {
        Some(0) => b"/",
        Some(at) => &path[..at],
        None => b".",
    }
}

/// `ps_files_write`: the file for `key` reopened where the id changed,
/// cut to nothing where the data is shorter than what was read, and
/// written from offset zero.
fn write_key(req: &mut Request, key: &[u8], val: &[u8]) -> Result<bool, Thrown> {
    open_key(req, key)?;
    let Some((stream, st_size)) = data(req).and_then(|d| d.fd.map(|fd| (fd, d.st_size))) else {
        return Ok(false);
    };
    if i64::try_from(val.len()).unwrap_or(i64::MAX) < st_size {
        stream.truncate(req, 0);
    }
    stream.seek(req, 0, None);
    match stream.write(req, val) {
        Some(n) if n == val.len() => {
            stream.flush(req);
            Ok(true)
        }
        Some(_) => {
            docref(req, Level::Warning, "Write wrote less bytes than requested");
            Ok(false)
        }
        None => {
            docref(req, Level::Warning, "Write failed: Input/output error (5)");
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
        let dir = req.temp_dir().0;
        let dir = dir.to_string_lossy();
        let trimmed = dir.trim_end_matches('/');
        if trimmed.is_empty() {
            b"/".to_vec()
        } else {
            trimmed.as_bytes().to_vec()
        }
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
/// `ERANGE`.
fn strtol(s: &[u8], base: u32) -> Option<i64> {
    let text = String::from_utf8_lossy(s);
    let text = text.trim_start();
    let (negative, body) = match text.as_bytes().first() {
        Some(b'-') => (true, &text[1..]),
        Some(b'+') => (false, &text[1..]),
        _ => (false, text),
    };
    let digits: String = body.chars().take_while(|c| c.is_digit(base)).collect();
    if digits.is_empty() {
        return Some(0);
    }
    let n = i64::from_str_radix(&digits, base).ok()?;
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
pub(super) fn read(req: &mut Request, key: &[u8]) -> Result<Option<Vec<u8>>, Thrown> {
    open_key(req, key)?;
    let Some(stream) = data(req).and_then(|d| d.fd) else {
        return Ok(None);
    };
    let Some(stat) = stream.stat(req) else {
        return Ok(None);
    };
    let size = stat[7];
    if let Some(files) = data(req) {
        files.st_size = size;
    }
    if size == 0 {
        return Ok(Some(Vec::new()));
    }
    stream.seek(req, 0, None);
    let wanted = usize::try_from(size).unwrap_or(usize::MAX);
    let got = stream.copy_to_mem(req, Some(wanted))?;
    let bytes = got.as_bytes().to_vec();
    req.release(php_ext::values::Value::string(got));
    if bytes.len() != wanted {
        docref(
            req,
            Level::Warning,
            "Read returned less bytes than requested",
        );
        return Ok(None);
    }
    Ok(Some(bytes))
}

/// `PS_WRITE_FUNC(files)`.
pub(super) fn write(req: &mut Request, key: &[u8], val: &[u8]) -> Result<bool, Thrown> {
    write_key(req, key, val)
}

/// `PS_UPDATE_TIMESTAMP_FUNC(files)`: the file's modification time set
/// to now, and a file written where there is none yet, a new id's.
pub(super) fn update_timestamp(req: &mut Request, key: &[u8], val: &[u8]) -> Result<bool, Thrown> {
    let Some(path) = data(req).and_then(|d| path_create(d, key)) else {
        return Ok(false);
    };
    if exists(req, &path)?.is_none() {
        return write_key(req, key, val);
    }
    req.wrapper_op(&path, WrapperOp::Touch(None))
}

/// `PS_DESTROY_FUNC(files)`: the open file closed and removed; a file
/// never written is not an error.
pub(super) fn destroy(req: &mut Request, key: &[u8]) -> Result<bool, Thrown> {
    let Some(path) = data(req).and_then(|d| path_create(d, key)) else {
        return Ok(false);
    };
    if data(req).is_some_and(|d| d.fd.is_some()) {
        close_fd(req);
        if exists(req, &path)?.is_some() && !req.wrapper_op(&path, WrapperOp::Unlink)? {
            return Ok(exists(req, &path)?.is_none());
        }
    }
    Ok(true)
}

/// `PS_GC_FUNC(files)` over `ps_files_cleanup_dir`: every `sess_` file
/// of the directory not modified for `maxlifetime` seconds removed, and
/// their count; `-1` for a split directory, which php leaves to cron.
///
/// ```php
/// ob_start(); ini_set("session.save_path", __DIR__); ini_set("session.use_cookies", 0);
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
pub(super) fn gc(req: &mut Request, maxlifetime: i64) -> Result<i64, Thrown> {
    let Some((dir, depth)) = data(req).map(|d| (d.basedir.clone(), d.dirdepth)) else {
        return Ok(-1);
    };
    if depth != 0 {
        return Ok(-1);
    }
    if exists(req, &dir)?.is_none() {
        docref(
            req,
            Level::Notice,
            &format!(
                "ps_files_cleanup_dir: opendir({}) failed: {} (2)",
                String::from_utf8_lossy(&dir),
                req.os_error_text(2)
            ),
        );
        return Ok(-1);
    }
    let Some(listing) = req.open_dir(&dir)? else {
        return Ok(-1);
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX));
    let mut deleted = 0;
    while let Some(entry) = listing.read_dir(req)? {
        let name = entry.as_bytes().to_vec();
        req.release(php_ext::values::Value::string(entry));
        if !name.starts_with(PREFIX) || name.len() + dir.len() + 2 >= MAXPATHLEN {
            continue;
        }
        let mut path = dir.clone();
        path.push(b'/');
        path.extend_from_slice(&name);
        if let Some(stat) = exists(req, &path)?
            && now - stat[9] > maxlifetime
        {
            req.wrapper_op(&path, WrapperOp::Unlink)?;
            deleted += 1;
        }
    }
    listing.close(req);
    req.release(listing.as_value());
    Ok(deleted)
}

/// `ps_files_key_exists`.
fn key_exists(req: &mut Request, key: &[u8]) -> Result<bool, Thrown> {
    let Some(path) = data(req).and_then(|d| path_create(d, key)) else {
        return Ok(false);
    };
    Ok(exists(req, &path)?.is_some())
}

/// `PS_CREATE_SID_FUNC(files)`: a new id with no file of its name, three
/// tries.
pub(super) fn create_sid(req: &mut Request) -> Result<Option<Vec<u8>>, Thrown> {
    for _ in 0..4 {
        let Some(sid) = session::create_id(req) else {
            continue;
        };
        if data(req).is_none() || !key_exists(req, &sid)? {
            return Ok(Some(sid));
        }
    }
    Ok(None)
}

/// `PS_VALIDATE_SID_FUNC(files)`: whether the id's file exists.
pub(super) fn validate_sid(req: &mut Request, key: &[u8]) -> Result<bool, Thrown> {
    key_exists(req, key)
}
