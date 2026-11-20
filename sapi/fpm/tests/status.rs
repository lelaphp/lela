//! The full status page's process cells in the html, xml and json forms:
//! every word the request carried, its method, its URI and
//! query, its Basic user and its `SCRIPT_FILENAME`, is written through
//! the escape php's `fpm_status_handle_request` gives the URI in that
//! form,
//! `php_escape_html_entities_ex` under `ENT_DISALLOWED |
//! ENT_HTML_DOC_XML1 | ENT_COMPAT` in the html and xml forms and
//! `php_json_encode_string` under `PHP_JSON_INVALID_UTF8_IGNORE` in the
//! json form (`Z/sapi/fpm/fpm/fpm_status.c:553-596`). The status
//! request is the one process of a pool of one child, so its own words
//! are its row's; `cgi.fix_pathinfo=0` keeps the `SCRIPT_FILENAME` it
//! names.
//!
//! The URI cells are the bytes the pinned php-fpm 8.5.12 (md5
//! f7b17a549250f398e137eef2bd917f1c) wrote on 2026-11-11 for the same
//! requests under the same `-d` defines; the other cells are those words
//! through the same escape. A character XML 1.0 does not allow is written
//! as U+FFFD where `default_charset` is UTF-8 and as `&#xFFFD;` where it
//! names another charset, as php's escape takes its charset. Each row is read
//! cell by cell under the form's own markup, so a word that broke out of
//! its cell reads as a row of other cells, and each cell is compared
//! byte for byte, a byte past ASCII written `\xNN` only to say what
//! differed.

#![allow(
    clippy::disallowed_methods,
    reason = "a test starts its own threads (seams.toml rule fork.threads)"
)]

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use php_proto::fcgi::{self, Header, Kind};

/// The process columns of the page, as the json form names them.
const COLUMNS: [&str; 13] = [
    "pid",
    "state",
    "start time",
    "start since",
    "requests",
    "request duration",
    "request method",
    "request uri",
    "content length",
    "user",
    "script",
    "last request cpu",
    "last request memory",
];

/// A master over one static child with the status page at `/status`,
/// in a directory of the test's own, which it halts and removes when
/// dropped, as a test that panics drops it.
struct Master {
    dir: PathBuf,
    child: Child,
}

impl Master {
    fn start(name: &str, defines: &[&str]) -> Master {
        let dir =
            std::env::temp_dir().join(format!("sapi-fpm-status-{name}-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).expect("the test's directory is made");
        let conf = dir.join("php-fpm.conf");
        std::fs::write(
            &conf,
            format!(
                "[global]\nerror_log = {}\n[www]\nlisten = {}\npm = static\n\
                 pm.max_children = 1\npm.status_path = /status\n",
                dir.join("fpm.log").display(),
                dir.join("fpm.sock").display()
            ),
        )
        .expect("the configuration is written");
        let child = Command::new(env!("CARGO_BIN_EXE_lela-fpm"))
            .args(["-R", "-n", "-d", "cgi.fix_pathinfo=0"])
            .args(defines.iter().flat_map(|define| ["-d", define]))
            .args(["-F", "-y"])
            .arg(conf)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("php-fpm starts");
        Master { dir, child }
    }

    /// What request 1 with `pairs` as its parameters writes on
    /// `FCGI_STDOUT`, the connection tried for ten seconds.
    fn page(&self, pairs: &[(&[u8], &[u8])]) -> Vec<u8> {
        let socket = self.dir.join("fpm.sock");
        let started = Instant::now();
        let mut stream = loop {
            match UnixStream::connect(&socket) {
                Ok(stream) => break stream,
                Err(refused) => {
                    assert!(
                        started.elapsed() < Duration::from_secs(10),
                        "nothing listens at {} after ten seconds: {refused}",
                        socket.display()
                    );
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        };
        let mut params = Vec::new();
        for &(name, value) in pairs {
            fcgi::put_pair(&mut params, name, value);
        }
        let mut out = record(Kind::BeginRequest, &[0, 1, 0, 0, 0, 0, 0, 0]);
        out.extend(record(Kind::Params, &params));
        out.extend(record(Kind::Params, &[]));
        out.extend(record(Kind::Stdin, &[]));
        stream.write_all(&out).expect("the request is written");
        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .expect("the reply's reads are timed");
        let mut body = Vec::new();
        loop {
            let mut bytes = [0u8; 8];
            if stream.read_exact(&mut bytes).is_err() {
                return body;
            }
            let header = Header::decode(bytes);
            let len = usize::from(header.content_length);
            let mut content = vec![0u8; len + usize::from(header.padding_length)];
            stream
                .read_exact(&mut content)
                .expect("the record's content follows its header");
            content.truncate(len);
            match header.kind {
                Kind::Stdout => body.extend_from_slice(&content),
                Kind::EndRequest => return body,
                _ => {}
            }
        }
    }
}

impl Drop for Master {
    fn drop(&mut self) {
        self.child.kill().ok();
        self.child.wait().ok();
        std::fs::remove_dir_all(&self.dir).ok();
    }
}

/// One record of request 1.
fn record(kind: Kind, content: &[u8]) -> Vec<u8> {
    let header = Header::make(kind, 1, content.len());
    let mut out = header.encode().to_vec();
    out.extend_from_slice(content);
    out.resize(out.len() + usize::from(header.padding_length), 0);
    out
}

/// `bytes` as text, each byte past ASCII written `\xNN` and a `\` as
/// `\\`, so no two cells differing show alike.
fn shown(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|&b| {
            if b == b'\\' {
                String::from("\\\\")
            } else if b.is_ascii() {
                char::from(b).to_string()
            } else {
                format!("\\x{b:02x}")
            }
        })
        .collect()
}

/// The text of `body` from its first `at` on, `at` itself left behind.
fn after<'a>(body: &'a [u8], at: &[u8]) -> &'a [u8] {
    let from = body
        .windows(at.len())
        .position(|w| w == at)
        .unwrap_or_else(|| panic!("no {} in {}", shown(at), shown(body)));
    &body[from + at.len()..]
}

/// `rest` up to the first byte of `stop`, and what follows it.
fn until<'a>(rest: &'a [u8], stop: &[u8]) -> (&'a [u8], &'a [u8]) {
    let end = rest
        .iter()
        .position(|b| stop.contains(b))
        .unwrap_or(rest.len());
    rest.split_at(end)
}

