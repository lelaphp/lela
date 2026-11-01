//! A callback C calls from a thread of its own while the request runs
//! PHP code (#1640): the thread waits, and the request serves the call
//! at its next interrupt check, or lends itself to the thread while it
//! blocks, or ends with the call still posted and C reads zero. The C
//! side is a Rust function with C's calling convention, reached from
//! PHP code by `FFI::cast` of its address; its thread writes what the
//! callback returned to a file of its own, which the test reads. Every
//! engine native that blocks lends the request (#2401) but `curl_exec`
//! and `curl_multi_select`, whose waits are libcurl's (#2595), so the
//! window in which a call stays posted is held open by a native of this
//! test's own, `hold_unlent`, which blocks without lending and needs no
//! server to wait on.
//!
//! php holds a callback served on C's thread to `EG(stack_limit)`, the
//! limit the request took on its own thread, so where C's thread stands
//! in memory decides what php does: one below the request's thread is
//! past the limit and refused, as every thread started later is on
//! Linux, which maps each thread's stack below the ones before it, and
//! one above is checked against nothing, as every later thread is on
//! macOS (#3615). This engine holds the serve to the limit
//! `zend_call_stack_init` works out on C's thread, the row
//! `trace:ffi-callback-thread-stack-limit` (#3697, #3742), so it is
//! served wherever the thread stands and refused at that thread's own
//! depth. Each test serves the request on a thread of its own and gives
//! C one started before it or after it, whichever stands where the test
//! asks ([`Stands`]), rather than the order the platform maps them in.
//! C is handed the callback with a pointer to the channel C's thread
//! waits on, as a C library takes its user data.

#![allow(
    clippy::disallowed_methods,
    reason = "a test starts its own threads (seams.toml rule fork.threads)"
)]

use std::path::PathBuf;
use std::sync::mpsc::{self, Sender};
use std::time::{Duration, Instant};

use php_ext::abi::{Extension, ExtensionBuilder};
use php_ext::attr::php_function;
use php_ext::test::{Capture, Engine, Files};

/// The callback's type as C sees it.
type Callback = extern "C" fn(i32) -> i32;

/// Where the thread a test's C library starts writes what the callback
/// returned to it: a file in `Files::new("ffi-threads-<name>", ..)`'s
/// directory.
fn returned_to(name: &str) -> PathBuf {
    std::env::temp_dir()
        .join(format!("dax-{}-ffi-threads-{name}", std::process::id()))
        .join("returned")
}

/// A C library that keeps the callback and calls it from a thread of
/// its own 100 ms after the call that handed it over, which has returned
/// by then: the callback is sent to that thread on `to`, the user data
/// the test's PHP code passes beside it.
#[allow(
    improper_ctypes_definitions,
    reason = "the user data is the test's own channel, which C passes through untouched"
)]
extern "C" fn later(callback: Callback, to: &Sender<Callback>) {
    to.send(callback).ok();
}

/// `hold_unlent(float $seconds): void`: the request held for that long in
/// a native that never lends it, as only curl's waits among the engine's
/// natives are (#2595).
#[php_function]
fn hold_unlent(seconds: f64) {
    std::thread::sleep(Duration::from_secs_f64(seconds));
}

/// The test's own extension, which carries [`hold_unlent`].
fn holding() -> Extension {
    ExtensionBuilder::default()
        .functions(vec![hold_unlent::declared()])
        .build("ffi_threads_hold")
}

/// Where C's thread stands in memory against the request's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Stands {
    /// Above it, so every frame C's thread calls back from is above the
    /// request's stack limit.
    Above,
    /// Below it, past that limit.
    Below,
}

/// One job a [`thread`] does.
type Job = Box<dyn FnOnce() + Send>;

/// A thread on a stack of its own, of `size` bytes where one is named,
/// which does the one job it is handed: where a local of its first frame
/// stands, and where to hand the job. A thread never handed one ends.
fn thread(size: Option<usize>) -> (usize, Sender<Job>) {
    let (stands, at) = mpsc::channel();
    let (hand, job) = mpsc::channel::<Job>();
    let builder = match size {
        Some(size) => std::thread::Builder::new().stack_size(size),
        None => std::thread::Builder::new(),
    };
    let spawned = builder.spawn(move || {
        let local = 0_u8;
        stands.send(std::ptr::from_ref(&local).addr()).ok();
        if let Ok(job) = job.recv() {
            job();
        }
    });
    assert!(spawned.is_ok(), "the thread did not spawn");
    let Ok(at) = at.recv() else {
        panic!("the thread did not start");
    };
    (at, hand)
}

/// [`served_with`] C's thread above the request's, as php serves it on
/// either platform, at the platform's own size.
fn served(name: &str, source: &str) -> (String, Option<String>) {
    served_with(name, source, Stands::Above, None)
}

