//! The request leak sensor (#1992) over this extension's wrapper: each
//! request below gives up everything the wrapper's hooks took, so an
//! armed engine ends it without the `InternalError` naming a block
//! still held.
#![cfg(debug_assertions)]

use php_ext::test::{Capture, Engine, LeakSensor};

/// What one request of an armed engine with this extension and the
/// three it is built over wrote.
fn armed(source: &str) -> String {
    let engine = Engine::builder()
        .extensions(&[
            php_ext_lexbor::extension(),
            php_ext_json::extension(),
            php_ext_uri::extension(),
            php_ext_standard::extension(),
            php_ext_hash::extension(),
            php_ext_spl::extension(),
            php_ext_phar::extension(),
        ])
        .sense_leaks(LeakSensor::Armed)
        .build();
    let served = engine.serve_capture(source, Capture::default());
    served.stdout_text()
}

/// An archive of one file, `b.txt`, written by hand as the witness of
/// `intercept::install` writes one, at the path `$p` names.
const ARCHIVE: &str = r#"
$p = sys_get_temp_dir() . '/leak-sensor-' . getmypid() . '.phar';
$n = 'b.txt'; $c = 'bee';
$m = pack('V', strlen($n)) . $n . pack('VVVVVV', strlen($c), 1700000000, strlen($c), crc32($c), 0x1a4, 0);
$m = pack('VnVV', 1, 0x1110, 0x10000, 0) . pack('V', 0) . $m;
$data = "<?php __HALT_COMPILER(); ?>\r\n" . pack('V', strlen($m)) . $m . $c;
file_put_contents($p, $data . sha1($data, true) . pack('V', 2) . 'GBMB');
"#;

/// The wrapper's `url_stat` answers the array the stream layer reads
/// its thirteen fields out of, and the stream layer gives it up once
/// read (#2102): `stat`, `filesize` and `file_exists` over an entry.
#[test]
fn the_array_a_wrapper_stat_answers_is_given_up() {
    let stdout = armed(&format!(
        r#"<?php {ARCHIVE}
echo stat("phar://$p/b.txt")['size'], ' ', filesize("phar://$p/b.txt"), ' ', var_export(file_exists("phar://$p/b.txt"), true), "\n";
unlink($p);
"#
    ));
    assert_eq!(stdout, "3 3 true\n");
}

/// `buildFromDirectory` hands the directory to the
/// `RecursiveDirectoryIterator` it constructs and keeps no owner of it
/// after, as php's `zval_ptr_dtor(&arg)` gives up the one it made
/// (#2210, `ext/phar/tests/bug54289.phpt`,
/// `cache_list/copyonwrite5.phar.phpt` and `copyonwrite6.phar.phpt`).
#[test]
fn the_directory_a_build_iterates_is_given_up() {
    let engine = Engine::builder()
        .extensions(&[
            php_ext_lexbor::extension(),
            php_ext_json::extension(),
            php_ext_uri::extension(),
            php_ext_standard::extension(),
            php_ext_hash::extension(),
            php_ext_spl::extension(),
            php_ext_phar::extension(),
        ])
        .ini("phar.readonly", "0")
        .sense_leaks(LeakSensor::Armed)
        .build();
    let served = engine.serve_capture(
        r#"<?php
$d = sys_get_temp_dir() . '/leak-build-' . getmypid(); @mkdir($d); file_put_contents("$d/f", 'x');
$p = "$d.phar"; @unlink($p);
echo count((new Phar($p))->buildFromDirectory($d)), "\n";
unlink("$d/f"); rmdir($d); unlink($p);
"#,
        Capture::default(),
    );
    assert_eq!(served.stdout_text(), "1\n");
}
