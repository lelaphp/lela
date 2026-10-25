//! `Z/ext/standard/php_fopen_wrapper.c`: the `php://` wrapper, which
//! `PHP_MINIT(basic)` registers first among `standard`'s wrappers. Its
//! opener names what it opens and builds no kind of its own but
//! `php://output`'s: the memory and temporary streams, `php://input`'s body,
//! the host's standard streams and the copies of its file descriptors
//! are the stream layer's (`main/streams/`), reached through
//! [`Streams`], and `php://filter` opens its resource through the layer
//! and appends the filters it names, as php's opener does.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use php_ext::raising::{Level, Thrown, arg_value_error, docref, throw_error};
use php_ext::request::{
    FilterChain, Ini, Opening, OwnedStream, Request, Standard, Stream, StreamOps, StreamWrapper,
    Streams,
};
use php_ext::values::Owner;

use crate::throwables::Throwables;
use crate::r#type::strtol;
use crate::url::php_url_decode;

/// `php_stream_php_wrapper`: `php`, no URL wrapper, which opens streams
/// and nothing else; every other operation on a `php://` path answers as
/// a wrapper without it does.
pub(crate) fn wrapper() -> StreamWrapper {
    StreamWrapper {
        scheme: "php".into(),
        label: "PHP",
        is_url: false,
        open: Some(url_wrap_php),
        open_dir: None,
        stat: None,
        unlink: None,
        rename: None,
        mkdir: None,
        rmdir: None,
        set_stat: None,
        stream_stat: None,
    }
}

/// `php_stream_output_ops` (`Z/ext/standard/php_fopen_wrapper.c:52`),
/// `php://output`: a write is `PHPWRITE`, through the `php_output` layer, a
/// read is eof and `-1`, and there is no seek.
struct Output;

impl StreamOps for Output {
    fn label(&self) -> &'static str {
        "Output"
    }

    fn write(&mut self, req: &mut Request, buf: &[u8]) -> Option<usize> {
        req.response_mut().write(buf);
        Some(buf.len())
    }

    fn read(&mut self, _req: &mut Request, _buf: &mut [u8], eof: &mut bool) -> Option<usize> {
        *eof = true;
        None
    }

    fn close(&mut self, _req: &mut Request) {}
}

/// `php_stream_url_wrap_php` (`Z/ext/standard/php_fopen_wrapper.c:176`):
/// the path after `php://`, case-insensitively, as `temp`, with
/// `/maxmemory:N` after it, `memory`, `output`, `input`, `stdin`,
/// `stdout`, `stderr`, `fd/N` or `filter/...`; anything else is php's
/// `Invalid php:// URL specified` and nothing opened. `input`, `stdin`
/// and `fd/` are refused an include under `allow_url_include=0`.
fn url_wrap_php(
    req: &mut Request,
    path: &[u8],
    mode: &[u8],
    opening: Opening,
) -> Result<Option<Owner<Stream>>, Thrown> {
    let path = if path.len() >= 6 && path[..6].eq_ignore_ascii_case(b"php://") {
        &path[6..]
    } else {
        path
    };
    let lower = path.to_ascii_lowercase();
    if let Some(rest) = lower.strip_prefix(b"temp") {
        let mut max_memory = None;
        if let Some(digits) = rest.strip_prefix(b"/maxmemory:") {
            let bound = strtol(digits, 10);
            if bound < 0 {
                return Err(arg_value_error(req, 2, "be greater than or equal to 0"));
            }
            max_memory = Some(usize::try_from(bound).unwrap_or(usize::MAX));
        }
        return req.temp_stream(mode, max_memory).map(Some);
    }
    if lower.starts_with(b"filter/") {
        return filter(req, &path[6..], mode, opening);
    }
    match lower.as_slice() {
        b"memory" => req.memory_stream(mode).map(Some),
        b"output" => req.new_stream(Box::new(Output), b"wb").map(Some),
        b"input" | b"stdin" if include_refused(req, opening) => Ok(None),
        b"input" => req.input_stream().map(Some),
        b"stdin" => req.standard_stream(Standard::Stdin, mode),
        b"stdout" => req.standard_stream(Standard::Stdout, mode),
        b"stderr" => req.standard_stream(Standard::Stderr, mode),
        _ if lower.starts_with(b"fd/") => fd(req, &path[3..], mode, opening),
        _ => {
            docref(req, Level::Warning, "Invalid php:// URL specified");
            Ok(None)
        }
    }
}

