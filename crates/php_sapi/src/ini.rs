//! The line's `-d` entries as a host reads them before the engine does
//! (`Z/main/php_ini_builder.c`): split as `php_ini_builder_define` splits
//! them; the few a host reads itself once the engine has started, php's
//! CGI its `cgi.*` entries and the built-in server `cli_server.color`,
//! read from the engine as `INI_BOOL` reads them; and
//! `php_init_config`'s search for the php.ini files the engine reads
//! (design §4.1, revision 57).

use std::ffi::OsString;

use crate::cgi::path_of;
use crate::{Engine, IniFile, IniFiles, PHP_CONFIG_FILE_PATH, PHP_CONFIG_FILE_SCAN_DIR};

/// `php_ini_builder_define`'s split of one `-d`: `name=value` at the
/// first `=`, or `name=1` for a bare name. Its line, quoted or not, and what the INI
/// grammar reads from it are the engine's builder's.
#[must_use]
pub fn define(given: &str) -> (String, String) {
    match given.split_once('=') {
        Some((name, value)) => (name.to_owned(), value.to_owned()),
        None => (given.to_owned(), String::from("1")),
    }
}

/// The engine's entry `name` read as `OnUpdateBool` reads it: the value
/// php.ini, the line or the entry's default left it at startup.
#[must_use]
pub fn enabled(engine: &Engine, name: &str) -> bool {
    engine.ini_get(name.as_bytes()).is_some_and(|value| {
        let value = String::from_utf8_lossy(&value);
        let value = value.trim();
        ["on", "yes", "true"]
            .iter()
            .any(|word| value.eq_ignore_ascii_case(word))
            || value.parse::<i64>().is_ok_and(|n| n != 0)
    })
}

/// The CLI's `sapi_cli_ini_defaults` (`Z/sapi/cli/php_cli.c:408-412`),
/// set before any php.ini is read, so a file overrides it
/// ([`crate::EngineBuilder::ini_default`]).
pub const CLI_DEFAULTS: [(&str, &str); 1] = [("display_errors", "1")];

/// The CLI's `HARDCODED_INI` (`Z/sapi/cli/php_cli.c:116-121`), prepended
/// to its `-d` entries as its first `ini_entries`, so no php.ini
/// overrides it; php's built-in server, a host of its own, takes none.
pub const CLI_HARDCODED: [(&str, &str); 5] = [
    ("html_errors", "0"),
    ("implicit_flush", "1"),
    ("output_buffering", "0"),
    ("max_execution_time", "0"),
    ("max_input_time", "-1"),
];

/// The two switches of a host's line that steer `php_init_config`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Search {
    /// `-c`, `php_ini_path_override`: the file, and the search path.
    pub path: Option<Vec<u8>>,
    /// `-n`, `php_ini_ignore`: no search and no scan, though `-c` is read.
    pub ignore: bool,
}

