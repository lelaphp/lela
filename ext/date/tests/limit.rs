//! A request that `memory_limit` ends while it holds dates, and the
//! engine that serves the next.
#![cfg(debug_assertions)]

use php_ext::test::{Capture, Engine};

/// The limit's fatal mid-loop releases every date the array held, and
/// the engine serves the next request as the first. The size the fatal
/// asks for is the oracle's only from php's start, every extension's
/// request state laid, so it is pinned by the witness on
/// `DateTime::__construct` under `[regime.jit-off]`; this engine lays
/// ext/date's alone and is held to the site, the limit and the exit.
#[test]
fn tight_limit_fatal_releases_every_date_and_serves_again() {
    let engine = Engine::builder()
        .extensions(&[php_ext_date::extension()])
        .ini("date.timezone", "UTC")
        .ini("html_errors", "0")
        .ini("display_errors", "1")
        .ini("log_errors", "0")
        .ini("fatal_error_backtraces", "Off")
        .ini("memory_limit", "2M")
        .build();
    let mut first = None;
    for _ in 0..2 {
        let served = engine.serve_capture_in(
            &std::env::current_dir().expect("the test runs in a directory"),
            b"witness.php",
            include_str!("limit.php"),
            Capture::default(),
        );
        assert_eq!(served.exit_code, 255);
        let stdout = served.stdout_text();
        assert!(
            stdout.starts_with(
                "\nFatal error: Allowed memory size of 2097152 bytes exhausted (tried to allocate "
            ),
            "{stdout}"
        );
        assert!(stdout.ends_with("witness.php on line 5\n"), "{stdout}");
        assert_eq!(first.get_or_insert_with(|| stdout.clone()), &stdout);
    }
}
