//! `glob://`, `Z/main/streams/glob_wrapper.c`'s `php_glob_stream_wrapper`,
//! which php registers from `basic_functions.c` beside `php` and `file`:
//! a wrapper that opens no stream and whose directory is what
//! `php_glob` matches, which the engine serves as php's glob stream
//! ([`DirListing::Glob`]): each entry the part of a match after its last
//! `/`, the part before it the stream's path, and the matches its count.
//! `php_glob` is this crate's (design §4.10), so the wrapper is too,
//! registered at request start through the engine's one wrapper table;
//! the table lists it after the engine's own `data`, where php registers
//! `glob` before it.

use std::os::unix::ffi::OsStrExt;

use php_ext::raising::Thrown;
use php_ext::request::{DirListing, MAXPATHLEN, Request, StreamWrapper};

use super::php_glob;

/// The wrapper, `glob` under its label `glob`, with a directory opener
/// alone.
///
/// ```php
/// $d = __DIR__ . '/glob-wrapper-witness';
/// @mkdir($d); touch("$d/b.txt"); touch("$d/a.txt"); touch("$d/c.log");
/// $h = opendir("glob://$d/*.txt");
/// while (($e = readdir($h)) !== false) echo $e, "\n";
/// rewinddir($h); var_dump(readdir($h), stream_get_meta_data($h)['wrapper_type']);
/// chdir($d); var_dump(scandir('glob://*.log'), scandir('glob://nothing*'));
/// var_dump(@fopen('glob://*', 'r'), error_get_last()['message']);
/// foreach (glob("$d/*") as $f) unlink($f); rmdir($d);
/// ```
/// ```expect
/// a.txt
/// b.txt
/// string(5) "a.txt"
/// string(4) "glob"
/// array(1) {
///   [0]=>
///   string(5) "c.log"
/// }
/// array(0) {
/// }
/// bool(false)
/// string(76) "fopen(glob://*): Failed to open stream: wrapper does not support stream open"
/// ```
pub(crate) fn wrapper() -> StreamWrapper {
    StreamWrapper {
        scheme: String::from("glob"),
        label: "glob",
        is_url: false,
        open: None,
        open_dir: Some(open_dir),
        stat: None,
        unlink: None,
        rename: None,
        mkdir: None,
        rmdir: None,
        set_stat: None,
        stream_stat: None,
    }
}

/// `php_glob_stream_opener` (`Z/main/streams/glob_wrapper.c:213`): the
/// pattern after `glob://`, a relative one under the working directory as
/// php's thread-safe build prefixes it and strips the prefix again, the
/// matches `php_glob` makes of it with no flags, none for a pattern
/// nothing matches and a refusal for one it cannot read; those the
/// `open_basedir` guard refuses left out, silently; the matches and
/// the pattern handed to the engine's glob stream, which splits each as
/// it reads it (`php_glob_stream_path_split`).
#[allow(clippy::unnecessary_wraps)] // `StreamWrapper::open_dir`'s signature
fn open_dir(req: &mut Request, path: &[u8]) -> Result<Option<DirListing>, Thrown> {
    let path = path.strip_prefix(b"glob://").unwrap_or(path);
    let mut pattern = path.to_vec();
    let mut cwd_skip = 0;
    if pattern.first() != Some(&b'/') {
        let cwd = req.environment().cwd().as_os_str().as_bytes().to_vec();
        cwd_skip = cwd.len() + 1;
        let mut prefixed = cwd;
        prefixed.push(b'/');
        prefixed.extend_from_slice(&pattern);
        // `snprintf(work_pattern, MAXPATHLEN, ...)`.
        prefixed.truncate(MAXPATHLEN - 1);
        pattern = prefixed;
    }
    let matches = match php_glob::php_glob(&pattern, 0) {
        Ok(matches) => matches,
        Err(php_glob::PHP_GLOB_NOMATCH) => Vec::new(),
        Err(_) => return Ok(None),
    };
    // "if open_basedir in use, check and filter restricted paths": a
    // match the guard refuses is left out without a word.
    let matches = matches
        .into_iter()
        .map(|m| m.get(cwd_skip..).unwrap_or(&m[..]).to_vec())
        .filter(|m| req.environment_mut().check_open_basedir(m, false).is_ok())
        .collect();
    Ok(Some(DirListing::Glob {
        pattern: path.to_vec(),
        matches,
    }))
}
