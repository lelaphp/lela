//! The built-in server's path rules (`Z/sapi/cli/php_cli_server.c`), which
//! `php -S` finds a request's file by and the production server finds its
//! static files by (#2346): the path, made `normalize_vpath`'s
//! (`php_proto::http`), translated under the document root as
//! `php_cli_server_request_translate_vpath` translates it, its extension
//! read as `php_cli_server_request_ctor` reads it, and the request a
//! program's where that extension is `php` and the path translated to a
//! file, a static file's otherwise, as `php_cli_server_dispatch` tells
//! them apart. No path leaves the root: `normalize_vpath` has taken every
//! `..`, decoded or not, before the root is put in front.

use crate::cgi::path_of;

/// A path translated under the document root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Translated {
    /// `vpath`: the path as far as something was found under the root,
    /// with the index file where that is a directory.
    pub vpath: Vec<u8>,
    /// `path_translated`: the root and `vpath`.
    pub path_translated: Vec<u8>,
    /// `path_info`: what was walked off the path to find it.
    pub path_info: Option<Vec<u8>>,
    /// `sb.st_size` of what was found.
    pub size: u64,
}

impl Translated {
    /// Whether the request it was translated for is a program's
    /// ([`is_program`]).
    #[must_use]
    pub fn is_program(&self) -> bool {
        is_program(extension(&self.vpath), true)
    }
}

/// `php_cli_server_request_translate_vpath`
/// (`Z/sapi/cli/php_cli_server.c:1440`): `vpath` under `root`, walked back
/// a segment at a time until something is there, what was walked off the
/// path's info; a directory is its `index.php` or `index.html`, and one
/// with neither translates to nothing.
#[must_use]
pub fn translate(vpath: &[u8], root: &[u8]) -> Option<Translated> {
    let mut buf = root.to_vec();
    let start = buf.len();
    if !vpath.is_empty() && vpath[0] != b'/' {
        buf.push(b'/');
    }
    buf.extend_from_slice(vpath);
    let end = buf.len();
    let mut q = end;
    let mut path_info: Option<Vec<u8>> = None;
    let mut size = 0;
    while q > 0 {
        if let Ok(meta) = std::fs::metadata(path_of(&buf[..q])) {
            size = meta.len();
            if meta.is_dir() {
                let mut dir = buf[..q].to_vec();
                if dir.last() != Some(&b'/') {
                    dir.push(b'/');
                }
                let (candidate, len) =
                    [&b"index.php"[..], b"index.html"]
                        .into_iter()
                        .find_map(|file| {
                            let candidate = [dir.as_slice(), file].concat();
                            let meta = std::fs::metadata(path_of(&candidate)).ok()?;
                            meta.is_file().then_some((candidate, meta.len()))
                        })?;
                size = len;
                buf = candidate;
                q = buf.len();
            }
            break;
        }
        let cut = buf[..q].iter().rposition(|&b| b == b'/').unwrap_or(0);
        path_info = Some(buf[cut..end.max(cut)].to_vec());
        q = cut;
    }
    let q = q.max(start.min(buf.len()));
    Some(Translated {
        vpath: buf[start.min(q)..q].to_vec(),
        path_translated: buf[..q].to_vec(),
        path_info,
        size,
    })
}

/// The request's extension, as `php_cli_server_request_ctor` reads it: what
/// follows the last `.` of `vpath`, a `/` after it or not, and nothing
/// where it has none.
#[must_use]
pub fn extension(vpath: &[u8]) -> &[u8] {
    vpath
        .iter()
        .rposition(|&b| b == b'.')
        .map_or(&[], |dot| &vpath[dot + 1..])
}

/// Whether a request is a program's, by `php_cli_server_dispatch`'s rule
/// (`Z/sapi/cli/php_cli_server.c:2294`): its extension is `php`, without
/// regard to case, and its path translated to a file; every other
/// request is a static file's, so no PHP source is served as one.
#[must_use]
pub fn is_program(extension: &[u8], translated: bool) -> bool {
    extension.eq_ignore_ascii_case(b"php") && translated
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A file is found as named; a path past a file is that file with
    /// the rest its path's info; a directory is its index; and a
    /// directory with no index translates to nothing.
    #[test]
    fn a_path_translates_to_the_file_it_names_under_the_root() {
        let root = std::env::temp_dir().join(format!("php-sapi-vpath-{}", std::process::id()));
        std::fs::remove_dir_all(&root).ok();
        for dir in ["a", "empty"] {
            std::fs::create_dir_all(root.join(dir)).ok();
        }
        std::fs::write(root.join("a/index.html"), "<p>").ok();
        std::fs::write(root.join("t.php"), "<?php").ok();
        let at = root.as_os_str().as_encoded_bytes();
        let joined = |tail: &str| [at, tail.as_bytes()].concat();
        let file = translate(b"/t.php", at);
        assert_eq!(
            file,
            Some(Translated {
                vpath: b"/t.php".to_vec(),
                path_translated: joined("/t.php"),
                path_info: None,
                size: 5,
            })
        );
        let past = translate(b"/t.php/x/y", at);
        assert_eq!(past.as_ref().map(|t| &t.vpath[..]), Some(&b"/t.php"[..]));
        assert_eq!(past.and_then(|t| t.path_info), Some(b"/x/y".to_vec()));
        let index = translate(b"/a/", at);
        assert_eq!(index.map(|t| t.vpath), Some(b"/a/index.html".to_vec()));
        assert_eq!(translate(b"/empty", at), None);
        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn the_extension_follows_the_last_dot_and_only_php_with_a_file_is_a_program() {
        assert_eq!(extension(b"/a.b/c"), b"b/c");
        assert_eq!(extension(b"/x.PHP"), b"PHP");
        assert_eq!(extension(b"/none"), b"");
        assert!(is_program(b"PHP", true));
        assert!(!is_program(b"php", false));
        assert!(!is_program(b"phps", true));
    }
}
