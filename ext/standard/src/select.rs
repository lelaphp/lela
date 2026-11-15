//! The two natives of `streamsfuncs.c` over a stream's host fd
//! (`Z/ext/standard/streamsfuncs.c:601-843`, `:1601`): `stream_select`,
//! which casts each stream of its three arrays to the fd its kind
//! reads (`PHP_STREAM_AS_FD_FOR_SELECT`) and hands the sets to
//! `select(2)`, and `stream_isatty`. The casting is the engine's streams
//! facet; the two C calls are `src/ffi/`'s.
//!
//! The two natives, as `basic_functions.stub.php` declares them.
//!
//! ```php
//! echo new ReflectionFunction('stream_select'), new ReflectionFunction('stream_isatty');
//! ```
//! ```expect
//! Function [ <internal:standard> function stream_select ] {
//!
//!   - Parameters [5] {
//!     Parameter #0 [ <required> ?array &$read ]
//!     Parameter #1 [ <required> ?array &$write ]
//!     Parameter #2 [ <required> ?array &$except ]
//!     Parameter #3 [ <required> ?int $seconds ]
//!     Parameter #4 [ <optional> ?int $microseconds = null ]
//!   }
//!   - Return [ int|false ]
//! }
//! Function [ <internal:standard> function stream_isatty ] {
//!
//!   - Parameters [1] {
//!     Parameter #0 [ <required> $stream ]
//!   }
//!   - Return [ bool ]
//! }
//! ```
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use std::ops::ControlFlow;
use std::os::fd::RawFd;

use php_ext::abi::{OrFalse, Ref};
use php_ext::attr::php_function;
use php_ext::raising::{Level, Thrown, arg_value_error, docref, type_error, value_error};
use php_ext::request::{Cast, Request, Stream, Streams as _};
use php_ext::values::walk::{self, Hold};
use php_ext::values::{Key, Owner, PhpArray, Value, ValueRef};

/// `?array &$read` and its two siblings: the caller's array, or `null`,
/// by reference.
type StreamArray = Ref<Option<PhpArray>>;

/// `php_stream_from_zval_no_verify` (`Z/main/php_streams.h:319`) over
/// `zend_fetch_resource2_ex`: the stream a value names, and php's
/// `TypeError` for a value that is no resource or a resource of another
/// type, a closed one among them, named by `native`, the function
/// `get_active_function_name` answers.
pub(super) fn fetch_stream(
    req: &mut Request,
    native: &str,
    value: &Value,
) -> Result<Stream, Thrown> {
    let value = value.deref();
    let what = match value.as_ref() {
        ValueRef::Resource(_) => match Stream::from_value(req, &value) {
            Some(stream) => return Ok(stream),
            None => "resource",
        },
        _ => "argument",
    };
    let text = format!("{native}(): supplied {what} is not a valid stream resource");
    Err(type_error(req, &text))
}

/// `Z_PARAM_ARRAY_EX2(arr, 1, 1, 0)`: an array, `null` or no argument
/// for `None`, and php's `TypeError` for anything else.
/// The fds of one array's streams, `stream_array_to_fd_set`
/// (`Z/ext/standard/streamsfuncs.c:601`): each element fetched as a
/// stream and cast for `select(2)`, php's warning for a kind with no
/// fd.
fn to_fd_set(
    req: &mut Request,
    array: PhpArray,
    streams: &mut Vec<Stream>,
) -> Result<Vec<RawFd>, Thrown> {
    let mut fds = Vec::new();
    // The array is walked with no owner of it taken: the by-reference
    // argument holds it while code of the program runs under the walk,
    // and where that code frees it, the walk ends there.
    walk::foreach(req, array, Hold::None, |req, row| -> walk::Step {
        let element = row.value().deref();
        let stream = fetch_stream(req, "stream_select", &element)?;
        if let Some(fd) = stream.cast(req, Cast::Select, true)
            && fd != -1
        {
            fds.push(fd);
            streams.push(stream);
        }
        Ok(ControlFlow::Continue(()))
    })?;
    Ok(fds)
}

