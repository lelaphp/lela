//! CGI/1.1 as php's CGI host and its `FastCGI` process manager read and
//! write it (`sapi/cgi/cgi_main.c` and `sapi/fpm/fpm/fpm_main.c` at the
//! pin): the environment a server hands a request in, read into the plain
//! fields php's `SG(request_info)` keeps (`init_request_info`, one
//! algorithm in both, php-fpm's reading Apache's proxy URLs besides), and
//! the head a response goes out with (`sapi_cgi_send_headers`). Bytes and
//! plain values: the one question asked of the file system, whether a
//! path names a file, is the caller's to answer.

use std::borrow::Cow;

/// An environment, as `environ` holds it: names and values in order, a
/// name set again keeping its place, as `setenv(3)` keeps it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Environment(pub Vec<(Vec<u8>, Vec<u8>)>);

impl Environment {
    /// `getenv(3)`.
    #[must_use]
    pub fn get(&self, name: &[u8]) -> Option<&[u8]> {
        self.0
            .iter()
            .find(|(had, _)| had == name)
            .map(|(_, value)| value.as_slice())
    }

    /// `CGI_PUTENV` and `FCGI_PUTENV`: set, a name set again keeping its
    /// place, or unset for `None`; the value set is answered.
    pub fn put(&mut self, name: &[u8], value: Option<&[u8]>) -> Option<Vec<u8>> {
        let Some(value) = value else {
            self.0.retain(|(had, _)| had != name);
            return None;
        };
        match self.0.iter_mut().find(|(had, _)| had == name) {
            Some((_, had)) => *had = value.to_vec(),
            None => self.0.push((name.to_vec(), value.to_vec())),
        }
        Some(value.to_vec())
    }

    /// The environment as php reads it through a `char *`: each value
    /// up to its first NUL ([`crate::c_str`]). A `FastCGI` parameter
    /// carries its length and may hold a NUL, which `init_request_info`
    /// and the request loop (`FCGI_GETENV`,
    /// `Z/sapi/fpm/fpm/fpm_main.c:974`), `getenv()` and `$_SERVER`
    /// (`cgi_php_load_env_var`'s `strlen`, `:512`) all stop at;
    /// `apache_request_headers` alone reads the whole value
    /// (`sapi_add_request_header`'s `add_assoc_stringl_ex`,
    /// `Z/main/SAPI.c:1172`), and a process's environment holds none. A
    /// name stays whole, as `fcgi_getenv` finds a parameter by its name
    /// and length.
    #[must_use]
    pub fn cut_at_nul(mut self) -> Environment {
        for (_, value) in &mut self.0 {
            let kept = crate::c_str(value).len();
            value.truncate(kept);
        }
        self
    }
}

/// What a file system answers about a path: `realpath(3)` succeeding and
/// what `stat(2)` finds, the questions `init_request_info` and the
/// built-in server's path rules (`crate::vpath`) ask, each of the path
/// as [`crate::c_str`] reads it.
pub trait Files {
    /// Whether `realpath(3)` resolves the path.
    fn resolves(&self, path: &[u8]) -> bool;
    /// What `stat(2)` finds at the path, following a symlink; nothing
    /// where it fails.
    fn stat(&self, path: &[u8]) -> Option<Stat>;
    /// Whether `stat(2)` finds a regular file at the path.
    fn is_file(&self, path: &[u8]) -> bool {
        matches!(self.stat(path), Some(Stat::File(_)))
    }
}

/// What `stat(2)` found: its `st_mode`'s kind, and its `st_size`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stat {
    Directory,
    /// A regular file, of so many bytes.
    File(u64),
    /// A device, a FIFO or a socket, of its `st_size`.
    Other(u64),
}

/// The `cgi.*` entries `init_request_info` reads, at php's defaults, and
/// whether it reads Apache's proxy URLs as php-fpm's does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    /// `cgi.fix_pathinfo`.
    pub fix_pathinfo: bool,
    /// `cgi.discard_path`.
    pub discard_path: bool,
    /// How a `SCRIPT_FILENAME` of Apache's proxy is read.
    pub proxy: Proxy,
}

/// php-fpm's reading of a `SCRIPT_FILENAME` of `proxy:fcgi://` or
/// `proxy:balancer://`, which `mod_proxy_fcgi` sends, and php-cgi's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Proxy {
    /// php-cgi's: the variable read as it stands.
    Ignored,
    /// php-fpm's: the URL cut to its path, which stays percent-encoded
    /// where `fastcgi.script_path_encoded` says.
    Read { encoded: bool },
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            fix_pathinfo: true,
            discard_path: false,
            proxy: Proxy::Ignored,
        }
    }
}

