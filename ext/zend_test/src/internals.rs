//! The test functions of php's `test.c` that reach past what a program
//! sees, each over the face item that answers it here (#2388): the
//! machine stack's bounds and `EG(stack_limit)`, whether an address is
//! the request heap's, `CG(map_ptr_last)`, the log at a severity,
//! opcache's preload, libxml's process defaults through ext/libxml, and
//! the URI parsers through ext/standard and ext/uri (design §4.10). Each
//! is laid into [`crate::functions`]' list at its place in the stub.
//!
//! Three of `test.stub.php`'s functions #2388 set out break what the
//! engine holds. Two are ported as php's (ruled on #2937 and #2976):
//!
//! - `zend_test_crash(?string $message = null)` prints its message and
//!   ends the process with the `SIGSEGV` php's read of `(char *) 1`
//!   takes, raised in `crate::ffi` (`unsafe.homes`) rather than read.
//! - `zend_test_gh18756()` starts a heap apart from the request's,
//!   collects it twice and shuts it down, the regression GH-18756 where
//!   `zend_mm_gc` freed a heap's main chunk, over `php_heap`'s
//!   [`StandaloneHeap`], a safe item.
//!
//! The third, `zend_delref(mixed $variable)`, is registered as php
//! declares it and takes no count off, where php's drops one by hand: the
//! engine exposes no unsound refcount decrement, a `by-design` row
//! (`trace:zend-delref-no-decrement`, ruled on #3016).
//!
//! `zend_test_set_fmode` is Windows' alone, and is left out as php
//! leaves it out on every other platform.

use php_ext::abi::{FromArgument as _, Raw};
use php_ext::attr::php_function;
use php_ext::raising::{Thrown, argument_error, throw, throw_error};
use php_ext::request::{Request, StandaloneHeap};
use php_ext::values::{Emalloc, Owner, PhpArray, PhpString, Value};
use php_ext_standard::UriParser;
use php_ext_uri::{ReadAs, UriStruct};

use crate::throwables::Throwables;

/// `LOG_DEBUG`, the `syslog(3)` level `zend_test_log_err_debug` logs at.
const LOG_DEBUG: i32 = 7;

/// `zend_leak_bytes(int $bytes = 3): void`: an `emalloc` the program
/// never frees, which `memory_get_usage()` counts until the request's
/// end. A size the heap's rounding overflows is php 8.5.12's allocator
/// panic, `zend_mm_panic`: its line alone on the standard error, with no
/// `Fatal error:`, file or line, and php's process ends on `abort()`,
/// which the CLI copies and a host that serves many requests ends the
/// request alone for (`Ending::Terminated`, #3515). php's leak
/// lasts until its heap is freed whole; here the request state keeps
/// the block and frees it at `RSHUTDOWN`, as [`crate::functions`]'
/// `zend_leak_variable` keeps its owner, so nothing the program sees
/// tells the two apart.
///
/// ```php
/// $m = memory_get_usage();
/// zend_leak_bytes();
/// zend_leak_bytes(100);
/// var_dump(memory_get_usage() - $m);
/// ```
/// ```expect
/// int(128)
/// ```
///
/// ```php
/// zend_leak_bytes(-1);
/// ```
/// ```expect signal=6
/// ```
/// ```stderr
/// Possible integer overflow in memory allocation (18446744073709551615 + %d)
/// ```
///
/// What the program wrote before stands, and nothing after it runs:
///
/// ```php
/// register_shutdown_function(function () { echo "shutdown\n"; });
/// echo "before\n";
/// zend_leak_bytes(-1);
/// echo "after\n";
/// ```
/// ```expect signal=6 process
/// before
/// ```
/// ```stderr
/// Possible integer overflow in memory allocation (18446744073709551615 + %d)
/// ```
#[php_function]
pub(crate) fn zend_leak_bytes(
    req: &mut Request,
    #[php(default = 3)] bytes: i64,
) -> Result<(), Thrown> {
    let size = usize::try_from(u64::from_ne_bytes(bytes.to_ne_bytes())).unwrap_or(usize::MAX);
    let leaked = Emalloc::new(req, size)?;
    req.state::<crate::State>().leaked_bytes.push(leaked);
    Ok(())
}