/// `stream_array_from_fd_set` (`Z/ext/standard/streamsfuncs.c:636`): a
/// new array of the elements whose fd `select(2)` left set, each
/// under its key, written over the argument.
fn from_fd_set(
    req: &mut Request,
    target: &StreamArray,
    array: PhpArray,
    set: &[RawFd],
) -> Result<(), Thrown> {
    let mut out = PhpArray::new(req)?;
    // What a fetch threw, which php leaves in `EG(exception)` as its walk
    // goes on past the element (`php_stream_from_zval_no_verify`, then
    // `continue`), each later one chained on.
    let mut pending: Option<Thrown> = None;
    // `ZEND_HASH_FOREACH_KEY_VAL(Z_ARRVAL_P(stream_array), …)`
    // (`Z/ext/standard/streamsfuncs.c:648`), holding nothing, as
    // [`to_fd_set`] walks.
    let walked = walk::foreach(req, array, Hold::None, |req, row| -> walk::Step {
        let key = row.key().clone();
        let element = row.value().deref();
        let stream = match fetch_stream(req, "stream_select", &element) {
            Ok(stream) => stream,
            Err(thrown) => {
                pending = Some(match pending.take() {
                    Some(earlier) => earlier.chain(req, thrown),
                    None => thrown,
                });
                return Ok(ControlFlow::Continue(()));
            }
        };
        if let Some(fd) = stream.cast(req, Cast::Select, true)
            && set.contains(&fd)
        {
            keep(req, &mut out, &key, element)?;
        }
        Ok(ControlFlow::Continue(()))
    });
    if let Err(thrown) = walked {
        out.give_up(req);
        return Err(match pending {
            Some(earlier) => earlier.chain(req, thrown),
            None => thrown,
        });
    }
    // `zval_ptr_dtor(stream_array); ZVAL_ARR(stream_array, ht)`, whatever
    // the walk left pending.
    target.set(req, Some(out))?;
    pending.map_or(Ok(()), Err)
}

/// One element copied into the array written back, key and value one
/// more owner each, as `zend_hash_update` and `zval_add_ref` take them.
fn keep(
    req: &mut Request,
    out: &mut Owner<PhpArray>,
    key: &Key,
    element: Value,
) -> Result<(), Thrown> {
    let element = Owner::take(req, element);
    let key = Owner::take(req, key.clone());
    out.set(req, key, element)
}

/// `stream_array_emulate_read_fd_set` (`Z/ext/standard/streamsfuncs.c:682`):
/// the streams of the read array that hold bytes in their read buffer,
/// written over the argument where there is one; how many there were.
fn emulate_read_fd_set(
    req: &mut Request,
    target: &StreamArray,
    array: PhpArray,
) -> Result<usize, Thrown> {
    let mut out = PhpArray::new(req)?;
    let mut ret = 0;
    // `ZEND_HASH_FOREACH_KEY_VAL(Z_ARRVAL_P(stream_array), …)`
    // (`Z/ext/standard/streamsfuncs.c:694`), holding nothing, as
    // [`to_fd_set`] walks.
    walk::foreach(req, array, Hold::None, |req, row| -> walk::Step {
        let key = row.key().clone();
        let element = row.value().deref();
        let stream = fetch_stream(req, "stream_select", &element)?;
        if stream.unread(req) > 0 {
            keep(req, &mut out, &key, element)?;
            ret += 1;
        }
        Ok(ControlFlow::Continue(()))
    })?;
    if ret > 0 {
        target.set(req, Some(out))?;
    } else {
        out.give_up(req);
    }
    Ok(ret)
}