/// `rest` with `head` taken off its front, which it must start with.
fn take<'a>(rest: &'a [u8], head: &[u8]) -> &'a [u8] {
    assert!(
        rest.starts_with(head),
        "{} where {} was the next of the row",
        shown(rest),
        shown(head)
    );
    &rest[head.len()..]
}

/// The first process row of a full page in `form`, read cell by cell
/// under the form's markup: an html or xml cell ends at the first `<`,
/// a json string at its first unescaped `"`, a json number at `,` or `}`.
fn cells(form: &str, body: &[u8]) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    match form {
        "html" => {
            let mut rest = after(body, b"</th></tr>\n<tr>");
            for _ in COLUMNS {
                rest = take(rest, b"<td>");
                let (cell, more) = until(rest, b"<");
                out.push(cell.to_vec());
                rest = take(more, b"</td>");
            }
            take(rest, b"</tr>\n");
        }
        "xml" => {
            let mut rest = after(body, b"<process>");
            for name in COLUMNS {
                let tag = name.replace(' ', "-");
                rest = take(rest, format!("<{tag}>").as_bytes());
                let (cell, more) = until(rest, b"<");
                out.push(cell.to_vec());
                rest = take(more, format!("</{tag}>").as_bytes());
            }
            take(rest, b"</process>\n");
        }
        _ => {
            let mut rest = after(body, b"\"processes\":[{");
            for (n, name) in COLUMNS.iter().enumerate() {
                if n > 0 {
                    rest = take(rest, b",");
                }
                rest = take(rest, format!("\"{name}\":").as_bytes());
                if let Some(text) = rest.strip_prefix(b"\"") {
                    let mut end = 0;
                    while text[end] != b'"' {
                        end += if text[end] == b'\\' { 2 } else { 1 };
                    }
                    out.push(text[..end].to_vec());
                    rest = &text[end + 1..];
                } else {
                    let (cell, more) = until(rest, b",}");
                    out.push(cell.to_vec());
                    rest = more;
                }
            }
            take(rest, b"}]}");
        }
    }
    out
}

