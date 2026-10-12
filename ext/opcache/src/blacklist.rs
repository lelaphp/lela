//! `zend_accelerator_blacklist.c` at the tag: the files the unit cache
//! leaves alone, read once at startup from the files
//! `opcache.blacklist_filename` names. php compiles the entries into one
//! anchored pattern, `^(entry|entry|…)`, each entry's `?` one byte that is
//! not a slash, `*` any number of them, `**` any bytes at all, and every other byte
//! itself; a path is blacklisted where a prefix of it matches an entry,
//! which [`prefix_matches`] answers without the regular expression.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use php_ext::request::MAXPATHLEN;

/// The blacklist: each entry's path as `zend_accel_blacklist_loadone`
/// expanded it, in the order the files and their lines gave them.
#[derive(Debug, Default)]
pub(crate) struct Blacklist {
    entries: Vec<Vec<u8>>,
}

impl Blacklist {
    /// `zend_accel_blacklist_load`: every file the pattern `filename`
    /// globs to, in the sorted order `glob(3)` gives, each read line by
    /// line; nothing for an empty name, and nothing for a pattern that
    /// finds no file, whose warning php logs at a verbosity the default
    /// level does not reach.
    pub(crate) fn load(filename: &[u8]) -> Blacklist {
        let mut blacklist = Blacklist::default();
        if filename.is_empty() {
            return blacklist;
        }
        for file in glob(&String::from_utf8_lossy(filename)) {
            blacklist.load_one(&file);
        }
        blacklist
    }

    /// `zend_accel_blacklist_loadone`: each line as [`fgets`] hands it
    /// over, less its line ending, the carriage returns before it and one
    /// pair of surrounding quotes, which takes a lone `"` to nothing; an
    /// empty line and a `;` comment skipped; each entry expanded as
    /// [`expand`] resolves it, a relative one against the directory the
    /// file really is in. php expands into one buffer, so an entry it
    /// cannot expand is the one before it again, or empty.
    fn load_one(&mut self, file: &Path) {
        let Ok(text) = std::fs::read(file) else {
            return;
        };
        let base = std::fs::canonicalize(file)
            .ok()
            .and_then(|real| real.parent().map(Path::to_path_buf))
            .map(|base| base.into_os_string().into_encoded_bytes());
        let mut real_path = Vec::new();
        for line in fgets(&text) {
            let line = match line.strip_suffix(b"\n") {
                Some(line) => line.strip_suffix(b"\r").unwrap_or(line),
                None => line,
            };
            let start = line.iter().position(|b| *b != b'\r').unwrap_or(line.len());
            let mut line = &line[start..];
            if line.starts_with(b"\"") && line.ends_with(b"\"") {
                // php's `path_length -= 2`, which a lone `"` takes below one.
                line = line.get(1..line.len() - 1).unwrap_or_default();
            }
            if line.is_empty() || line.starts_with(b";") {
                continue;
            }
            if let Some(expanded) = expand(line, base.as_deref()) {
                real_path = expanded;
            }
            self.entries.push(real_path.clone());
        }
    }

    /// The entries, what `opcache_get_configuration()['blacklist']` lists.
    pub(crate) fn entries(&self) -> &[Vec<u8>] {
        &self.entries
    }

    /// `zend_accel_blacklist_is_blacklisted`: whether an entry matches a
    /// prefix of `path`.
    pub(crate) fn is_blacklisted(&self, path: &[u8]) -> bool {
        self.entries.iter().any(|entry| prefix_matches(entry, path))
    }
}

/// `fgets(buf, MAXPATHLEN, fp)` read to the end, each piece cut at its
/// first NUL as `strlen` reads it: a line with its `\n`, or the first
/// `MAXPATHLEN - 1` bytes of a longer one, whose rest is the next piece.
fn fgets(text: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut rest = text;
    std::iter::from_fn(move || {
        if rest.is_empty() {
            return None;
        }
        let most = rest.len().min(MAXPATHLEN - 1);
        let take = rest[..most]
            .iter()
            .position(|b| *b == b'\n')
            .map_or(most, |at| at + 1);
        let (piece, tail) = rest.split_at(take);
        rest = tail;
        let end = piece.iter().position(|b| *b == 0).unwrap_or(piece.len());
        Some(&piece[..end])
    })
}

/// How many links `tsrm_realpath_r` follows in one path before it gives
/// the path up, php's `LINK_MAX`: Linux's 127. macOS's is 32767, and a
/// path with more than 127 links is a loop that fails there too, later.
const LINK_MAX: u32 = 127;