/// `SG(request_info)` as `init_request_info` leaves it
/// (`Z/sapi/cgi/cgi_main.c:1159`), with the response code it sets.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RequestInfo {
    /// `path_translated`: the file served, `None` where the environment
    /// names none, which is a php-cgi started from a shell, or one whose path
    /// climbs out through `..`.
    pub path_translated: Option<Vec<u8>>,
    /// `request_uri`, what `PHP_SELF` is built from.
    pub request_uri: Option<Vec<u8>>,
    /// `request_method`.
    pub method: Option<Vec<u8>>,
    /// `query_string`.
    pub query: Option<Vec<u8>>,
    /// `content_type`: empty where the environment has none.
    pub content_type: Option<Vec<u8>>,
    /// `content_length`, as `atol` reads it.
    pub content_length: i64,
    /// `SG(sapi_headers).http_response_code`: 200, or 404 where the path
    /// named no file.
    pub response_code: i64,
    /// `auth_user` and `auth_password`, from a `Basic` authorization.
    pub auth: Option<(Vec<u8>, Option<Vec<u8>>)>,
}

/// `init_request_info` (`Z/sapi/cgi/cgi_main.c:1159`,
/// `Z/sapi/fpm/fpm/fpm_main.c:971`) over `env`, which it rewrites as php's
/// does: IIS's `PATH_INFO` cut past `SCRIPT_NAME`; under php-fpm a proxy
/// URL cut to its path; `DOCUMENT_ROOT` from `doc_root` where the
/// environment has none; and `SCRIPT_FILENAME`, `PATH_INFO`,
/// `SCRIPT_NAME` and `PATH_TRANSLATED` fixed where `cgi.fix_pathinfo`
/// finds the file the path names, each value replaced kept under its
/// `ORIG_` name.
#[must_use]
#[allow(clippy::too_many_lines)] // php's one function, kept as one
pub fn request_info(
    env: &mut Environment,
    settings: Settings,
    doc_root: Option<&[u8]>,
    files: &dyn Files,
    number: CNumber,
) -> RequestInfo {
    let mut env_script_filename = env.get(b"SCRIPT_FILENAME").map(<[u8]>::to_vec);
    let env_path_translated = env.get(b"PATH_TRANSLATED").map(<[u8]>::to_vec);
    let mut info = RequestInfo {
        response_code: 200,
        method: env.get(b"REQUEST_METHOD").map(<[u8]>::to_vec),
        ..RequestInfo::default()
    };
    if env_script_filename.is_none() && env_path_translated.is_none() {
        return info;
    }
    let content_length = env.get(b"CONTENT_LENGTH").map(<[u8]>::to_vec);
    let content_type = env.get(b"CONTENT_TYPE").map(<[u8]>::to_vec);
    let mut env_path_info = env.get(b"PATH_INFO").map(<[u8]>::to_vec);
    let mut env_script_name = env.get(b"SCRIPT_NAME").map(<[u8]>::to_vec);
    if let (Some(software), Some(name), Some(path_info)) = (
        env.get(b"SERVER_SOFTWARE"),
        &env_script_name,
        &env_path_info,
    ) && software.starts_with(b"Microsoft-IIS")
        && path_info.starts_with(name)
    {
        let path_info = path_info.clone();
        env.put(b"ORIG_PATH_INFO", Some(&path_info));
        let rest = &path_info[name.len()..];
        env_path_info = env.put(b"PATH_INFO", (!rest.is_empty()).then_some(rest));
    }
    let mut apache_was_here = false;
    let encoded = match settings.proxy {
        Proxy::Read { encoded } => encoded,
        Proxy::Ignored => true,
    };
    if settings.proxy != Proxy::Ignored
        && let Some(filename) = &mut env_script_filename
    {
        for prefix in [&b"proxy:fcgi://"[..], b"proxy:balancer://"] {
            if filename.len() >= prefix.len()
                && filename[..prefix.len()].eq_ignore_ascii_case(prefix)
            {
                if let Some(slash) = filename[prefix.len()..].iter().position(|b| *b == b'/') {
                    filename.drain(..prefix.len() + slash);
                    apache_was_here = true;
                }
                if let Some(q) = filename.iter().position(|b| *b == b'?') {
                    filename.truncate(q);
                }
            }
        }
        if apache_was_here && !encoded {
            *filename = raw_url_decode(filename);
        }
        // php cuts the variable's own bytes in place.
        if env.get(b"SCRIPT_FILENAME") != Some(filename.as_slice()) {
            let cut = filename.clone();
            env.put(b"SCRIPT_FILENAME", Some(&cut));
        }
    }
    let mut script = match &env_script_filename {
        Some(filename) => filename.clone(),
        None => env_path_translated.clone().unwrap_or_default(),
    };
    let mut path_translated_kept = Some(script.clone());
    if settings.fix_pathinfo {
        let env_redirect_url = env.get(b"REDIRECT_URL").map(<[u8]>::to_vec);
        let mut env_document_root = env.get(b"DOCUMENT_ROOT").map(<[u8]>::to_vec);
        let orig_path_translated = env_path_translated.clone();
        let orig_path_info = env_path_info.clone();
        let orig_script_name = env_script_name.clone();
        let orig_script_filename = env_script_filename.clone();
        if env_document_root.is_none()
            && let Some(root) = doc_root
        {
            env_document_root = env.put(b"DOCUMENT_ROOT", Some(root));
        }
        if !apache_was_here
            && let (Some(translated), Some(redirect)) = (&env_path_translated, &env_redirect_url)
            && *translated != script
        {
            script.clone_from(translated);
            env_script_name = Some(redirect.clone());
        }
        if !script.is_empty() && (script.last() == Some(&b'/') || !files.resolves(&script)) {
            let mut pt = script.clone();
            let mut found = false;
            while let Some(slash) = pt
                .iter()
                .rposition(|b| *b == b'/')
                .or_else(|| pt.iter().rposition(|b| *b == b'\\'))
            {
                pt.truncate(slash);
                if !files.is_file(&pt) {
                    continue;
                }
                found = true;
                // The file is `pt`, and what the path had past it is
                // `PATH_INFO`'s tail.
                let slen = script.len() - pt.len();
                let pilen = env_path_info.as_ref().map_or(0, Vec::len);
                let (path_info, tflag) = if apache_was_here {
                    let tail = script[pt.len()..].to_vec();
                    let differs = orig_path_info.as_ref() != Some(&tail);
                    (Some(tail), slen != 0 && differs)
                } else {
                    let tail = env_path_info
                        .as_ref()
                        .filter(|_| pilen > slen)
                        .map(|info| info[pilen - slen..].to_vec());
                    let differs = tail.is_some() && tail != orig_path_info;
                    (tail, differs)
                };
                if tflag && let Some(path_info) = path_info {
                    let mut decoded = None;
                    if let Some(orig) = &orig_path_info {
                        env.put(b"ORIG_PATH_INFO", Some(orig));
                        let current = env_path_info.clone().unwrap_or_default();
                        let head = if apache_was_here {
                            current.clone()
                        } else {
                            current[..current.len() - path_info.len()].to_vec()
                        };
                        if orig_script_name.as_deref() == Some(head.as_slice()) {
                            info.request_uri.clone_from(&orig_script_name);
                        } else {
                            if let Some(name) = &orig_script_name {
                                env.put(b"ORIG_SCRIPT_NAME", Some(name));
                            }
                            info.request_uri = env.put(b"SCRIPT_NAME", Some(&head));
                        }
                    } else if apache_was_here && let Some(name) = env_script_name.clone() {
                        if encoded && path_info.contains(&b'%') {
                            decoded = Some(raw_url_decode(&path_info));
                        }
                        let suffix = |tail: &[u8]| name.len() > tail.len() && name.ends_with(tail);
                        let cut = if name.len() > slen && name[name.len() - slen..] == path_info[..]
                        {
                            Some(name.len() - slen)
                        } else {
                            decoded
                                .as_ref()
                                .filter(|d| suffix(d))
                                .map(|d| name.len() - d.len())
                        };
                        if let Some(cut) = cut {
                            if let Some(orig) = &orig_script_name {
                                env.put(b"ORIG_SCRIPT_NAME", Some(orig));
                            }
                            info.request_uri = env.put(b"SCRIPT_NAME", Some(&name[..cut]));
                        }
                    }
                    env_path_info =
                        env.put(b"PATH_INFO", Some(decoded.as_deref().unwrap_or(&path_info)));
                }
                if orig_script_filename.as_deref() != Some(pt.as_slice()) {
                    if let Some(name) = &orig_script_filename {
                        env.put(b"ORIG_SCRIPT_FILENAME", Some(name));
                    }
                    env.put(b"SCRIPT_FILENAME", Some(&pt));
                }
                script.clone_from(&pt);
                if let Some(root) = &env_document_root {
                    let root = root.strip_suffix(b"/").unwrap_or(root);
                    let mut translated = root.to_vec();
                    translated.extend_from_slice(env_path_info.as_deref().unwrap_or_default());
                    if let Some(orig) = &orig_path_translated {
                        env.put(b"ORIG_PATH_TRANSLATED", Some(orig));
                    }
                    env.put(b"PATH_TRANSLATED", Some(&translated));
                } else if let Some(name) = &env_script_name
                    && find(&pt, name).is_some()
                {
                    let root_len = pt.len() - name.len();
                    let mut translated = pt[..root_len].to_vec();
                    translated.extend_from_slice(env_path_info.as_deref().unwrap_or_default());
                    if let Some(orig) = &orig_path_translated {
                        env.put(b"ORIG_PATH_TRANSLATED", Some(orig));
                    }
                    env.put(b"PATH_TRANSLATED", Some(&translated));
                }
                break;
            }
            if !found {
                if let Some(name) = &orig_script_filename {
                    env.put(b"ORIG_SCRIPT_FILENAME", Some(name));
                }
                env.put(b"SCRIPT_FILENAME", None);
                path_translated_kept = None;
                info.response_code = 404;
            }
            if info.request_uri.is_none() {
                if orig_script_name.is_some() && orig_script_name == env_script_name {
                    info.request_uri = orig_script_name;
                } else {
                    if let Some(name) = &orig_script_name {
                        env.put(b"ORIG_SCRIPT_NAME", Some(name));
                    }
                    info.request_uri = env.put(b"SCRIPT_NAME", env_script_name.as_deref());
                }
            }
            if found {
                path_translated_kept = Some(script.clone());
            }
        } else {
            if orig_script_filename.as_deref() != Some(script.as_slice()) {
                if let Some(name) = &orig_script_filename {
                    env.put(b"ORIG_SCRIPT_FILENAME", Some(name));
                }
                env.put(b"SCRIPT_FILENAME", Some(&script));
            }
            if !apache_was_here && env_redirect_url.is_some() {
                if let Some(orig) = &orig_path_info {
                    env.put(b"ORIG_PATH_INFO", Some(orig));
                    env.put(b"PATH_INFO", None);
                }
                if let Some(orig) = &orig_path_translated {
                    env.put(b"ORIG_PATH_TRANSLATED", Some(orig));
                    env.put(b"PATH_TRANSLATED", None);
                }
            }
            if env_script_name == orig_script_name {
                info.request_uri = env_script_name;
            } else {
                if let Some(name) = &orig_script_name {
                    env.put(b"ORIG_SCRIPT_NAME", Some(name));
                }
                info.request_uri = env.put(b"SCRIPT_NAME", env_script_name.as_deref());
            }
            path_translated_kept = Some(script.clone());
        }
    } else {
        info.request_uri = env_path_info.or(env_script_name);
        if !settings.discard_path
            && let Some(translated) = env_path_translated
        {
            path_translated_kept = Some(translated);
        }
    }
    info.path_translated = path_translated_kept.filter(|path| valid_path(path));
    info.query = env.get(b"QUERY_STRING").map(<[u8]>::to_vec);
    info.content_type = Some(content_type.unwrap_or_default());
    info.content_length = content_length.as_deref().map_or(0, number.strtol);
    info.auth = env.get(b"HTTP_AUTHORIZATION").and_then(basic_auth);
    info
}