/// One request whose `$later` hands C a callback, served on a thread of
/// its own with C's thread standing as `c` says, of `size` bytes where
/// one is named, and what the callback returned to C once C's thread
/// wrote it: `None` where the thread was still waiting ten seconds on.
/// What it returned is written under a name of its own and renamed into
/// place, so the poll never reads the file between its creation and its
/// write, which a plain `fs::write` lets it read as `""` (#2699).
fn served_with(
    name: &str,
    source: &str,
    c: Stands,
    size: Option<usize>,
) -> (String, Option<String>) {
    let files = Files::new(&format!("ffi-threads-{name}"), &[]);
    let out = returned_to(name);
    assert_eq!(out.parent(), Some(files.path()));
    let (waiting, handed) = mpsc::channel::<Callback>();
    let (at, to) = (
        later as extern "C" fn(Callback, &Sender<Callback>) as usize,
        std::ptr::from_ref(&waiting).expose_provenance(),
    );
    let source = format!(
        "<?php\n$ffi = FFI::cdef('typedef int (*cb)(int); typedef void (*later)(cb, void *);');\n$later = fn ($f) => $ffi->cast('later', {at})($f, $ffi->cast('void *', {to}));\n{source}"
    );
    let before = thread(size);
    let (stands, request) = thread(None);
    let after = thread(size);
    let Some((_, c_thread)) = [before, after]
        .into_iter()
        .find(|(at, _)| (*at > stands) == (c == Stands::Above))
    else {
        panic!("no thread started around the request's stands {c:?} it");
    };
    let file = out.clone();
    let calls: Job = Box::new(move || {
        let Ok(callback) = handed.recv() else {
            return;
        };
        std::thread::sleep(Duration::from_millis(100));
        let returned = callback(21);
        let partial = file.with_extension("writing");
        if std::fs::write(&partial, returned.to_string()).is_ok() {
            std::fs::rename(&partial, &file).ok();
        }
    });
    c_thread.send(calls).ok();
    let (said, stdout) = mpsc::channel();
    let serves: Job = Box::new(move || {
        let engine = Engine::builder()
            .extensions(&[
                php_ext_lexbor::extension(),
                php_ext_uri::extension(),
                php_ext_standard::extension(),
                php_ext_sockets::extension(),
                php_ext_ffi::extension(),
                holding(),
            ])
            .ini("ffi.enable", "1")
            .ini("html_errors", "0")
            .build();
        let served = engine.serve_capture(&source, Capture::default());
        said.send(served.stdout_text()).ok();
    });
    request.send(serves).ok();
    let Ok(stdout) = stdout.recv() else {
        panic!("the request's thread ended without its output");
    };
    // The request is over, and a callback it never handed over lets C's
    // thread end.
    drop(waiting);
    let until = Instant::now() + Duration::from_secs(10);
    while Instant::now() < until {
        if let Ok(returned) = std::fs::read_to_string(&out) {
            return (stdout, Some(returned));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    (stdout, None)
}

/// The thread calls back while the request sleeps in `usleep`: the
/// request is lent to it, so the callback runs inside the sleep, on the
/// thread, and C reads what it returned.
#[test]
fn a_callback_posted_while_the_request_sleeps_is_served_in_the_sleep() {
    let (stdout, returned) = served(
        "sleeping",
        r#"$t0 = hrtime(true);
$later(function ($x) use ($t0) {
    echo "called $x ", hrtime(true) - $t0 < 600000000 ? "inside" : "after", " the sleep\n";
    return $x * 2;
});
usleep(1200000);
echo "slept\n";
"#,
    );
    assert_eq!(stdout, "called 21 inside the sleep\nslept\n");
    assert_eq!(returned.as_deref(), Some("42"));
}

/// The same sleep, with C's thread below the request's, as every thread
/// a library starts stands on Linux: php refuses the call there before
/// the callable runs, its frames past the stack limit the request took
/// on its own thread (php 8.4.27 NTS in Docker, 3 runs of 3, #3615), and
/// the pinned php on macOS, where the thread stands above, serves it
/// unchecked. This engine holds the serve to the limit worked out on C's
/// thread, so it is served wherever the thread stands
/// (`trace:ffi-callback-thread-stack-limit`, #3697).
#[test]
fn a_callback_from_a_thread_below_the_request_s_stack_is_served_at_that_thread_s_limit() {
    let (stdout, returned) = served_with(
        "below",
        r#"$t0 = hrtime(true);
$later(function ($x) use ($t0) {
    echo "called $x ", hrtime(true) - $t0 < 600000000 ? "inside" : "after", " the sleep\n";
    return $x * 2;
});
usleep(1200000);
echo "slept\n";
"#,
        Stands::Below,
        None,
    );
    assert_eq!(stdout, "called 21 inside the sleep\nslept\n");
    assert_eq!(returned.as_deref(), Some("42"));
}

/// `serialize()` of a nest deeper than the stack, in a callback served on
/// C's thread, below the request's, of a size of this test's choosing:
/// refused with php's stack-limit `Error` at that thread's own depth,
/// which the callable catches and returns from. The size the sentence
/// names is the thread's less `zend.reserved_stack_size`, which is
/// 32 KiB on macOS and 48 KiB elsewhere, and less a guard page the
/// platform may deduct.
#[test]
fn a_deep_recursion_in_a_callback_on_a_library_s_thread_is_refused_at_that_thread_s_depth() {
    const SIZE: usize = 4 << 20;
    let (stdout, returned) = served_with(
        "deep",
        r#"$later(function ($x) {
    $a = []; for ($i = 0; $i < 100000; $i++) { $a = [$a]; }
    try { serialize($a); echo "whole\n"; } catch (Error $e) { echo $e->getMessage(), "\n"; }
    return $x * 2;
});
usleep(1200000);
echo "slept\n";
"#,
        Stands::Below,
        Some(SIZE),
    );
    let Some((size, after)) = stdout
        .strip_prefix("Maximum call stack size of ")
        .and_then(|rest| rest.split_once(' '))
    else {
        panic!("{stdout}");
    };
    let size: usize = size.parse().unwrap_or(0);
    assert!(
        (SIZE - (256 << 10)..=SIZE).contains(&size),
        "{size} is not the thread's {SIZE} less its reserve: {stdout}"
    );
    assert_eq!(
        after,
        "bytes (zend.max_allowed_stack_size - zend.reserved_stack_size) reached. Infinite recursion?\n\
         slept\n",
        "{stdout}"
    );
    assert_eq!(returned.as_deref(), Some("42"), "{stdout}");
}