/// `expand_filepath_ex`, `expand_filepath_with_mode` under
/// `CWD_FILEPATH`: `path` against `base`, or the process's directory
/// where the file had none, resolved by [`virtual_file`]; `None` where php
/// returns `NULL` and leaves the entry it was writing as it was.
fn expand(path: &[u8], base: Option<&[u8]>) -> Option<Vec<u8>> {
    if path.is_empty() {
        return None;
    }
    let cwd = if path.starts_with(b"/") {
        Vec::new()
    } else if let Some(base) = base {
        if base.len() > MAXPATHLEN - 1 {
            return None;
        }
        base.to_vec()
    } else if let Ok(cwd) = std::env::current_dir() {
        cwd.into_os_string().into_encoded_bytes()
    } else if std::fs::File::open(Path::new(OsStr::from_bytes(path))).is_ok() {
        // php hands back the relative path as written where `getcwd()`
        // fails and the file opens.
        return Some(path[..path.len().min(MAXPATHLEN - 1)].to_vec());
    } else {
        Vec::new()
    };
    virtual_file(&cwd, path)
}

/// `virtual_file_ex` on Unix: `path` joined to `cwd` unless absolute,
/// resolved by [`realpath_r`], a trailing slash kept where it was
/// written, and `.` for a relative path that resolves to nothing.
fn virtual_file(cwd: &[u8], path: &[u8]) -> Option<Vec<u8>> {
    if path.is_empty() || path.len() >= MAXPATHLEN - 1 {
        return None;
    }
    let mut resolved = vec![0u8; MAXPATHLEN];
    let mut start = 1;
    let len = if path.starts_with(b"/") {
        resolved[..path.len()].copy_from_slice(path);
        path.len()
    } else if cwd.is_empty() {
        start = 0;
        resolved[..path.len()].copy_from_slice(path);
        path.len()
    } else {
        if path.len() + cwd.len() + 1 >= MAXPATHLEN - 1 {
            return None;
        }
        resolved[..cwd.len()].copy_from_slice(cwd);
        let mut at = cwd.len();
        if !cwd.ends_with(b"/") {
            resolved[at] = b'/';
            at += 1;
        }
        resolved[at..at + path.len()].copy_from_slice(path);
        at + path.len()
    };
    let add_slash = resolved[len - 1] == b'/';
    let mut links = 0;
    let mut len = realpath_r(&mut resolved, start, len, &mut links, false)?;
    if start == 0 && len == 0 {
        resolved[len] = b'.';
        len += 1;
    }
    if add_slash && len > 0 && resolved[len - 1] != b'/' {
        if len >= MAXPATHLEN - 1 {
            return None;
        }
        resolved[len] = b'/';
        len += 1;
    }
    resolved.truncate(len);
    Some(resolved)
}

/// `tsrm_realpath_r` under `CWD_FILEPATH`, without the realpath cache,
/// which changes no answer: `path[..len]` resolved in place from its last
/// component back, `path[..start]` left alone. An empty component and
/// `.` are dropped; `..` drops the component before it once that is
/// resolved, so it climbs out of where a link led; a link is replaced by
/// its target, resolved in turn; a component that does not exist is kept
/// as written; one that exists and is not a directory where a directory
/// is due fails the path. The length resolved, or `None` where php
/// answers `-1`.
fn realpath_r(
    path: &mut [u8],
    start: usize,
    mut len: usize,
    links: &mut u32,
    mut is_dir: bool,
) -> Option<usize> {
    loop {
        if len <= start {
            return Some(start);
        }
        let mut i = len;
        while i > start && path[i - 1] != b'/' {
            i -= 1;
        }
        if i == len || (i + 1 == len && path[i] == b'.') {
            len = i.saturating_sub(1);
            is_dir = true;
            continue;
        }
        if i + 2 == len && path[i] == b'.' && path[i + 1] == b'.' {
            return parent(path, start, i, links);
        }
        let tmp = path[..len].to_vec();
        let meta = std::fs::symlink_metadata(Path::new(OsStr::from_bytes(&tmp))).ok();
        let j = if meta.as_ref().is_some_and(std::fs::Metadata::is_symlink) {
            *links += 1;
            if *links > LINK_MAX {
                return None;
            }
            let target = std::fs::read_link(Path::new(OsStr::from_bytes(&tmp))).ok()?;
            let target = target.as_os_str().as_bytes();
            let j = target.len();
            if j >= MAXPATHLEN {
                return None;
            }
            path[..j].copy_from_slice(target);
            if target.starts_with(b"/") {
                realpath_r(path, 1, j, links, is_dir)?
            } else {
                if i == 0 || i + j >= MAXPATHLEN - 1 {
                    return None;
                }
                path.copy_within(..j, i);
                path[..i - 1].copy_from_slice(&tmp[..i - 1]);
                path[i - 1] = b'/';
                realpath_r(path, start, i + j, links, is_dir)?
            }
        } else {
            if meta.is_some_and(|meta| is_dir && !meta.is_dir()) {
                return None;
            }
            let mut j = start;
            if i > start + 1 {
                // Some leading directories may be inaccessible.
                j = realpath_r(path, start, i - 1, links, true)?;
                if j > start {
                    path[j] = b'/';
                    j += 1;
                }
            }
            if j + len >= MAXPATHLEN - 1 + i {
                return None;
            }
            path[j..j + len - i].copy_from_slice(&tmp[i..len]);
            j + len - i
        };
        return Some(j);
    }
}