/// `php_raw_url_decode`: each `%XX` its byte.
fn raw_url_decode(text: &[u8]) -> Vec<u8> {
    let hex = |c: u8| char::from(c).to_digit(16);
    let mut out = Vec::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        if text[i] == b'%'
            && i + 2 < text.len()
            && let (Some(hi), Some(lo)) = (hex(text[i + 1]), hex(text[i + 2]))
        {
            out.push(u8::try_from(hi * 16 + lo).unwrap_or(0));
            i += 3;
            continue;
        }
        out.push(text[i]);
        i += 1;
    }
    out
}

/// `is_valid_path`: no `..` segment.
fn valid_path(path: &[u8]) -> bool {
    !path.split(|b| *b == b'/').any(|segment| segment == b"..")
}

/// Where `needle` first stands in `haystack`, `strstr`.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// C's `strtol(s, NULL, 10)` and `atoi`, which `init_request_info`
/// reads `CONTENT_LENGTH` with (`atol`, a long on a 64-bit host),
/// `sapi_cgi_send_headers` a status line's code and `zend_ini_parse_bool`
/// an entry. They skip what `isspace` counts under the process's
/// `LC_CTYPE`, which is the C library's to say. This crate binds none
/// and names no engine crate (design §4.8), so the host hands it the
/// engine's one reading, `php_sapi::cgi::C_NUMBER`.
#[derive(Debug, Clone, Copy)]
pub struct CNumber {
    pub strtol: fn(&[u8]) -> i64,
    pub atoi: fn(&[u8]) -> i32,
}

