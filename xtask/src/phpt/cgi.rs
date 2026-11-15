//! A CGI-shaped test, run as run-tests.php runs one at the pinned tag
//! (run-tests.php:1891-1901, 1995-2008, 2279-2425, 2530-2575): through the
//! CGI host beside the binary under test, `-C` and `-f` before the program,
//! the request it describes laid into the environment, the body on stdin,
//! and what it wrote parted into the headers before the first blank line
//! and the body after it, the headers held to `--EXPECTHEADERS--`.
//!
//! The host is the php-cgi `[sapi]` pins for the runner's control, and
//! the engine's own `php-cgi` beside the binary under test, `sapi/cgi`'s
//! binary (ADR-0008), which is where a test that looks for php-cgi
//! beside `TEST_PHP_EXECUTABLE` finds it.
//! [`request`] is the one translation of a test's sections into what its
//! host is handed, whichever host it is.

use std::ffi::OsString;
use std::os::unix::ffi::OsStringExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::environment::Environment;
use super::file::Phpt;
use super::{Judged, Judgement};
use crate::oracle::{self, Normalise};

/// Where run-tests.php finds php-cgi in a build tree, php-src's layout and
/// not this workspace's.
const BESIDE: &str = "sapi/cgi/php-cgi";

/// `binary` under the name php-cgi: a link of that name to it at
/// [`BESIDE`] under `root`, the witness lane's `php-cgi` run under the
/// name php-cgi builds it by.
pub(crate) fn linked(binary: &Path, root: &Path) -> Result<PathBuf> {
    let link = root.join(BESIDE);
    crate::make_parent(&link)?;
    crate::remove(&link)?;
    std::os::unix::fs::symlink(binary, &link)
        .with_context(|| format!("linking {} to {}", link.display(), binary.display()))?;
    Ok(link)
}

/// What one CGI-shaped test hands its host past the recipe: the
/// environment, whose later entries stand over earlier ones, what it reads
/// on stdin, and the words after the program.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Request {
    pub(crate) env: Environment,
    pub(crate) stdin: Option<Vec<u8>>,
    pub(crate) args: Vec<String>,
}

/// The request a CGI-shaped test describes, as run-tests.php builds it
/// (run-tests.php:2279-2425): over `env`, the test's own environment with
/// the CGI variables emptied and its `--ENV--` over them, `REDIRECT_STATUS`, the query string of `--GET--`,
/// `program` as `PATH_TRANSLATED` and `SCRIPT_FILENAME`, `--COOKIE--` as
/// `HTTP_COOKIE`, each where `--ENV--` left it empty, and then the one body
/// section it carries: `--POST_RAW--` and `--PUT--` with their
/// `Content-Type:` line taken out as `CONTENT_TYPE`, `--POST--` as a form,
/// or none, a `GET` with the words of `--ARGS--` after `--`. A body section
/// with nothing left in it is upstream's bork, and the compressed forms
/// are a skip, since the runner has no encoder of php's (`gzencode`,
/// `gzcompress`); both are the `Err`, with the judgement it comes to.
pub(crate) fn request(
    phpt: &Phpt,
    program: &Path,
    env: &Environment,
) -> Result<Request, (Refused, String)> {
    let mut out = env.clone();
    out.set("REDIRECT_STATUS", "1");
    let path = program.as_os_str().as_encoded_bytes().to_vec();
    let query = trim_bytes(phpt.section("GET").unwrap_or_default()).to_vec();
    for (name, value) in [
        ("QUERY_STRING", query),
        ("PATH_TRANSLATED", path.clone()),
        ("SCRIPT_FILENAME", path),
    ] {
        if !out.given(name) {
            out.set(name, bytes(value));
        }
    }
    let cookie = trim_bytes(phpt.section("COOKIE").unwrap_or_default()).to_vec();
    out.set("HTTP_COOKIE", bytes(cookie));
    let raw = [("POST_RAW", "POST"), ("PUT", "PUT")]
        .into_iter()
        .find(|(section, _)| phpt.filled(section).is_some());
    let (stdin, args) = if let Some((section, method)) = raw {
        let (content_type, body) = raw_body(phpt.section(section).unwrap_or_default());
        if !out.given("CONTENT_TYPE")
            && let Some(content_type) = content_type
        {
            out.set("CONTENT_TYPE", bytes(content_type));
        }
        out.set("CONTENT_LENGTH", bytes(body.len().to_string().into_bytes()));
        if method == "PUT" || !out.given("REQUEST_METHOD") {
            out.set("REQUEST_METHOD", method);
        }
        if body.is_empty() {
            return Err((Refused::Bork, "empty $request".to_owned()));
        }
        (Some(body), Vec::new())
    } else if let Some(post) = phpt.filled("POST") {
        let post = trim_bytes(post);
        out.set("REQUEST_METHOD", "POST");
        if !out.given("CONTENT_TYPE") {
            out.set("CONTENT_TYPE", "application/x-www-form-urlencoded");
        }
        if !out.given("CONTENT_LENGTH") {
            out.set("CONTENT_LENGTH", bytes(post.len().to_string().into_bytes()));
        }
        (Some(post.to_vec()), Vec::new())
    } else if let Some(section) = ["GZIP_POST", "DEFLATE_POST"]
        .into_iter()
        .find(|section| phpt.filled(section).is_some())
    {
        return Err((
            Refused::Skip,
            format!(
                "a --{section}-- body is compressed by php's zlib, which this runner has no encoder of"
            ),
        ));
    } else {
        out.set("REQUEST_METHOD", "GET");
        out.set("CONTENT_TYPE", "");
        out.set("CONTENT_LENGTH", "");
        let mut args = Vec::new();
        if let Some(words) = phpt.text("ARGS") {
            args.push("--".to_owned());
            args.extend(words.split_whitespace().map(str::to_owned));
        }
        (None, args)
    };
    Ok(Request {
        env: out,
        stdin,
        args,
    })
}

