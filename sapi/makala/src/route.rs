//! Where a request goes under the document root: a program's path split
//! at its first `.php` segment as the recipe's nginx splits it,
//! else `php -S`'s path rules (`php_proto::vpath`), and over
//! them the refusals of a server that faces the internet. The
//! path is `normalize_vpath`'s, decoded and with every `..` taken, so no
//! request names a file outside the root; a symlink under the root is
//! followed wherever it leads, as `php -S` and nginx's `disable_symlinks
//! off` follow one. What the operator's [`Hidden`] refuses is refused here
//! and nowhere else: `php -S` keeps its rules.

use std::sync::Arc;

use php_proto::vpath::{self, Translated};
use php_sapi::cgi::Disk;
use php_sapi::{HostPath, ResponseHead};

use crate::options::{Hidden, Options};
use crate::static_file::StaticFile;

/// The extensions of PHP's source under another name than `php`:
/// a file with one is never sent as a static file unless the operator
/// serves hidden paths, matched without regard to case.
const SOURCE_EXTENSIONS: [&[u8]; 9] = [
    b"phps", b"phtml", b"inc", b"php3", b"php4", b"php5", b"php6", b"php7", b"php8",
];

/// The one dot segment a path may begin with (RFC 8615): ACME's
/// challenges and `security.txt` stand under it.
const WELL_KNOWN: &[u8] = b".well-known";

/// The `open_basedir` guard a static file passes before it opens: the
/// engine's `Engine::check_open_basedir`, the path it may open, or none
/// where the guard refuses it.
pub(crate) type Guard = Box<dyn Fn(&[u8]) -> Option<HostPath> + Send + Sync>;

/// Where requests go: the document root, what of it [`Hidden`] refuses,
/// and the guard a static file passes. Its one question asks the disk, so
/// the connection side asks it on tokio's blocking pool, never on its one
/// thread (M4).
pub(crate) struct Router {
    root: Arc<[u8]>,
    hidden: Hidden,
    guard: Guard,
}

impl Router {
    /// The router of `options`' document root and [`Hidden`], its static
    /// files passing `guard`.
    pub(crate) fn new(options: &Options, guard: Guard) -> Router {
        Router {
            root: options.document_root.as_os_str().as_encoded_bytes().into(),
            hidden: options.hidden,
            guard,
        }
    }

    /// The document root, as the CGI environment names it.
    pub(crate) fn root(&self) -> &Arc<[u8]> {
        &self.root
    }

    /// Where `method path` goes: a path [`Hidden`] refuses is not found,
    /// before the disk is asked; else a program's where a segment of the
    /// path ends in `.php`, split there as the recipe's nginx splits it
    /// (`vpath::split_at_program`), whatever is on the disk; else
    /// the path translated as `php -S` translates it, a program's where
    /// `php_cli_server_dispatch` says so, the path past it its path's info,
    /// and a static file's otherwise, in
    /// `php_cli_server_begin_send_static`'s order: a method that writes
    /// refused, then a path that translated to nothing, then, under
    /// [`Hidden::Refused`], a path past the file it found or a file of
    /// PHP's source, then a file outside `open_basedir`, which no oracle
    /// holds, answered as a file makala may not serve is, then a file
    /// that does not open. A path holding a NUL
    /// is never split, and `vpath::holds_nul` answers it under
    /// [`Hidden::Refused`] as a path with nothing at it is; under
    /// [`Hidden::Served`] a static file's is refused with 400 after a
    /// method that writes, and a program's is routed as one, whose request
    /// serves nothing, as `php -S` refuses them. The walk that
    /// routes is the walk that splits, so `/upload/x.jpg/y.php` serves no
    /// program the walk finds; split at `y.php`, `cgi.fix_pathinfo`'s walk
    /// in php-fpm's checks finds `x.jpg`, the exploit's shape, and
    /// `security.limit_extensions`' default refuses it (`request::refusal`),
    /// so no file whose name does not end in `.php` runs, and a directory
    /// named `d.php` is answered 404, as php-fpm under the recipe answers.
    /// `php -S` sends `x.jpg` for `/upload/x.jpg/`, and so does
    /// [`Hidden::Served`], where [`Hidden::Refused`] serves a static file
    /// only as named.
    pub(crate) fn route(&self, method: &[u8], path: &[u8]) -> Route {
        let hidden = self.hidden;
        let mut uri = php_proto::http::normalize_vpath(path);
        if !uri.starts_with(b"/") {
            uri.insert(0, b'/');
        }
        if hidden == Hidden::Refused && has_dot_segment(&uri) {
            return Route::Refused(Refusal::NotFound);
        }
        if let Some(program) = vpath::split_at_program(&uri, &self.root) {
            return Route::Program(program);
        }
        let writes = [&b"DELETE"[..], b"PUT", b"PATCH"].contains(&method);
        match vpath::translate(&uri, &self.root, &Disk) {
            Some(found) if vpath::holds_nul(&found.path_translated) => match hidden {
                Hidden::Served if found.is_program() => Route::Program(found),
                _ if writes => Route::Refused(Refusal::MethodNotAllowed),
                Hidden::Served => Route::Refused(Refusal::BadRequest),
                Hidden::Refused => Route::Refused(Refusal::NotFound),
            },
            Some(found) if found.is_program() => Route::Program(found),
            _ if writes => Route::Refused(Refusal::MethodNotAllowed),
            Some(found)
                if hidden == Hidden::Served
                    || (found.path_info.is_none()
                        && !is_source(vpath::extension(&found.vpath))) =>
            {
                (self.guard)(&found.path_translated)
                    .and_then(|host| StaticFile::open(&host, &found))
                    .map_or(Route::Refused(Refusal::NotFound), Route::Static)
            }
            _ => Route::Refused(Refusal::NotFound),
        }
    }
}

