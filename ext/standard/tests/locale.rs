//! A request's locale is its own (design §5.14, revision 40): two
//! requests served at once by one engine on two threads, one under
//! `de_DE` and one under the process's `C`, each read their own locale
//! while the other's is installed, and the thread a request ran on runs
//! the next request under the process's locale again.
//!
//! A test that names a locale asks [`has_locale`] first and, on a host
//! whose C library lacks it, says it skipped and measures nothing; CI
//! generates every locale these tests and the witnesses name
//! (`.github/workflows/harness.yml`), so there it runs.

#![allow(
    clippy::disallowed_methods,
    reason = "a test starts its own threads (seams.toml rule fork.threads)"
)]

use std::path::Path;
use std::thread;

use php_ext::test::{Capture, Engine, Files};

/// The source of one request with the rendezvous directory spliced in:
/// a request waits on a marker file the other touches, for at most two
/// seconds, so each reads its locale while the other's is installed.
fn source(dir: &Path, body: &str) -> String {
    let dir = dir.display();
    let wait =
        "function wait_for($f) { for ($t = 0; !file_exists($f) && $t < 2000; $t++) usleep(1000); }";
    format!("<?php $d = '{dir}'; {wait} {body}")
}

fn served(engine: &Engine, source: &str) -> String {
    let served = engine.serve_capture(source, Capture::default());
    assert_eq!(served.fatal, None);
    served.stdout_text()
}

/// Whether the C library has the locale `name`, asked the way a phpt's
/// `--SKIPIF--` asks: a request's `setlocale()`, which answers what
/// `newlocale(3)` answers for the name. A test that needs the locale and
/// finds it absent says so on stderr and returns, since a Rust test has
/// no skip of its own.
fn has_locale(engine: &Engine, name: &str) -> bool {
    let probe =
        format!(r#"<?php echo setlocale(LC_ALL, "{name}") === false ? "absent" : "present";"#);
    let present = served(engine, &probe) == "present";
    if !present {
        eprintln!("skipped: the C library has no {name} locale on this host");
    }
    present
}

#[test]
fn two_requests_at_once_never_read_each_other_s_locale() {
    let engine = Engine::builder()
        .extensions(&[
            php_ext_lexbor::extension(),
            php_ext_uri::extension(),
            php_ext_standard::extension(),
        ])
        .build();
    if !has_locale(&engine, "de_DE") {
        return;
    }
    let files = Files::new("standard-locale", &[]);
    let dir = files.path();
    let german = source(
        dir,
        r#"setlocale(LC_ALL, "de_DE"); touch("$d/german"); wait_for("$d/c");
        echo sprintf("%.2f", 3.14), "|", setlocale(LC_ALL, "0"), "|", strcoll("a", "B") < 0 ? "a<B" : "B<a";
        touch("$d/german-done");"#,
    );
    let c = source(
        dir,
        r#"wait_for("$d/german");
        echo sprintf("%.2f", 3.14), "|", setlocale(LC_ALL, "0"), "|", strcoll("a", "B") < 0 ? "a<B" : "B<a";
        touch("$d/c"); wait_for("$d/german-done");
        echo "|", sprintf("%.2f", 3.14);"#,
    );
    let after = source(
        dir,
        r#"echo sprintf("%.2f", 3.14), "|", setlocale(LC_ALL, "0");"#,
    );
    let (german, c, after) = thread::scope(|scope| {
        let german = scope.spawn(|| {
            let first = served(&engine, &german);
            (first, served(&engine, &after))
        });
        let c = scope.spawn(|| served(&engine, &c));
        let (german, after) = german.join().expect("the de_DE request's thread");
        (german, c.join().expect("the C request's thread"), after)
    });
    assert_eq!(german, "3,14|de_DE|a<B");
    assert_eq!(c, "3.14|C/C.UTF-8/C/C/C/C|B<a|3.14");
    assert_eq!(after, "3.14|C/C.UTF-8/C/C/C/C");
}

/// `setlocale(LC_ALL, "")` under an environment naming a locale the C
/// library refuses answers `false` and moves no category, as php built
/// without libintl does:
/// the engine's side of `trace:setlocale-libintl-fallback` (#1126), where
/// the pinned oracle's libintl answers `C/UTF-8/C/C/C/C` from its own
/// fallback and the machine's language settings. A witness cannot hold
/// it, since a fence is held to the oracle's bytes; the row is the
/// accepted difference, and this test is what turns red if the engine's
/// answer moves.
#[test]
fn an_environment_locale_the_c_library_refuses_fails_setlocale() {
    let engine = Engine::builder()
        .extensions(&[
            php_ext_lexbor::extension(),
            php_ext_uri::extension(),
            php_ext_standard::extension(),
        ])
        .build();
    let answered = served(
        &engine,
        r#"<?php $before = setlocale(LC_ALL, "0"); putenv("LC_ALL="); putenv("LANG=xx_XX");
        var_dump(setlocale(LC_ALL, ""), setlocale(LC_ALL, "0") === $before);"#,
    );
    assert_eq!(answered, "bool(false)\nbool(true)\n");
}
