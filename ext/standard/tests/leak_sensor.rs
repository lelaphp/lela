//! The request leak sensor (#1992) over this extension's natives: each
//! request below gives up everything its natives took, so an armed
//! engine ends it without the `InternalError` naming a block still held.
#![cfg(debug_assertions)]

use php_ext::test::{Capture, Engine, LeakSensor};

/// What one request of an armed engine with this extension alone wrote.
fn armed(source: &str) -> String {
    let engine = Engine::builder()
        .extensions(&[php_ext_standard::extension()])
        .sense_leaks(LeakSensor::Armed)
        .build();
    let served = engine.serve_capture(source, Capture::default());
    String::from_utf8_lossy(&served.stdout).into_owned()
}

/// `fgetcsv` and `fscanf` read a line for their parser, `fgetcsv` the
/// next one too where an enclosure runs past the first, and keep none
/// of it (#2086).
#[test]
fn a_line_read_for_a_parser_is_given_up() {
    let stdout = armed(
        r#"<?php
$m = fopen('php://memory', 'r+'); fwrite($m, "a,\"b\nc\",d\n12 apples\n"); rewind($m);
echo implode('|', fgetcsv($m, null, ',', '"', '')), "\n";
echo implode('|', fscanf($m, "%d %s")), "\n";
"#,
    );
    assert_eq!(stdout, "a|b\nc|d\n12|apples\n");
}

/// `file` splits the whole file it read into the lines it returns and
/// keeps the whole no longer (#2086, `Zend/tests/foreach/bug34065.phpt`).
#[test]
fn the_file_file_splits_is_given_up() {
    let stdout = armed(
        r#"<?php
$f = sys_get_temp_dir() . '/leak-sensor-file-' . getmypid() . '.txt';
file_put_contents($f, "a\nb\n");
echo implode('|', file($f)), count(file($f, FILE_IGNORE_NEW_LINES)), "\n"; unlink($f);
"#,
    );
    assert_eq!(stdout, "a\n|b\n2\n");
}

/// `readdir`, `scandir` and `glob` hand each name they read to the
/// program and keep none of it (#2087).
#[test]
fn the_names_a_directory_listing_reads_are_given_up() {
    let stdout = armed(
        r#"<?php
$d = sys_get_temp_dir() . '/leak-sensor-dir-' . getmypid(); @mkdir($d); touch("$d/a.txt"); touch("$d/b.txt");
$h = opendir($d); $n = 0; while (readdir($h) !== false) $n++; closedir($h);
echo $n, ' ', implode(',', scandir($d)), ' ', count(glob("$d/*.txt")), "\n";
unlink("$d/a.txt"); unlink("$d/b.txt"); rmdir($d);
"#,
    );
    assert_eq!(stdout, "4 .,..,a.txt,b.txt 2\n");
}

/// `array_diff` and `array_intersect` compare their values as strings
/// and keep none of the strings they converted, over the names
/// `stream_get_wrappers` and `stream_get_filters` list and a string
/// the program builds (#2066).
#[test]
fn the_strings_a_set_function_compares_are_given_up() {
    let stdout = armed(
        r#"<?php
$made = str_repeat('a', 3);
echo implode(',', array_intersect(stream_get_wrappers(), ['php', 'file'])), ' ', count(array_diff(stream_get_filters(), ['string.rot13'])) > 0 ? 'y' : 'n', ' ', implode(',', array_diff([$made, 'b', $made . 'c'], ['b'])), ' ', implode(',', array_intersect([$made, 'b'], [$made])), "\n";
"#,
    );
    assert_eq!(stdout, "php,file y aaa,aaac aaa\n");
}