/// The `..` branch of `tsrm_realpath_r`: the directory before the `..`
/// at `i`, resolved, less its last component; a relative path keeps a
/// leading `..` it cannot climb out of.
fn parent(path: &mut [u8], start: usize, i: usize, links: &mut u32) -> Option<usize> {
    if i <= start + 1 {
        return Some(if start > 0 { start } else { i + 2 });
    }
    let mut j = realpath_r(path, start, i - 1, links, true)?;
    if j > start {
        j -= 1;
        while j > start && path[j] != b'/' {
            j -= 1;
        }
        if start == 0 {
            if j == 0 && path.starts_with(b"../") {
                path[3..6].copy_from_slice(b"../");
                j = 5;
            } else if j > 0 && path[j + 1..].starts_with(b"../") {
                j += 4;
                path[j..j + 3].copy_from_slice(b"../");
                j += 2;
            }
        }
    } else if start == 0 && j == 0 {
        path[..3].copy_from_slice(b"../");
        j = 2;
    }
    Some(j)
}

/// Whether `entry`, read as `zend_accel_blacklist_update_regexp` writes it
/// into the pattern, matches a prefix of `path`.
///
/// The entry `/a/?b` is `^/a/[^/]b`, `/a/*/c` is `^/a/[^/]*/c`, and
/// `/a/**` is `^/a/.*`; the pattern is anchored at the start alone.
fn prefix_matches(entry: &[u8], path: &[u8]) -> bool {
    match entry {
        [] => true,
        [b'*', b'*', rest @ ..] => (0..=path.len()).any(|at| prefix_matches(rest, &path[at..])),
        [b'*', rest @ ..] => {
            let run = path.iter().position(|b| *b == b'/').unwrap_or(path.len());
            (0..=run).any(|at| prefix_matches(rest, &path[at..]))
        }
        [b'?', rest @ ..] => {
            matches!(path.first(), Some(b) if *b != b'/') && prefix_matches(rest, &path[1..])
        }
        [byte, rest @ ..] => path.first() == Some(byte) && prefix_matches(rest, &path[1..]),
    }
}

/// `php_glob(pattern, 0)`: the paths that match, sorted, a component
/// with `*`, `?` or `[` read against its directory's names, a dot file
/// found only by a component that starts with a dot.
fn glob(pattern: &str) -> Vec<PathBuf> {
    let absolute = pattern.starts_with('/');
    let mut found = vec![if absolute {
        PathBuf::from("/")
    } else {
        PathBuf::new()
    }];
    let components: Vec<&str> = pattern.split('/').filter(|c| !c.is_empty()).collect();
    for component in components {
        let wild = component.contains(['*', '?', '[']);
        let mut next = Vec::new();
        for dir in &found {
            if !wild {
                next.push(dir.join(component));
                continue;
            }
            let listed = if dir.as_os_str().is_empty() {
                std::fs::read_dir(".")
            } else {
                std::fs::read_dir(dir)
            };
            let Ok(listed) = listed else {
                continue;
            };
            let mut names: Vec<String> = listed
                .filter_map(Result::ok)
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|name| component.starts_with('.') || !name.starts_with('.'))
                .filter(|name| fnmatch(component.as_bytes(), name.as_bytes()))
                .collect();
            names.sort();
            next.extend(names.into_iter().map(|name| dir.join(name)));
        }
        found = next;
    }
    found.retain(|path| path.exists());
    found
}