/// `php_init_config`'s search (`Z/main/php_ini.c:428-714`) for the host
/// named `sapi`, which searches the cwd unless it sets
/// `php_ini_ignore_cwd`: `-c` as the file and the path, or else `PHPRC`
/// as both, then the cwd, the binary's directory and
/// `PHP_CONFIG_FILE_PATH`; the file opened directly, else
/// `php-<sapi>.ini` and then `php.ini` along the path; then each `.ini`
/// of each directory of `PHP_INI_SCAN_DIR`, or of the build's scan
/// directory, in `php_alphasort` order. Under `-n` only `-c` is read.
#[must_use]
pub fn search(sapi: &str, given: &Search, ignore_cwd: bool) -> IniFiles {
    let env = |name: &str| std::env::var_os(name).map(OsString::into_encoded_bytes);
    let (mut file, mut path) = (None, Vec::new());
    if let Some(given) = &given.path {
        (file, path) = (Some(given.clone()), given.clone());
    } else if !given.ignore {
        let mut add = |dir: &[u8]| {
            if !path.is_empty() {
                path.push(b':');
            }
            path.extend_from_slice(dir);
        };
        if let Some(rc) = env("PHPRC").filter(|rc| !rc.is_empty()) {
            add(&rc);
            file = Some(rc);
        }
        if !ignore_cwd {
            add(b".");
        }
        if let Some(binary) = std::env::current_exe()
            .and_then(std::fs::canonicalize)
            .ok()
            .map(|binary| binary.into_os_string().into_encoded_bytes())
        {
            match binary.iter().rposition(|b| *b == b'/') {
                Some(at) if at > 0 => add(&binary[..at]),
                _ => add(&binary),
            }
        }
        add(PHP_CONFIG_FILE_PATH.as_bytes());
    }
    let mut loaded = None;
    if !given.ignore || given.path.is_some() {
        loaded = file
            .filter(|file| !file.is_empty())
            .filter(|file| std::fs::metadata(path_of(file)).is_ok_and(|meta| !meta.is_dir()))
            .and_then(|file| {
                let bytes = std::fs::read(path_of(&file)).ok()?;
                let real = std::fs::canonicalize(path_of(&file)).ok()?;
                Some(IniFile::new(
                    real.into_os_string().into_encoded_bytes(),
                    bytes,
                ))
            })
            .or_else(|| along(format!("php-{sapi}.ini").as_bytes(), &path))
            .or_else(|| along(b"php.ini", &path));
    }
    let scan = env("PHP_INI_SCAN_DIR").unwrap_or_else(|| PHP_CONFIG_FILE_SCAN_DIR.into());
    if given.ignore || scan.is_empty() {
        return IniFiles::new(PHP_CONFIG_FILE_PATH.as_bytes(), loaded, None, Vec::new());
    }
    let mut scanned = Vec::new();
    for dir in scan.split(|b| *b == b':') {
        let dir = if dir.is_empty() {
            PHP_CONFIG_FILE_SCAN_DIR.as_bytes()
        } else {
            dir
        };
        let Ok(entries) = std::fs::read_dir(path_of(dir)) else {
            continue;
        };
        let mut names: Vec<Vec<u8>> = entries
            .filter_map(|entry| Some(entry.ok()?.file_name().into_encoded_bytes()))
            .filter(|name| name.ends_with(b".ini"))
            .collect();
        names.sort();
        for name in names {
            let mut file = dir.to_vec();
            if !dir.ends_with(b"/") {
                file.push(b'/');
            }
            file.extend_from_slice(&name);
            if std::fs::metadata(path_of(&file)).is_ok_and(|meta| meta.is_file())
                && let Ok(bytes) = std::fs::read(path_of(&file))
            {
                scanned.push(IniFile::new(file, bytes));
            }
        }
    }
    IniFiles::new(
        PHP_CONFIG_FILE_PATH.as_bytes(),
        loaded,
        Some(&scan),
        scanned,
    )
}

/// `php_fopen_with_path`: `name` opened in each directory of `path` until
/// one opens, the walk ending at an empty entry, or opened as given where
/// the path is empty; the path recorded as `CWD_EXPAND` writes it.
fn along(name: &[u8], path: &[u8]) -> Option<IniFile> {
    let tries: Vec<Vec<u8>> = if path.is_empty() {
        vec![name.to_vec()]
    } else {
        path.split(|b| *b == b':')
            .take_while(|dir| !dir.is_empty())
            .map(|dir| [dir, b"/", name].concat())
            .collect()
    };
    tries.into_iter().find_map(|tried| {
        let bytes = std::fs::read(path_of(&tried)).ok()?;
        Some(IniFile::new(expand(&tried), bytes))
    })
}

/// `CWD_EXPAND`: `path` made absolute against the cwd and its `.` and
/// `..` taken lexically, no link resolved.
fn expand(path: &[u8]) -> Vec<u8> {
    let mut whole = Vec::new();
    if !path.starts_with(b"/")
        && let Ok(cwd) = std::env::current_dir()
    {
        whole = cwd.into_os_string().into_encoded_bytes();
    }
    whole.push(b'/');
    whole.extend_from_slice(path);
    let mut parts: Vec<&[u8]> = Vec::new();
    for part in whole.split(|b| *b == b'/') {
        match part {
            b"" | b"." => {}
            b".." => {
                parts.pop();
            }
            part => parts.push(part),
        }
    }
    parts
        .iter()
        .flat_map(|part| [b"/".as_slice(), part])
        .flatten()
        .copied()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `-c` naming a directory finds `php-<sapi>.ini` before `php.ini`,
    /// a file is read as given, and `-n` beside it scans nothing.
    #[test]
    fn a_directory_given_by_c_is_searched_for_the_sapi_s_file_first() {
        let dir = std::env::temp_dir().join(format!("php-sapi-ini-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let _ = std::fs::write(dir.join("php.ini"), "a=1\n");
        let _ = std::fs::write(dir.join("php-cli.ini"), "a=2\n");
        let given = |path: &std::path::Path| Search {
            path: Some(path.as_os_str().as_encoded_bytes().to_vec()),
            ignore: true,
        };
        let found = search("cli", &given(&dir), true);
        assert!(
            found
                .loaded()
                .is_some_and(|path| path.ends_with(b"/php-cli.ini"))
        );
        let found = search("cgi-fcgi", &given(&dir.join("php.ini")), false);
        assert!(
            found
                .loaded()
                .is_some_and(|path| path.ends_with(b"/php.ini"))
        );
        assert_eq!((found.scan_dir(), found.scanned().count()), (None, 0));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_path_is_expanded_without_resolving_links() {
        assert_eq!(expand(b"/a/./b/../c/php.ini"), b"/a/c/php.ini");
    }
}