/// `zend_delref(mixed $variable): void`: php's body takes one count off
/// a refcounted argument with `Z_TRY_DELREF_P` and frees nothing
/// (`Z/ext/zend_test/test.c:213-224`); this one takes the argument and
/// returns null, the divergence `trace:zend-delref-no-decrement`
/// (ruled on #3016). php's own tests reach it only with `null` or the
/// wrong argument count, where the two do the same.
///
/// ```php
/// try { zend_delref(); } catch (ArgumentCountError $e) { echo $e->getMessage(), "\n"; }
/// var_dump(zend_delref(null));
/// ```
/// ```expect
/// zend_delref() expects exactly 1 argument, 0 given
/// NULL
/// ```
#[php_function]
pub(crate) fn zend_delref(variable: Value) {
    let _ = variable;
}

/// `zend_test_emalloc(int $size): void`: one `emalloc` of `$size` bytes,
/// freed at once, so `memory_get_usage()` reads what it read before and
/// its peak counts the block; an ask past the memory limit is php's
/// fatal, and one past the hard limit ends the process where it stands
/// (`Zend/tests/oom_hard_limit_exceeded.phpt`). The bytes come zeroed,
/// so php's `memset` over them is left out.
///
/// ```php
/// $m = memory_get_usage();
/// memory_reset_peak_usage();
/// zend_test_emalloc(1000000);
/// var_dump(memory_get_usage() - $m, memory_get_peak_usage() - $m);
/// ```
/// ```expect
/// int(0)
/// int(1003520)
/// ```
///
/// ```php
/// ini_set('memory_limit', '4M');
/// zend_test_emalloc(6 * 1024 * 1024);
/// echo "not reached\n";
/// ```
/// ```expect exit=255
/// Fatal error: Allowed memory size of 4194304 bytes exhausted (tried to allocate 6291456 bytes) in %s on line 3
/// ```
#[php_function]
pub(crate) fn zend_test_emalloc(req: &mut Request, size: i64) -> Result<(), Thrown> {
    let size = usize::try_from(size.cast_unsigned()).unwrap_or(usize::MAX);
    Emalloc::new(req, size)?.free(req);
    Ok(())
}

/// `zend_test_zend_call_stack_get(): ?array`: the calling thread's
/// machine stack as `zend_call_stack_get` reads it, its base and size,
/// with the frame's own address and `EG(stack_limit)`, each `%p` or
/// `0x%zx`; `null` on a platform that cannot describe the thread.
///
/// ```php
/// $s = zend_test_zend_call_stack_get();
/// var_dump(array_keys($s), count(preg_grep('/^0x[0-9a-f]+$/', $s)));
/// [$base, $size, $here, $limit] = array_map('hexdec', array_values($s));
/// var_dump($base > $here, $here > $limit, $limit >= $base - $size);
/// ```
/// ```expect
/// array(4) {
///   [0]=>
///   string(4) "base"
///   [1]=>
///   string(8) "max_size"
///   [2]=>
///   string(8) "position"
///   [3]=>
///   string(15) "EG(stack_limit)"
/// }
/// int(4)
/// bool(true)
/// bool(true)
/// bool(true)
/// ```
#[php_function]
pub(crate) fn zend_test_zend_call_stack_get(
    req: &mut Request,
) -> Result<Option<Owner<PhpArray>>, Thrown> {
    let Some(stack) = req.limits().call_stack_bounds() else {
        return Ok(None);
    };
    let rows = [
        (&b"base"[..], stack.base),
        (b"max_size", stack.max_size),
        (b"position", position()),
        (b"EG(stack_limit)", req.limits().stack_limit_address()),
    ];
    let mut array = PhpArray::new(req)?;
    for (name, address) in rows {
        let text = PhpString::new(req, format!("{address:#x}").as_bytes())?;
        array.set_str(req, name, text.into_value())?;
    }
    Ok(Some(array))
}

