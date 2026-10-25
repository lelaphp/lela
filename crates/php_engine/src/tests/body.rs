//! A body the host streams (#2338, design §4.3 seam 4): the request pulls
//! it as php's `sapi_read_post_block` pulls stdin, while the program
//! runs, so a body that arrives in pieces reads as one handed over whole,
//! and one declared over `post_max_size` is never read. What `php://input`
//! reads is ext/standard's stream over [`Request::input_bytes`], which
//! this engine of no extension reaches through a native of its own.

use std::io::Read;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{Receiver, sync_channel};

use php_macros::php_function;

use crate::api::abi::{Extension, ExtensionBuilder};
use crate::api::raising::Thrown;
use crate::api::request::{BodyLimits, Request};
use crate::api::values::{Owner, PhpArray, PhpString};
use crate::engine::{Body, Engine, Method, RequestDescription, WebRequest};
use crate::test::Capture;

/// `body_input(): string`: what `php://input` reads,
/// `php_stream_input_read` over the request's body.
#[php_function]
fn body_input(req: &mut Request) -> Result<Owner<PhpString>, Thrown> {
    let bytes = req.input_bytes();
    PhpString::new(req, &bytes)
}

/// `parse_body(): array`: `request_parse_body()` with no options, as
/// ext/standard's native makes its pair, `[$_POST, $_FILES]`.
#[php_function]
fn parse_body(req: &mut Request) -> Result<Owner<PhpArray>, Thrown> {
    let (post, files) = req.variables_mut().parse_body(&BodyLimits::default())?;
    let mut pair = PhpArray::new(req)?;
    pair.append(req, post.into_value())?;
    pair.append(req, files.into_value())?;
    Ok(pair)
}

fn input() -> Extension {
    ExtensionBuilder::default()
        .functions(vec![body_input::declared(), parse_body::declared()])
        .build("body")
}

/// A host's reader over a bounded channel the connection feeds, a piece
/// at a time: each read hands out what is left of the piece it holds,
/// and waits for the next where it holds none; the channel closed is
/// the body's end. Every read is counted.
struct Pieces {
    arriving: Receiver<Vec<u8>>,
    held: Vec<u8>,
    at: usize,
    reads: Arc<AtomicUsize>,
}

impl Read for Pieces {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        if self.at == self.held.len() {
            let Ok(piece) = self.arriving.recv() else {
                return Ok(0);
            };
            self.held = piece;
            self.at = 0;
        }
        let n = buf.len().min(self.held.len() - self.at);
        buf[..n].copy_from_slice(&self.held[self.at..self.at + n]);
        self.at += n;
        Ok(n)
    }
}

/// A form of 20,014 bytes, past `SAPI_POST_BLOCK_SIZE`, so the form
/// reader asks for it in more than one block.
fn form() -> Vec<u8> {
    let mut form = b"a=1&b=two&big=".to_vec();
    form.resize(form.len() + 20_000, b'x');
    form
}

/// `program` served as a `POST` of `content_type` whose body `body` is,
/// fed by a thread of its own in pieces of uneven sizes through a channel
/// of one slot, so each piece waits for the request to take the one
/// before it: what the program wrote, and how many reads the request
/// made of the body.
fn posted(engine: &Engine, content_type: &str, body: &[u8], program: &str) -> (String, usize) {
    sent(engine, Method::Post, content_type, body, program)
}

/// [`posted`] under `method`.
fn sent(
    engine: &Engine,
    method: Method,
    content_type: &str,
    body: &[u8],
    program: &str,
) -> (String, usize) {
    let length = i64::try_from(body.len()).unwrap_or(i64::MAX);
    streamed(engine, (method, length), content_type, body, program)
}

