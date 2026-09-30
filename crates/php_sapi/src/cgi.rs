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

use php_proto::cgi::{Environment, Files, RequestInfo, Settings};

use crate::ini::enabled;
use crate::{Method, ResponseHead, WebRequest};

/// The entries whose default php's CGI and `FastCGI` hosts keep where the
/// engine keeps the CLI's (`php_cli.c`'s `HARDCODED_INI`): laid over the
/// engine's defaults before the line's `-d`, as `main.c`'s own table has
/// them.
pub const HOST_INI: [(&str, &str); 2] = [("html_errors", "1"), ("max_execution_time", "30")];

/// The file system as `init_request_info` asks it.
#[derive(Debug, Default, Clone, Copy)]
pub struct Disk;

impl Files for Disk {
    fn resolves(&self, path: &[u8]) -> bool {
        std::fs::canonicalize(path_of(path)).is_ok()
    }

    fn is_file(&self, path: &[u8]) -> bool {
        std::fs::metadata(path_of(path)).is_ok_and(|meta| meta.is_file())
    }
}

/// A path's bytes as the file system names it.
#[must_use]
pub fn path_of(bytes: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;
    PathBuf::from(OsStr::from_bytes(bytes))
}

/// The two entries `init_request_info` reads, `cgi.fix_pathinfo` on and
/// `cgi.discard_path` off where the line sets neither.
#[must_use]
pub fn settings(ini: &[(String, String)]) -> Settings {
    Settings {
        fix_pathinfo: enabled(ini, "cgi.fix_pathinfo", true),
        discard_path: enabled(ini, "cgi.discard_path", false),
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
        body: Vec::new(),
        headers: php_proto::cgi::request_headers(env),
        no_headers: false,
        response_code: info.response_code,
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

/// The response's head as `sapi_cgi_send_headers` writes it: the status
/// line where the request set one, each header line, and the blank line.
#[must_use]
pub fn head(head: &ResponseHead) -> Vec<u8> {
    php_proto::cgi::head(head.code, head.status_line.as_deref(), &head.lines)
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
