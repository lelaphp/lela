//! A request's locale is its own (design §5.14, revision 40): two
//! requests served at once by one engine on two threads, one under
//! `de_DE` and one under the process's `C`, each read their own locale
//! while the other's is installed, and the thread a request ran on runs
//! the next request under the process's locale again.

use std::path::Path;
use std::thread;

use php_ext::test::{Capture, Engine};

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
    String::from_utf8_lossy(&served.stdout).into_owned()
}

#[test]
fn two_requests_at_once_never_read_each_other_s_locale() {
    let dir = std::env::temp_dir().join(format!("php-locale-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("the rendezvous directory");
    for marker in ["german", "c", "german-done"] {
        let _ = std::fs::remove_file(dir.join(marker));
    }
    let engine = Engine::builder()
        .extensions(&[php_ext_standard::extension()])
        .build();
    let german = source(
        &dir,
        r#"setlocale(LC_ALL, "de_DE"); touch("$d/german"); wait_for("$d/c");
        echo sprintf("%.2f", 3.14), "|", setlocale(LC_ALL, "0"), "|", strcoll("a", "B") < 0 ? "a<B" : "B<a";
        touch("$d/german-done");"#,
    );
    let c = source(
        &dir,
        r#"wait_for("$d/german");
        echo sprintf("%.2f", 3.14), "|", setlocale(LC_ALL, "0"), "|", strcoll("a", "B") < 0 ? "a<B" : "B<a";
        touch("$d/c"); wait_for("$d/german-done");
        echo "|", sprintf("%.2f", 3.14);"#,
    );
    let after = source(
        &dir,
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
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(german, "3,14|de_DE|a<B");
    assert_eq!(c, "3.14|C/C.UTF-8/C/C/C/C|B<a|3.14");
    assert_eq!(after, "3.14|C/C.UTF-8/C/C/C/C");
}
