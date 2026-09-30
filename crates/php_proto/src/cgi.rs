//! CGI/1.1 as php's CGI host reads and writes it (`sapi/cgi/cgi_main.c` at
//! the pin): the environment a server hands a request in, read into the
//! plain fields php's `SG(request_info)` keeps (`init_request_info`), and
//! the head a response goes out with (`sapi_cgi_send_headers`). Bytes and
//! plain values: the one question asked of the file system, whether a
//! path names a file, is the caller's to answer.

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

    /// `CGI_PUTENV`: `setenv(3)`, or `unsetenv(3)` for `None`; the value
    /// set is answered.
    fn put(&mut self, name: &[u8], value: Option<&[u8]>) -> Option<Vec<u8>> {
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
}

/// What a file system answers about a path: the two questions
/// `init_request_info` asks, `realpath(3)` succeeding and `stat(2)`
/// finding a regular file.
pub trait Files {
    /// Whether `realpath(3)` resolves the path.
    fn resolves(&self, path: &[u8]) -> bool;
    /// Whether `stat(2)` finds a regular file at the path.
    fn is_file(&self, path: &[u8]) -> bool;
}

/// The `cgi.*` entries `init_request_info` reads, at php's defaults.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Settings {
    /// `cgi.fix_pathinfo`.
    pub fix_pathinfo: bool,
    /// `cgi.discard_path`.
    pub discard_path: bool,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            fix_pathinfo: true,
            discard_path: false,
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