/// `php_handle_auth_data`'s `Basic` half: the user and the password.
fn basic_auth(auth: &[u8]) -> Option<(Vec<u8>, Option<Vec<u8>>)> {
    match crate::http::credentials(auth)? {
        crate::http::Credentials::Basic { user, password } => Some((user, password)),
        crate::http::Credentials::Digest(_) => None,
    }
}

/// The request headers a CGI environment carries, as php-cgi's
/// `apache_request_headers` reads them: each `HTTP_` variable with the
/// prefix taken off, `_` read as `-`, and each letter lowered but the
/// first and the one after a `-`, whatever it is; `CONTENT_TYPE` and `CONTENT_LENGTH`
/// under their header names.
#[must_use]
pub fn request_headers(env: &Environment) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut headers = Vec::new();
    for (name, value) in &env.0 {
        let name = if let Some(rest) = name.strip_prefix(b"HTTP_").filter(|r| !r.is_empty()) {
            // `apache_request_headers`' walk (`Z/sapi/cgi/cgi_main.c:1619`):
            // the first byte as it stands, each `_` a `-` with the byte
            // after it as it stands, and every other byte lowered.
            let mut out = Vec::with_capacity(rest.len());
            let mut bytes = rest.iter().copied();
            if let Some(first) = bytes.next() {
                out.push(first);
            }
            while let Some(c) = bytes.next() {
                if c == b'_' {
                    out.push(b'-');
                    if let Some(next) = bytes.next() {
                        out.push(next);
                    }
                } else {
                    out.push(c.to_ascii_lowercase());
                }
            }
            out
        } else if name == b"CONTENT_TYPE" {
            b"Content-Type".to_vec()
        } else if name == b"CONTENT_LENGTH" {
            b"Content-Length".to_vec()
        } else {
            continue;
        };
        headers.push((name, value.clone()));
    }
    headers
}

