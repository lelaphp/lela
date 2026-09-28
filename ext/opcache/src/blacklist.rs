//! `zend_accelerator_blacklist.c` at the tag: the files the unit cache
//! leaves alone, read once at startup from the files
//! `opcache.blacklist_filename` names. php compiles the entries into one
//! anchored pattern, `^(entry|entry|…)`, each entry's `?` one byte that is
//! not a slash, `*` any number of them, `**` any bytes at all, and every other byte
//! itself; a path is blacklisted where a prefix of it matches an entry,
//! which [`prefix_matches`] answers without the regular expression.

use std::path::{Path, PathBuf};

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

    /// `zend_accel_blacklist_loadone`: each line less its line ending,
    /// the carriage returns before it and one pair of surrounding
    /// quotes; an empty line and a `;` comment skipped; a relative entry
    /// expanded against the directory the file really is in.
    fn load_one(&mut self, file: &Path) {
        let Ok(text) = std::fs::read(file) else {
            return;
        };
        let base = std::fs::canonicalize(file)
            .ok()
            .and_then(|real| real.parent().map(Path::to_path_buf));
        for line in text.split(|b| *b == b'\n') {
            let line = line.strip_suffix(b"\r").unwrap_or(line);
            let start = line.iter().position(|b| *b != b'\r').unwrap_or(line.len());
            let mut line = &line[start..];
            if line.len() >= 2 && line.starts_with(b"\"") && line.ends_with(b"\"") {
                line = &line[1..line.len() - 1];
            }
            if line.is_empty() || line.starts_with(b";") {
                continue;
            }
            self.entries.push(expand(line, base.as_deref()));
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

/// `expand_filepath_ex` under `CWD_EXPAND`: `path` against `base`, or the
/// process's directory where the file had none, with `.` and `..` taken
/// out and no link followed.
fn expand(path: &[u8], base: Option<&Path>) -> Vec<u8> {
    let joined = if path.starts_with(b"/") {
        path.to_vec()
    } else {
        let base = base.map_or_else(
            || std::env::current_dir().unwrap_or_default(),
            Path::to_path_buf,
        );
        let mut joined = base.into_os_string().into_encoded_bytes();
        joined.push(b'/');
        joined.extend_from_slice(path);
        joined
    };
    let mut parts: Vec<&[u8]> = Vec::new();
    for part in joined.split(|b| *b == b'/') {
        match part {
            b"" | b"." => {}
            b".." => {
                parts.pop();
            }
            part => parts.push(part),
        }
    }
    let mut expanded = Vec::with_capacity(joined.len());
    for part in parts {
        expanded.push(b'/');
        expanded.extend_from_slice(part);
    }
    if expanded.is_empty() {
        expanded.push(b'/');
    }
    expanded
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
    use super::{expand, fnmatch, prefix_matches};
    use std::path::Path;

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
            expand(b"./current.php", Some(Path::new("/t/d"))),
            b"/t/d/current.php"
        );
        assert_eq!(expand(b"../x.inc", Some(Path::new("/t/d"))), b"/t/x.inc");
        assert_eq!(expand(b"/abs//y", None), b"/abs/y");
    }

    #[test]
    fn a_glob_component_is_fnmatch() {
        assert!(fnmatch(b"opcache-*.blacklist", b"opcache-1.blacklist"));
        assert!(fnmatch(b"a[0-9]?", b"a7z"));
        assert!(!fnmatch(b"a[!0-9]", b"a7"));
    }
}