/// `zend_test_zend_call_stack_use_all(): int`: the native recursing until
/// its frame reaches 4 KiB above the thread's end, the frames counted,
/// which holds that the stack `zend_call_stack_get` describes is the
/// stack the thread has. Where the platform cannot describe it, php's
/// bare `return` answers `null` under the declared `int`, which only a
/// debug build of php verifies, and so does this.
///
/// ```php
/// var_dump(zend_test_zend_call_stack_use_all() > 1000);
/// echo (new ReflectionFunction('zend_test_zend_call_stack_use_all'))->getReturnType(), "\n";
/// ```
/// ```expect
/// bool(true)
/// int
/// ```
#[php_function]
pub(crate) fn zend_test_zend_call_stack_use_all(req: &mut Request) -> Result<Raw<i64>, Thrown> {
    let counted = req
        .limits()
        .call_stack_bounds()
        .map_or(Value::null(), |stack| {
            // `zend_call_stack_limit(base, max_size, 4096)`.
            let limit = (stack.base.saturating_sub(stack.max_size)).saturating_add(4096);
            Value::int(use_all(limit))
        });
    Raw::from_value(req, counted)
}

/// `zend_call_stack_use_all`: one frame more until
/// `zend_call_stack_overflowed(limit)`, through a function pointer the
/// optimiser cannot see through, as php's `volatile` one, so every level
/// is a frame of its own.
fn use_all(limit: usize) -> i64 {
    if position() <= limit {
        return 1;
    }
    let next: fn(usize) -> i64 = std::hint::black_box(use_all);
    1 + next(limit)
}

/// `zend_call_stack_position`: the address of a local of this frame, or
/// of the caller's where it is inlined; either stands at the caller's
/// depth to within a frame.
#[inline]
fn position() -> usize {
    let here = 0u8;
    std::ptr::addr_of!(here).addr()
}

/// `zend_get_map_ptr_last(): int`, `CG(map_ptr_last)`: the map pointers
/// the engine has handed the units it published, which a request that
/// declares a class leaves where it was (GH-8646). The count is the
/// engine's, not php's, so only its standing still is held to the oracle.
///
/// ```php
/// $before = zend_get_map_ptr_last();
/// class MyClass {}
/// var_dump(is_int($before), zend_get_map_ptr_last() === $before);
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// ```
#[php_function]
pub(crate) fn zend_get_map_ptr_last(req: &mut Request) -> i64 {
    i64::from(req.code_mut().unit_cache().map_ptr_last())
}

/// `zend_test_override_libxml_global_state(): void`: libxml's deprecated
/// process defaults set as a careless library leaves them, which no
/// parse php's extensions make reads (GHSA-3qrf-m4j2-pcrr).
///
/// ```php
/// zend_test_override_libxml_global_state();
/// $d = new DOMDocument;
/// $d->loadXML("<!DOCTYPE a [<!ENTITY e 'x'>]><a> &e; <b/> </a>");
/// echo $d->saveXML();
/// var_dump(simplexml_load_string("<a> <b/> </a>")->asXML());
/// ```
/// ```expect
/// <?xml version="1.0"?>
/// <!DOCTYPE a [
/// <!ENTITY e "x">
/// ]>
/// <a> &e; <b/> </a>
/// string(36) "<?xml version="1.0"?>
/// <a> <b/> </a>
/// "
/// ```
#[php_function]
pub(crate) fn zend_test_override_libxml_global_state() {
    php_ext_libxml::ffi::Library::override_global_state();
}

/// `zend_test_is_pcre_bundled(): bool`, `HAVE_BUNDLED_PCRE`: false, as the
/// engine links the platform's libpcre2-8 and never php-src's
/// `pcre2lib`, nor the copy `pcre2-sys` carries (ext/pcre's build).
///
/// ```php
/// var_dump(zend_test_is_pcre_bundled());
/// ```
/// ```expect
/// bool(false)
/// ```
#[php_function]
pub(crate) fn zend_test_is_pcre_bundled() -> bool {
    false
}

/// `zend_test_is_zend_ptr(int $addr): bool`, `is_zend_ptr`: whether the
/// address lies inside the request heap's chunks or huge blocks, read
/// off the heap's lists and never at the address (GH-14626).
///
/// ```php
/// $s = str_repeat('a', 2 * 1024 * 1024);
/// var_dump(zend_test_is_zend_ptr(0), zend_test_is_zend_ptr(1 << 30), zend_test_is_zend_ptr(-1));
/// ```
/// ```expect
/// bool(false)
/// bool(false)
/// bool(false)
/// ```
#[php_function]
pub(crate) fn zend_test_is_zend_ptr(req: &mut Request, addr: i64) -> bool {
    usize::try_from(u64::from_ne_bytes(addr.to_ne_bytes()))
        .is_ok_and(|address| req.memory().heap_holds(address))
}