/// The thread calls back while the request waits for a child in
/// `proc_close`: the wait lends the request (#2401), so the callback runs
/// inside it, on the thread, and C reads what it returned.
#[test]
fn a_callback_posted_while_proc_close_waits_is_served_in_the_wait() {
    let (stdout, returned) = served(
        "closing",
        r#"$t0 = hrtime(true);
$later(function ($x) use ($t0) {
    echo "called $x ", hrtime(true) - $t0 < 600000000 ? "inside" : "after", " the wait\n";
    return $x * 2;
});
$child = proc_open(['sleep', '1.2'], [], $pipes);
$code = proc_close($child);
echo "closed $code\n";
"#,
    );
    assert_eq!(stdout, "called 21 inside the wait\nclosed 0\n");
    assert_eq!(returned.as_deref(), Some("42"));
}

/// The thread calls back while the request reads a socket in
/// `socket_read`, and the callback closes that socket (#2601): the fd the
/// read waits on stays the socket's until the read returns, which the
/// write after the close wakes, and the read then answers as php's
/// closed-socket path does. The socket's receive timeout bounds the read
/// where nothing wakes it.
#[test]
fn a_socket_closed_by_a_callback_in_its_own_read_is_closed_when_the_read_returns() {
    let (stdout, returned) = served(
        "reading",
        r#"socket_create_pair(AF_UNIX, SOCK_STREAM, 0, $p);
socket_set_option($p[1], SOL_SOCKET, SO_RCVTIMEO, ['sec' => 3, 'usec' => 0]);
$later(function ($x) use ($p) {
    socket_close($p[1]);
    try { socket_close($p[1]); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    socket_write($p[0], "x");
    return $x * 2;
});
try { var_dump(socket_read($p[1], 10)); } catch (Error $e) { echo $e->getMessage(), "\n"; }
try { socket_write($p[1], "y"); } catch (Error $e) { echo $e->getMessage(), "\n"; }
"#,
    );
    assert_eq!(
        stdout,
        "socket_close(): Argument #1 ($socket) has already been closed\n\
         socket_read(): Argument #1 ($socket) has already been closed\n\
         socket_write(): Argument #1 ($socket) has already been closed\n"
    );
    assert_eq!(returned.as_deref(), Some("42"));
}

/// The thread calls back while the request waits in `stream_select`, and
/// the callback closes the stream the wait stands on, then writes to its
/// pair (#2664): the stream's fd stays open until the wait returns, which
/// the write wakes, and the walk after it meets the stream closed, php's
/// `TypeError` for a closed element.
#[test]
fn a_stream_closed_in_stream_select_keeps_its_fd_until_the_wait_returns() {
    let (stdout, returned) = served(
        "selecting",
        r#"$p = stream_socket_pair(STREAM_PF_UNIX, STREAM_SOCK_STREAM, 0);
$later(function ($x) use ($p) { fclose($p[0]); fwrite($p[1], "x"); return $x * 2; });
$r = [$p[0]]; $w = $e = null;
$t0 = hrtime(true);
try { var_dump(stream_select($r, $w, $e, 3)); } catch (TypeError $t) { echo $t->getMessage(), "\n"; }
echo hrtime(true) - $t0 < 2000000000 ? "woke" : "slept", "\n";
"#,
    );
    assert_eq!(
        stdout,
        "stream_select(): supplied resource is not a valid stream resource\nwoke\n"
    );
    assert_eq!(returned.as_deref(), Some("42"));
}

/// The callback a `stream_select` wait serves assigns `null` to the
/// array the wait was handed (#2664): the call holds it, and the walk
/// after the wait reads it and writes the ready streams back.
#[test]
fn stream_select_holds_its_arrays_across_the_wait() {
    let (stdout, returned) = served(
        "select-arrays",
        r#"$p = stream_socket_pair(STREAM_PF_UNIX, STREAM_SOCK_STREAM, 0);
$r = [str_repeat('k', 3) => $p[0]]; $w = $e = null;
$later(function ($x) use (&$r, $p) { $r = null; fwrite($p[1], "x"); return $x * 2; });
var_dump(stream_select($r, $w, $e, 3), array_keys($r), $r['kkk'] === $p[0]);
"#,
    );
    assert_eq!(
        stdout,
        "int(1)\narray(1) {\n  [0]=>\n  string(3) \"kkk\"\n}\nbool(true)\n"
    );
    assert_eq!(returned.as_deref(), Some("42"));
}

/// The callback a `socket_read` wait serves closes the stream the socket
/// was imported from, which owns the fd the read waits on, and writes to
/// its pair (#2664): the fd stays open until the read returns, which reads
/// what was written.
#[test]
fn a_stream_closed_under_its_imported_socket_s_read_keeps_its_fd() {
    let (stdout, returned) = served(
        "imported",
        r#"$p = stream_socket_pair(STREAM_PF_UNIX, STREAM_SOCK_STREAM, 0);
$s = socket_import_stream($p[0]);
$later(function ($x) use ($p) { fclose($p[0]); fwrite($p[1], "y"); return $x * 2; });
var_dump(socket_read($s, 10));
"#,
    );
    assert_eq!(stdout, "string(1) \"y\"\n");
    assert_eq!(returned.as_deref(), Some("42"));
}

/// The thread calls back while the request is in a native that does not
/// lend it, and the request reaches its next check only to end there,
/// on `max_execution_time`: the call is still posted at the request's
/// end, nothing calls the callable, and C reads zero.
#[test]
fn a_callback_still_posted_at_the_request_s_end_returns_zero() {
    let (stdout, returned) = served(
        "ending",
        r#"set_time_limit(1);
$later(function ($x) { echo "called $x\n"; return $x * 2; });
hold_unlent(1.5);
echo "not reached\n";
"#,
    );
    assert!(
        stdout.contains("Maximum execution time of 1 second exceeded"),
        "{stdout}"
    );
    assert!(
        !stdout.contains("called") && !stdout.contains("not reached"),
        "{stdout}"
    );
    assert_eq!(returned.as_deref(), Some("0"));
}

/// The variable that runs a test's body in the child process its parent
/// starts, rather than starting one.
#[cfg(target_os = "macos")]
const CHILD: &str = "PHP_EXT_FFI_THREADS_CHILD";

/// The thread calls back after the request ended on a fatal, before the
/// thread posted the call: the request's end freed the callback's code,
/// since a callback lives only as long as the request that made it, and
/// C calls code that is gone. The call is made in a child process, which
/// this test holds to the fault that follows; libffi unmaps a freed
/// closure's code only where it lays closures out in trampoline tables,
/// as Apple's does, so elsewhere the fault is not certain.
#[cfg(target_os = "macos")]
#[test]
fn a_callback_called_after_its_request_ended_faults_as_in_php() {
    use std::os::unix::process::ExitStatusExt;

    let name = "a_callback_called_after_its_request_ended_faults_as_in_php";
    if std::env::var_os(CHILD).is_some() {
        let (stdout, returned) = served(
            "after-end",
            r#"$later(function ($x) { echo "called $x\n"; return $x * 2; });
undefined_function();
"#,
        );
        panic!("the call after the end returned {returned:?}; the request printed {stdout}");
    }
    let Ok(exe) = std::env::current_exe() else {
        panic!("the test binary's path");
    };
    let status = std::process::Command::new(exe)
        .args(["--exact", name, "--test-threads=1"])
        .env(CHILD, "1")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    let signal = status.ok().and_then(|status| status.signal());
    assert!(
        signal == Some(libc::SIGSEGV) || signal == Some(libc::SIGBUS),
        "the child ended {signal:?}"
    );
}
