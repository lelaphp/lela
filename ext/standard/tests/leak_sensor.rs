//! The request leak sensor (#1992) over this extension's natives: each
//! request below gives up everything its natives took, so an armed
//! engine ends it without the `InternalError` naming a block still held.
//! A leak from a filter an extension supplies names that extension.
#![cfg(debug_assertions)]

use php_ext::abi::ExtensionBuilder;
use php_ext::raising::Thrown;
use php_ext::request::{
    Brigade, FilterFlags, FilterStatus, Request, Stream, StreamFilter, Streams as _,
};
use php_ext::test::{Capture, Engine, LeakSensor};
use php_ext::values::{PhpString, Value};

/// What one request of an armed engine with this extension alone wrote.
fn armed(source: &str) -> String {
    let engine = Engine::builder()
        .extensions(&[
            php_ext_lexbor::extension(),
            php_ext_uri::extension(),
            php_ext_standard::extension(),
        ])
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

/// `implode` converts a piece that is neither a string nor an integer
/// with `zval_get_tmp_string` and gives the string up once its bytes are
/// copied, `zend_tmp_string_release`: what an object's `__toString()`
/// returns and a float's rendering (#2098, #2085).
#[test]
fn the_strings_implode_converts_are_given_up() {
    let stdout = armed(
        r#"<?php
class A { function __toString(): string { return str_repeat('a', 3); } }
echo implode(',', [new A, 2.5, new A]), ' ', implode([1.5]), "\n";
"#,
    );
    assert_eq!(stdout, "aaa,2.5,aaa 1.5\n");
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

/// `array_flip`, `array_count_values`, `array_fill_keys` and
/// `array_combine` key an array by numeric strings and keep none of
/// them: the integer key gives up the string it was read from (#1996).
#[test]
fn a_numeric_string_made_an_integer_key_is_given_up() {
    let stdout = armed(
        r#"<?php
$n = [strval(99 + strlen('a')), str_repeat('7', 2), str_repeat('k', 2)];
foreach ([array_flip($n), array_count_values($n), array_fill_keys($n, 0), array_combine($n, $n)] as $a) {
    echo implode(',', array_keys($a)), '=', implode(',', $a), "\n";
}
"#,
    );
    assert_eq!(
        stdout,
        "100,77,kk=0,1,2\n100,77,kk=1,1,1\n100,77,kk=0,0,0\n100,77,kk=100,77,kk\n"
    );
}

/// `parse_str` registers each variable as `php_register_variable_ex`
/// does, and keeps none of the keys its bracket walk makes (#2076).
#[test]
fn the_keys_a_variable_registration_makes_are_given_up() {
    let stdout = armed(
        r#"<?php
parse_str('arr[]=foo&123[]=x&a[b][c]=1&a[b][]=2&7=x', $o);
echo implode(',', array_keys($o)), ' ', $o['arr'][0], $o[123][0], $o['a']['b']['c'], $o['a']['b'][0], $o[7], "\n";
"#,
    );
    assert_eq!(stdout, "arr,123,a,7 foox12x\n");
}

/// A user wrapper whose class is an interface, a trait or an abstract
/// class opens nothing and makes no throwable, as php's test of the
/// class's kind in `user_stream_create_object` returns before
/// `object_init_ex`; an enum's `Error` is thrown to the program (#2179,
/// `ext/standard/tests/streams/bug74951.phpt`).
#[test]
fn a_wrapper_class_that_cannot_be_instantiated_keeps_no_error() {
    let stdout = armed(
        r#"<?php
abstract class A {} interface I {} trait T {} enum E { case X; }
foreach (['A', 'I', 'T', 'E'] as $c) {
    stream_wrapper_register("w$c", $c);
    try { var_dump(@fopen("w$c://x", 'r')); } catch (Error $e) { echo $e->getMessage(), "\n"; }
}
"#,
    );
    assert_eq!(
        stdout,
        "bool(false)\nbool(false)\nbool(false)\nCannot instantiate enum E\n"
    );
}

/// A read that fails on a throw, here `unserialize_callback_func`'s,
/// makes none of the `__wakeup()` calls it queued, as
/// `zend_call_function` returns at `EG(exception)` without calling, so
/// the throwable a queued call would make is never made and none is
/// dropped (#2231, `ext/standard/tests/serialize/bug64354_2.phpt`).
#[test]
fn a_read_failed_on_a_throw_makes_no_delayed_call() {
    let stdout = armed(
        r#"<?php
class A { function __wakeup() { throw new Exception("wakeup A"); } }
function cb($class) { throw new Exception("no $class"); }
ini_set('unserialize_callback_func', 'cb');
try { unserialize('a:2:{i:0;O:1:"A":0:{}i:1;O:1:"B":0:{}}'); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
"#,
    );
    assert_eq!(stdout, "no B\n");
}

/// `array_splice` holds its table across the removal and deletes from
/// it in place, as `php_splice` does under `GC_ADDREF(in_hash)` and
/// `HT_ALLOW_COW_VIOLATION`, so a copy of the table a throwing
/// destructor's trace takes while the removal runs is no second name
/// for it (#2089, `ext/standard/tests/array/gh19926.phpt`).
#[test]
fn a_splice_whose_destructors_throw_releases_no_freed_table() {
    let stdout = armed(
        r#"<?php
class T { function __destruct() { throw new Exception("t"); } }
function f() {
    $arr = [new T, new T, 3];
    try { array_splice($arr, 0, 2); } catch (Exception $e) { echo $e->getMessage(), " ", $e->getPrevious()->getMessage(), "\n"; }
    var_dump($arr);
}
f();
"#,
    );
    assert_eq!(stdout, "t t\narray(1) {\n  [0]=>\n  int(3)\n}\n");
}

/// `serialize()`'s `__sleep` arm looks each name up in the object's
/// properties as they stand when it reads the name, as
/// `php_var_serialize_get_sleep_props` finds each in the table: a
/// property the warning's handler overwrote is read as it is now, not
/// through a view of the value the write freed (#2103).
#[test]
fn a_sleep_name_reads_the_property_as_it_stands() {
    let stdout = armed(
        r#"<?php
set_error_handler(function() { global $o, $z; $o->b = null; $GLOBALS['y'] = $z . $z; return true; });
class T { public $a = 1; public $b; function __sleep() { return [1, 'b']; } }
$o = new T; $v = 'vvvvv'; $z = 'ZZZZZ'; $o->b = $v . $v;
echo serialize($o), "\n";
"#,
    );
    assert_eq!(stdout, "O:1:\"T\":1:{s:1:\"b\";N;}\n");
}

/// A `__sleep` name whose conversion throws ends the walk, and the
/// array the arm was gathering is given up (#2103).
#[test]
fn a_sleep_name_that_throws_keeps_no_array() {
    let stdout = armed(
        r#"<?php
class N { function __toString(): string { throw new Exception("n"); } }
class T { public $a = 1; function __sleep() { return ['a', new N]; } }
try { echo serialize(new T), "\n"; } catch (Exception $e) { echo $e->getMessage(), "\n"; }
"#,
    );
    assert!(stdout.ends_with("n\n"), "{stdout}");
}

/// A write that releases a variable's object in place, as `extract`'s
/// `EXTR_REFS` prefix helpers do, tells the value a destructor the
/// release serves wrote there from the object it released, even where
/// that destructor freed the object through the variable and the
/// program's new object took its address: the new object is released
/// once, after the write, and nothing is held (#2242). Each round
/// drains the debug quarantine by one more object of the same size, so
/// rounds lay the new object where the old one stood. The variable is a
/// CV the main file's table points at (`$p_x`), and then a bucket
/// `$GLOBALS` moved into a place of its own (`p_y`, which no frame
/// compiles).
#[test]
fn a_write_in_place_tells_a_new_value_at_a_reused_address_from_the_old() {
    let stdout = armed(
        r#"<?php
class X { function __construct(public $n) {} function __destruct() { echo "X{$this->n} "; } }
class O { public $d; }
class D { function __destruct() {
    $v = $GLOBALS['v'];
    $GLOBALS[$v] = 1;
    $k = []; for ($i = 0; $i < $GLOBALS['n']; $i++) $k[] = new O;
    $k = null;
    $GLOBALS[$v] = new O;
    $GLOBALS[$v]->d = new X($GLOBALS['n']);
} }
$v = 'p_x';
for ($n = 0; $n < 40; $n++) {
    unset($p_x, $src);
    $p_x = new O; $p_x->d = new D;
    $src = ['x' => 5];
    extract($src, EXTR_REFS | EXTR_PREFIX_ALL, 'p');
}
echo "| ";
$v = 'p_y';
for ($n = 0; $n < 40; $n++) {
    unset($GLOBALS['p_y'], $src);
    $GLOBALS['p_y'] = new O; $GLOBALS['p_y']->d = new D;
    $src = ['y' => 5];
    extract($src, EXTR_REFS | EXTR_PREFIX_ALL, 'p');
}
echo "end\n";
"#,
    );
    let rounds = (0..40).fold(String::new(), |mut rounds, n| {
        rounds.push('X');
        rounds.push_str(&n.to_string());
        rounds.push(' ');
        rounds
    });
    assert_eq!(stdout, format!("{rounds}| {rounds}end\n"));
}

/// A reference cell an `EXTR_REFS` write releases in place, whose
/// destroy serves a destructor that unsets the variable still naming it
/// and writes a new object there: the cell is destroyed once, its bytes
/// given back once, and the new object is released at the write, which
/// php leaks until the request ends (#2245).
#[test]
fn a_cell_released_in_place_is_destroyed_once_where_its_destructor_unsets_it() {
    let stdout = armed(
        r#"<?php
class X { function __destruct() { echo "dtor X\n"; } }
class D { function __destruct() {
    echo "dtor D\n";
    unset($GLOBALS['p_x']);
    $GLOBALS['p_x'] = new X;
} }
$a = [new D]; $p_x = &$a[0]; unset($a);
$src = ['x' => 5];
extract($src, EXTR_REFS | EXTR_PREFIX_ALL, 'p');
var_dump($p_x);
echo "end\n";
"#,
    );
    assert_eq!(stdout, "dtor D\ndtor X\nint(5)\nend\n");
}

/// A filter an extension's factory made, faulty on purpose: each call
/// makes a string and forgets its owner, so it is neither given up nor
/// reported dropped, then hands the buckets on.
struct LeakyFilter;

impl StreamFilter for LeakyFilter {
    fn filter(
        &mut self,
        req: &mut Request,
        _stream: Stream,
        input: &mut Brigade,
        output: &mut Brigade,
        consumed: Option<&mut usize>,
        _flags: FilterFlags,
    ) -> Result<FilterStatus, Thrown> {
        if let Ok(made) = PhpString::new(req, b"in filter") {
            // The leak is the point: neither given up nor reported dropped.
            #[allow(clippy::mem_forget)]
            std::mem::forget(made);
        }
        let mut count = 0;
        while let Some(bucket) = input.pop_front() {
            count += bucket.len();
            output.push_back(bucket);
        }
        if let Some(n) = consumed {
            *n = count;
        }
        Ok(FilterStatus::PassOn)
    }
}

#[allow(clippy::unnecessary_wraps)] // `FilterFactory`'s signature
fn leaky_factory(
    _req: &mut Request,
    _name: &[u8],
    _params: Option<&Value>,
    _persistent: bool,
) -> Result<Option<Box<dyn StreamFilter>>, Thrown> {
    Ok(Some(Box::new(LeakyFilter)))
}

#[allow(clippy::unnecessary_wraps)] // the ABI's `rinit` hook
fn register_leaky(req: &mut Request) -> Result<(), Thrown> {
    req.register_filter(b"leaky", leaky_factory);
    Ok(())
}

/// A block a filter an extension supplied allocated is named with that
/// extension, though `fwrite`, ext/standard's native, ran the filter
/// (#2113).
#[test]
fn a_leak_from_a_supplied_filter_names_its_extension() {
    let engine = Engine::builder()
        .extensions(&[
            php_ext_lexbor::extension(),
            php_ext_uri::extension(),
            php_ext_standard::extension(),
            ExtensionBuilder::default()
                .rinit(register_leaky)
                .build("leak_filter"),
        ])
        .sense_leaks(LeakSensor::Armed)
        .build();
    let served = engine.serve_capture(
        "<?php $m = fopen('php://memory', 'w+'); stream_filter_append($m, 'leaky', STREAM_FILTER_WRITE);\nfwrite($m, 'x'); echo 'written';\n",
        Capture::default(),
    );
    let stdout = String::from_utf8_lossy(&served.stdout);
    assert!(stdout.starts_with("written"), "{stdout}");
    assert!(
        stdout.contains("InternalError: the request ended with"),
        "{stdout}"
    );
    assert!(
        stdout.contains(", allocated by leak_filter in "),
        "{stdout}"
    );
}
