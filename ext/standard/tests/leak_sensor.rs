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

/// An include through `php://filter` keeps neither the path its
/// resource opened at nor, where a filter refuses the read, anything of
/// the open; the included file is named as the file it read, as php
/// names it (#2067).
#[test]
fn an_include_through_a_filter_gives_up_the_path_it_opened() {
    let stdout = armed(
        r#"<?php
$f = sys_get_temp_dir() . '/leak-sensor-include-' . getmypid() . '.php';
file_put_contents($f, '<?php echo "in ", __FILE__ === realpath($GLOBALS["f"]) ? "file" : __FILE__, "\n";');
class F extends php_user_filter { }
stream_filter_register('f.fatal', F::class);
var_dump(@include 'php://filter/read=f.fatal/resource=' . $f);
include 'php://filter/read=string.rot13|string.rot13/resource=' . $f;
echo in_array(realpath($f), get_included_files(), true) ? 'named' : implode(',', get_included_files()), "\n";
unlink($f);
"#,
    );
    assert_eq!(stdout, "bool(false)\nin file\nnamed\n");
}

/// `get_meta_tags` reads its file a byte at a time and keeps none of the
/// bytes past the tags it returns (#2088).
#[test]
fn the_bytes_get_meta_tags_scans_are_given_up() {
    let stdout = armed(
        r#"<?php
$f = sys_get_temp_dir() . '/leak-sensor-meta-' . getmypid() . '.html';
file_put_contents($f, "<html><head><meta name=\"author\" content=\"Jo\"><meta name=k content=v></head></html>");
foreach (get_meta_tags($f) as $k => $v) echo $k, '=', $v, ' ';
echo "\n"; unlink($f);
"#,
    );
    assert_eq!(stdout, "author=Jo k=v \n");
}

/// `copy`, `stream_copy_to_stream`, `getimagesize`,
/// `php_strip_whitespace` and `proc_open`'s command and environment keep
/// none of the strings they read or converted (#2090).
#[test]
fn the_whole_file_and_copy_natives_give_up_what_they_read() {
    let stdout = armed(
        r#"<?php
$d = sys_get_temp_dir() . '/leak-sensor-copy-' . getmypid(); @mkdir($d);
file_put_contents("$d/a", str_repeat('x', 10));
$a = fopen("$d/a", 'r'); $b = fopen('php://memory', 'w+');
var_dump(copy("$d/a", "$d/b"), stream_copy_to_stream($a, $b), stream_copy_to_stream($a, $b));
file_put_contents("$d/i.gif", "GIF89a\x02\x00\x03\x00\x80\x00\x00");
echo getimagesize("$d/i.gif")[3], ' ', strlen(php_strip_whitespace("$d/a")), "\n";
$p = proc_open(['/bin/sh', '-c', 'echo $A' . str_repeat(' ', 1)], [1 => ['pipe', 'w']], $pipes, null, ['A' => str_repeat('b', 3)]);
echo stream_get_contents($pipes[1]); proc_close($p);
unlink("$d/a"); unlink("$d/b"); unlink("$d/i.gif"); rmdir($d);
"#,
    );
    assert_eq!(
        stdout,
        "bool(true)\nint(10)\nint(0)\nwidth=\"2\" height=\"3\" 10\nbbb\n"
    );
}

/// `range()` raising its `ValueError` makes no array first, as php's
/// checks stand before `array_init_size` (#2091).
#[test]
fn a_range_refused_makes_no_array() {
    let stdout = armed(
        r#"<?php
foreach ([[1, 5, 0], [1, 3, 5], [1, 5, -2], [1.0, 3.0, 5.0], ['a', 'c', 5], ['a', 'e', -1]] as [$a, $b, $s]) {
    try { range($a, $b, $s); } catch (ValueError $e) { echo strlen($e->getMessage()) > 0 ? 'e' : '-'; }
}
echo count(range(1, 3)), count(range(3.0, 1.0)), count(range('a', 'c')), "\n";
"#,
    );
    assert_eq!(stdout, "eeeeee333\n");
}

