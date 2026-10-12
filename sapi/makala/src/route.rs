//! Where a request goes under the document root: `php -S`'s path rules
//! (`php_proto::vpath`, #2346, #2588), and over them the refusals of a server that
//! faces the internet (#2586). The path is `normalize_vpath`'s, decoded and
//! with every `..` taken, so no request names a file outside the root; a
//! symlink under the root is followed wherever it leads, as `php -S` and
//! nginx's `disable_symlinks off` follow one. What the operator's
//! [`Hidden`] refuses is refused here and nowhere else: `php -S` keeps its
//! rules.

use std::sync::Arc;

use php_proto::vpath::{self, Translated};
use php_sapi::ResponseHead;
use php_sapi::cgi::Disk;

use crate::options::{Hidden, Options};
use crate::static_file::StaticFile;

/// The extensions of PHP's source under another name than `php` (#2586):
/// a file with one is never sent as a static file unless the operator
/// serves hidden paths, matched without regard to case.
const SOURCE_EXTENSIONS: [&[u8]; 9] = [
    b"phps", b"phtml", b"inc", b"php3", b"php4", b"php5", b"php6", b"php7", b"php8",
];

/// The one dot segment a path may begin with (RFC 8615): ACME's
/// challenges and `security.txt` stand under it.
const WELL_KNOWN: &[u8] = b".well-known";

/// Where requests go: the document root, and what of it [`Hidden`]
/// refuses. Its one question asks the disk, so the connection side asks
/// it on tokio's blocking pool, never on its one thread (M4).
pub(crate) struct Router {
    root: Arc<[u8]>,
    hidden: Hidden,
}

impl Router {
    /// The router of `options`' document root and [`Hidden`].
    pub(crate) fn new(options: &Options) -> Router {
        Router {
            root: options.document_root.as_os_str().as_encoded_bytes().into(),
            hidden: options.hidden,
        }
    }

    /// The document root, as the CGI environment names it.
    pub(crate) fn root(&self) -> &Arc<[u8]> {
        &self.root
    }

    /// Where `method path` goes: a path [`Hidden`] refuses is not found,
    /// before the disk is asked; else the path translated as `php -S`
    /// translates it, a program's where `php_cli_server_dispatch` says so,
    /// the path past it its path's info (#2637), and a static file's
    /// otherwise, in `php_cli_server_begin_send_static`'s order: a method
    /// that writes refused, then a path that translated to nothing, then,
    /// under [`Hidden::Refused`], a path past the file it found or a file
    /// of PHP's source, then a file that does not open. A path holding a
    /// NUL (`vpath::holds_nul`) is answered under [`Hidden::Refused`] as a
    /// path with nothing at it is; under [`Hidden::Served`] a static
    /// file's is refused with 400 after a method that writes, and a
    /// program's is routed as one, whose request serves nothing, as `php -S`
    /// refuses them (#2695). The walk that
    /// routes is the walk that splits, so `/upload/x.jpg/y.php` serves no
    /// program (`cgi.fix_pathinfo`'s class of exploit needs a front end
    /// that routes on the suffix while PHP finds a shorter path); `php -S`
    /// sends `x.jpg` for it, and so does [`Hidden::Served`], where
    /// [`Hidden::Refused`] serves a static file only as named (#2642).
    pub(crate) fn route(&self, method: &[u8], path: &[u8]) -> Route {
        let hidden = self.hidden;
        let mut uri = php_proto::http::normalize_vpath(path);
        if !uri.starts_with(b"/") {
            uri.insert(0, b'/');
        }
        if hidden == Hidden::Refused && has_dot_segment(&uri) {
            return Route::Refused(Refusal::NotFound);
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
                StaticFile::open(&found).map_or(Route::Refused(Refusal::NotFound), Route::Static)
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
