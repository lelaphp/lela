//! Where a request goes under the document root: `php -S`'s path rules
//! (`php_proto::vpath`, #2346, #2588), and over them the refusals of a server that
//! faces the internet (#2586). The path is `normalize_vpath`'s, decoded and
//! with every `..` taken, so no request names a file outside the root; a
//! symlink under the root is followed wherever it leads, as `php -S` and
//! nginx's `disable_symlinks off` follow one. What the operator's
//! [`Hidden`] refuses is refused here and nowhere else: `php -S` keeps its
//! rules.

use php_proto::vpath::{self, Translated};
use php_sapi::cgi::Disk;

use crate::options::Hidden;

/// The extensions of PHP's source under another name than `php` (#2586):
/// a file with one is never sent as a static file unless the operator
/// serves hidden paths, matched without regard to case.
const SOURCE_EXTENSIONS: [&[u8]; 8] = [
    b"phps", b"phtml", b"inc", b"php3", b"php4", b"php5", b"php7", b"php8",
];

/// The one dot segment a path may begin with (RFC 8615): ACME's
/// challenges and `security.txt` stand under it.
const WELL_KNOWN: &[u8] = b".well-known";

/// Where one request goes.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Route {
    /// A program's, run through the engine.
    Program(Translated),
    /// A static file's, sent from the disk.
    Static(Translated),
    /// Answered by the server, with no body.
    Refused(Refusal),
}

/// A request the server answers on its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// 400: a program whose path holds a NUL, which `php -S` refuses.
    BadRequest,
    /// 404: nothing under the root, or a path [`Hidden`] refuses, which is
    /// answered as nothing, so the server never says whether it exists.
    NotFound,
    /// 405: a method that writes, of a static file, with the methods
    /// `php -S` allows.
    MethodNotAllowed,
}

impl Refusal {
    /// The status code.
    pub(crate) fn code(self) -> u16 {
        match self {
            Refusal::BadRequest => 400,
            Refusal::NotFound => 404,
            Refusal::MethodNotAllowed => 405,
        }
    }

    /// The header lines the refusal carries.
    pub(crate) fn lines(self) -> Vec<Vec<u8>> {
        match self {
            Refusal::MethodNotAllowed => vec![b"Allow: GET, HEAD, POST".to_vec()],
            Refusal::BadRequest | Refusal::NotFound => Vec::new(),
        }
    }
}

/// Where `method path` goes under `root`: a path [`Hidden`] refuses is
/// not found, before the disk is asked; else the path translated as
/// `php -S` translates it, a program's where `php_cli_server_dispatch`
/// says so and a static file's otherwise, in `php_cli_server_begin_send_static`'s
/// order: a method that writes refused, then a path that translated to
/// nothing, then, under [`Hidden::Refused`], a file of PHP's source.
pub(crate) fn route(method: &[u8], path: &[u8], root: &[u8], hidden: Hidden) -> Route {
    let mut uri = php_proto::http::normalize_vpath(path);
    if !uri.starts_with(b"/") {
        uri.insert(0, b'/');
    }
    if hidden == Hidden::Refused && has_dot_segment(&uri) {
        return Route::Refused(Refusal::NotFound);
    }
    let found = vpath::translate(&uri, root, &Disk);
    match found {
        Some(found) if found.is_program() => {
            if found.path_translated.contains(&0) {
                return Route::Refused(Refusal::BadRequest);
            }
            Route::Program(found)
        }
        _ if [&b"DELETE"[..], b"PUT", b"PATCH"].contains(&method) => {
            Route::Refused(Refusal::MethodNotAllowed)
        }
        Some(found)
            if !(hidden == Hidden::Refused && is_source(vpath::extension(&found.vpath))) =>
        {
            Route::Static(found)
        }
        _ => Route::Refused(Refusal::NotFound),
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
        for source in ["inc", "phtml", "phps", "PHP5", "Php8"] {
            assert!(is_source(source.as_bytes()), "{source}");
        }
        for other in ["php", "php9", "html", "inc/x", ""] {
            assert!(!is_source(other.as_bytes()), "{other}");
        }
    }
}