/// [`sent`] with the `Content-Length` the host declares, which need not
/// be the body's.
fn streamed(
    engine: &Engine,
    (method, content_length): (Method, i64),
    content_type: &str,
    body: &[u8],
    program: &str,
) -> (String, usize) {
    let (feed, arriving) = sync_channel(1);
    let reads = Arc::new(AtomicUsize::new(0));
    let description = RequestDescription {
        sapi_name: String::from("cgi-fcgi"),
        script_name: Some(b"body.php".to_vec()),
        source: Some(program.as_bytes().to_vec()),
        cwd: std::env::current_dir().expect("the working directory"),
        web: Some(WebRequest {
            method: Some(method),
            content_type: Some(content_type.as_bytes().to_vec()),
            content_length,
            body: Body::Stream(Box::new(Pieces {
                arriving,
                held: Vec::new(),
                at: 0,
                reads: Arc::clone(&reads),
            })),
            response_code: 200,
            ..WebRequest::default()
        }),
        ..RequestDescription::default()
    };
    let mut sink = Capture::default();
    std::thread::scope(|scope| {
        scope.spawn(move || {
            let mut rest = body;
            for size in [1, 999, 4097, 3, 16_384].into_iter().cycle() {
                if rest.is_empty() {
                    break;
                }
                let (piece, after) = rest.split_at(size.min(rest.len()));
                // A request that stopped reading leaves the rest unsent;
                // the sender dropped once all is sent is the body's end.
                if feed.send(piece.to_vec()).is_err() {
                    break;
                }
                rest = after;
            }
        });
        engine.serve_into(description, &mut sink);
    });
    (
        String::from_utf8_lossy(&sink.stdout).into_owned(),
        reads.load(Ordering::Relaxed),
    )
}

/// A form that arrives in pieces while the request runs is `$_POST` and
/// `php://input` as php-cgi makes them of the same body on its stdin,
/// and `php://input` reads it again whole; the oracle's line for the
/// same program, `echo $_POST['a'], ' ', $_POST['b'], ' ',
/// strlen($_POST['big']), ' ', strlen($input), ' ', $input ===
/// file_get_contents('php://input') …`, is `1 two 20000 20014 true true`.
#[test]
fn a_form_that_arrives_in_pieces_is_post_and_input_as_php_reads_it() {
    let engine = Engine::cli_builder().extensions(&[input()]).build();
    let (stdout, reads) = posted(
        &engine,
        "application/x-www-form-urlencoded",
        &form(),
        "<?php $input = body_input();
        echo $_POST['a'], ' ', $_POST['b'], ' ', strlen($_POST['big']), ' ', strlen($input), ' ',
            $input === body_input() ? 'true' : 'false', ' ',
            strncmp($input, 'a=1&b=two&big=xx', 16) === 0 ? 'true' : 'false', \"\\n\";",
    );
    assert_eq!(stdout, "1 two 20000 20014 true true\n");
    assert!(reads > 5, "the body was read in its pieces: {reads} reads");
}

/// A body no content type's reader takes is read by `php://input` alone,
/// as it arrives, and `$_POST` is empty: php-cgi answers `0 20014` for
/// `count($_POST), ' ', strlen(file_get_contents('php://input'))`.
#[test]
fn a_body_no_reader_takes_is_read_by_input_alone_as_it_arrives() {
    let engine = Engine::cli_builder().extensions(&[input()]).build();
    let (stdout, _) = posted(
        &engine,
        "application/octet-stream",
        &form(),
        "<?php echo $_POST === [] ? 0 : 1, ' ', strlen(body_input()), \"\\n\";",
    );
    assert_eq!(stdout, "0 20014\n");
}

/// A body declared over `post_max_size` gives php's warning at request
/// startup and an empty `$_POST`, and the request never reads it
/// (`sapi_read_standard_form_data`, `Z/main/SAPI.c:258`); php-cgi under
/// `-d post_max_size=1K -d html_errors=0` writes the same warning before
/// `array(0) {}`.
#[test]
fn a_body_declared_over_post_max_size_is_warned_of_and_never_read() {
    let engine = Engine::cli_builder()
        .ini("post_max_size", "1K")
        .extensions(&[input()])
        .build();
    let (stdout, reads) = posted(
        &engine,
        "application/x-www-form-urlencoded",
        &form(),
        "<?php echo $_POST === [] ? \"empty\\n\" : \"full\\n\";",
    );
    assert_eq!(
        stdout,
        "\nWarning: PHP Request Startup: POST Content-Length of 20014 bytes exceeds the limit of 1024 bytes in Unknown on line 0\nempty\n"
    );
    assert_eq!(reads, 0);
}

