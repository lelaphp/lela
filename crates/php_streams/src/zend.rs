//! What Zend asks of main's streams (design §4.3 seam 5): the
//! [`StreamHooks`] `php_module_startup` points `zend_stream_open_function`
//! and `zend_resolve_path` at, which `ext/standard` hands the engine
//! with `ExtensionBuilder::stream_hooks`, and the CLI's standard handles
//! `do_cli` registers and reads.

use crate::api::layer::{Face as _, IncludeSource, StreamHooks, scheme_length};

use crate::api::raising::Thrown;
use crate::api::request::{Request, ScannerFile, Stream, Streams, Unresolved, UrlStat, Whence};
use crate::api::values::{PhpString, Resource, Value};
use crate::state::Layer;
use crate::streams::{Located, OpenOptions, StreamLayer};

/// The stream layer's hooks, which `ext/standard` hands the engine with
/// `ExtensionBuilder::stream_hooks` as php's `basic_functions` registers
/// the plain files wrapper in its `MINIT`.
pub const STREAM_HOOKS: StreamHooks = StreamHooks {
    open,
    close_include,
    resolve_path,
    timestamp,
    spend,
    open_primary,
    close_primary,
    standard_handles: crate::streams::standard_handles,
    get_line,
    body_open,
    body_append,
    body_discard,
    body_read,
    body_close,
    state: crate::state::engine_state,
    engine_end: crate::state::engine_end,
};

/// `php_stream_open_for_zend_ex` and `zend_stream_fixup` for an include
/// ([`ScannerFile`]): the file read whole, with the `*opened_path` its
/// opener set, its `zend_file_handle` held for [`close_include`] once the
/// engine has compiled it; a read that raised destroys it here.
fn open(req: &mut Request, path: &[u8]) -> Result<Option<IncludeSource>, Thrown> {
    let Some(mut fh) = ScannerFile::open(req, path)? else {
        req.streams().including.push(None);
        return Ok(None);
    };
    let source = match fh.load(req) {
        Ok(source) => source.map(<[u8]>::to_vec),
        Err(thrown) => {
            if !req.diagnostics().bailing_out() {
                fh.destroy(req);
            }
            return Err(thrown);
        }
    };
    let opened_path = fh.opened_path().map(|p| p.as_bytes().to_vec());
    req.streams().including.push(Some(fh));
    Ok(source.map(|source| IncludeSource {
        source,
        opened_path,
    }))
}

/// [`StreamHooks::close_include`]: the `zend_file_handle` [`open`] held
/// last destroyed, or, where a fatal is ending the request, left unreached as
/// `zend_bailout` leaves `zend_destroy_file_handle`: its stream stands
/// among the request's resources, which close it at shutdown.
fn close_include(req: &mut Request) {
    if let Some(fh) = req.streams().including.pop().flatten()
        && !req.diagnostics().bailing_out()
    {
        fh.destroy(req);
    }
}

/// [`StreamHooks::open_primary`]: the program opened as
/// `php_fopen_primary_script` opens it ([`ScannerFile::open_held`]) and
/// held as the host's `zend_file_handle`, its `*opened_path` handed back;
/// `None` where it did not open. The `Err` is what the open threw.
///
/// ```php
/// var_dump(get_resources(), fopen('php://memory', 'r'));
/// ```
/// ```request fpm
/// --GET--
/// q=1
/// ```
/// ```expect
/// X-Powered-By: PHP/8.5.12
/// Content-type: text/html; charset=UTF-8
///
/// array(1) {
///   [1]=>
///   resource(1) of type (stream)
/// }
/// resource(3) of type (stream)
/// ```
fn open_primary(req: &mut Request, path: &[u8]) -> Result<Option<PhpString>, Thrown> {
    let Some(fh) = ScannerFile::open_held(req, path)? else {
        return Ok(None);
    };
    let opened_path = fh.opened_path();
    req.streams().primary = Some(fh);
    Ok(opened_path)
}

