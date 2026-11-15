//! The built-in server's path rules (`Z/sapi/cli/php_cli_server.c`), which
//! `php -S` finds a request's file by and the production server routes a
//! request by where no segment of its path ends in `.php`;
//! where one does, the production server splits it as nginx's
//! recipe does instead ([`split_at_program`]). The path, made
//! [`normalize_vpath`](crate::http::normalize_vpath)'s, translated under
//! the document root as `php_cli_server_request_translate_vpath`
//! translates it, its extension read as `php_cli_server_request_ctor`
//! reads it, and the request a program's where that extension is `php`
//! and the path translated to a file, a static file's otherwise, as
//! `php_cli_server_dispatch` tells them apart. No path leaves the root:
//! `normalize_vpath` has taken every `..`, decoded or not, before the
//! root is put in front. The file system is asked through [`Files`], as
//! `init_request_info` asks it.

use crate::cgi::{Files, Stat};

/// A path translated under the document root, by [`translate`]'s walk or
/// [`split_at_program`]'s split.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Translated {
    /// `vpath`: the path as far as something was found under the root,
    /// with the index file where that is a directory; or as far as the
    /// split cut it.
    pub vpath: Vec<u8>,
    /// `path_translated`: the root and `vpath`.
    pub path_translated: Vec<u8>,
    /// `path_info`: what was walked off the path to find it, or what the
    /// split cut off; none where nothing was.
    pub path_info: Option<Vec<u8>>,
    /// `sb.st_size` of what was found; 0 for a split, which asks no disk
    /// and is a program's, whose request reads no size.
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

/// Whether a translated path holds a NUL, a path `php -S` refuses
/// (`can't handle paths that contain nul bytes`,
/// `Z/sapi/cli/php_cli_server.c:2127`, `:2155`): the walk's
/// `stat(2)` read it only up to the NUL ([`Files`]), so what it found is
/// not the file the path names. A static file's is refused with 400
/// (`php_cli_server_begin_send_static`); a program's request starts and
/// its program is never served (`php_cli_server_dispatch_script`), so it
/// ends with the head the request has, a 200's, and no body, while the
/// server logs the 400 of the page it built and never sent.
#[must_use]
pub fn holds_nul(path_translated: &[u8]) -> bool {
    path_translated.contains(&0)
}