/// The words of one status request, and the cells each form writes for
/// its method, URI, user and `SCRIPT_FILENAME`.
struct Case<'a> {
    /// The `-d` defines the master starts with past `cgi.fix_pathinfo=0`.
    defines: &'a [&'a str],
    method: &'a [u8],
    /// `HTTP_AUTHORIZATION`'s Basic credentials, base64 of the user, `:`
    /// and the password `x`.
    basic: &'a [u8],
    script: &'a [u8],
    /// What `QUERY_STRING` holds past `full&<form>`.
    query: &'a [u8],
    /// The cells of the html and the xml forms, `{form}` in the URI's
    /// the form's name.
    markup: [&'a [u8]; 4],
    json: [&'a [u8]; 4],
}

/// Each form's page for `case`'s request, held to its cells.
fn holds(name: &str, case: &Case) {
    let master = Master::start(name, case.defines);
    for form in ["html", "xml", "json"] {
        let mut query = format!("full&{form}").into_bytes();
        query.extend_from_slice(case.query);
        let mut basic = b"Basic ".to_vec();
        basic.extend_from_slice(case.basic);
        let body = master.page(&[
            (b"REQUEST_METHOD", case.method),
            (b"SCRIPT_NAME", b"/status"),
            (b"SCRIPT_FILENAME", case.script),
            (b"QUERY_STRING", &query),
            (b"HTTP_AUTHORIZATION", &basic),
        ]);
        let row = cells(form, &body);
        let want: Vec<Vec<u8>> = if form == "json" {
            case.json.iter().map(|c| c.to_vec()).collect()
        } else {
            case.markup
                .iter()
                .map(|c| {
                    String::from_utf8_lossy(c)
                        .replace("{form}", form)
                        .into_bytes()
                })
                .collect()
        };
        let got = [&row[6], &row[7], &row[9], &row[10]];
        for ((column, got), want) in ["request method", "request uri", "user", "script"]
            .iter()
            .zip(got)
            .zip(&want)
        {
            assert_eq!(
                shown(got),
                shown(want),
                "the {form} form's {column} cell, in\n{}",
                shown(&body)
            );
        }
        assert_eq!(row[1], b"Running", "the {form} form's state cell");
    }
}

/// A Basic user that is markup, `<script>alert(1)</script>'"&`, with a
/// method, a `SCRIPT_FILENAME` and a query of the same characters, an
/// `é`, DEL and a control character, which XML 1.0 does not allow.
#[test]
fn a_request_s_markup_is_written_inert_in_every_form() {
    holds(
        "markup",
        &Case {
            defines: &[],
            method: b"G<&>\"'\xc3\xa9\x7f\x01X",
            basic: b"PHNjcmlwdD5hbGVydCgxKTwvc2NyaXB0PiciJjp4",
            script: b"/x/<s>&\"'\xc3\xa9.php",
            query: b"&<q>\"'\xc3\xa9\x7f\x01/\\",
            markup: [
                b"G&lt;&amp;&gt;&quot;'\xc3\xa9\x7f\xef\xbf\xbdX",
                b"/status?full&amp;{form}&amp;&lt;q&gt;&quot;'\xc3\xa9\x7f\xef\xbf\xbd/\\",
                b"&lt;script&gt;alert(1)&lt;/script&gt;'&quot;&amp;",
                b"/x/&lt;s&gt;&amp;&quot;'\xc3\xa9.php",
            ],
            json: [
                b"G<&>\\\"'\\u00e9\x7f\\u0001X",
                b"\\/status?full&json&<q>\\\"'\\u00e9\x7f\\u0001\\/\\\\",
                b"<script>alert(1)<\\/script>'\\\"&",
                b"\\/x\\/<s>&\\\"'\\u00e9.php",
            ],
        },
    );
}

/// Words that are not UTF-8, each a `\xff` among markup: the html and
/// xml forms write such a word as nothing, and the json form drops the
/// byte.
#[test]
fn a_request_s_word_that_is_not_utf_8_is_written_as_php_s_escape_writes_it() {
    holds(
        "not-utf-8",
        &Case {
            defines: &[],
            method: b"G<\xff>",
            basic: b"PHX/Pjp4",
            script: b"/x/<\xff>.php",
            query: b"&<q\xff>",
            markup: [b"", b"/status?", b"", b""],
            json: [b"G<>", b"\\/status?full&json&<q>", b"<u>", b"\\/x\\/<>.php"],
        },
    );
}

/// The same words under `default_charset=ISO-8859-1`: the html and xml
/// forms write the control character as `&#xFFFD;`, the replacement
/// php's escape takes in a charset other than UTF-8, and the json form
/// writes what it writes in UTF-8.
#[test]
fn a_character_xml_does_not_allow_is_written_as_the_charset_s_replacement() {
    holds(
        "iso-8859-1",
        &Case {
            defines: &["default_charset=ISO-8859-1"],
            method: b"G<&>\"'\xc3\xa9\x7f\x01X",
            basic: b"PHNjcmlwdD5hbGVydCgxKTwvc2NyaXB0PiciJjp4",
            script: b"/x/<s>&\"'\xc3\xa9.php",
            query: b"&<q>\"'\xc3\xa9\x7f\x01/\\",
            markup: [
                b"G&lt;&amp;&gt;&quot;'\xc3\xa9\x7f&#xFFFD;X",
                b"/status?full&amp;{form}&amp;&lt;q&gt;&quot;'\xc3\xa9\x7f&#xFFFD;/\\",
                b"&lt;script&gt;alert(1)&lt;/script&gt;'&quot;&amp;",
                b"/x/&lt;s&gt;&amp;&quot;'\xc3\xa9.php",
            ],
            json: [
                b"G<&>\\\"'\\u00e9\x7f\\u0001X",
                b"\\/status?full&json&<q>\\\"'\\u00e9\x7f\\u0001\\/\\\\",
                b"<script>alert(1)<\\/script>'\\\"&",
                b"\\/x\\/<s>&\\\"'\\u00e9.php",
            ],
        },
    );
}