/// `get_headers` over a user wrapper's stream keeps neither the meta
/// data it read the headers from nor the wrapper's object that data
/// holds (#2091).
#[test]
fn the_meta_data_get_headers_reads_is_given_up() {
    let stdout = armed(
        r"<?php
class W { public $context; function stream_open($p, $m, $o, &$op) { return true; } function stream_eof() { return true; } }
stream_wrapper_register('web', 'W', STREAM_IS_URL);
var_dump(get_headers('web://x'));
",
    );
    assert_eq!(stdout, "bool(false)\n");
}

/// A user filter whose `filter()` throws as its stream closes is given
/// up with the stream all the same, its `onClose()` not called while
/// the throwable is pending (#2091).
#[test]
fn a_user_filter_that_threw_is_given_up() {
    let stdout = armed(
        r#"<?php
class F { public $filtername, $params; public int $stream = 1; function filter($in, $out, &$consumed, $closing): int { return PSFS_PASS_ON; } function onClose() { echo "closed\n"; } }
stream_filter_register('f', 'F');
$m = fopen('php://memory', 'w');
stream_filter_append($m, 'f', STREAM_FILTER_WRITE);
try { fclose($m); } catch (TypeError $e) { echo get_class($e), "\n"; }
"#,
    );
    assert_eq!(stdout, "TypeError\n");
}

/// The string `assert.callback` is made into, and the callable
/// `assert_options()` sets, are given up as the request ends, as
/// `PHP_RSHUTDOWN(assert)` gives up `ASSERTG(callback)` (#2063).
#[test]
fn the_assert_callback_is_given_up_at_the_request_s_end() {
    for set in [
        "@ini_set('assert.callback', 'strlen');",
        "@ini_set('assert.callback', 'nope');",
        "class K { function m() {} } @assert_options(ASSERT_CALLBACK, [new K, 'm']);",
    ] {
        let stdout = armed(&format!(
            "<?php {set} try {{ assert(false); }} catch (Throwable $e) {{ echo get_class($e), \"\\n\"; }}"
        ));
        assert_eq!(stdout, "AssertionError\n", "{set}");
    }
}

/// `str_replace()` reads each search, replacement and subject row as
/// `zval_get_tmp_string` reads it and gives the string up once it is
/// read, a string row's added owner and a number's new string alike
/// (#2084); the leak sensor is armed (#1992).
#[test]
fn str_replace_gives_up_the_strings_it_reads() {
    let stdout = armed(
        "<?php $r = str_repeat('r', 20); $s = str_repeat('s', 2);\n\
         echo str_replace([$s, 'x', 7], [$r, 8, $r], 'ssx7'), \"\\n\";\n\
         foreach (str_replace([$s, 'q'], $r, [-1 => -1, 'k' => 'ss', 3 => 1.5]) as $k => $v) { echo $k, '=', $v, \"\\n\"; }\n\
         class T { function __toString(): string { throw new Exception('t'); } }\n\
         try { str_replace('s', $r, [$s, new T]); } catch (Exception $e) { echo $e->getMessage(), \"\\n\"; }\n",
    );
    assert_eq!(
        stdout,
        "rrrrrrrrrrrrrrrrrrrr8rrrrrrrrrrrrrrrrrrrr\n-1=-1\nk=rrrrrrrrrrrrrrrrrrrr\n3=1.5\nt\n"
    );
}

/// `pack` gives up the string each `a`, `A`, `Z`, `h` and `H` code
/// reads, as php's `zend_tmp_string_release`, and the string a `*`
/// count converted its argument to where it stood, as php's frame gives
/// up its slot (#2095).
#[test]
fn pack_gives_up_the_strings_it_reads() {
    let stdout = armed(
        "<?php $s = str_repeat('s', 2); $h = str_repeat('4', 2);\n\
         echo bin2hex(pack('a4A*Z*h*H2', $s, $s, $s, $h, $h)), \"\\n\";\n\
         echo pack('a*', 12.5), \"\\n\";\n\
         try { pack('a*C', $s); } catch (ValueError $e) { echo $e->getMessage(), \"\\n\"; }\n",
    );
    assert_eq!(
        stdout,
        "7373000073737373004444\n12.5\nType C: too few arguments\n"
    );
}
