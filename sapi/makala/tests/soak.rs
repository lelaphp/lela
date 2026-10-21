//! The concurrency soak's smoke, per commit (#2350, as #2336 rules): many
//! clients sending a mixed set of requests to one makala at once, each
//! response held to the same request served alone, and the request leak
//! sensor, armed in this debug build, finding nothing in the error log.
//! The nightly soak is `xtask soak`, ten minutes over php's own corpus;
//! this is its shape over requests that each touch state a request must
//! not hand on: statics, globals, ini, output buffers, headers, cookies, a
//! class declared per request, a body read as it arrives and an exception.

#![allow(
    clippy::disallowed_methods,
    reason = "a test starts its own threads (seams.toml rule fork.threads)"
)]

mod common;

use std::io::Write;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use common::{Request, Server, read_response};

/// How long the clients send. #2336 rules 30 s; under a loaded machine's
/// debug build each client still sends tens of requests in that time.
const SMOKE: Duration = Duration::from_secs(30);

/// How many clients send at once: more than the request threads and their
/// queue, so some wait in the kernel's listen queue as a burst would.
const CLIENTS: usize = 24;

/// Each program and the request sent to it, every answer a function of
/// the request alone.
const PROGRAMS: [(&str, &str); 6] = [
    (
        "state.php",
        "<?php function n() { static $n = 0; return ++$n; }\n\
         $GLOBALS['seen'] = ($GLOBALS['seen'] ?? 0) + 1;\n\
         ini_set('precision', $_GET['p']);\n\
         header('X-P: ' . ini_get('precision'));\n\
         echo n(), n(), ' ', $GLOBALS['seen'], ' ', 1 / 3, \"\\n\";",
    ),
    (
        "body.php",
        "<?php echo strlen(file_get_contents('php://input')), ' ', json_encode($_POST), \"\\n\";",
    ),
    (
        "cookie.php",
        "<?php setcookie('back', strrev($_COOKIE['c'] ?? ''));\n\
         ob_start(); echo str_repeat('ab', 4096); $kept = ob_get_clean();\n\
         echo md5($kept), ' ', count($_COOKIE), \"\\n\";",
    ),
    (
        "class.php",
        "<?php class Seen { public static int $made = 0; public function __construct() { self::$made++; } }\n\
         for ($i = 0; $i < (int) $_GET['n']; $i++) { new Seen(); }\n\
         echo Seen::$made, ' ', class_exists('Kept', false) ? 'leaked' : 'fresh', \"\\n\";",
    ),
    (
        "work.php",
        "<?php $a = []; for ($i = 0; $i < 20000; $i++) { $a[\"k$i\"] = str_repeat(chr(65 + $i % 26), $i % 7); }\n\
         ksort($a); echo md5(serialize($a)), \"\\n\";",
    ),
    (
        "throws.php",
        "<?php class Kept {}\n\
         try { throw new RuntimeException($_GET['m']); } catch (RuntimeException $e) { echo $e->getMessage(), \"\\n\"; }\n\
         trigger_error('noted', E_USER_NOTICE);",
    ),
];

/// The request each client sends for the program numbered `n`.
fn request(n: usize) -> Vec<u8> {
    let (name, _) = PROGRAMS[n % PROGRAMS.len()];
    let get = |target: &str| Request::get(&format!("/{target}")).close();
    match name {
        "state.php" => get("state.php?p=4"),
        "body.php" => Request::post("/body.php")
            .close()
            .field("Content-Type", "application/x-www-form-urlencoded")
            .body(format!("a={n}&b=%E2%82%AC{}", "x".repeat(n % 7 * 40))),
        "cookie.php" => get("cookie.php").field("Cookie", "c=makala"),
        "class.php" => get("class.php?n=3"),
        "throws.php" => get("throws.php?m=caught"),
        _ => get(name),
    }
    .bytes()
}

/// The response's status, its header lines but the server's `date`, and
/// its body: what is held, since the date moves.
fn answer(server: &Server, n: usize) -> String {
    let mut stream = server.connect();
    stream
        .write_all(&request(n))
        .unwrap_or_else(|e| panic!("{e}"));
    let response = read_response(&mut stream);
    let headers: Vec<String> = (response.headers.iter())
        .filter(|(name, _)| !name.eq_ignore_ascii_case("date"))
        .map(|(name, value)| format!("{name}: {value}"))
        .collect();
    format!(
        "{}\n{}\n\n{}",
        response.status,
        headers.join("\n"),
        String::from_utf8_lossy(&response.body)
    )
}

/// Every response served while many are in flight is the one its request
/// gets alone, and the leak sensor logs no finding.
#[test]
fn requests_served_at_once_are_each_served_as_alone() {
    let root = common::root("soak", &PROGRAMS);
    let log = root.join("errors.log");
    let log_arg = log.to_string_lossy().into_owned();
    let server = Server::with_args(
        &root,
        &[
            "-d",
            "log_errors=1",
            "--threads",
            "4",
            "--error-log",
            &log_arg,
        ],
    );
    // A body varies with `n`, so alone is per request sent.
    let kinds = PROGRAMS.len() * 7;
    let alone: Vec<String> = (0..kinds).map(|n| answer(&server, n)).collect();
    for n in 0..PROGRAMS.len() {
        assert_eq!(
            answer(&server, n),
            alone[n],
            "{} alone, again",
            PROGRAMS[n].0
        );
    }
    assert!(alone[0].contains("12 1 0.3333"), "{}", alone[0]);
    assert!(alone[3].ends_with("3 fresh\n"), "{}", alone[3]);
    let (answered, mismatched) = (AtomicU64::new(0), AtomicU64::new(0));
    let deadline = Instant::now() + SMOKE;
    std::thread::scope(|scope| {
        for client in 0..CLIENTS {
            let (server, alone, answered, mismatched) = (&server, &alone, &answered, &mismatched);
            scope.spawn(move || {
                let mut n = client;
                while Instant::now() < deadline {
                    let got = answer(server, n % kinds);
                    if got != alone[n % kinds] {
                        mismatched.fetch_add(1, Ordering::Relaxed);
                        eprintln!(
                            "mismatch for request {n}:\n{got}\nalone:\n{}",
                            alone[n % kinds]
                        );
                    }
                    answered.fetch_add(1, Ordering::Relaxed);
                    n += CLIENTS;
                }
            });
        }
    });
    server.terminate();
    let logged = std::fs::read_to_string(&log).unwrap_or_default();
    let answered = answered.into_inner();
    assert!(answered >= CLIENTS as u64, "only {answered} answered");
    assert_eq!(mismatched.into_inner(), 0, "of {answered} answered");
    for found in ["InternalError", "panicked"] {
        assert!(
            !logged.contains(found),
            "{found} in the error log:\n{logged}"
        );
    }
    assert!(
        logged.contains("noted"),
        "php's log reaches the error log:\n{logged}"
    );
}
