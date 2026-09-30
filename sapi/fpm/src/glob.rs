//! `php_glob` under `PHP_GLOB_MARK` (`Z/main/php_glob.c`), what an
//! `include` of `php-fpm.conf` names its files with: each component of
//! the pattern holding `*`, `?` or `[` matched against its directory's
//! entries as `fnmatch` matches them, a leading `.` only by a leading
//! `.`, the paths sorted, and a directory marked with a trailing `/`.

use crate::conf::path;

/// Whether a component holds one of the pattern's characters.
fn magic(component: &[u8]) -> bool {
    component.iter().any(|b| matches!(b, b'*' | b'?' | b'['))
}

/// `fnmatch` of one component, `\` quoting the byte after it.
fn matches(pattern: &[u8], name: &[u8]) -> bool {
    if name.first() == Some(&b'.') && pattern.first() != Some(&b'.') {
        return false;
    }
    match_from(pattern, name)
}

fn match_from(pattern: &[u8], name: &[u8]) -> bool {
    let Some((&p, rest)) = pattern.split_first() else {
        return name.is_empty();
    };
    match p {
        b'*' => (0..=name.len()).any(|skip| match_from(rest, &name[skip..])),
        b'?' => !name.is_empty() && match_from(rest, &name[1..]),
        b'[' => {
            let Some((&c, tail)) = name.split_first() else {
                return false;
            };
            let Some(close) = rest
                .iter()
                .skip(1)
                .position(|b| *b == b']')
                .map(|at| at + 1)
            else {
                return c == b'[' && match_from(rest, tail);
            };
            let (negate, set) = match rest[0] {
                b'!' => (true, &rest[1..close]),
                _ => (false, &rest[..close]),
            };
            let mut hit = false;
            let mut i = 0;
            while i < set.len() {
                if i + 2 < set.len() && set[i + 1] == b'-' {
                    hit |= set[i] <= c && c <= set[i + 2];
                    i += 3;
                } else {
                    hit |= set[i] == c;
                    i += 1;
                }
            }
            hit != negate && match_from(&rest[close + 1..], tail)
        }
        b'\\' if !rest.is_empty() => {
            name.first() == Some(&rest[0]) && match_from(&rest[1..], &name[1..])
        }
        _ => name.first() == Some(&p) && match_from(rest, &name[1..]),
    }
}

/// Every path `pattern` names, sorted, a directory ending in `/`; none
/// where nothing matches.
pub(crate) fn glob(pattern: &[u8]) -> Vec<Vec<u8>> {
    let absolute = pattern.first() == Some(&b'/');
    let mut found: Vec<Vec<u8>> = vec![if absolute { b"/".to_vec() } else { Vec::new() }];
    for component in pattern.split(|b| *b == b'/').filter(|c| !c.is_empty()) {
        let mut next = Vec::new();
        for base in &found {
            let join = |name: &[u8]| {
                let mut path = base.clone();
                if !path.is_empty() && path.last() != Some(&b'/') {
                    path.push(b'/');
                }
                path.extend_from_slice(name);
                path
            };
            if !magic(component) {
                next.push(join(component));
                continue;
            }
            let dir = if base.is_empty() {
                b".".to_vec()
            } else {
                base.clone()
            };
            let Ok(entries) = std::fs::read_dir(path(&dir)) else {
                continue;
            };
            let mut names: Vec<Vec<u8>> = entries
                .filter_map(Result::ok)
                .map(|entry| {
                    use std::os::unix::ffi::OsStrExt;
                    entry.file_name().as_bytes().to_vec()
                })
                .filter(|name| matches(component, name))
                .collect();
            names.sort();
            next.extend(names.iter().map(|name| join(name)));
        }
        found = next;
    }
    let mut out: Vec<Vec<u8>> = found
        .into_iter()
        .filter_map(|found| {
            let meta = std::fs::symlink_metadata(path(&found)).ok()?;
            let mut found = found;
            if std::fs::metadata(path(&found)).is_ok_and(|m| m.is_dir()) || meta.is_dir() {
                found.push(b'/');
            }
            Some(found)
        })
        .collect();
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::matches;

    #[test]
    fn a_component_matches_as_fnmatch_matches_it() {
        assert!(matches(b"*.conf", b"www.conf"));
        assert!(!matches(b"*.conf", b".hidden.conf"));
        assert!(matches(b"w?w.[a-c]onf", b"www.conf"));
        assert!(!matches(b"[!w]*", b"www"));
    }
}