use crate::http::reason;

/// `SAPI_CGI_MAX_HEADER_LENGTH`: the most a `Status:` line php builds holds.
const MAX_HEADER_LENGTH: usize = 1024;

/// When the head opens with a status line, `cgi.nph`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Opens {
    /// Where the response code is not 200, `cgi.nph` off.
    #[default]
    Changed,
    /// Always, `cgi.nph` on: a 200 too is `Status: 200 OK`.
    Always,
}

/// How a status line `header()` was given is written, `cgi.rfc2616_headers`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Form {
    /// `Status:` and an `HTTP/` line's text past its first space,
    /// `cgi.rfc2616_headers` off.
    #[default]
    Status,
    /// The line whole, `HTTP/1.1 404 Not Found`, `cgi.rfc2616_headers` on.
    Line,
}

/// The shape `sapi_cgi_send_headers` gives the head, read off `cgi.nph`
/// and `cgi.rfc2616_headers` at the send (`Z/sapi/cgi/cgi_main.c:381-400`,
/// `Z/sapi/fpm/fpm/fpm_main.c:320-340`), so an `ini_set` before it counts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Shape {
    pub opens: Opens,
    pub form: Form,
}

impl Shape {
    /// The shape the two entries' values give, each read as
    /// `zend_ini_parse_bool` reads it: `true`, `yes` or `on` in any case,
    /// else C's `atoi` of it not 0.
    #[must_use]
    pub fn of(nph: &[u8], rfc2616_headers: &[u8], number: CNumber) -> Shape {
        let on = |value: &[u8]| {
            [&b"true"[..], b"yes", b"on"]
                .iter()
                .any(|word| value.eq_ignore_ascii_case(word))
                || (number.atoi)(value) != 0
        };
        Shape {
            opens: if on(nph) {
                Opens::Always
            } else {
                Opens::Changed
            },
            form: if on(rfc2616_headers) {
                Form::Line
            } else {
                Form::Status
            },
        }
    }
}

