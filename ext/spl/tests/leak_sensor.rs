//! The request leak sensor (#1992) over this extension's natives: each
//! request below gives up everything its natives took, so an armed
//! engine ends it without the `InternalError` naming a block still held.
#![cfg(debug_assertions)]

use php_ext::test::{Capture, Engine, LeakSensor};

/// What one request of an armed engine with this extension and the
/// standard one it reads files through wrote.
fn armed(source: &str) -> String {
    let engine = Engine::builder()
        .extensions(&[php_ext_standard::extension(), php_ext_spl::extension()])
        .sense_leaks(LeakSensor::Armed)
        .build();
    let served = engine.serve_capture(source, Capture::default());
    String::from_utf8_lossy(&served.stdout).into_owned()
}

/// `SplFileObject`'s current line, read by `fgets`, by `current` and by
/// iteration, and its CSV rows keep nothing past the request (#2086).
#[test]
fn a_line_an_spl_file_object_reads_is_given_up() {
    let stdout = armed(
        r#"<?php
$t = new SplTempFileObject(); $t->fwrite("a,b\nc,\"d\ne\"\n"); $t->rewind();
echo $t->fgets(); $t->rewind(); echo $t->current();
foreach ($t as $n => $line) echo $n, ':', $line;
$t->rewind(); echo implode('|', $t->fgetcsv(',', '"', '')), "\n", implode('|', $t->fgetcsv(',', '"', '')), "\n";
"#,
    );
    assert_eq!(stdout, "a,b\na,b\n0:a,b\n1:c,\"d\n2:e\"\na|b\nc|d\ne\n");
}