/// A form whose parse takes the request past `memory_limit` ends it in
/// the limit's fatal before the program runs, as php 8.5.12's handler
/// stops registering once `ZEND_HAS_OOM()` and `sapi_handle_post` raises
/// the delayed OOM with nothing executing; the figure is the chunk the
/// body's `smart_str`, grown `BUFSIZ` at a time, first asks past the limit.
/// php-cgi under `-d memory_limit=4M -d html_errors=0` writes the same
/// fatal and its one-row trace for `a=` and three million `x`, and
/// `ran 1 2 3` for a small form.
#[test]
fn a_form_past_the_memory_limit_ends_the_request_before_the_script() {
    let engine = Engine::cli_builder()
        .ini("memory_limit", "4M")
        .extensions(&[input()])
        .build();
    let program = "<?php echo 'ran ', $_POST['a'] ?? '', ' ', $_POST['b'] ?? '', ' ', $_POST['c']['x']['y'] ?? '', \"\\n\";";
    let mut big = b"a=".to_vec();
    big.resize(big.len() + 3_000_000, b'x');
    big.extend_from_slice(b"&b=1");
    let (stdout, _) = posted(&engine, "application/x-www-form-urlencoded", &big, program);
    assert_eq!(
        stdout,
        "\nFatal error: Allowed memory size of 4194304 bytes exhausted (tried to allocate 2097152 bytes) in Unknown on line 0\nStack trace:\n#0 {main}\n"
    );
    let (stdout, _) = posted(
        &engine,
        "application/x-www-form-urlencoded",
        b"a=1&b=2&c[x][y]=3",
        program,
    );
    assert_eq!(stdout, "ran 1 2 3\n");
}

/// A multipart body whose read takes the request past `memory_limit`
/// ends it in the limit's fatal before the program runs, as php 8.5.12's
/// `rfc1867_post_handler` does: a field's value grown on the heap a read
/// at a time, fallibly, and a header line's value grown as its
/// `smart_string` grows, each stopped at the limit. The figures, the
/// trace and the small body's `ran 1 1` are php-cgi 8.5.12's under `-n -d
/// memory_limit=4M -d html_errors=0` over the same three bodies on its
/// standard input; the small body's second `Content-Disposition` and its
/// `X-1` line are passed over, as 8.5.12 keeps only the first of the two
/// lines it reads.
#[test]
fn a_multipart_body_past_the_memory_limit_ends_the_request_before_the_script() {
    let engine = Engine::cli_builder()
        .ini("memory_limit", "4M")
        .extensions(&[input()])
        .build();
    let program =
        "<?php echo 'ran ', $_POST['a'] ?? '', ' ', isset($_FILES['u']) ? 1 : 0, \"\\n\";";
    let multipart = "multipart/form-data; boundary=b";
    let fatal = "\nFatal error: Allowed memory size of 4194304 bytes exhausted (tried to allocate 2097152 bytes) in Unknown on line 0\nStack trace:\n#0 {main}\n";
    let mut value = b"--b\r\nContent-Disposition: form-data; name=\"a\"\r\n\r\n".to_vec();
    value.resize(value.len() + 5_000_000, b'x');
    value.extend_from_slice(b"\r\n--b--\r\n");
    assert_eq!(posted(&engine, multipart, &value, program).0, fatal);
    let mut header = b"--b\r\nContent-Type: pad\r\n".to_vec();
    for _ in 0..1500 {
        header.push(b' ');
        header.resize(header.len() + 4000, b'a');
        header.extend_from_slice(b"\r\n");
    }
    header.extend_from_slice(b"Content-Disposition: form-data; name=\"a\"\r\n\r\nv\r\n--b--\r\n");
    assert_eq!(posted(&engine, multipart, &header, program).0, fatal);
    let small = b"--b\r\nX-1: v\r\nContent-Disposition: form-data; name=\"a\"\r\nContent-Disposition: form-data; name=\"z\"\r\n\r\n1\r\n--b\r\nContent-Disposition: form-data; name=\"u\"; filename=\"f.txt\"\r\nContent-Type: text/plain\r\n\r\nxx\r\n--b--\r\n";
    assert_eq!(posted(&engine, multipart, small, program).0, "ran 1 1\n");
}