/// One line of the head `sapi_cgi_send_headers` writes ([`lines`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Line<'a> {
    /// The response's status line.
    Status(Status<'a>),
    /// A header line the request set, as it set it.
    Field(&'a [u8]),
}

impl Line<'_> {
    /// The line as it goes on the wire, without its line end.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        match self {
            Line::Status(status) => status.bytes(),
            Line::Field(line) => line,
        }
    }
}

/// The response's status as `sapi_cgi_send_headers` writes it, whole:
/// `Status: 404 Not Found`, or under `cgi.rfc2616_headers` the `HTTP/`
/// status line `header()` was given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Status<'a>(Cow<'a, [u8]>);

impl Status<'_> {
    /// The line as it goes on the wire, without its line end.
    #[must_use]
    pub fn bytes(&self) -> &[u8] {
        &self.0
    }

    /// The status a web server in front of php reads off the line, as
    /// nginx's `ngx_http_fastcgi_process_header` reads `Status:`
    /// (`src/http/modules/ngx_http_fastcgi_module.c:2067-2083` at the
    /// release `oracle.toml`'s `[fastcgi]` names): the code, its first
    /// three bytes as digits, as `ngx_atoi(value, 3)` reads them, and the
    /// text past the space after them, which the server's status line
    /// carries as its reason phrase; `None` for a code that is not three
    /// digits, which nginx refuses as an invalid status. An `HTTP/` line
    /// is read past its first space.
    #[must_use]
    pub fn read(&self) -> Option<(u16, &[u8])> {
        let line = self.bytes();
        let value = if is_named(line, b"Status:") {
            line[b"Status:".len()..].trim_ascii_start()
        } else {
            let space = line.iter().position(|b| *b == b' ')?;
            &line[space + 1..]
        };
        let digits = value
            .get(..3)
            .filter(|d| d.iter().all(u8::is_ascii_digit))?;
        let code = digits
            .iter()
            .fold(0u16, |code, d| code * 10 + u16::from(d - b'0'));
        let reason = value[3..].strip_prefix(b" ").unwrap_or(&value[3..]);
        Some((code, reason))
    }
}

/// Whether a header line is the header `name` names, its colon included,
/// with a value after it: `sapi_cgi_send_headers`' `strncasecmp`.
fn is_named(line: &[u8], name: &[u8]) -> bool {
    line.len() > name.len() && line[..name.len()].eq_ignore_ascii_case(name)
}

/// `sapi_cgi_send_headers` ([`lines`]) on the wire: each line and its CRLF,
/// then the blank line.
#[must_use]
pub fn head(
    shape: Shape,
    code: i64,
    status_line: Option<&[u8]>,
    lines: &[Vec<u8>],
    number: CNumber,
) -> Vec<u8> {
    let mut out = Vec::new();
    for line in self::lines(shape, code, status_line, lines, number) {
        out.extend_from_slice(line.bytes());
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"\r\n");
    out
}

