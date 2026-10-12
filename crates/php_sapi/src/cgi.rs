//! What php's CGI and its `FastCGI` process manager share, each keeping a
//! copy in `sapi/cgi/cgi_main.c` and `sapi/fpm/fpm/fpm_main.c`: the
//! request a server describes in the CGI environment, read as
//! `init_request_info` reads it (`php_proto::cgi`), made the request of
//! the web a request description carries, with `PHP_SELF` and the `Basic`
//! credentials `sapi_cgi_register_variables` and `php_handle_auth_data`
//! register; and the response's head, written as `sapi_cgi_send_headers`
//! writes it before the body. Where the environment comes from, the
//! process's or a `FastCGI` request's parameters, and where the head is
//! written, are each host's.

use std::ffi::OsStr;
use std::path::PathBuf;

use php_proto::cgi::{Environment, Files, RequestInfo, Settings, Shape, Stat};

use crate::ini::enabled;
use crate::{Engine, IniEvent, IniValue, Method, ResponseHead, WebRequest};

/// The file system as `init_request_info` and the built-in server's path
/// rules (`php_proto::vpath`) ask it.
#[derive(Debug, Default, Clone, Copy)]
pub struct Disk;

impl Files for Disk {
    fn resolves(&self, path: &[u8]) -> bool {
        std::fs::canonicalize(path_of(path)).is_ok()
    }

    fn stat(&self, path: &[u8]) -> Option<Stat> {
        let meta = std::fs::metadata(path_of(path)).ok()?;
        Some(if meta.is_dir() {
            Stat::Directory
        } else if meta.is_file() {
            Stat::File(meta.len())
        } else {
            Stat::Other(meta.len())
        })
    }
}

/// A path's bytes as the file system names it.
#[must_use]
pub fn path_of(bytes: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;
    PathBuf::from(OsStr::from_bytes(bytes))
}

/// The two entries `init_request_info` reads, `cgi.fix_pathinfo` and
/// `cgi.discard_path`, as the engine started with them.
#[must_use]
pub fn settings(engine: &Engine) -> Settings {
    Settings {
        fix_pathinfo: enabled(engine, "cgi.fix_pathinfo"),
        discard_path: enabled(engine, "cgi.discard_path"),
        ..Settings::default()
    }
}

/// The request of the web `env` describes, as `init_request_info` read it
/// into `info`: the method, the path asked for, the query string, the
/// cookie header, the content type and its length, the request headers
/// and the response code the request starts with. The body is the host's
/// to read and lay in, and the head written unless the host says not.
#[must_use]
pub fn web(env: &Environment, info: &RequestInfo) -> WebRequest {
    WebRequest {
        method: info.method.as_deref().map(Method::of),
        request_uri: info.request_uri.clone(),
        query: info.query.clone(),
        cookies: env.get(b"HTTP_COOKIE").map(<[u8]>::to_vec),
        content_type: info.content_type.clone(),
        content_length: info.content_length,
        body: crate::Body::default(),
        headers: php_proto::cgi::request_headers(env),
        no_headers: false,
        response_code: info.response_code,
        head_ini: &HEAD_INI,
    }
}

/// The server variables a CGI request registers past the environment,
/// which the engine imports first for every host: `PHP_SELF`, and
/// `PHP_AUTH_USER` and `PHP_AUTH_PW` where the request carries `Basic`
/// credentials.
#[must_use]
pub fn server_variables(
    env: &Environment,
    info: &RequestInfo,
    settings: Settings,
) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut rows = vec![(b"PHP_SELF".to_vec(), php_self(env, info, settings))];
    if let Some((user, password)) = &info.auth {
        rows.push((b"PHP_AUTH_USER".to_vec(), user.clone()));
        if let Some(password) = password {
            rows.push((b"PHP_AUTH_PW".to_vec(), password.clone()));
        }
    }
    rows
}

/// `sapi_cgi_register_variables`' `PHP_SELF`
/// (`Z/sapi/cgi/cgi_main.c:690`): under `cgi.fix_pathinfo`, `SCRIPT_NAME`
/// as the request found it and `PATH_INFO` after it, either alone, or
/// nothing; `request_uri` alone otherwise.
fn php_self(env: &Environment, info: &RequestInfo, settings: Settings) -> Vec<u8> {
    let script_name = info.request_uri.clone();
    if !settings.fix_pathinfo {
        return script_name.unwrap_or_default();
    }
    match (script_name, env.get(b"PATH_INFO")) {
        (Some(mut name), Some(info)) => {
            name.extend_from_slice(info);
            name
        }
        (None, Some(info)) => info.to_vec(),
        (Some(name), None) => name,
        (None, None) => Vec::new(),
    }
}

/// The entries `sapi_cgi_send_headers` reads at the send, `CGIG(nph)`
/// and `CGIG(rfc2616_headers)`, which a request's [`web`] names for its
/// head.
const HEAD_INI: [&[u8]; 2] = [b"cgi.nph", b"cgi.rfc2616_headers"];