/// Bytes the test wrote as an environment value, no encoding of them.
fn bytes(value: Vec<u8>) -> OsString {
    OsString::from_vec(value)
}

/// The request a test describes where it is CGI-shaped, and none where it
/// is not.
pub(super) fn request_of(
    phpt: &Phpt,
    program: &Path,
    env: &Environment,
) -> Result<Option<Request>, (Refused, String)> {
    phpt.is_cgi()
        .then(|| request(phpt, program, env))
        .transpose()
}

/// What a request that cannot be run comes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refused {
    Bork,
    Skip,
}

impl Refused {
    /// The verdict, with why.
    pub(super) fn judged(self, id: &str, name: &str, why: String) -> Judged {
        match self {
            Refused::Skip => Judged::new(id, name, Judgement::Skip, why),
            Refused::Bork => super::borked(id, name, why),
        }
    }
}

/// A `--POST_RAW--` or `--PUT--` section read as upstream reads it: trimmed,
/// cut into lines at `\n`, the first `Content-Type:` line taken out as the
/// content type, and the rest joined by `\n`.
fn raw_body(section: &[u8]) -> (Option<Vec<u8>>, Vec<u8>) {
    let mut content_type = None;
    let mut body = Vec::new();
    let mut started = false;
    for line in trim_bytes(section).split(|b| *b == b'\n') {
        if content_type.is_none()
            && let Some(value) = self::content_type(line)
        {
            content_type = Some(value);
            continue;
        }
        if started {
            body.push(b'\n');
        }
        started = true;
        body.extend_from_slice(line);
    }
    (content_type, body)
}

/// The value of a `Content-Type:` line of a raw body, as upstream's
/// `/^Content-Type:(.*)/i` takes it, trimmed and with its `\r` taken out.
fn content_type(line: &[u8]) -> Option<Vec<u8>> {
    const NAME: &[u8] = b"content-type:";
    let head = line.get(..NAME.len())?;
    head.eq_ignore_ascii_case(NAME).then(|| {
        let value: Vec<u8> = line[NAME.len()..]
            .iter()
            .copied()
            .filter(|b| *b != b'\r')
            .collect();
        trim_bytes(&value).to_vec()
    })
}

/// php's `trim()` with its default set.
fn trim(text: &str) -> String {
    text.trim_matches(super::adapter::TRIM).to_owned()
}

/// The same over bytes.
fn trim_bytes(bytes: &[u8]) -> &[u8] {
    let set = |b: &u8| matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0 | 0x0b);
    let start = bytes.iter().position(|b| !set(b)).unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|b| !set(b))
        .map_or(start, |at| at + 1);
    &bytes[start..end]
}

/// What a CGI run wrote, as it wrote it, parted as run-tests.php parts it
/// (run-tests.php:2530-2543): where `/^(.*?)\r?\n\r?\n(.*)/s` matches, the
/// lines before the first blank one, each with a colon read as a header by
/// its name, a later one of a name standing over an earlier, and the body
/// after it, normalised as the recipe normalises a run; the whole of it
/// as the body otherwise.
pub(super) fn parted(out: &[u8], rules: &Normalise) -> (Vec<(String, String)>, Vec<u8>) {
    let Some((end, body)) = first_blank(out) else {
        return (Vec::new(), oracle::normalise(rules, out.to_vec()));
    };
    let mut headers: Vec<(String, String)> = Vec::new();
    for line in out[..end].split(|b| matches!(b, b'\n' | b'\r')) {
        let line = String::from_utf8_lossy(line);
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let (name, value) = (trim(name), trim(value));
        headers.retain(|(had, _)| *had != name);
        headers.push((name, value));
    }
    (headers, oracle::normalise(rules, out[body..].to_vec()))
}