/// [`StreamHooks::close_primary`]: `zend_destroy_file_handle` over the
/// program's `zend_file_handle`, its stream closed and its
/// `*opened_path` given up.
fn close_primary(req: &mut Request) {
    if let Some(fh) = req.streams().primary.take() {
        fh.destroy(req);
    }
}

/// [`StreamHooks::timestamp`]: `zend_get_stream_timestamp`, the
/// wrapper's `url_stat` asked under `PHP_STREAM_URL_STAT_QUIET` with
/// nothing cached, a wrapper with no `url_stat` stamped at one second.
fn timestamp(req: &mut Request, path: &[u8]) -> Option<[i64; 13]> {
    match req.wrapper_url_stat(path).ok()? {
        UrlStat::Found(sb) => Some(sb),
        UrlStat::Unsupported => Some(std::array::from_fn(|at| i64::from(at == 9))),
        UrlStat::Failed | UrlStat::Refused => None,
    }
}

/// [`StreamHooks::body_open`]: the temp stream a request's body is
/// buffered in, its owner the request's resource list's.
fn body_open(req: &mut Request, dir: Option<&[u8]>) -> Result<Resource, Thrown> {
    let stream = crate::streams::body_stream(req, dir)?;
    let body = stream.resource();
    req.keep_in_list(stream.into_resource());
    Ok(body)
}

/// [`StreamHooks::body_append`]: the block written at the stream's end.
fn body_append(req: &mut Request, body: Resource, bytes: &[u8]) -> bool {
    let Some(stream) = Stream::from_value(req, &body.as_value()) else {
        return false;
    };
    stream.seek(req, 0, Whence::End);
    stream.write(req, bytes) == Some(bytes.len())
}

/// [`StreamHooks::body_discard`]: the stream emptied.
fn body_discard(req: &mut Request, body: Resource) {
    if let Some(stream) = Stream::from_value(req, &body.as_value()) {
        stream.truncate(req, 0).ok();
    }
}

/// [`StreamHooks::body_read`]: the stream read whole from its start.
fn body_read(req: &mut Request, body: Resource) -> Vec<u8> {
    let Some(stream) = Stream::from_value(req, &body.as_value()) else {
        return Vec::new();
    };
    stream.seek(req, 0, Whence::Set);
    let mut bytes = Vec::new();
    let mut block = [0; 8192];
    while let Some(n) = stream.read_into(req, &mut block).filter(|n| *n > 0) {
        bytes.extend_from_slice(&block[..n]);
    }
    bytes
}

/// [`StreamHooks::body_close`]: the stream closed.
fn body_close(req: &mut Request, body: Resource) {
    if let Some(stream) = Stream::from_value(req, &body.as_value()) {
        stream.close(req);
    }
}

/// The stream opcache's `zend_stream_open` of a file it holds opens and
/// frees: a resource of the `stream` type made and released, its number
/// spent.
fn spend(req: &mut Request) {
    let kind = req.types().stream;
    if let Ok(opened) = Resource::bare(req, kind) {
        opened.give_up(req);
    }
}

/// `php_stream_get_line` on the stream `value` names, as `do_cli` reads
/// `STDIN` for `-R`, `-F` and `-B`.
fn get_line(req: &mut Request, value: &Value) -> Option<Vec<u8>> {
    let stream = req.stream_of(value)?;
    stream.get_line(req, None).ok().flatten()
}

