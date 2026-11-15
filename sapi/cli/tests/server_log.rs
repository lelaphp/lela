//! What a request can write into `php -S`'s log: the line for a
//! request that ended in a fatal names the file the fatal was raised in,
//! and a `\r` or `\n` in that file's name is written as two bytes
//! (`php_sapi::log_field`), so the request writes no line of its own.

mod server;

use std::time::Duration;

use server::Server;

/// The included file, named with a line break, whose compile is fatal
/// with a message that names no file.
const INCLUDED: &str = "x\r\nFORGED.php";

#[test]
fn a_fatal_in_a_file_named_with_a_line_break_is_one_line_of_the_log() {
    let server = Server::start(
        "log-field",
        &[
            (
                "index.php",
                "<?php include __DIR__ . \"/x\\r\\nFORGED.php\";",
            ),
            (INCLUDED, "<?php break;"),
        ],
        &[],
    );
    server.get("/index.php");
    let root = std::fs::canonicalize(&server.root).unwrap_or_else(|e| panic!("the root: {e}"));
    let escaped = format!("{}/x\\r\\nFORGED.php on line 1", root.display());
    let logged = server.wait_for(Duration::from_secs(10), |lines| {
        lines.iter().any(|line| line.contains("GET /index.php - "))
    });
    let log = server.log();
    assert!(logged, "no line for the request: {log:?}");
    let line = log
        .iter()
        .find(|line| line.contains("GET /index.php - "))
        .map_or("", String::as_str);
    assert!(
        line.ends_with(&format!(
            "]: GET /index.php - 'break' not in the 'loop' or 'switch' context in {escaped}"
        )),
        "{log:?}"
    );
    assert!(
        !log.iter().any(|line| line.starts_with("FORGED")),
        "{log:?}"
    );
}
