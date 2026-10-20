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

use php_proto::c_str;
use php_proto::cgi::{CNumber, Environment, Files, RequestInfo, Settings, Shape, Stat};

use crate::ini::enabled;
use crate::{Engine, HostPage, IniEvent, IniValue, Method, ResponseHead, WebRequest};

/// The file system as the built-in server's path rules (`php_proto::vpath`)
/// ask it, and as [`CachedDisk`] asks what `stat(2)` finds, each path read
/// as `realpath(3)` and
/// `stat(2)` read the C string php hands them: up to its first NUL. So
/// `run.php\0.txt` finds `run.php`, as php -S's walk finds it before
/// refusing the path for its NUL (#2695).
#[derive(Debug, Default, Clone, Copy)]
pub struct Disk;

impl Files for Disk {
    fn resolves(&self, path: &[u8]) -> bool {
        std::fs::canonicalize(path_of(c_str(path))).is_ok()
    }

    fn stat(&self, path: &[u8]) -> Option<Stat> {
        let meta = std::fs::metadata(path_of(c_str(path))).ok()?;
        Some(if meta.is_dir() {
            Stat::Directory
        } else if meta.is_file() {
            Stat::File(meta.len())
        } else {
            Stat::Other(meta.len())
        })
    }
}

/// The file system as php-cgi's and php-fpm's `init_request_info` asks
/// it: whether the program's path resolves is `tsrm_realpath`, through
/// the engine's realpath cache ([`Engine::realpath`]), so a host serving
/// many requests stats a program's path once per `realpath_cache_ttl`,
/// as a php-fpm child does; what `stat(2)` finds is [`Disk`]'s. Each
/// path is read up to its first NUL, as [`Disk`] reads it.
#[derive(Clone, Copy)]
pub struct CachedDisk<'e>(pub &'e Engine);

impl Files for CachedDisk<'_> {
    fn resolves(&self, path: &[u8]) -> bool {
        self.0.realpath(c_str(path)).is_some()
    }

    fn stat(&self, path: &[u8]) -> Option<Stat> {
        Disk.stat(path)
    }
}

/// A path's bytes as the file system names it.
#[must_use]
pub fn path_of(bytes: &[u8]) -> PathBuf {
    use std::os::unix::ffi::OsStrExt;
    PathBuf::from(OsStr::from_bytes(bytes))
}

/// C's `strtol` and `atoi` as the hosts' C library reads them, the
/// engine's one reading (`crate::c_number`), handed to `php_proto::cgi`,
/// which binds no C library: a `CONTENT_LENGTH`, a status line's code and
/// a `cgi.nph` value skip what `isspace` counts under the process's
/// `LC_CTYPE`, `0xA0` on macOS's `C.UTF-8` among them, as php-cgi's and
/// php-fpm's do.
pub const C_NUMBER: CNumber = CNumber {
    strtol: crate::c_number::strtol,
    atoi: crate::c_number::atoi,
};

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
/// cookie header, the content type and its length, the request headers,
/// the response code the request starts with and the copy of the
/// program's path `init_request_info` `estrdup`s. The body is the host's
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
        path_copies: usize::from(info.path_translated.is_some()),
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
        C_NUMBER,
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
        C_NUMBER,
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
    Shape::of(entry(b"cgi.nph"), entry(b"cgi.rfc2616_headers"), C_NUMBER)
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

/// The page php-cgi and php-fpm answer for a program that
/// `php_fopen_primary_script` did not open, `error` the open's, none
/// where there was no path to open (`Z/sapi/cgi/cgi_main.c:2478-2487`,
/// `Z/sapi/fpm/fpm/fpm_main.c:1879-1888`): `Access denied.` under a 403
/// where the open failed with `EACCES` alone, and `No input file
/// specified.` under a 404 for every other errno, `EPERM` among them,
/// which Rust's `ErrorKind::PermissionDenied` takes with `EACCES`.
#[must_use]
pub fn unopened(error: Option<&std::io::Error>) -> HostPage {
    let denied = error.and_then(std::io::Error::raw_os_error) == Some(libc::EACCES);
    let (code, body): (i64, &[u8]) = if denied {
        (403, b"Access denied.\n")
    } else {
        (404, b"No input file specified.\n")
    };
    HostPage {
        code,
        headers: Vec::new(),
        body: body.to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use php_proto::cgi::{Opens, Shape};

    use super::C_NUMBER;

    /// The edges C's library reads, as the engine's `c_number` reads
    /// them: a long saturating by its sign, and `atoi`'s cut of it to
    /// `int`, so `cgi.nph=4294967296` is off as php's
    /// `zend_ini_parse_bool` reads it.
    #[test]
    fn the_hosts_read_numbers_with_the_engine_s_strtol_and_atoi() {
        assert_eq!((C_NUMBER.strtol)(b" \t\x0b-12x"), -12);
        assert_eq!((C_NUMBER.strtol)(b"99999999999999999999"), i64::MAX);
        assert_eq!((C_NUMBER.atoi)(b"4294967497"), 201);
        assert_eq!(
            Shape::of(b"4294967296", b"0", C_NUMBER).opens,
            Opens::Changed
        );
        assert_eq!(Shape::of(b"1", b"0", C_NUMBER).opens, Opens::Always);
    }

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

    /// `EACCES` alone is `Access denied.`: `EPERM`, which macOS answers
    /// for a file under a directory the process may not read, is `No
    /// input file specified.`, as is every other errno and no path at
    /// all, each php's page whole (#2876).
    #[test]
    fn a_script_that_does_not_open_is_denied_for_eacces_alone() {
        let page = |errno: Option<i32>| {
            let error = errno.map(std::io::Error::from_raw_os_error);
            let page = unopened(error.as_ref());
            (page.code, page.body, page.headers.is_empty())
        };
        let denied = (403, b"Access denied.\n".to_vec(), true);
        let missing = (404, b"No input file specified.\n".to_vec(), true);
        assert_eq!(page(Some(libc::EACCES)), denied);
        assert_eq!(page(Some(libc::EPERM)), missing);
        assert_eq!(page(Some(libc::ENOENT)), missing);
        assert_eq!(page(Some(libc::EISDIR)), missing);
        assert_eq!(page(None), missing);
    }

    /// A path is asked of the disk as far as its first NUL, as `stat(2)`
    /// and `realpath(3)` read a C string: `t.txt\0.php` is `t.txt`, and a
    /// path cut short of anything there finds nothing (#2695).
    #[test]
    fn the_disk_reads_a_path_up_to_its_first_nul() {
        let dir = std::env::temp_dir().join(format!("php-sapi-disk-nul-{}", std::process::id()));
        assert!(std::fs::create_dir_all(&dir).is_ok());
        assert!(std::fs::write(dir.join("t.txt"), "four").is_ok());
        let named = |rest: &str| [dir.as_os_str().as_encoded_bytes(), rest.as_bytes()].concat();
        assert_eq!(Disk.stat(&named("/t.txt\0.php")), Some(Stat::File(4)));
        assert_eq!(Disk.stat(&named("\0/t.txt")), Some(Stat::Directory));
        assert!(Disk.resolves(&named("/t.txt\0/x")));
        assert_eq!(Disk.stat(&named("/t.tx\0t")), None);
        std::fs::remove_dir_all(&dir).ok();
    }
}