/// Where the first `\r?\n\r?\n` stands: its start and the body's.
fn first_blank(out: &[u8]) -> Option<(usize, usize)> {
    let mut at = 0;
    while at < out.len() {
        if out[at] == b'\n' {
            let mut next = at + 1;
            if out.get(next) == Some(&b'\r') {
                next += 1;
            }
            if out.get(next) == Some(&b'\n') {
                let start = if at > 0 && out[at - 1] == b'\r' {
                    at - 1
                } else {
                    at
                };
                return Some((start, next + 1));
            }
        }
        at += 1;
    }
    None
}

/// `--EXPECTHEADERS--` held to the headers a run wrote
/// (run-tests.php:2549-2576): each wanted line with a colon, by its name,
/// is met by a header of that name whose value php's `==` finds equal.
/// A name is held as [`Names`] says.
/// `None` where every one is met; the wanted lines and the ones the run
/// wrote of those names otherwise, each joined by `\n`, which upstream
/// shows above the body with `--HEADERS--` between.
pub(super) fn headers_missed(
    wanted: &str,
    (headers, names): (&[(String, String)], Names),
) -> Option<(String, String)> {
    let mut want: Vec<(String, String)> = Vec::new();
    for line in wanted.split(['\n', '\r']) {
        if let Some((name, value)) = line.split_once(':') {
            let (name, value) = (trim(name), trim(value));
            want.retain(|(had, _)| *had != name);
            want.push((name, value));
        }
    }
    let mut failed = false;
    let mut got = Vec::new();
    for (name, value) in &want {
        let found = (headers.iter()).find(|(had, _)| names.same(had, name));
        if let Some((_, had)) = found {
            got.push(format!("{name}: {had}"));
        }
        if !found.is_some_and(|(_, had)| loose_eq(had, value)) {
            failed = true;
        }
    }
    failed.then(|| {
        let wanted: Vec<String> = want.iter().map(|(n, v)| format!("{n}: {v}")).collect();
        (wanted.join("\n"), got.join("\n"))
    })
}

/// A verdict with `--EXPECTHEADERS--` held beside the body's: headers it
/// missed fail the test whatever the body did, the wanted and the written
/// shown above the body with `--HEADERS--` between
/// (run-tests.php:2660-2667). What was met, and the expected and actual
/// texts a difference is shown between.
pub(super) fn with_headers(
    wanted: Option<&str>,
    headers: (&[(String, String)], Names),
    (met, expected): (bool, &[u8]),
    actual: Vec<u8>,
) -> (bool, Vec<u8>, Vec<u8>) {
    match wanted.and_then(|wanted| headers_missed(wanted, headers)) {
        None => (met, expected.to_vec(), actual),
        Some((wanted, written)) => {
            let above = |lines: String, body: &[u8]| {
                let mut shown = format!("{lines}\n--HEADERS--\n").into_bytes();
                shown.extend_from_slice(body);
                shown
            };
            (false, above(wanted, expected), above(written, &actual))
        }
    }
}

/// How a header's name written is held to the name wanted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Names {
    /// As written, as run-tests.php holds php-cgi's.
    AsWritten,
    /// Without regard to case, as the served subset holds makala's, which
    /// writes every name lowercase by design.
    Folded,
}

impl Names {
    /// Whether the name written, `had`, is the name wanted.
    fn same(self, had: &str, wanted: &str) -> bool {
        match self {
            Names::AsWritten => had == wanted,
            Names::Folded => had.eq_ignore_ascii_case(wanted),
        }
    }
}

