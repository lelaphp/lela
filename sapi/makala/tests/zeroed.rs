//! One request thread with `LELA_ZERO_TOUCHED_PAGES` on (ruled
//! 2026-11-09): each request's heap zeroes the pages it touched before its
//! chunks are kept for the next, which this one thread is then handed.
//! What PHP reads moves for none of it, and a request that wrote across
//! many chunks, one the memory limit ended and the one after them are
//! each served whole, with the debug build's sensors on under
//! `release-da`, which reach the kept chunks of the request before.

#![allow(
    clippy::disallowed_methods,
    reason = "a test starts its own threads (seams.toml rule fork.threads)"
)]

mod common;

use common::{Request, Server, read_response, root};

/// The variable `php_heap` reads once per process, and a number that is
/// not zero: a copy, since the constant is not `php_heap`'s to export,
/// which [`the_variable_is_the_one_php_heap_reads`] holds to `php_heap`'s.
const ZEROED: (&str, &str) = ("LELA_ZERO_TOUCHED_PAGES", "1");

/// The test below starts makala with the variable `php_heap` reads: one
/// named otherwise would serve every request as it does with the opt-in
/// off, and pass all the same.
#[test]
fn the_variable_is_the_one_php_heap_reads() {
    let cache = common::manifest_dir().join("../../crates/php_heap/src/mm/cache.rs");
    let text = std::fs::read_to_string(&cache)
        .unwrap_or_else(|e| panic!("reading {}: {e}", cache.display()));
    let declared = format!("pub(super) const ZEROED: &str = {:?};", ZEROED.0);
    assert!(text.contains(&declared), "php_heap declares no {declared}");
}

/// Two requests on the one thread, twice over, so each after the first
/// is handed the chunks the one before it touched: one that writes about
/// six chunks of strings, frees half of them before its end and reads
/// back what it kept, and one the limit ends. `ini_get_all()` names
/// nothing of the switch.
#[test]
fn a_thread_that_zeroes_what_each_request_touched_serves_each_request_whole() {
    let root = root(
        "zeroed",
        &[
            (
                "wide.php",
                "<?php $a = []; for ($i = 0; $i < 3000; $i++) { $a[] = str_repeat(chr(65 + $i % 26), 4000); } \
                 for ($i = 0; $i < 3000; $i += 2) { unset($a[$i]); } \
                 $sum = 0; foreach ($a as $s) { $sum += ord($s[3999]); } \
                 $names = array_filter(array_keys(ini_get_all()), fn ($k) => stripos($k, 'zero') !== false); \
                 echo count($a), ' ', $sum, ' ', json_encode(array_values($names));",
            ),
            (
                "limit.php",
                "<?php $a = []; while (true) { $a[] = str_repeat('x', 100000); }",
            ),
        ],
    );
    let server = Server::with_env(
        &root,
        &[
            "--threads",
            "1",
            "-d",
            "memory_limit=32M",
            "-d",
            "display_errors=1",
        ],
        &[ZEROED],
    );
    let get = |target: &str| {
        let mut stream = server.send(&Request::get(target).host("localhost").close());
        read_response(&mut stream)
    };
    for round in 0..2 {
        let wide = get("/wide.php");
        assert_eq!(wide.status, "HTTP/1.1 200 OK", "round {round}: {wide:?}");
        // Of each 26 strings kept, every other letter from B; their last
        // bytes summed.
        let sum: u32 = (0..3000).filter(|i| i % 2 == 1).map(|i| 65 + i % 26).sum();
        assert_eq!(
            String::from_utf8_lossy(&wide.body),
            format!("1500 {sum} []"),
            "round {round}"
        );
        let limit = get("/limit.php");
        let said = String::from_utf8_lossy(&limit.body);
        assert!(
            said.contains("Allowed memory size of 33554432 bytes exhausted"),
            "round {round}: {limit:?}"
        );
    }
}
