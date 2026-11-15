//! The request leak sensor over this extension's natives: each
//! request below gives up everything its natives took, so an armed
//! engine ends it without the `InternalError` naming a block still held.
#![cfg(debug_assertions)]

use php_ext::test::{Capture, Engine};

/// What one request of an armed engine with this extension and the
/// standard and pcre ones it lists and matches names through wrote.
fn armed(source: &str) -> String {
    let engine = Engine::builder()
        .extensions(&[
            php_ext_lexbor::extension(),
            php_ext_uri::extension(),
            php_ext_standard::extension(),
            php_ext_pcre::extension(),
            php_ext_zip::extension(),
        ])
        .build();
    let served = engine.serve_capture(source, Capture::default());
    served.stdout_text()
}

/// `ZipArchive::addPattern` and `addGlob` keep none of the names they
/// list to match.
#[test]
fn the_names_a_pattern_lists_are_given_up() {
    let stdout = armed(
        r#"<?php
$d = sys_get_temp_dir() . '/leak-sensor-zip-' . getmypid(); @mkdir($d); touch("$d/a.txt"); touch("$d/b.log");
$z = new ZipArchive; $z->open("$d.zip", ZipArchive::CREATE);
echo count($z->addPattern('/\.txt$/', $d)), count($z->addGlob("$d/*.log")), "\n";
$z->close(); unlink("$d.zip"); unlink("$d/a.txt"); unlink("$d/b.log"); rmdir($d);
"#,
    );
    assert_eq!(stdout, "11\n");
}