/// `init_request_info` (`Z/sapi/cgi/cgi_main.c:1159`) over `env`, which it
/// rewrites as php's does: `SCRIPT_FILENAME`, `PATH_INFO`, `SCRIPT_NAME`
/// and `PATH_TRANSLATED` fixed where `cgi.fix_pathinfo` finds the file
/// the path names, each value replaced kept under its `ORIG_` name.
#[must_use]
#[allow(clippy::too_many_lines)] // php's one function, kept as one
pub fn request_info(env: &mut Environment, settings: Settings, files: &dyn Files) -> RequestInfo {
    let env_script_filename = env.get(b"SCRIPT_FILENAME").map(<[u8]>::to_vec);
    let env_path_translated = env.get(b"PATH_TRANSLATED").map(<[u8]>::to_vec);
    let mut script_path_translated = env_script_filename
        .clone()
        .or_else(|| env_path_translated.clone());
    let mut info = RequestInfo {
        response_code: 200,
        ..RequestInfo::default()
    };
    let Some(mut script) = script_path_translated.take() else {
        return info;
    };
    let content_length = env.get(b"CONTENT_LENGTH").map(<[u8]>::to_vec);
    let content_type = env.get(b"CONTENT_TYPE").map(<[u8]>::to_vec);
    let mut env_path_info = env.get(b"PATH_INFO").map(<[u8]>::to_vec);
    let mut env_script_name = env.get(b"SCRIPT_NAME").map(<[u8]>::to_vec);
    let mut path_translated_kept = Some(script.clone());
    if settings.fix_pathinfo {
        let env_redirect_url = env.get(b"REDIRECT_URL").map(<[u8]>::to_vec);
        let env_document_root = env.get(b"DOCUMENT_ROOT").map(<[u8]>::to_vec);
        let orig_path_translated = env_path_translated.clone();
        let orig_path_info = env_path_info.clone();
        let orig_script_name = env_script_name.clone();
        let orig_script_filename = env_script_filename.clone();
        if let (Some(translated), Some(redirect)) = (&env_path_translated, &env_redirect_url)
            && *translated != script
        {
            script.clone_from(translated);
            env_script_name = Some(redirect.clone());
        }
        if script.last() == Some(&b'/') || !files.resolves(&script) {
            let mut pt = script.clone();
            let mut found = false;
            while let Some(slash) = pt.iter().rposition(|b| *b == b'/') {
                pt.truncate(slash);
                if !files.is_file(&pt) {
                    continue;
                }
                found = true;
                // The file is `pt`, and what the path had past it is
                // `PATH_INFO`'s tail.
                let slen = script.len() - pt.len();
                let path_info = env_path_info
                    .as_ref()
                    .map(|info| info[info.len().saturating_sub(slen)..].to_vec());
                if orig_path_info != path_info {
                    if let Some(orig) = &orig_path_info {
                        env.put(b"ORIG_PATH_INFO", Some(orig));
                        let head = &orig[..orig.len().saturating_sub(slen)];
                        if orig_script_name.as_deref() == Some(head) {
                            info.request_uri.clone_from(&orig_script_name);
                        } else {
                            if let Some(name) = &orig_script_name {
                                env.put(b"ORIG_SCRIPT_NAME", Some(name));
                            }
                            info.request_uri = env.put(b"SCRIPT_NAME", Some(head));
                        }
                    }
                    env_path_info = env.put(b"PATH_INFO", path_info.as_deref());
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
                    let ptlen = pt.len() - name.len();
                    let mut translated = pt[..ptlen].to_vec();
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
                if orig_script_name == env_script_name {
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
            if env_redirect_url.is_some() {
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
    info.method = env.get(b"REQUEST_METHOD").map(<[u8]>::to_vec);
    info.query = env.get(b"QUERY_STRING").map(<[u8]>::to_vec);
    info.content_type = Some(content_type.unwrap_or_default());
    info.content_length = content_length.as_deref().map_or(0, atol);
    info.auth = env.get(b"HTTP_AUTHORIZATION").and_then(basic_auth);
    info
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

/// C's `atol`.
fn atol(bytes: &[u8]) -> i64 {
    let bytes = bytes.trim_ascii_start();
    let (negative, digits) = match bytes.first() {
        Some(b'-') => (true, &bytes[1..]),
        Some(b'+') => (false, &bytes[1..]),
        _ => (false, bytes),
    };
    let mut value: i64 = 0;
    for digit in digits.iter().take_while(|b| b.is_ascii_digit()) {
        value = value.wrapping_mul(10).wrapping_add(i64::from(digit - b'0'));
    }
    if negative {
        value.wrapping_neg()
    } else {
        value
    }
}

/// `php_handle_auth_data`'s `Basic` half (`Z/main/main.c:2735`): the user
/// before the first `:` of the decoded credentials and the password
/// after it, where it is not empty.
fn basic_auth(auth: &[u8]) -> Option<(Vec<u8>, Option<Vec<u8>>)> {
    let head = auth.get(..6)?;
    if !head.eq_ignore_ascii_case(b"Basic ") {
        return None;
    }
    let decoded = base64(&auth[6..])?;
    let decoded = decoded.split(|b| *b == 0).next().unwrap_or_default();
    let colon = decoded.iter().position(|b| *b == b':')?;
    let pass = &decoded[colon + 1..];
    Some((
        decoded[..colon].to_vec(),
        (!pass.is_empty()).then(|| pass.to_vec()),
    ))
}

/// `php_base64_decode`, the lenient one: bytes outside the alphabet
/// skipped, and `None` for a lone digit left over.
fn base64(input: &[u8]) -> Option<Vec<u8>> {
    let value = |c: u8| match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    };
    let mut out = Vec::new();
    let mut acc = 0u32;
    let mut bits = 0;
    let mut digits = 0;
    for c in input {
        if *c == b'=' {
            break;
        }
        let Some(v) = value(*c) else { continue };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        digits += 1;
        if bits >= 8 {
            bits -= 8;
            out.push(u8::try_from((acc >> bits) & 0xff).unwrap_or(0));
        }
    }
    (digits % 4 != 1).then_some(out)
}

/// The request headers a CGI environment carries, as php-cgi's
/// `apache_request_headers` reads them: each `HTTP_` variable with the
/// prefix taken off, `_` read as `-`, and each letter lowered but the
/// first and the one after a `-`; `CONTENT_TYPE` and `CONTENT_LENGTH`
/// under their header names.
#[must_use]
pub fn request_headers(env: &Environment) -> Vec<(Vec<u8>, Vec<u8>)> {
    let mut headers = Vec::new();
    for (name, value) in &env.0 {
        let name = if let Some(rest) = name.strip_prefix(b"HTTP_").filter(|r| !r.is_empty()) {
            let mut out = Vec::with_capacity(rest.len());
            let mut upper = true;
            for &c in rest {
                if c == b'_' {
                    out.push(b'-');
                    upper = true;
                } else if upper {
                    out.push(c);
                    upper = false;
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

/// `http_status_map` (`Z/main/http_status_codes.h`): the reason phrase
/// php knows a code by.
#[must_use]
pub fn reason(code: i64) -> Option<&'static str> {
    const MAP: [(i64, &str); 48] = [
        (100, "Continue"),
        (101, "Switching Protocols"),
        (200, "OK"),
        (201, "Created"),
        (202, "Accepted"),
        (203, "Non-Authoritative Information"),
        (204, "No Content"),
        (205, "Reset Content"),
        (206, "Partial Content"),
        (300, "Multiple Choices"),
        (301, "Moved Permanently"),
        (302, "Found"),
        (303, "See Other"),
        (304, "Not Modified"),
        (305, "Use Proxy"),
        (307, "Temporary Redirect"),
        (308, "Permanent Redirect"),
        (400, "Bad Request"),
        (401, "Unauthorized"),
        (402, "Payment Required"),
        (403, "Forbidden"),
        (404, "Not Found"),
        (405, "Method Not Allowed"),
        (406, "Not Acceptable"),
        (407, "Proxy Authentication Required"),
        (408, "Request Timeout"),
        (409, "Conflict"),
        (410, "Gone"),
        (411, "Length Required"),
        (412, "Precondition Failed"),
        (413, "Request Entity Too Large"),
        (414, "Request-URI Too Long"),
        (415, "Unsupported Media Type"),
        (416, "Requested Range Not Satisfiable"),
        (417, "Expectation Failed"),
        (426, "Upgrade Required"),
        (428, "Precondition Required"),
        (429, "Too Many Requests"),
        (431, "Request Header Fields Too Large"),
        (451, "Unavailable For Legal Reasons"),
        (500, "Internal Server Error"),
        (501, "Not Implemented"),
        (502, "Bad Gateway"),
        (503, "Service Unavailable"),
        (504, "Gateway Timeout"),
        (505, "HTTP Version Not Supported"),
        (506, "Variant Also Negotiates"),
        (511, "Network Authentication Required"),
    ];
    MAP.iter()
        .find(|(had, _)| *had == code)
        .map(|(_, text)| *text)
}

/// `SAPI_CGI_MAX_HEADER_LENGTH`: the most a `Status:` line php builds holds.
const MAX_HEADER_LENGTH: usize = 1024;

/// `sapi_cgi_send_headers` (`Z/sapi/cgi/cgi_main.c:370`) with `cgi.nph`
/// and `cgi.rfc2616_headers` off: a response code other than 200 writes
/// a `Status:` line first, from an `HTTP/` status line's text past its
/// first space where one was given, else from the code and php's reason
/// for it, unless a `Status:` header stands in the lines; then each line,
/// the first `Status:` line alone of its kind, a `Content-Type` dropped
/// under a 304; then the blank line.
#[must_use]
pub fn head(code: i64, status_line: Option<&[u8]>, lines: &[Vec<u8>]) -> Vec<u8> {
    let is = |line: &[u8], name: &[u8]| {
        line.len() > name.len() && line[..name.len()].eq_ignore_ascii_case(name)
    };
    let mut out = Vec::new();
    let mut ignore_status = false;
    let mut response_status = code;
    if code != 200 {
        let from_line = status_line.and_then(|line| {
            let space = line.iter().position(|b| *b == b' ')?;
            (space >= 5 && line[..5].eq_ignore_ascii_case(b"HTTP/")).then(|| &line[space..])
        });
        let status = if let Some(rest) = from_line {
            response_status = atol(&rest[1..]);
            let mut built = b"Status:".to_vec();
            built.extend_from_slice(rest);
            Some(built)
        } else if lines.iter().any(|line| is(line, b"Status:")) {
            None
        } else {
            Some(match reason(code) {
                Some(text) => format!("Status: {code} {text}").into_bytes(),
                None => format!("Status: {code}").into_bytes(),
            })
        };
        if let Some(mut status) = status {
            status.truncate(MAX_HEADER_LENGTH - 1);
            out.extend_from_slice(&status);
            out.extend_from_slice(b"\r\n");
            ignore_status = true;
        }
    }
    for line in lines.iter().filter(|line| !line.is_empty()) {
        if is(line, b"Status:") {
            if !ignore_status {
                ignore_status = true;
                out.extend_from_slice(line);
                out.extend_from_slice(b"\r\n");
            }
        } else if response_status != 304 || !is(line, b"Content-Type:") {
            out.extend_from_slice(line);
            out.extend_from_slice(b"\r\n");
        }
    }
    out.extend_from_slice(b"\r\n");
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
        fn is_file(&self, path: &[u8]) -> bool {
            self.0.contains(&path)
        }
    }

    fn env(rows: &[(&str, &str)]) -> Environment {
        Environment(
            rows.iter()
                .map(|(n, v)| (n.as_bytes().to_vec(), v.as_bytes().to_vec()))
                .collect(),
        )
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
        let info = request_info(&mut vars, Settings::default(), &Fake(vec![b"/d/t.php"]));
        assert_eq!(info.path_translated.as_deref(), Some(&b"/d/t.php"[..]));
        assert_eq!(info.request_uri.as_deref(), Some(&b"/t.php"[..]));
        assert_eq!(info.method.as_deref(), Some(&b"POST"[..]));
        assert_eq!(info.content_type.as_deref(), Some(&b""[..]));
        assert_eq!(info.content_length, 13);
        assert_eq!(info.response_code, 200);
        assert_eq!(info.auth, Some((b"user".to_vec(), Some(b"pw".to_vec()))));
    }

    #[test]
    fn a_path_past_the_file_is_path_info() {
        let mut vars = env(&[
            ("SCRIPT_FILENAME", "/d/t.php/a/b"),
            ("PATH_INFO", "/t.php/a/b"),
        ]);
        let info = request_info(&mut vars, Settings::default(), &Fake(vec![b"/d/t.php"]));
        assert_eq!(info.path_translated.as_deref(), Some(&b"/d/t.php"[..]));
        assert_eq!(vars.get(b"PATH_INFO"), Some(&b"/a/b"[..]));
        assert_eq!(vars.get(b"SCRIPT_FILENAME"), Some(&b"/d/t.php"[..]));
        assert_eq!(
            vars.get(b"ORIG_SCRIPT_FILENAME"),
            Some(&b"/d/t.php/a/b"[..])
        );
        let mut none = env(&[("SCRIPT_FILENAME", "/nope/x.php")]);
        let info = request_info(&mut none, Settings::default(), &Fake(Vec::new()));
        assert_eq!((info.path_translated, info.response_code), (None, 404));
    }

    #[test]
    fn the_head_opens_with_the_status_where_the_code_is_not_200() {
        let lines = [b"X-Powered-By: PHP/8.5.9".to_vec(), b"Location: /".to_vec()];
        assert_eq!(
            head(302, None, &lines),
            b"Status: 302 Found\r\nX-Powered-By: PHP/8.5.9\r\nLocation: /\r\n\r\n"
        );
        assert_eq!(
            head(404, Some(b"HTTP/1.1 404 Nope"), &[]),
            b"Status: 404 Nope\r\n\r\n"
        );
        assert_eq!(
            head(200, None, &[b"Content-type: text/html".to_vec()]),
            b"Content-type: text/html\r\n\r\n"
        );
        assert_eq!(
            head(
                299,
                None,
                &[b"Status: 299 Mine".to_vec(), b"Status: 1".to_vec()]
            ),
            b"Status: 299 Mine\r\n\r\n"
        );
    }
}