/// `php_resolve_path` (`Z/main/fopen_wrappers.c:500`) over the ini's
/// `include_path`: a path with a scheme is resolved by its wrapper,
/// the plain files wrapper's to its realpath and any other's to
/// nothing; a path that begins `./` or `../`, or is absolute, or is
/// asked with no `include_path` at all, is `tsrm_realpath` over
/// itself; any other is tried under each entry of `include_path` in
/// turn, an entry that is a stream wrapper's URL (whose `://` is no
/// separator) found by that wrapper's `url_stat` and answered as the
/// URL, and last under the directory of the file being served
/// (`zend_get_executed_filename`), which is how a file beside the
/// primary file is found from another working directory.
/// `tsrm_realpath` is the kernel's canonical path against the
/// request's working directory ([`Environment::realpath`]), and here a
/// regular file alone, which is the `S_ISREG` the plain-files wrapper
/// holds an include to when it opens; php reports a directory with
/// whatever `errno` the failed open left, and this engine as a file
/// that is not there. The `Err` is the last attempt's reason, which
/// the stream warning prints, or the throwable a wrapper's
/// `url_stat` raised, which ends the walk with nothing reported
/// (`if (EG(exception)) return NULL`).
///
/// ```php
/// class W { public $context; function url_stat($path, $flags) { throw new Exception("stat failed"); } }
/// stream_wrapper_register("test", "W");
/// set_include_path("test://foo");
/// try { include "missing.php"; } catch (Exception $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// stat failed
/// ```
fn resolve_path(req: &mut Request, written: &[u8]) -> Result<Vec<u8>, Unresolved> {
    let not_found = || Unresolved::Missing(std::io::Error::from(std::io::ErrorKind::NotFound));
    let for_include = OpenOptions {
        for_include: true,
        report: false,
        ..OpenOptions::default()
    };
    if scheme_length(written).is_some() {
        return match req.locate_for(written, for_include) {
            (Located::Plain, local) => req.realpath_file(&local).map_err(Unresolved::Missing),
            _ => Err(not_found()),
        };
    }
    let include_path = req.ini().get(b"include_path").unwrap_or_default().to_vec();
    let dot_relative = written.starts_with(b"./") || written.starts_with(b"../");
    if dot_relative || written.starts_with(b"/") || include_path.is_empty() {
        return req.realpath_file(written).map_err(Unresolved::Missing);
    }
    let mut last = not_found();
    let mut entries: Vec<Vec<u8>> = include_path_entries(&include_path);
    if let Some(file) = req.frames().current_file()
        && let Some(slash) = file.iter().rposition(|b| *b == b'/')
        && slash > 0
    {
        entries.push(file[..slash].to_vec());
    }
    for entry in entries {
        let mut candidate = entry.clone();
        candidate.push(b'/');
        candidate.extend_from_slice(written);
        if scheme_length(&entry).is_some() {
            match req.locate_for(&candidate, for_include) {
                (Located::Nothing, _) => continue,
                (Located::Plain, local) => match req.realpath_file(&local) {
                    Ok(path) => return Ok(path),
                    Err(reason) => last = Unresolved::Missing(reason),
                },
                _ => match req.url_stat_quiet(&candidate, false).map(UrlStat::found) {
                    Ok(Some(_)) => return Ok(candidate),
                    Err(thrown) if thrown.exception().is_some() => {
                        return Err(Unresolved::Threw(thrown));
                    }
                    Ok(None) | Err(_) => {}
                },
            }
            continue;
        }
        match req.realpath_file(&candidate) {
            Ok(path) => return Ok(path),
            Err(reason) => last = Unresolved::Missing(reason),
        }
    }
    Err(last)
}

/// `include_path`'s entries as `php_resolve_path` walks them: split at
/// `:`, where an entry that begins with a wrapper's scheme keeps its
/// `://`, and `.://` or `..://` is no scheme.
fn include_path_entries(include_path: &[u8]) -> Vec<Vec<u8>> {
    let mut entries = Vec::new();
    let mut rest = include_path;
    while !rest.is_empty() {
        let n = rest
            .iter()
            .take_while(|b| b.is_ascii_alphanumeric() || matches!(b, b'+' | b'-' | b'.'))
            .count();
        let skip = if n > 1 && rest[n..].starts_with(b"://") && &rest[..n] != b".." {
            n + 3
        } else {
            0
        };
        if let Some(at) = rest[skip..].iter().position(|b| *b == b':') {
            entries.push(rest[..skip + at].to_vec());
            rest = &rest[skip + at + 1..];
        } else {
            entries.push(rest.to_vec());
            rest = &[];
        }
    }
    entries
}