/// `stream_select(?array &$read, ?array &$write, ?array &$except, ?int
/// $seconds, ?int $microseconds = null): int|false`,
/// `PHP_FUNCTION(stream_select)` (`Z/ext/standard/streamsfuncs.c:730`):
/// the streams of the three arrays cast to their fds, php's
/// warning for each that has none and its `ValueError` where no array
/// gave one; the timeout checked; a read array holding a stream with
/// bytes in its read buffer answered at once with those streams and the
/// other two arrays emptied; otherwise `select(2)`, each array cut to the
/// streams it left ready, and its count, one per fd, so a stream
/// named twice in one array is counted once and kept twice.
///
/// ```php
/// $f = fopen(__FILE__, 'r'); $w = fopen(__DIR__ . '/select.txt', 'w');
/// $r = ['a' => $f, 5 => $f]; $wr = [$w]; $e = null;
/// var_dump(stream_select($r, $wr, $e, 0), $r, $wr, $e);
/// $m = fopen('php://memory', 'r'); $r = [$m]; $wr = null;
/// try { stream_select($r, $wr, $e, 0); } catch (ValueError $x) { echo $x->getMessage(), "\n"; }
/// fgets($f); $r = [$f, $m]; $wr = [$w];
/// var_dump(stream_select($r, $wr, $e, 1), $r, $wr);
/// ```
/// ```expect
/// int(2)
/// array(2) {
///   ["a"]=>
///   resource(%d) of type (stream)
///   [5]=>
///   resource(%d) of type (stream)
/// }
/// array(1) {
///   [0]=>
///   resource(%d) of type (stream)
/// }
/// NULL
///
/// Warning: stream_select(): Cannot represent a stream of type MEMORY as a select()able descriptor in %s on line 6
/// No stream arrays were passed
///
/// Warning: stream_select(): Cannot represent a stream of type MEMORY as a select()able descriptor in %s on line 8
/// int(1)
/// array(1) {
///   [0]=>
///   resource(%d) of type (stream)
/// }
/// array(0) {
/// }
/// ```
///
/// The timeout is held before anything waits:
///
/// ```php
/// $r = [fopen(__FILE__, 'r')]; $w = $e = null;
/// try { stream_select($r, $w, $e, -1); } catch (ValueError $x) { echo $x->getMessage(), "\n"; }
/// try { stream_select($r, $w, $e, 0, -1); } catch (ValueError $x) { echo $x->getMessage(), "\n"; }
/// try { stream_select($r, $w, $e, null, 5); } catch (ValueError $x) { echo $x->getMessage(), "\n"; }
/// try { stream_select($r, $w, $e, 0, 0, 1); } catch (ArgumentCountError $x) { echo $x->getMessage(), "\n"; }
/// ```
/// ```expect
/// stream_select(): Argument #4 ($seconds) must be greater than or equal to 0
/// stream_select(): Argument #5 ($microseconds) must be greater than or equal to 0
/// stream_select(): Argument #5 ($microseconds) must be null when argument #4 ($seconds) is null
/// stream_select() expects at most 5 arguments, 6 given
/// ```
#[php_function]
fn stream_select(
    req: &mut Request,
    read: StreamArray,
    write: StreamArray,
    except: StreamArray,
    seconds: Option<i64>,
    #[php(default = null)] microseconds: Option<i64>,
) -> Result<OrFalse<i64>, Thrown> {
    let arrays = [&read, &write, &except].map(|each| each.get().copied().flatten());
    // Each array held for the call, so a callback a lent wait serves that
    // assigns over `$read` leaves the walk after the wait reading the
    // array it was handed.
    let held: Vec<Owner<PhpArray>> = arrays
        .iter()
        .flatten()
        .map(|array| Owner::take(req, *array))
        .collect();
    let selected = select_over(req, [&read, &write, &except], arrays, seconds, microseconds);
    for array in held {
        array.give_up(req);
    }
    selected
}