/// A multipart body `request_parse_body()` refuses part of the way
/// through gives back every block its parse asked of the heap: the
/// `RequestParseBodyException` thrown at `max_file_uploads` while a
/// file part's name and filename are held, and the one thrown at
/// `max_input_vars` while a field's name and value are held, leave the
/// heap as the request found it, so a debug engine's leak sensor names
/// nothing at the request's end. The messages are php-cgi 8.5.12's for
/// the same two `PUT` bodies under `-d max_file_uploads=1` and `-d
/// max_input_vars=1` (php-src's `multipart_max_files.phpt` and
/// `multipart_max_input_vars.phpt`).
#[test]
fn a_body_parse_refused_by_a_limit_gives_back_what_its_part_held() {
    let program = "<?php try { [$_POST, $_FILES] = parse_body(); } catch (RequestParseBodyException $e) { echo $e->getMessage(), \"\\n\"; } echo $_POST === [] && $_FILES === [] ? 'none' : 'some', \"\\n\";";
    let multipart = "multipart/form-data; boundary=b";
    let engine = Engine::cli_builder()
        .ini("max_file_uploads", "1")
        .extensions(&[input()])
        .build();
    let files = b"--b\r\nContent-Disposition: form-data; name=\"file1\"; filename=\"file1.txt\"\r\nContent-Type: text/plain\r\n\r\nfile data\r\n--b\r\nContent-Disposition: form-data; name=\"file2\"; filename=\"file2.txt\"\r\nContent-Type: text/plain\r\n\r\nfile data\r\n--b--\r\n";
    assert_eq!(
        sent(&engine, Method::Put, multipart, files, program).0,
        "Maximum number of allowable file uploads has been exceeded\nnone\n"
    );
    let engine = Engine::cli_builder()
        .ini("max_input_vars", "1")
        .extensions(&[input()])
        .build();
    let fields = b"--b\r\nContent-Disposition: form-data; name=\"field1\"\r\n\r\npost field data\r\n--b\r\nContent-Disposition: form-data; name=\"field2\"\r\n\r\npost field data\r\n--b--\r\n";
    assert_eq!(
        sent(&engine, Method::Put, multipart, fields, program).0,
        "Input variables exceeded 1. To increase the limit change max_input_vars in php.ini.\nnone\n"
    );
}

/// A multipart body read past `post_max_size` is filled no further, as
/// php 8.5.12's `fill_buffer` stops it, and `fileupload_done` warns of it,
/// whatever its `Content-Length` declared: the field the buffer held is
/// cut where the read stopped and the part after it never read. The
/// lines are php-cgi 8.5.12's under `-n -d post_max_size=10K -d
/// html_errors=0` for the same body on its standard input with
/// `CONTENT_LENGTH=-1`, which its `sapi_cgi_read_post` reads to the end.
#[test]
fn a_multipart_body_read_past_post_max_size_stops_filling_and_warns() {
    let engine = Engine::cli_builder()
        .ini("post_max_size", "10K")
        .extensions(&[input()])
        .build();
    let mut body = b"--b\r\nContent-Disposition: form-data; name=\"a\"\r\n\r\n".to_vec();
    body.resize(body.len() + 20_000, b'x');
    body.extend_from_slice(
        b"\r\n--b\r\nContent-Disposition: form-data; name=\"b\"\r\n\r\nlate\r\n--b--\r\n",
    );
    let program = "<?php echo isset($_POST['a']) ? strlen($_POST['a']) : 'none', ' ', $_POST['b'] ?? 'none', \"\\n\";";
    let (stdout, _) = streamed(
        &engine,
        (Method::Post, -1),
        "multipart/form-data; boundary=b",
        &body,
        program,
    );
    assert_eq!(
        stdout,
        "\nWarning: Actual POST length does not match Content-Length, and exceeds 10240 bytes in Unknown on line 0\n10239 none\n"
    );
}

/// Under `request_parse_body()` each refusal is a `RequestParseBodyException`
/// thrown as the parse goes on, so a second one in the same body leaves
/// with the first as its previous, as `zend_throw_exception_ex` chains
/// over a pending one. The two lines are php-cgi 8.5.12's for the same
/// `PUT` body under `-n -d max_input_vars=1 -d max_file_uploads=1 -d
/// max_multipart_body_parts=10`.
#[test]
fn two_refusals_in_one_request_parse_body_chain_as_php_chains_them() {
    let engine = Engine::cli_builder()
        .ini("max_input_vars", "1")
        .ini("max_file_uploads", "1")
        .ini("max_multipart_body_parts", "10")
        .extensions(&[input()])
        .build();
    let body = b"--b\r\nContent-Disposition: form-data; name=\"field1\"\r\n\r\none\r\n--b\r\nContent-Disposition: form-data; name=\"field2\"\r\n\r\ntwo\r\n--b\r\nContent-Disposition: form-data; name=\"file1\"; filename=\"file1.txt\"\r\nContent-Type: text/plain\r\n\r\nfile data\r\n--b\r\nContent-Disposition: form-data; name=\"file2\"; filename=\"file2.txt\"\r\nContent-Type: text/plain\r\n\r\nfile data\r\n--b--\r\n";
    let program = "<?php try { parse_body(); echo \"parsed\\n\"; } catch (RequestParseBodyException $e) { do { echo get_class($e), ': ', $e->getMessage(), \"\\n\"; } while ($e = $e->getPrevious()); }";
    let (stdout, _) = sent(
        &engine,
        Method::Put,
        "multipart/form-data; boundary=b",
        body,
        program,
    );
    assert_eq!(
        stdout,
        "RequestParseBodyException: Maximum number of allowable file uploads has been exceeded\nRequestParseBodyException: Input variables exceeded 1. To increase the limit change max_input_vars in php.ini.\n"
    );
}