/// `(options & STREAM_OPEN_FOR_INCLUDE) && !PG(allow_url_include)`, with
/// php's warning under `REPORT_ERRORS`, which an opener is never handed.
fn include_refused(req: &mut Request, opening: Opening) -> bool {
    let refused = opening.for_include
        && !req
            .ini()
            .get(b"allow_url_include")
            .is_none_or(Ini::parse_bool);
    if refused && opening.warn {
        docref(
            req,
            Level::Warning,
            "URL file-access is disabled in the server configuration",
        );
    }
    refused
}

/// `php://fd/N` (`Z/ext/standard/php_fopen_wrapper.c:292`): under the
/// CLI alone, `N` all decimal digits, then the layer's copy of that fd
/// ([`Streams::fd_stream`]), which holds it below `getdtablesize()`.
///
/// ```php
/// foreach (['php://fd', 'php://fd/', 'php://fd/-2', 'php://fd/1/', 'php://fd/1023'] as $p) {
///     var_dump(@fopen($p, 'w'));
///     echo preg_replace('/smaller than \d+/', 'smaller than N', error_get_last()['message']), "\n";
/// }
/// var_dump(get_resource_type(fopen('php://fd/2', 'w')));
/// ```
/// ```expect
/// bool(false)
/// fopen(php://fd): Failed to open stream: operation failed
/// bool(false)
/// fopen(php://fd/): Failed to open stream: php://fd/ stream must be specified in the form php://fd/<orig fd>
/// bool(false)
/// fopen(php://fd/-2): Failed to open stream: The file descriptors must be non-negative numbers smaller than N
/// bool(false)
/// fopen(php://fd/1/): Failed to open stream: php://fd/ stream must be specified in the form php://fd/<orig fd>
/// bool(false)
/// fopen(php://fd/1023): Failed to open stream: Error duping file descriptor 1023; possibly it doesn't exist: [9]: Bad file descriptor
/// string(6) "stream"
/// ```
fn fd(
    req: &mut Request,
    digits: &[u8],
    mode: &[u8],
    opening: Opening,
) -> Result<Option<Owner<Stream>>, Thrown> {
    if req.environment().sapi_name() != "cli" {
        if opening.warn {
            docref(
                req,
                Level::Warning,
                "Direct access to file descriptors is only available from command-line PHP",
            );
        }
        return Ok(None);
    }
    if include_refused(req, opening) {
        return Ok(None);
    }
    let body = digits.trim_ascii_start();
    let unsigned = body
        .strip_prefix(b"-")
        .or_else(|| body.strip_prefix(b"+"))
        .unwrap_or(body);
    if unsigned.is_empty() || !unsigned.iter().all(u8::is_ascii_digit) {
        req.wrapper_log_error("php://fd/ stream must be specified in the form php://fd/<orig fd>");
        return Ok(None);
    }
    req.fd_stream(strtol(digits, 10), mode)
}