/// `php_cli_server_request_translate_vpath`
/// (`Z/sapi/cli/php_cli_server.c:1440`): `vpath` under `root`, walked back
/// a segment at a time until `stat(2)` finds something there, what was
/// walked off the path's info; a directory is its `index.php` or
/// `index.html`, whichever is a regular file first, and one with neither
/// translates to nothing.
#[must_use]
pub fn translate(vpath: &[u8], root: &[u8], files: &dyn Files) -> Option<Translated> {
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
        if let Some(found) = files.stat(&buf[..q]) {
            if let Stat::File(len) | Stat::Other(len) = found {
                size = len;
            } else {
                let mut dir = buf[..q].to_vec();
                if dir.last() != Some(&b'/') {
                    dir.push(b'/');
                }
                let (candidate, len) =
                    [&b"index.php"[..], b"index.html"]
                        .into_iter()
                        .find_map(|file| {
                            let candidate = [dir.as_slice(), file].concat();
                            match files.stat(&candidate)? {
                                Stat::File(len) => Some((candidate, len)),
                                Stat::Directory | Stat::Other(_) => None,
                            }
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

/// `vpath` under `root` split as nginx's documented split splits a
/// request's path for php-fpm (`fastcgi_split_path_info
/// ^(.+?\.php)(/.*)$`, `oracle.toml`'s `[fastcgi]`): the program's path
/// as far as the first segment that ends in `.php`, in that case, with a
/// `/` or the path's end after it, and the rest its path's info. `vpath`
/// starts with a `/`, as [`normalize_vpath`](crate::http::normalize_vpath)
/// leaves it, so the pattern's `.+?` has its one byte before the `.php`.
/// The production server splits by it where `php -S` walks
/// ([`translate`]), whatever is on the disk, so a directory named `*.php`
/// is the program's path and no program is found in it. A path
/// holding a NUL is not split: nginx refuses one before any location.
#[must_use]
pub fn split_at_program(vpath: &[u8], root: &[u8]) -> Option<Translated> {
    if vpath.contains(&0) {
        return None;
    }
    let end = (b"/.php".len()..=vpath.len()).find(|&end| {
        vpath[..end].ends_with(b".php") && matches!(vpath.get(end), None | Some(b'/'))
    })?;
    let (name, info) = vpath.split_at(end);
    Some(Translated {
        vpath: name.to_vec(),
        path_translated: [root, name].concat(),
        path_info: (!info.is_empty()).then(|| info.to_vec()),
        size: 0,
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

    /// A file system of the paths it names, each a directory where it
    /// ends in `/` and a file of its length's bytes otherwise.
    struct Tree(&'static [&'static str]);

    impl Files for Tree {
        fn resolves(&self, path: &[u8]) -> bool {
            self.stat(path).is_some()
        }

        fn stat(&self, path: &[u8]) -> Option<Stat> {
            let path = path.strip_suffix(b"/").unwrap_or(path);
            self.0.iter().find_map(|had| match had.as_bytes() {
                dir if dir.strip_suffix(b"/") == Some(path) => Some(Stat::Directory),
                file => (file == path).then_some(Stat::File(file.len() as u64)),
            })
        }
    }

    const TREE: Tree = Tree(&[
        "/r/",
        "/r/a/",
        "/r/a/index.html",
        "/r/d.php/",
        "/r/d.php/index.php",
        "/r/empty/",
        "/r/t.php",
        "/r/up/",
        "/r/up/x.jpg",
    ]);

    /// A file is found as named; a path past a file is that file with
    /// the rest its path's info; a directory is its index; and a
    /// directory with no index translates to nothing.
    #[test]
    fn a_path_translates_to_the_file_it_names_under_the_root() {
        let file = translate(b"/t.php", b"/r", &TREE);
        assert_eq!(
            file,
            Some(Translated {
                vpath: b"/t.php".to_vec(),
                path_translated: b"/r/t.php".to_vec(),
                path_info: None,
                size: 8,
            })
        );
        let past = translate(b"/t.php/x/y", b"/r", &TREE);
        assert_eq!(past.as_ref().map(|t| &t.vpath[..]), Some(&b"/t.php"[..]));
        assert_eq!(past.and_then(|t| t.path_info), Some(b"/x/y".to_vec()));
        let index = translate(b"/a/", b"/r", &TREE);
        assert_eq!(index.map(|t| t.vpath), Some(b"/a/index.html".to_vec()));
        // php -S's walk runs a directory named `*.php`'s index, which the
        // production server's split does not.
        for path in [&b"/d.php/"[..], b"/d.php", b"/d.php/index.php"] {
            let found = translate(path, b"/r", &TREE);
            assert_eq!(found.map(|t| t.vpath), Some(b"/d.php/index.php".to_vec()));
        }
        assert_eq!(translate(b"/empty", b"/r", &TREE), None);
    }

    /// The walk stops at the first prefix that is there, so a program
    /// named past a file that is not one is that file's: `x.jpg` with
    /// `/y.php` its path's info, a static file's request.
    #[test]
    fn the_first_prefix_found_is_the_file_and_the_rest_its_path_s_info() {
        let found = translate(b"/up/x.jpg/y.php", b"/r", &TREE);
        assert_eq!(
            found.as_ref().map(|t| &t.vpath[..]),
            Some(&b"/up/x.jpg"[..])
        );
        assert_eq!(
            found.as_ref().and_then(|t| t.path_info.as_deref()),
            Some(&b"/y.php"[..])
        );
        assert_eq!(found.map(|t| t.is_program()), Some(false));
    }

    /// nginx's pattern's split: at the first `.php` that a `/` or the end
    /// follows, in that case only, whatever is on the disk; nothing where
    /// no segment ends so, or where the path holds a NUL.
    #[test]
    fn the_program_s_path_ends_at_the_first_php_segment() {
        for (path, split) in [
            ("/front.php/index.php", Some(("/front.php", "/index.php"))),
            ("/d.php/index.php", Some(("/d.php", "/index.php"))),
            ("/d.php/", Some(("/d.php", "/"))),
            ("/d.php", Some(("/d.php", ""))),
            ("/a.php.jpg/b.php/c", Some(("/a.php.jpg/b.php", "/c"))),
            ("/up/x.jpg/y.php", Some(("/up/x.jpg/y.php", ""))),
            ("/.php/x", Some(("/.php", "/x"))),
            ("/x.PHP/a", None),
            ("/a.php5/b", None),
            ("/a/", None),
            ("/x.php\0/y", None),
            ("", None),
        ] {
            let split = split.map(|(name, info): (&str, &str)| (name.as_bytes(), info.as_bytes()));
            let found = split_at_program(path.as_bytes(), b"/r");
            let found = found.as_ref().map(|t| {
                assert_eq!(
                    t.path_translated,
                    [&b"/r"[..], &t.vpath].concat(),
                    "{path:?}"
                );
                (&t.vpath[..], t.path_info.as_deref().unwrap_or_default())
            });
            assert_eq!(found, split, "{path:?}");
        }
    }

    /// [`TREE`] asked as `stat(2)` asks, of a C string: up to its NUL.
    struct AsC;

    impl Files for AsC {
        fn resolves(&self, path: &[u8]) -> bool {
            self.stat(path).is_some()
        }

        fn stat(&self, path: &[u8]) -> Option<Stat> {
            TREE.stat(crate::c_str(path))
        }
    }

    /// A path with a NUL is found as far as the NUL, as php -S's walk
    /// finds it, and is translated whole, NUL and all: a program's where
    /// it ends in `.php`, a static file's otherwise, and either one
    /// holding the NUL php -S refuses.
    #[test]
    fn a_path_with_a_nul_is_found_up_to_it_and_holds_it() {
        let found = translate(b"/t.php\0.txt", b"/r", &AsC);
        assert_eq!(
            found.as_ref().map(|t| &t.path_translated[..]),
            Some(&b"/r/t.php\0.txt"[..])
        );
        assert_eq!(
            found.as_ref().map(|t| holds_nul(&t.path_translated)),
            Some(true)
        );
        assert_eq!(found.map(|t| t.is_program()), Some(false));
        let program = translate(b"/up/x.jpg\0.php", b"/r", &AsC);
        assert_eq!(program.as_ref().map(Translated::is_program), Some(true));
        assert_eq!(program.map(|t| holds_nul(&t.path_translated)), Some(true));
        assert_eq!(
            translate(b"/t.php", b"/r", &AsC).map(|t| holds_nul(&t.path_translated)),
            Some(false)
        );
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
