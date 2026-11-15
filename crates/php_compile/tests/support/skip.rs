//! A test's skip for a fact this machine lacks, as the tests that cannot
//! depend on xtask take it: `ext/standard`'s for a locale
//! the C library has not, `php_compile`'s for a main thread whose size
//! cannot be read, and `ext/mysqlnd/tests/support/mysql.rs`'s for the
//! `MySQL` server, each including this file with `#[path]`. It lives in
//! `php_compile`, the lowest of those crates, which each of the others is
//! built from, so a change here stales each of their readings, whose keys
//! cover its tree.
//!
//! [`skip`] is xtask's `Promise::or_skip` for them: the skip said on
//! stderr and, where xtask's `XTASK_SKIPPED` names a file, appended to it
//! as `<test>\t<word>`, so `xtask test` counts it in
//! `skipped_unpromised`; or a failure where the machine promised the
//! fact, its word among those of `XTASK_PROMISED`.

#![allow(
    dead_code,
    reason = "each test binary uses its own part of what is shared"
)]

use std::fs::OpenOptions;
use std::io::Write as _;

/// xtask's `oracle::PROMISE_VARIABLE` and `oracle::SKIPPED_VARIABLE`, and
/// the words `Promise::word` gives the facts these tests skip for, spelt
/// here since these tests cannot depend on xtask; xtask's
/// `the_tests_beyond_xtask_skip_by_its_own_names_and_words` holds each
/// spelling to its own.
const PROMISE_VARIABLE: &str = "XTASK_PROMISED";
const SKIPPED_VARIABLE: &str = "XTASK_SKIPPED";

/// The `MySQL` server of `ci/mysql/compose.yaml`.
pub const MYSQL: &str = "mysql";
/// Every locale a test names, which CI generates.
pub const LOCALE: &str = "locale";
/// A main thread whose size `php_heap::main_call_stack_size` reads.
pub const MAIN_STACK: &str = "main-stack";

/// The skip of a test that found the fact `word` absent, `absent` saying
/// how: said on stderr and its line appended to the file
/// [`SKIPPED_VARIABLE`] names, the test named by the thread libtest runs
/// it on, and a thread the test started, which has none, by its id; or a
/// failure where [`PROMISE_VARIABLE`] lists `word`.
pub fn skip(word: &str, absent: &str) {
    let promised = std::env::var(PROMISE_VARIABLE)
        .is_ok_and(|words| words.split_whitespace().any(|listed| listed == word));
    assert!(
        !promised,
        "{absent}; {PROMISE_VARIABLE} promised the {word}"
    );
    eprintln!("{absent}; skipping");
    if let Some(file) = std::env::var_os(SKIPPED_VARIABLE) {
        let thread = std::thread::current();
        let test = thread
            .name()
            .map_or_else(|| format!("{:?}", thread.id()), str::to_owned);
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(file)
            .and_then(|mut out| out.write_all(format!("{test}\t{word}\n").as_bytes()))
            .expect("the skip is written to the file XTASK_SKIPPED names");
    }
}
