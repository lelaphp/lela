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
use crate::api::request::Request;
use crate::api::values::{Owner, PhpString};
use crate::engine::{Body, Engine, Method, RequestDescription, WebRequest};
use crate::test::Capture;

/// `body_input(): string`: what `php://input` reads,
/// `php_stream_input_read` over the request's body.
#[php_function]
fn body_input(req: &mut Request) -> Result<Owner<PhpString>, Thrown> {
    let bytes = req.input_bytes();
    PhpString::new(req, &bytes)
}

fn input() -> Extension {
    ExtensionBuilder::default()
        .functions(vec![body_input::declared()])
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
    let (feed, arriving) = sync_channel(1);
    let reads = Arc::new(AtomicUsize::new(0));
    let description = RequestDescription {
        sapi_name: String::from("cgi-fcgi"),
        script_name: Some(b"body.php".to_vec()),
        source: Some(program.as_bytes().to_vec()),
        cwd: std::env::current_dir().unwrap_or_default(),
        web: Some(WebRequest {
            method: Some(Method::Post),
            content_type: Some(content_type.as_bytes().to_vec()),
            content_length: i64::try_from(body.len()).unwrap_or(i64::MAX),
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
