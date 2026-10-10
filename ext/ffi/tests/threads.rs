//! A callback C calls from a thread of its own while the request runs
//! PHP code (#1640): the thread waits, and the request serves the call
//! at its next interrupt check, or lends itself to the thread while it
//! blocks, or ends with the call still posted and C reads zero. The C
//! side is a Rust function with C's calling convention, reached from
//! PHP code by `FFI::cast` of its address; its thread writes what the
//! callback returned to a file of its own, which the test reads. Every
//! engine native that blocks lends the request (#2401), so the window
//! in which a call stays posted is held open by a native of this
//! test's own, `hold_unlent`, which blocks without lending.

#![allow(
    clippy::disallowed_methods,
    reason = "a test starts its own threads (seams.toml rule fork.threads)"
)]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use php_ext::abi::{Extension, ExtensionBuilder};
use php_ext::attr::php_function;
use php_ext::test::{Capture, Engine};

/// The callback's type as C sees it.
type Callback = extern "C" fn(i32) -> i32;

/// Where the thread a test's C library starts writes what the callback
/// returned to it.
fn returned_to(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(format!("ffi-threads-{name}"))
}

/// A C library that keeps the callback and calls it from a thread of
/// its own, `delay` after the call that handed it over, which has
/// returned by then.
fn call_later(callback: Callback, delay: Duration, name: &'static str) {
    std::thread::spawn(move || {
        std::thread::sleep(delay);
        let returned = callback(21);
        std::fs::write(returned_to(name), returned.to_string()).ok();
    });
}

extern "C" fn later_sleeping(callback: Callback) {
    call_later(callback, Duration::from_millis(100), "sleeping");
}

extern "C" fn later_ending(callback: Callback) {
    call_later(callback, Duration::from_millis(100), "ending");
}

extern "C" fn later_closing(callback: Callback) {
    call_later(callback, Duration::from_millis(100), "closing");
}

/// `hold_unlent(float $seconds): void`: the request held for that long in
/// a native that never lends it, as no engine native that blocks is.
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

/// One request that hands `later` a callback, and what the callback
/// returned to C once the thread wrote it: `None` where the thread was
/// still waiting ten seconds on.
fn served(later: extern "C" fn(Callback), name: &str, source: &str) -> (String, Option<String>) {
    let out = returned_to(name);
    std::fs::create_dir_all(env!("CARGO_TARGET_TMPDIR")).ok();
    std::fs::remove_file(&out).ok();
    let at = later as usize;
    let source = format!(
        "<?php\n$ffi = FFI::cdef('typedef int (*cb)(int); typedef void (*later)(cb);');\n$later = $ffi->cast('later', {at});\n{source}"
    );
    let engine = Engine::builder()
        .extensions(&[
            php_ext_lexbor::extension(),
            php_ext_uri::extension(),
            php_ext_standard::extension(),
            php_ext_ffi::extension(),
            holding(),
        ])
        .ini("ffi.enable", "1")
        .ini("html_errors", "0")
        .build();
    let served = engine.serve_capture(&source, Capture::default());
    let stdout = String::from_utf8_lossy(&served.stdout).into_owned();
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
        later_sleeping,
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

/// The thread calls back while the request waits for a child in
/// `proc_close`: the wait lends the request (#2401), so the callback runs
/// inside it, on the thread, and C reads what it returned.
#[test]
fn a_callback_posted_while_proc_close_waits_is_served_in_the_wait() {
    let (stdout, returned) = served(
        later_closing,
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

/// The thread calls back while the request is in a native that does not
/// lend it, and the request reaches its next check only to end there,
/// on `max_execution_time`: the call is still posted at the request's
/// end, nothing calls the callable, and C reads zero.
#[test]
fn a_callback_still_posted_at_the_request_s_end_returns_zero() {
    let (stdout, returned) = served(
        later_ending,
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
            later_ending,
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