/// `zend_test_log_err_debug(string $str): void`:
/// `php_log_err_with_severity(str, LOG_DEBUG)`, one entry of the log
/// `error_log` names.
///
/// ```php
/// $f = sys_get_temp_dir() . "/zend-test-log-" . getmypid();
/// ini_set('error_log', $f);
/// zend_test_log_err_debug("a debug line");
/// echo preg_replace('/^\[[^]]+\] /', '', file_get_contents($f));
/// unlink($f);
/// ```
/// ```expect
/// a debug line
/// ```
#[php_function]
pub(crate) fn zend_test_log_err_debug(req: &mut Request, str: PhpString) {
    req.diagnostics_mut()
        .log_err_with_severity(str.as_bytes(), LOG_DEBUG);
}

/// `zend_test_crash(?string $message = null): void`: `$message` printed
/// as `php_printf` prints it, what the host holds pushed as
/// `sapi_flush` pushes it, so the message is seen before the end as
/// php's unbuffered CLI shows it, and the process ended by `SIGSEGV`, the
/// signal php's read of `(char *) 1` delivers (`crate::ffi`, #2976). No
/// witness reaches the end; `tests/crash.rs` holds it to the oracle's
/// bytes and signal.
///
/// ```php
/// try { zend_test_crash("a", "b"); } catch (ArgumentCountError $e) { echo $e->getMessage(), "\n"; }
/// try { zend_test_crash([]); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// zend_test_crash() expects at most 1 argument, 2 given
/// zend_test_crash(): Argument #1 ($message) must be of type ?string, array given
/// ```
#[php_function]
pub(crate) fn zend_test_crash(
    req: &mut Request,
    #[php(default = null)] message: Option<PhpString>,
) {
    if let Some(message) = message {
        req.response_mut().write(message.as_bytes());
    }
    req.response_mut().flush();
    crate::ffi::segfault()
}

/// `zend_test_raise_and_throw(int $signal): void`: `raise(3)` of the
/// signal, the long narrowed to an `int` as C narrows it, then
/// `Exception` thrown, so the signal's handler runs with a throwable
/// pending; a signal `raise` refuses is php's `Error` instead.
///
/// ```php
/// try { zend_test_raise_and_throw(0); } catch (Exception $e) { echo get_class($e), ": ", $e->getMessage(), "\n"; }
/// try { zend_test_raise_and_throw(-1); } catch (Error $e) { echo get_class($e), ": ", $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Exception: Exception after raise()
/// Error: raise() failed
/// ```
#[php_function]
pub(crate) fn zend_test_raise_and_throw(req: &mut Request, signal: i64) -> Result<(), Thrown> {
    let [a, b, c, d, ..] = signal.to_le_bytes();
    let throwables = Throwables::of(req)?;
    if !crate::ffi::raise(i32::from_le_bytes([a, b, c, d])) {
        return Err(throw_error(req, throwables.error, "raise() failed"));
    }
    Err(throw(req, throwables.exception, "Exception after raise()"))
}

/// `zend_test_gh18756(): void`: `zend_mm_startup`, `zend_mm_gc` twice and
/// `zend_mm_shutdown(heap, true, false)`, on a heap apart from the
/// request's, which keeps its main chunk through both sweeps
/// (GH-18756).
///
/// ```php
/// zend_test_gh18756();
/// echo "==DONE==\n";
/// try { zend_test_gh18756(1); } catch (ArgumentCountError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// ==DONE==
/// zend_test_gh18756() expects exactly 0 arguments, 1 given
/// ```
#[php_function]
pub(crate) fn zend_test_gh18756() {
    let mut heap = StandaloneHeap::startup();
    heap.gc();
    heap.gc();
    heap.shutdown();
}

/// `zend_test_opcache_preloading(): bool`, `opcache_preloading()`:
/// whether this is the request `opcache.preload`'s file is served in.
///
/// ```php
/// var_dump(zend_test_opcache_preloading());
/// ```
/// ```expect
/// bool(false)
/// ```
#[php_function]
pub(crate) fn zend_test_opcache_preloading(req: &mut Request) -> bool {
    req.code().preloading()
}