/// `file_uploads` and `enable_post_data_reading` are `OnUpdateBool` in
/// php (`Z/main/main.c`), read by `zend_ini_parse_bool`, so either
/// quoted in a `-d` line as `"yes"` or `"On"`, which the ini syntax keeps
/// as written, turns it on, and `abc` turns it off; a
/// multipart form's field is `$_POST` while `enable_post_data_reading`
/// is on, and its file `$_FILES` while `file_uploads` is on too.
#[test]
fn file_uploads_and_enable_post_data_reading_read_as_booleans() {
    let multipart = b"--b\r\nContent-Disposition: form-data; name=\"f\"\r\n\r\nv\r\n\
--b\r\nContent-Disposition: form-data; name=\"u\"; filename=\"u.txt\"\r\n\
Content-Type: text/plain\r\n\r\nup\r\n--b--\r\n";
    let under = |file_uploads: &str, reading: &str| {
        let engine = Engine::cli_builder()
            .ini("file_uploads", file_uploads)
            .ini("enable_post_data_reading", reading)
            .build();
        posted(
            &engine,
            "multipart/form-data; boundary=b",
            multipart,
            "<?php echo isset($_POST['f']) ? 1 : 0, ' ', isset($_FILES['u']) ? 1 : 0;",
        )
        .0
    };
    assert_eq!(under("\"yes\"", "\"On\""), "1 1");
    assert_eq!(under("abc", "\"true\""), "1 0");
    assert_eq!(under("\"On\"", "abc"), "0 0");
}

/// `max_file_uploads` and `max_multipart_body_parts` have no handler in
/// php (`Z/main/main.c:873-874`): `rfc1867_post_handler` reads each with
/// `INI_INT`, `zend_ini_long`'s `ZEND_STRTOL(value, NULL, 0)`, so `0x1`
/// is one upload, `08` is none (the `0` before an octal's stray `8`),
/// and `0x3` is three parts. The oracle's php-cgi, handed the same body
/// under the same `-d`, counts the same files and fields: `1 2`, `0 2`
/// and `2 1` by `count()`, after the same warnings.
#[test]
fn max_file_uploads_and_body_parts_read_by_base_zero_strtol() {
    let part = |name: &str, file: bool| {
        let filename = if file {
            format!("; filename=\"{name}.txt\"")
        } else {
            String::new()
        };
        format!("--B\r\nContent-Disposition: form-data; name=\"{name}\"{filename}\r\n\r\nx\r\n")
    };
    let body = [
        part("a", true),
        part("b", true),
        part("c", false),
        part("d", false),
    ]
    .concat()
        + "--B--\r\n";
    let under = |name: &str, value: &str| {
        let engine = Engine::cli_builder().ini(name, value).build();
        posted(
            &engine,
            "multipart/form-data; boundary=B",
            body.as_bytes(),
            "<?php foreach (['a', 'b'] as $f) { echo isset($_FILES[$f]) ? 1 : 0; } echo ' '; \
             foreach (['c', 'd'] as $p) { echo isset($_POST[$p]) ? 1 : 0; }",
        )
        .0
    };
    let exceeded = "\nWarning: PHP Request Startup: Maximum number of allowable file uploads has been exceeded in Unknown on line 0\n";
    assert_eq!(under("max_file_uploads", "0x1"), format!("{exceeded}10 11"));
    assert_eq!(under("max_file_uploads", "08"), format!("{exceeded}00 11"));
    assert_eq!(
        under("max_multipart_body_parts", "0x3"),
        "\nWarning: PHP Request Startup: Multipart body parts limit exceeded 3. To increase the limit change max_multipart_body_parts in php.ini. in Unknown on line 0\n11 10"
    );
}