/// The response's head as `sapi_cgi_send_headers` writes it, in the
/// shape `cgi.nph` and `cgi.rfc2616_headers` give it at the send: the
/// status line where the request set one or `cgi.nph` asks for it, each
/// header line, and the blank line.
#[must_use]
pub fn head(head: &ResponseHead) -> Vec<u8> {
    php_proto::cgi::head(
        shape(head),
        head.code,
        head.status_line.as_deref(),
        &head.lines,
    )
}

/// [`head`] line by line ([`php_proto::cgi::lines`]): what a web server in
/// front of php-fpm reads the response's status and header lines from.
#[must_use]
pub fn lines(head: &ResponseHead) -> Vec<php_proto::cgi::Line<'_>> {
    php_proto::cgi::lines(
        shape(head),
        head.code,
        head.status_line.as_deref(),
        &head.lines,
    )
}

/// The shape the head's `cgi.nph` and `cgi.rfc2616_headers` give it at the
/// send, each empty where the request's host names neither.
fn shape(head: &ResponseHead) -> Shape {
    let entry = |name: &[u8]| {
        head.ini
            .iter()
            .find(|(had, _)| *had == name)
            .map_or(&[][..], |(_, value)| value.as_slice())
    };
    Shape::of(entry(b"cgi.nph"), entry(b"cgi.rfc2616_headers"))
}

/// The directory `php_cgi_ini_activate_user_config` scans to for
/// `path_translated`: its directory, with the slash after it.
#[must_use]
pub fn user_ini_dir(path_translated: &[u8]) -> Vec<u8> {
    let mut dir = match path_translated.iter().rposition(|b| *b == b'/') {
        Some(at) => path_translated[..at].to_vec(),
        None => b".".to_vec(),
    };
    dir.push(b'/');
    dir
}

/// `php_cgi_ini_activate_user_config`'s scan, which php-fpm keeps a copy
/// of (`Z/sapi/cgi/cgi_main.c:783-859`): `user_ini.filename` read in each
/// directory from `doc_root`, one trailing slash dropped, down to `dir`
/// ([`user_ini_dir`]) where `dir` lies under it, or in `dir` alone where
/// it does not, a relative `dir` made real first; each file's entries in
/// order, a later one replacing an earlier of its name. The host sets
/// them as `IniOrigin::PerDir` after the php.ini's `[PATH=]` tables, and
/// caches them for `user_ini.cache_ttl` seconds where it serves more
/// than one request. Nothing where the filename is empty.
#[must_use]
pub fn user_ini(engine: &Engine, dir: &[u8], doc_root: &[u8]) -> Vec<(Vec<u8>, Vec<u8>)> {
    let filename = engine.ini_get(b"user_ini.filename").unwrap_or_default();
    if filename.is_empty() {
        return Vec::new();
    }
    let doc_root = doc_root.strip_suffix(b"/").unwrap_or(doc_root);
    let path = if dir.first() == Some(&b'/') {
        dir.to_vec()
    } else {
        match std::fs::canonicalize(path_of(dir)) {
            Ok(real) => real.into_os_string().into_encoded_bytes(),
            Err(_) => return Vec::new(),
        }
    };
    let mut entries: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
    let mut read = |at: &[u8]| {
        let file = [at, b"/", &filename].concat();
        let Ok(text) = std::fs::read(path_of(&file)) else {
            return;
        };
        for event in engine.parse_ini(&text).0 {
            if let IniEvent::Entry(key, Some(value)) = event {
                let key = ini_text(&key);
                entries.retain(|(k, _)| *k != key);
                entries.push((key, ini_text(&value)));
            }
        }
    };
    let (shorter, longer) = if path.len() > doc_root.len() {
        (doc_root, path.as_slice())
    } else {
        (path.as_slice(), doc_root)
    };
    if longer.starts_with(shorter) {
        let mut at = doc_root.len();
        while let Some(slash) = path
            .get(at..)
            .and_then(|rest| rest.iter().position(|b| *b == b'/'))
        {
            read(&path[..at + slash]);
            at += slash + 1;
        }
    } else {
        read(path.strip_suffix(b"/").unwrap_or(&path));
    }
    entries
}

/// An INI value's text, which the normal scanner always makes.
#[must_use]
pub fn ini_text(value: &IniValue) -> Vec<u8> {
    match value {
        IniValue::Str(s) => s.clone(),
        IniValue::Bool(true) => b"1".to_vec(),
        IniValue::Long(n) => n.to_string().into_bytes(),
        IniValue::Double(d) => d.to_string().into_bytes(),
        IniValue::Bool(false) | IniValue::Null => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn php_self_is_the_script_name_and_the_path_info() {
        let env = Environment(vec![(b"PATH_INFO".to_vec(), b"/a".to_vec())]);
        let info = RequestInfo {
            request_uri: Some(b"/t.php".to_vec()),
            ..RequestInfo::default()
        };
        assert_eq!(php_self(&env, &info, Settings::default()), b"/t.php/a");
        assert_eq!(
            php_self(
                &Environment::default(),
                &RequestInfo::default(),
                Settings::default()
            ),
            b""
        );
    }
}