/// php 8's `==` between two strings: as numbers where both are numeric,
/// byte for byte otherwise.
fn loose_eq(a: &str, b: &str) -> bool {
    let number = |s: &str| {
        let s = s.trim_start_matches([' ', '\t', '\n', '\r', '\x0B', '\x0C']);
        s.parse::<f64>().ok().filter(|_| {
            s.bytes()
                .all(|c| c.is_ascii_digit() || matches!(c, b'.' | b'-' | b'+' | b'e' | b'E'))
        })
    };
    match (number(a), number(b)) {
        (Some(x), Some(y)) => (x - y).abs() == 0.0,
        _ => a == b,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn phpt(text: &str) -> Phpt {
        Phpt::parse(Path::new("x.phpt"), text.as_bytes()).unwrap()
    }

    fn value<'a>(request: &'a Request, name: &str) -> Option<&'a str> {
        request
            .env
            .entries()
            .iter()
            .rev()
            .find(|(had, _)| had == name)
            .and_then(|(_, value)| value.to_str())
    }

    #[test]
    fn a_post_section_is_a_form_on_stdin() {
        let test = phpt("--TEST--\nn\n--POST--\na=Hello+World\n--FILE--\nx\n--EXPECT--\n1\n");
        let request = request(&test, Path::new("/t/x.php"), &Environment::default()).unwrap();
        assert_eq!(value(&request, "REQUEST_METHOD"), Some("POST"));
        assert_eq!(
            value(&request, "CONTENT_TYPE"),
            Some("application/x-www-form-urlencoded")
        );
        assert_eq!(value(&request, "CONTENT_LENGTH"), Some("13"));
        assert_eq!(value(&request, "SCRIPT_FILENAME"), Some("/t/x.php"));
        assert_eq!(value(&request, "REDIRECT_STATUS"), Some("1"));
        assert_eq!(request.stdin.as_deref(), Some(&b"a=Hello+World"[..]));
    }

    #[test]
    fn a_raw_post_takes_its_content_type_line_out() {
        let test = phpt(
            "--TEST--\nn\n--POST_RAW--\nContent-Type: multipart/form-data; boundary=x\n--x\r\nbody\n--x--\n--FILE--\nx\n--EXPECT--\n1\n",
        );
        let request = request(&test, Path::new("/t/x.php"), &Environment::default()).unwrap();
        assert_eq!(
            value(&request, "CONTENT_TYPE"),
            Some("multipart/form-data; boundary=x")
        );
        assert_eq!(request.stdin.as_deref(), Some(&b"--x\r\nbody\n--x--"[..]));
        assert_eq!(value(&request, "CONTENT_LENGTH"), Some("15"));
    }

    #[test]
    fn a_get_and_a_cookie_and_the_environment_s_own_script_name() {
        let test = phpt(
            "--TEST--\nn\n--GET--\na=1\n--COOKIE--\nc=v\n--ARGS--\nx y\n--FILE--\nx\n--EXPECT--\n1\n",
        );
        let env = Environment::from(vec![(
            "SCRIPT_FILENAME".to_owned(),
            OsString::from("/own.php"),
        )]);
        let request = request(&test, Path::new("/t/x.php"), &env).unwrap();
        assert_eq!(value(&request, "QUERY_STRING"), Some("a=1"));
        assert_eq!(value(&request, "HTTP_COOKIE"), Some("c=v"));
        assert_eq!(value(&request, "REQUEST_METHOD"), Some("GET"));
        assert_eq!(value(&request, "SCRIPT_FILENAME"), Some("/own.php"));
        assert_eq!(value(&request, "PATH_TRANSLATED"), Some("/t/x.php"));
        assert_eq!(request.args, ["--", "x", "y"]);
        assert_eq!(request.stdin, None);
    }

    #[test]
    fn the_headers_are_the_lines_before_the_first_blank_one() {
        let rules = Normalise::default();
        let (headers, body) = parted(
            b"X-Powered-By: PHP/X.Y.Z\nStatus: 404 Not Found\n\nbody\n\nmore",
            &rules,
        );
        assert_eq!(
            headers,
            [
                ("X-Powered-By".to_owned(), "PHP/X.Y.Z".to_owned()),
                ("Status".to_owned(), "404 Not Found".to_owned())
            ]
        );
        assert_eq!(body, b"body\n\nmore");
        let (crlf, empty) = parted(b"Status: 302 Found\r\nX: 1\r\n\r\n", &rules);
        assert_eq!(crlf.len(), 2);
        assert_eq!(empty, b"");
        assert_eq!(parted(b"no headers", &rules).1, b"no headers");
        assert_eq!(
            headers_missed(
                "Status: 404 Not Found\nX-Powered-By: PHP/X.Y.Z",
                (&headers, Names::AsWritten)
            ),
            None
        );
        assert_eq!(
            headers_missed("Location: /\nStatus: 302", (&headers, Names::AsWritten)),
            Some((
                "Location: /\nStatus: 302".to_owned(),
                "Status: 404 Not Found".to_owned()
            ))
        );
        let length = [("Content-length".to_owned(), "03".to_owned())];
        assert_eq!(
            headers_missed("Content-length: 3", (&length, Names::AsWritten)),
            None
        );
        // A name of another case is met only where names are folded, as
        // makala's lowercase ones are.
        let lower = [("content-length".to_owned(), "3".to_owned())];
        assert!(headers_missed("Content-length: 3", (&lower, Names::AsWritten)).is_some());
        assert_eq!(
            headers_missed("Content-length: 3", (&lower, Names::Folded)),
            None
        );
    }
}
