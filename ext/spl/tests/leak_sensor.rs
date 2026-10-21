//! The request leak sensor (#1992) over this extension's natives: each
//! request below gives up everything its natives took, so an armed
//! engine ends it without the `InternalError` naming a block still held.
#![cfg(debug_assertions)]

use php_ext::test::{Capture, Engine, LeakSensor};

/// What one request of an armed engine with this extension and the
/// standard one it reads files through wrote.
fn armed(source: &str) -> String {
    let engine = Engine::builder()
        .extensions(&[
            php_ext_lexbor::extension(),
            php_ext_json::extension(),
            php_ext_uri::extension(),
            php_ext_standard::extension(),
            php_ext_spl::extension(),
        ])
        .sense_leaks(LeakSensor::Armed)
        .build();
    let served = engine.serve_capture(source, Capture::default());
    served.stdout_text()
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

/// The names `DirectoryIterator`, `FilesystemIterator`,
/// `RecursiveDirectoryIterator` and `GlobIterator` read from a directory
/// as they step are kept only as each entry's own (#2087).
#[test]
fn the_names_a_directory_iterator_reads_are_given_up() {
    let stdout = armed(
        r#"<?php
$d = sys_get_temp_dir() . '/leak-sensor-spl-dir-' . getmypid(); @mkdir("$d/sub", 0777, true); touch("$d/a.txt"); touch("$d/sub/b.txt");
$n = 0; foreach (new DirectoryIterator($d) as $f) { $n++; $f->getFilename(); }
foreach (new FilesystemIterator($d) as $k => $f) $n++;
foreach (new RecursiveIteratorIterator(new RecursiveDirectoryIterator($d, FilesystemIterator::SKIP_DOTS)) as $f) $n++;
foreach (new GlobIterator("$d/*.txt") as $f) $n++;
echo $n, "\n";
unlink("$d/sub/b.txt"); unlink("$d/a.txt"); rmdir("$d/sub"); rmdir($d);
"#,
    );
    assert_eq!(stdout, "9\n");
}

/// `spl_autoload_register()` with no callable registers `spl_autoload`
/// under its name, which the stack keeps its own owner of, so the name
/// made to resolve it is given up, once registered and once found
/// registered already (#2075); the leak sensor is armed (#1992).
#[test]
fn the_default_autoloader_keeps_no_name() {
    let stdout = armed(
        "<?php spl_autoload_register(); spl_autoload_register(); foreach (spl_autoload_functions() as $f) { echo $f, \"\\n\"; }",
    );
    assert_eq!(stdout, "spl_autoload\n");
}

/// `RecursiveTreeIterator` renders an entry from its string,
/// `spl_recursive_tree_iterator_get_entry`'s `zval_try_get_string`, and
/// gives up the string and its copy of the element, whether the
/// conversion answers or throws (#2117, `ext/pdo/tests/pdo_031.phpt`;
/// #2130).
#[test]
fn a_tree_iterator_s_entries_are_given_up() {
    let stdout = armed(
        r#"<?php
class T { function __toString(): string { throw new Exception('no'); } }
$a = [[str_repeat('a', 3), 12], new T];
try { foreach (new RecursiveTreeIterator(new RecursiveArrayIterator($a, RecursiveArrayIterator::CHILD_ARRAYS_ONLY), RecursiveTreeIterator::BYPASS_KEY) as $v) echo $v, "\n"; } catch (Exception $e) { echo $e->getMessage(), "\n"; }
"#,
    );
    assert_eq!(stdout, "|-Array\n| |-aaa\n| \\-12\nno\n");
}

/// An `ArrayObject` over an object takes one owner of the value
/// `$ao[] = ...` hands it and gives that owner to the object's table,
/// where it once kept an owner unwritten and unreleased: its append is a
/// sink taking an owner (#1997).
#[test]
fn an_append_to_an_object_s_table_keeps_nothing() {
    let stdout = armed(
        "<?php $o = new stdClass; $ao = @new ArrayObject($o); $ao[] = str_repeat('a', 3); echo \"ok\\n\";",
    );
    assert_eq!(stdout, "ok\n");
}

/// `$ao[] = $v` over an object appends to the object's property table,
/// which keeps the value as its own (#2175), and `var_dump` prints the
/// row under its integer key.
#[test]
fn a_value_appended_to_an_object_s_table_is_the_table_s() {
    let stdout = armed(
        r#"<?php
$o = new stdClass; $ao = @new ArrayObject($o); $ao[] = str_repeat("a", 3);
$i = @new ArrayIterator($o); $i[] = str_repeat("b", 2); $c = clone $o; unset($o);
var_dump($c, count($ao));
"#,
    );
    assert_eq!(
        stdout,
        "object(stdClass)#4 (2) {\n  [0]=>\n  string(3) \"aaa\"\n  [1]=>\n  string(2) \"bb\"\n}\nint(2)\n"
    );
}

/// An `unserialize()` whose next class's autoloader throws makes none
/// of the `__wakeup()` calls it queued, so the throwing `__wakeup()`
/// queued before it makes no second throwable to drop: #2231's snippet,
/// `ext/standard/tests/serialize/bug64354_2.phpt`, armed here because
/// `spl_autoload_register` is this extension's and ext/standard's tests
/// cannot reach it.
#[test]
fn an_autoloader_throw_in_unserialize_keeps_no_wakeup_throwable() {
    let stdout = armed(
        r#"<?php
class A { public function __wakeup() { throw new Exception("Failed"); } }
spl_autoload_register(function($class) { throw new Exception("Failed"); });
try { var_dump(unserialize('a:2:{i:0;O:1:"A":0:{}i:1;O:1:"B":0:{}}')); } catch (Exception $e) { var_dump($e->getMessage()); }
"#,
    );
    assert_eq!(stdout, "string(6) \"Failed\"\n");
}

/// A read that throws inside `SplObjectStorage::unserialize`,
/// `SplDoublyLinkedList::unserialize` or `ArrayObject::unserialize`
/// makes none of the `__wakeup()` calls queued before it, as php's
/// `zend_call_function` refuses each at `EG(exception)`, so the
/// throwing `__wakeup()` makes no second throwable to drop, whether the
/// read throws on a typed property or in an autoloader (#2235).
#[test]
fn a_read_that_throws_in_an_spl_unserialize_makes_no_delayed_call() {
    let stdout = armed(
        r#"<?php
class A { function __wakeup() { echo "wakeup A\n"; throw new Exception("wake"); } }
class T { public int $p; }
$s = new SplObjectStorage;
try { $s->unserialize('x:i:2;O:1:"A":0:{},N;;O:1:"T":1:{s:1:"p";s:1:"x";},N;;m:a:0:{}'); } catch (Throwable $e) {}
$l = new SplDoublyLinkedList;
try { $l->unserialize('i:0;:O:1:"A":0:{}:O:1:"T":1:{s:1:"p";s:1:"x";}'); } catch (Throwable $e) {}
$o = new ArrayObject;
try { $o->unserialize('x:i:0;a:2:{i:0;O:1:"A":0:{}i:1;O:1:"T":1:{s:1:"p";s:1:"x";}};m:a:0:{}'); } catch (Throwable $e) {}
spl_autoload_register(function ($class) { throw new Exception("no $class"); });
try { $s->unserialize('x:i:2;O:1:"A":0:{},N;;O:1:"B":0:{},N;;m:a:0:{}'); } catch (Throwable $e) {}
echo "end\n";
"#,
    );
    assert_eq!(stdout, "end\n");
}