/// Where one request goes.
pub(crate) enum Route {
    /// A program's, served through its own `Engine::serve` on a request
    /// thread.
    Program(Translated),
    /// A static file's, opened, sent from the connection side.
    Static(StaticFile),
    /// Answered by the server, with no body.
    Refused(Refusal),
}

/// A request the server answers on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// 404: nothing under the root, or a path [`Hidden`] refuses, which is
    /// answered as nothing, so the server never says whether it exists.
    NotFound,
    /// 405: a method that writes, of a static file, with the methods
    /// `php -S` allows.
    MethodNotAllowed,
    /// 400: a static file's path holding a NUL, under
    /// [`Hidden::Served`], as `php_cli_server_begin_send_static` refuses
    /// one.
    BadRequest,
}

impl Refusal {
    /// The response's head: the refusal's code, and the methods `php -S`
    /// allows with a 405.
    pub(crate) fn head(self) -> ResponseHead {
        let (code, lines) = match self {
            Refusal::NotFound => (404, Vec::new()),
            Refusal::MethodNotAllowed => (405, vec![b"Allow: GET, HEAD, POST".to_vec()]),
            Refusal::BadRequest => (400, Vec::new()),
        };
        ResponseHead {
            code,
            lines,
            ..ResponseHead::default()
        }
    }
}

/// Whether a segment of `uri` starts with a `.`, but for a first segment
/// of [`WELL_KNOWN`]: `.env`, `.git/config`, `.htaccess`, `.user.ini`.
fn has_dot_segment(uri: &[u8]) -> bool {
    uri.split(|b| *b == b'/')
        .enumerate()
        .any(|(n, segment)| segment.starts_with(b".") && !(n == 1 && segment == WELL_KNOWN))
}

/// Whether `extension` is one of [`SOURCE_EXTENSIONS`].
fn is_source(extension: &[u8]) -> bool {
    SOURCE_EXTENSIONS
        .iter()
        .any(|source| extension.eq_ignore_ascii_case(source))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dot_segment_is_hidden_but_under_well_known() {
        for hidden in [
            "/.env",
            "/.git/config",
            "/a/.htaccess",
            "/.well-known/.x",
            "/a/.well-known/t",
        ] {
            assert!(has_dot_segment(hidden.as_bytes()), "{hidden}");
        }
        for shown in ["/", "/a.b/c", "/.well-known/acme-challenge/t", "/x.php"] {
            assert!(!has_dot_segment(shown.as_bytes()), "{shown}");
        }
    }

    #[test]
    fn php_s_source_extensions_match_in_any_case() {
        for source in ["inc", "phtml", "phps", "PHP5", "php6", "Php8"] {
            assert!(is_source(source.as_bytes()), "{source}");
        }
        for other in ["php", "php9", "html", "inc/x", ""] {
            assert!(!is_source(other.as_bytes()), "{other}");
        }
    }
}