/// `zend_test_uri_parser(string $uri, string $parser): array`: `$uri`
/// under the parser `php_uri_get_parser` names, its components read
/// normalized and raw, and the struct `php_uri_parse_to_struct` makes,
/// whose absent `port` is zero; a refused URI is the parser's throw.
///
/// ```php
/// $r = zend_test_uri_parser("https://%75ser@%65x.com:8080/p%61th?q#f", "parse_url");
/// echo json_encode($r), "\n";
/// echo json_encode(zend_test_uri_parser("https://example.com", "Uri\\Rfc3986\\Uri")["struct"]), "\n";
/// try { zend_test_uri_parser("x", "nope"); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { zend_test_uri_parser("invalid uri", "Uri\\WhatWg\\Url"); } catch (Uri\WhatWg\InvalidUrlException $e) { echo $e->getMessage(), " ", count($e->errors), "\n"; }
/// try { zend_test_uri_parser("http:///x", "parse_url"); } catch (Uri\InvalidUriException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// {"normalized":{"scheme":"https","username":"user","password":null,"host":"ex.com","port":8080,"path":"\/path","query":"q","fragment":"f"},"raw":{"scheme":"https","username":"%75ser","password":null,"host":"%65x.com","port":8080,"path":"\/p%61th","query":"q","fragment":"f"},"struct":{"scheme":"https","username":"%75ser","password":null,"host":"%65x.com","port":8080,"path":"\/p%61th","query":"q","fragment":"f"}}
/// {"scheme":"https","username":null,"password":null,"host":"example.com","port":0,"path":"","query":null,"fragment":null}
/// zend_test_uri_parser(): Argument #1 ($uri) Unknown parser
/// The specified URI is malformed (MissingSchemeNonRelativeUrl) 1
/// The specified URI is malformed
/// ```
#[php_function]
pub(crate) fn zend_test_uri_parser(
    req: &mut Request,
    uri: PhpString,
    parser: PhpString,
) -> Result<Owner<PhpArray>, Thrown> {
    let Some(parser) = UriParser::named(parser.as_bytes()) else {
        let ce = Throwables::of(req)?;
        return Err(argument_error(req, ce.value_error, 1, "Unknown parser"));
    };
    let text = uri.as_bytes();
    // A parse that answers no struct and throws nothing is a component
    // the parser could not read, whose `php_uri_get_*` php leaves unset.
    let normalized = parser.parse_to_struct(req, text, ReadAs::NormalizedAscii, false)?;
    let raw = parser.parse_to_struct(req, text, ReadAs::Raw, false)?;
    let read = raw.clone();
    let mut array = PhpArray::new(req)?;
    for (name, components, struct_port) in [
        (&b"normalized"[..], normalized, false),
        (b"raw", raw, false),
        (b"struct", read, true),
    ] {
        let row = components_array(req, components.unwrap_or_default(), struct_port)?;
        array.set_str(req, name, row.into_value())?;
    }
    Ok(array)
}

/// One URI's components under php's keys, `ZEND_STR_SCHEME` to
/// `ZEND_STR_FRAGMENT`: each string or `null`, and `port` an integer or
/// `null`, or for the struct `php_uri.port`, zero where absent.
fn components_array(
    req: &mut Request,
    uri: UriStruct,
    struct_port: bool,
) -> Result<Owner<PhpArray>, Thrown> {
    let port = match uri.port {
        Some(port) => Owner::int(port),
        None if struct_port => Owner::int(0),
        None => Owner::null(),
    };
    let mut array = PhpArray::new(req)?;
    let strings = [
        (&b"scheme"[..], uri.scheme),
        (b"username", uri.user),
        (b"password", uri.password),
        (b"host", uri.host),
    ];
    for (name, value) in strings {
        let value = string_or_null(req, value)?;
        array.set_str(req, name, value)?;
    }
    array.set_str(req, b"port", port)?;
    for (name, value) in [
        (&b"path"[..], uri.path),
        (b"query", uri.query),
        (b"fragment", uri.fragment),
    ] {
        let value = string_or_null(req, value)?;
        array.set_str(req, name, value)?;
    }
    Ok(array)
}

/// `ZVAL_STR_COPY` of a component, or `ZVAL_NULL` where there is none.
fn string_or_null(req: &mut Request, value: Option<Vec<u8>>) -> Result<Owner<Value>, Thrown> {
    match value {
        Some(bytes) => Ok(PhpString::new(req, &bytes)?.into_value()),
        None => Ok(Owner::null()),
    }
}