/// `sapi_cgi_send_headers` (`Z/sapi/cgi/cgi_main.c:370`,
/// `Z/sapi/fpm/fpm/fpm_main.c:308`) in `shape`, line by line: a response
/// code other than 200, or any under `cgi.nph`, writes a status line
/// first; under `cgi.rfc2616_headers` the status line `header()` was
/// given whole where it was, else `Status:` and an `HTTP/` status line's
/// text past its first space where one was given, else the code and php's
/// reason for it, unless a `Status:` header stands in the lines; then each
/// line, the first `Status:` line alone of its kind, a `Content-Type`
/// dropped under a 304.
#[must_use]
pub fn lines<'a>(
    shape: Shape,
    code: i64,
    status_line: Option<&'a [u8]>,
    lines: &'a [Vec<u8>],
    number: CNumber,
) -> Vec<Line<'a>> {
    let mut out = Vec::new();
    let mut ignore_status = false;
    let mut response_status = code;
    if shape.opens == Opens::Always || code != 200 {
        let status = if let (Form::Line, Some(line)) = (shape.form, status_line) {
            if let Some(space) = line.iter().position(|b| *b == b' ') {
                response_status = i64::from((number.atoi)(&line[space + 1..]));
            }
            Some(line.to_vec())
        } else if let Some(rest) = status_line.and_then(|line| {
            let space = line.iter().position(|b| *b == b' ')?;
            (space >= 5 && line[..5].eq_ignore_ascii_case(b"HTTP/")).then(|| &line[space..])
        }) {
            response_status = i64::from((number.atoi)(&rest[1..]));
            let mut built = b"Status:".to_vec();
            built.extend_from_slice(rest);
            Some(built)
        } else if lines.iter().any(|line| is_named(line, b"Status:")) {
            None
        } else {
            Some(match reason(code) {
                Some(text) => format!("Status: {code} {text}").into_bytes(),
                None => format!("Status: {code}").into_bytes(),
            })
        };
        if let Some(mut status) = status {
            status.truncate(MAX_HEADER_LENGTH - 1);
            out.push(Line::Status(Status(status.into())));
            ignore_status = true;
        }
    }
    for line in lines.iter().filter(|line| !line.is_empty()) {
        if is_named(line, b"Status:") {
            if !ignore_status {
                ignore_status = true;
                out.push(Line::Status(Status(line.as_slice().into())));
            }
        } else if response_status != 304 || !is_named(line, b"Content-Type:") {
            out.push(Line::Field(line));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake(Vec<&'static [u8]>);

    impl Files for Fake {
        fn resolves(&self, path: &[u8]) -> bool {
            self.0.contains(&path)
        }
        fn stat(&self, path: &[u8]) -> Option<Stat> {
            self.0.contains(&path).then_some(Stat::File(0))
        }
    }

    fn env(rows: &[(&str, &str)]) -> Environment {
        Environment(
            rows.iter()
                .map(|(n, v)| (n.as_bytes().to_vec(), v.as_bytes().to_vec()))
                .collect(),
        )
    }

    /// A reading of plain decimal digits for the tests, which hold no
    /// edge C's library reads; those are `php_engine`'s `c_number` tests.
    const DIGITS: CNumber = CNumber {
        strtol: leading,
        atoi: |b| i32::try_from(leading(b)).unwrap_or(0),
    };

    /// The digits a value opens with, as a number.
    fn leading(b: &[u8]) -> i64 {
        b.iter()
            .take_while(|d| d.is_ascii_digit())
            .fold(0, |n, d| n * 10 + i64::from(d - b'0'))
    }

    #[test]
    fn a_request_names_its_file_method_query_and_body() {
        let mut vars = env(&[
            ("SCRIPT_FILENAME", "/d/t.php"),
            ("REQUEST_METHOD", "POST"),
            ("QUERY_STRING", "a=1"),
            ("CONTENT_LENGTH", "13"),
            ("SCRIPT_NAME", "/t.php"),
            ("HTTP_AUTHORIZATION", "Basic dXNlcjpwdw=="),
        ]);
        let info = request_info(
            &mut vars,
            Settings::default(),
            None,
            &Fake(vec![b"/d/t.php"]),
            DIGITS,
        );
        assert_eq!(info.path_translated.as_deref(), Some(&b"/d/t.php"[..]));
        assert_eq!(info.request_uri.as_deref(), Some(&b"/t.php"[..]));
        assert_eq!(info.method.as_deref(), Some(&b"POST"[..]));
        assert_eq!(info.content_type.as_deref(), Some(&b""[..]));
        assert_eq!(info.content_length, 13);
        assert_eq!(info.response_code, 200);
        assert_eq!(info.auth, Some((b"user".to_vec(), Some(b"pw".to_vec()))));
    }

    /// A parameter holding a NUL is read up to it, so the file it names
    /// is the one before the NUL, and a value with none stands whole
    /// (#2757).
    #[test]
    fn a_parameter_cut_at_its_nul_names_the_file_before_it() {
        let mut vars = env(&[
            ("SCRIPT_FILENAME", "/d/t.php\0.txt"),
            ("QUERY_STRING", "a=1\0&b=2"),
            ("REQUEST_METHOD", "GET"),
        ])
        .cut_at_nul();
        assert_eq!(vars.get(b"QUERY_STRING"), Some(&b"a=1"[..]));
        assert_eq!(vars.get(b"REQUEST_METHOD"), Some(&b"GET"[..]));
        let info = request_info(
            &mut vars,
            Settings::default(),
            None,
            &Fake(vec![b"/d/t.php"]),
            DIGITS,
        );
        assert_eq!(info.path_translated.as_deref(), Some(&b"/d/t.php"[..]));
        assert_eq!(info.query.as_deref(), Some(&b"a=1"[..]));
        assert_eq!(info.response_code, 200);
    }

    #[test]
    fn a_path_past_the_file_is_path_info() {
        let mut vars = env(&[
            ("SCRIPT_FILENAME", "/d/t.php/a/b"),
            ("PATH_INFO", "/t.php/a/b"),
        ]);
        let info = request_info(
            &mut vars,
            Settings::default(),
            None,
            &Fake(vec![b"/d/t.php"]),
            DIGITS,
        );
        assert_eq!(info.path_translated.as_deref(), Some(&b"/d/t.php"[..]));
        assert_eq!(vars.get(b"PATH_INFO"), Some(&b"/a/b"[..]));
        assert_eq!(vars.get(b"SCRIPT_FILENAME"), Some(&b"/d/t.php"[..]));
        assert_eq!(
            vars.get(b"ORIG_SCRIPT_FILENAME"),
            Some(&b"/d/t.php/a/b"[..])
        );
        let mut none = env(&[("SCRIPT_FILENAME", "/nope/x.php")]);
        let info = request_info(
            &mut none,
            Settings::default(),
            None,
            &Fake(Vec::new()),
            DIGITS,
        );
        assert_eq!((info.path_translated, info.response_code), (None, 404));
    }

    #[test]
    fn the_head_opens_with_the_status_where_the_code_is_not_200() {
        let lines = [b"X-Powered-By: PHP/8.5.9".to_vec(), b"Location: /".to_vec()];
        assert_eq!(
            head(Shape::default(), 302, None, &lines, DIGITS),
            b"Status: 302 Found\r\nX-Powered-By: PHP/8.5.9\r\nLocation: /\r\n\r\n"
        );
        assert_eq!(
            head(
                Shape::default(),
                404,
                Some(b"HTTP/1.1 404 Nope"),
                &[],
                DIGITS
            ),
            b"Status: 404 Nope\r\n\r\n"
        );
        assert_eq!(
            head(
                Shape::default(),
                200,
                None,
                &[b"Content-type: text/html".to_vec()],
                DIGITS,
            ),
            b"Content-type: text/html\r\n\r\n"
        );
        assert_eq!(
            head(
                Shape::default(),
                299,
                None,
                &[b"Status: 299 Mine".to_vec(), b"Status: 1".to_vec()],
                DIGITS,
            ),
            b"Status: 299 Mine\r\n\r\n"
        );
    }

    /// A web server reads a status line's code and the reason after it,
    /// and refuses a code that is not three digits.
    #[test]
    fn a_status_line_is_read_as_a_web_server_reads_it() {
        let status = |line: &'static [u8]| {
            let status = Status(line.into());
            status.read().map(|(code, reason)| (code, reason.to_vec()))
        };
        assert_eq!(
            status(b"Status: 403 Access Denied"),
            Some((403, b"Access Denied".to_vec()))
        );
        assert_eq!(status(b"status:399"), Some((399, Vec::new())));
        assert_eq!(
            status(b"HTTP/1.1 201 A long one"),
            Some((201, b"A long one".to_vec()))
        );
        assert_eq!(status(b"Status: 1"), None);
        let lines = [b"Content-Type: text/html".to_vec(), b"X: 1".to_vec()];
        assert_eq!(
            super::lines(
                Shape::default(),
                304,
                Some(b"HTTP/1.1 304 Not Modified"),
                &lines,
                DIGITS,
            ),
            [
                Line::Status(Status(b"Status: 304 Not Modified".as_slice().into())),
                Line::Field(b"X: 1")
            ]
        );
    }

    #[test]
    fn nph_opens_every_head_and_rfc2616_writes_the_status_line_whole() {
        let lines = [b"X-Powered-By: PHP/8.5.9".to_vec()];
        let nph = Shape::of(b"1", b"0", DIGITS);
        assert_eq!(nph.opens, Opens::Always);
        assert_eq!(
            head(nph, 200, None, &lines, DIGITS),
            b"Status: 200 OK\r\nX-Powered-By: PHP/8.5.9\r\n\r\n"
        );
        let line = Shape::of(b"off", b"Yes", DIGITS);
        assert_eq!(line.form, Form::Line);
        assert_eq!(
            head(line, 404, Some(b"HTTP/1.1 404 Not Found"), &[], DIGITS),
            b"HTTP/1.1 404 Not Found\r\n\r\n"
        );
        assert_eq!(
            head(
                line,
                304,
                Some(b"HTTP/1.1 304 Not Modified"),
                &[b"Content-Type: x".to_vec()],
                DIGITS,
            ),
            b"HTTP/1.1 304 Not Modified\r\n\r\n"
        );
        assert_eq!(
            head(line, 200, Some(b"HTTP/1.1 200 OK"), &[], DIGITS),
            b"\r\n"
        );
        assert_eq!(Shape::of(b"", b"0", DIGITS), Shape::default());
    }
}