/// `php://filter/` (`Z/ext/standard/php_fopen_wrapper.c:339`): the
/// resource after the first `/resource=` opened as its own wrapper opens
/// it, reporting nothing of its own, then each `/`-separated part before
/// it applied to it: `read=` a `|`-separated list for the read chain,
/// `write=` one for the write chain, and a bare list for the chains
/// `fopen`'s `$mode` reads and writes, each name URL-decoded, and php's `Unable to
/// create filter (%s)` after the filter layer's own warning for one
/// that cannot be made. A path with no `/resource=` is php's `Error`.
///
/// ```php
/// var_dump(file_get_contents('php://filter/read=string.toupper|string.rot13/resource=data:,abc'));
/// var_dump(file_get_contents('php://filter/string.toupper/nope%2Ex/resource=data:,abc'));
/// $f = fopen('php://filter/read=string.rot13/resource=data:,abc', 'r');
/// echo json_encode(array_slice(stream_get_meta_data($f), 1, 2)), ' ', stream_get_meta_data($f)['uri'], "\n";
/// var_dump(@file_get_contents('php://filter/read=string.rot13/resource=nofile'), error_get_last()['message']);
/// $w = fopen('php://filter/write=string.toupper/resource=php://memory', 'w+');
/// fwrite($w, "shout"); rewind($w); var_dump(fread($w, 10));
/// try { fopen('php://filter/read=string.rot13', 'r'); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(3) "NOP"
///
/// Warning: file_get_contents(): Unable to locate filter "nope.x" in %s on line 3
///
/// Warning: file_get_contents(): Unable to create filter (nope.x) in %s on line 3
/// string(3) "ABC"
/// {"wrapper_type":"PHP","stream_type":"RFC2397"} php://filter/read=string.rot13/resource=data:,abc
/// bool(false)
/// string(106) "file_get_contents(php://filter/read=string.rot13/resource=nofile): Failed to open stream: operation failed"
/// string(5) "SHOUT"
/// No URL resource specified
/// ```
///
/// The resource's opened path is the filter's own, so a file included
/// through the filter is named as the file it read, in `__FILE__` and in
/// `get_included_files()`:
///
/// ```php
/// $f = __DIR__ . '/filter-include-witness.php';
/// file_put_contents($f, '<?php echo __FILE__ === realpath($GLOBALS["f"]) ? "real" : __FILE__, "\n";');
/// include 'php://filter/read=string.rot13|string.rot13/resource=' . $f;
/// var_dump(in_array(realpath($f), get_included_files(), true)); unlink($f);
/// ```
/// ```expect
/// real
/// bool(true)
/// ```
///
/// php walks the names with `php_strtok_r(pathdup + 1, "/", ...)` over
/// its copy of the path cut at `/resource=`. Where `/resource=` starts
/// the path, the cut is the copy's first byte, so the walk starts past
/// it and reads the resource itself: each `/`-separated segment of
/// `resource=<path>` is taken for a filter name and raises both
/// warnings on each chain `$mode` names, and the resource is still
/// opened:
///
/// ```php
/// var_dump(file_get_contents('php://filter/resource=data:text/plain,abc'));
/// var_dump(file_get_contents('php://filter//resource=data:,abc'));
/// ```
/// ```expect
/// Warning: file_get_contents(): Unable to locate filter "resource=data:text" in %s on line 2
///
/// Warning: file_get_contents(): Unable to create filter (resource=data:text) in %s on line 2
///
/// Warning: file_get_contents(): Unable to locate filter "plain,abc" in %s on line 2
///
/// Warning: file_get_contents(): Unable to create filter (plain,abc) in %s on line 2
/// string(3) "abc"
/// string(3) "abc"
/// ```
fn filter(
    req: &mut Request,
    path: &[u8],
    mode: &[u8],
    opening: Opening,
) -> Result<Option<Owner<Stream>>, Thrown> {
    let Some(at) = path.windows(10).position(|w| w == b"/resource=") else {
        let ce = Throwables::of(req)?;
        return Err(throw_error(req, ce.error, "No URL resource specified"));
    };
    // `php_stream_open_wrapper_ex(p + 10, mode, options, opened_path,
    // context)`: the options and the `*opened_path` handed on, so an
    // include through the filter is named as the file it read.
    let Some(stream) = req.open_stream_with(&path[at + 10..], mode, opening)? else {
        return Ok(None);
    };
    let read = mode.contains(&b'r') || mode.contains(&b'+');
    let write = mode.iter().any(|b| matches!(b, b'w' | b'+' | b'a'));
    // `php_strtok_r(pathdup + 1, "/", &token)` past the cut at
    // `/resource=`: where the cut is `pathdup[0]`, past it is the resource.
    let names = if at == 0 { &path[1..] } else { &path[1..at] };
    for part in names.split(|b| *b == b'/').filter(|p| !p.is_empty()) {
        let (list, chains) = if part.len() >= 5 && part[..5].eq_ignore_ascii_case(b"read=") {
            (&part[5..], (true, false))
        } else if part.len() >= 6 && part[..6].eq_ignore_ascii_case(b"write=") {
            (&part[6..], (false, true))
        } else {
            (part, (read, write))
        };
        // `if (EG(exception)) { php_stream_close(stream); return NULL; }`
        if let Err(thrown) = apply_filter_list(req, *stream, list, chains) {
            stream.free(req);
            return Err(thrown);
        }
    }
    Ok(Some(stream))
}

/// `php_stream_apply_filter_list` (`Z/ext/standard/php_fopen_wrapper.c:149`):
/// each `|`-separated name URL-decoded and a filter of it appended to the
/// read chain and to the write chain as `(read, write)` asks, php's
/// `Unable to create filter (%s)` after the filter layer's own warning
/// for each that cannot be made.
fn apply_filter_list(
    req: &mut Request,
    stream: Stream,
    list: &[u8],
    (read, write): (bool, bool),
) -> Result<(), Thrown> {
    for name in list.split(|b| *b == b'|').filter(|n| !n.is_empty()) {
        let name = php_url_decode(name);
        for (wanted, chain) in [(read, FilterChain::Read), (write, FilterChain::Write)] {
            if wanted && !stream.append_filter(req, &name, chain)? {
                let text = format!(
                    "Unable to create filter ({})",
                    String::from_utf8_lossy(&name)
                );
                docref(req, Level::Warning, &text);
            }
        }
    }
    Ok(())
}