/// `stream_select` past its arguments, over the arrays it holds.
fn select_over(
    req: &mut Request,
    [read, write, except]: [&StreamArray; 3],
    [r_array, w_array, e_array]: [Option<PhpArray>; 3],
    seconds: Option<i64>,
    microseconds: Option<i64>,
) -> Result<OrFalse<i64>, Thrown> {
    let (sec, usec) = (seconds, microseconds);
    let mut sets: [Vec<RawFd>; 3] = [Vec::new(), Vec::new(), Vec::new()];
    let mut streams = Vec::new();
    for (set, array) in sets.iter_mut().zip([r_array, w_array, e_array]) {
        if let Some(array) = array {
            *set = to_fd_set(req, array, &mut streams)?;
        }
    }
    if sets.iter().all(Vec::is_empty) {
        return Err(value_error(req, "No stream arrays were passed"));
    }
    let max_fd = sets.iter().flatten().copied().max().unwrap_or(0);
    if max_fd >= crate::ffi::FD_SETSIZE {
        let text = format!(
            "You MUST recompile PHP with a larger value of FD_SETSIZE.\nIt is set to {}, but you have descriptors numbered at least as high as {max_fd}.\n --enable-fd-setsize={} is recommended, but you may want to set it\nto equal the maximum number of open files supported by your system,\nin order to avoid seeing this error again at a later date.",
            crate::ffi::FD_SETSIZE,
            (max_fd + 1024) & !1023
        );
        docref(req, Level::Warning, &text);
        return Ok(OrFalse::False);
    }
    if sec.is_none() && usec.is_some_and(|usec| usec != 0) {
        return Err(arg_value_error(
            req,
            5,
            "be null when argument #4 ($seconds) is null",
        ));
    }
    let timeout = match sec {
        Some(sec) if sec < 0 => {
            return Err(arg_value_error(req, 4, "be greater than or equal to 0"));
        }
        Some(_) if usec.is_some_and(|usec| usec < 0) => {
            return Err(arg_value_error(req, 5, "be greater than or equal to 0"));
        }
        Some(sec) => {
            let usec = usec.unwrap_or(0);
            Some((sec + usec / 1_000_000, usec % 1_000_000))
        }
        None => None,
    };
    if let Some(array) = r_array {
        let ready = emulate_read_fd_set(req, read, array)?;
        if ready > 0 {
            if w_array.is_some() {
                write.set(req, Some(Owner::take(req, PhpArray::empty())))?;
            }
            if e_array.is_some() {
                except.set(req, Some(Owner::take(req, PhpArray::empty())))?;
            }
            return Ok(OrFalse::Value(i64::try_from(ready).unwrap_or(i64::MAX)));
        }
    }
    let [read_set, write_set, except_set] = &sets;
    // The streams held out of their resources for the wait, so a
    // callback's `fclose` of one closes no fd the wait stands on until it
    // returns.
    let waited = req.holding_streams(&streams, |req| {
        req.limits_mut()
            .blocking(|| crate::ffi::select([read_set, write_set, except_set], timeout))
    });
    let (retval, ready) = match waited {
        Ok(answered) => answered,
        Err(error) => {
            let text = format!(
                "Unable to select [{}]: {} (max_fd={max_fd})",
                error.raw_os_error().unwrap_or(0),
                req.environment()
                    .os_error_text(error.raw_os_error().unwrap_or(0))
            );
            docref(req, Level::Warning, &text);
            return Ok(OrFalse::False);
        }
    };
    for ((target, array), set) in [read, write, except]
        .into_iter()
        .zip([r_array, w_array, e_array])
        .zip(&ready)
    {
        if let Some(array) = array {
            from_fd_set(req, target, array, set)?;
        }
    }
    Ok(OrFalse::Value(i64::from(retval)))
}

/// `stream_isatty($stream): bool`, `PHP_FUNCTION(stream_isatty)`
/// (`Z/ext/standard/streamsfuncs.c:1601`): the stream's fd, cast for
/// `select(2)` and then plainly, each only where `php_stream_can_cast`
/// says the kind would answer it, `false` for a kind with none, and then
/// `isatty(3)` of it. What a witness's streams are here is a file,
/// memory, and the standard streams as a pipe hands them over, none of
/// them a terminal.
///
/// ```php
/// var_dump(stream_isatty(fopen(__FILE__, 'r')), stream_isatty(fopen('php://memory', 'r')), stream_isatty(fopen('php://stderr', 'w')));
/// try { stream_isatty(1); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(false)
/// bool(false)
/// bool(false)
/// stream_isatty(): Argument #1 ($stream) must be of type resource, int given
/// ```
///
/// The question is the kind's own answer: a user-space stream's
/// `stream_cast` is served for each, and a stream it refuses is not
/// sought.
///
/// ```php
/// class W { public $context;
///     function stream_open($p, $m, $o, &$op) { return true; }
///     function stream_seek($o, $w) { echo "sought\n"; return true; }
///     function stream_cast($as) { echo "cast $as\n"; return false; } }
/// stream_wrapper_register('ww', 'W');
/// var_dump(stream_isatty(fopen('ww://x', 'r')));
/// ```
/// ```expect
/// cast 3
/// cast 0
/// bool(false)
/// ```
#[php_function]
fn stream_isatty(req: &mut Request, stream: Stream) -> bool {
    // Each cast asked first and taken only where the kind would answer
    // it, and a cast that fails after that is php's `-1`, no terminal.
    let fd = if stream.can_cast(req, Cast::Select).is_ok() {
        stream.cast(req, Cast::Select, false)
    } else if stream.can_cast(req, Cast::Internal).is_ok() {
        stream.cast(req, Cast::Internal, false)
    } else {
        return false;
    };
    fd.is_some_and(crate::ffi::isatty)
}