/// `fnmatch(3)` over one path component: `*`, `?`, a bracket set with
/// ranges and `!` or `^` negation, and `\` quoting the next byte.
fn fnmatch(pattern: &[u8], name: &[u8]) -> bool {
    match pattern {
        [] => name.is_empty(),
        [b'*', rest @ ..] => (0..=name.len()).any(|at| fnmatch(rest, &name[at..])),
        [b'?', rest @ ..] => !name.is_empty() && fnmatch(rest, &name[1..]),
        [b'[', rest @ ..] => {
            let Some((&first, tail)) = name.split_first() else {
                return false;
            };
            let (negated, set) = match rest {
                [b'!' | b'^', set @ ..] => (true, set),
                set => (false, set),
            };
            let Some(close) = set.iter().skip(1).position(|b| *b == b']').map(|at| at + 1) else {
                return first == b'[' && fnmatch(rest, tail);
            };
            let class = &set[..close];
            let mut hit = false;
            let mut at = 0;
            while at < class.len() {
                if at + 2 < class.len() && class[at + 1] == b'-' {
                    hit |= (class[at]..=class[at + 2]).contains(&first);
                    at += 3;
                } else {
                    hit |= class[at] == first;
                    at += 1;
                }
            }
            hit != negated && fnmatch(&set[close + 1..], tail)
        }
        [b'\\', quoted, rest @ ..] => name.first() == Some(quoted) && fnmatch(rest, &name[1..]),
        [byte, rest @ ..] => name.first() == Some(byte) && fnmatch(rest, &name[1..]),
    }
}

#[cfg(test)]
mod tests {
    use super::{MAXPATHLEN, expand, fgets, fnmatch, prefix_matches};

    #[test]
    fn a_line_is_read_in_the_pieces_fgets_hands_over() {
        let mut long = vec![b'a'; MAXPATHLEN + 3];
        long.extend_from_slice(b"\nb\0c\nd");
        let pieces: Vec<&[u8]> = fgets(&long).collect();
        assert_eq!(pieces.len(), 4);
        assert_eq!(pieces[0].len(), MAXPATHLEN - 1);
        assert_eq!(pieces[1], b"aaaa\n");
        assert_eq!(pieces[2], b"b");
        assert_eq!(pieces[3], b"d");
    }

    #[test]
    fn an_entry_matches_a_prefix_with_php_s_wildcards() {
        assert!(prefix_matches(
            b"/tmp/path/?nocache.inc",
            b"/tmp/path/Xnocache.inc"
        ));
        assert!(!prefix_matches(
            b"/tmp/path/?nocache.inc",
            b"/tmp/path//nocache.inc"
        ));
        assert!(prefix_matches(
            b"/tmp/path/*/somedir",
            b"/tmp/path/a/somedir/f.php"
        ));
        assert!(!prefix_matches(
            b"/tmp/path/*/somedir",
            b"/tmp/path/a/b/somedir"
        ));
        assert!(prefix_matches(b"/tmp/**/x", b"/tmp/a/b/x"));
        assert!(prefix_matches(b"/path/to/foo", b"/path/to/foo2.php"));
        assert!(!prefix_matches(b"/path/to/foo", b"/path/to"));
    }

    #[test]
    fn a_relative_entry_is_expanded_against_the_file_s_directory() {
        assert_eq!(
            expand(b"./current.php", Some(b"/nonexistent-t1710/d")),
            Some(b"/nonexistent-t1710/d/current.php".to_vec())
        );
        assert_eq!(
            expand(b"../x.inc", Some(b"/nonexistent-t1710/d")),
            Some(b"/nonexistent-t1710/x.inc".to_vec())
        );
        assert_eq!(
            expand(b"/nonexistent-t1710//y/", None),
            Some(b"/nonexistent-t1710/y/".to_vec())
        );
        assert_eq!(expand(b"/", None), Some(b"/".to_vec()));
    }

    #[test]
    fn an_entry_under_a_link_is_expanded_where_the_link_leads() {
        let dir = std::env::temp_dir().join(format!("t1710-blacklist-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        let real = dir.join("a/real");
        assert!(std::fs::create_dir_all(&real).is_ok());
        assert!(std::os::unix::fs::symlink(&real, dir.join("link")).is_ok());
        assert!(std::fs::write(real.join("f"), b"").is_ok());
        let root = std::fs::canonicalize(&dir)
            .map(|root| root.into_os_string().into_encoded_bytes())
            .unwrap_or_default();
        let base = dir.clone().into_os_string().into_encoded_bytes();
        let resolved = |path: &[u8]| expand(path, Some(&base)).map(|p| p[root.len()..].to_vec());
        assert_eq!(resolved(b"link/c.php"), Some(b"/a/real/c.php".to_vec()));
        assert_eq!(resolved(b"link/none/x"), Some(b"/a/real/none/x".to_vec()));
        assert_eq!(resolved(b"link/../y"), Some(b"/a/y".to_vec()));
        assert_eq!(resolved(b"link/"), Some(b"/a/real/".to_vec()));
        assert_eq!(expand(b"link/f/g", Some(&base)), None);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_glob_component_is_fnmatch() {
        assert!(fnmatch(b"opcache-*.blacklist", b"opcache-1.blacklist"));
        assert!(fnmatch(b"a[0-9]?", b"a7z"));
        assert!(!fnmatch(b"a[!0-9]", b"a7"));
    }
}
