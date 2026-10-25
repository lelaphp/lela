//! The extension through the `Capture` sink (design §4.10, ADR-0002): a
//! fresh engine with this crate alone, never the bundle.

#![allow(
    clippy::disallowed_methods,
    reason = "a test starts its own threads (seams.toml rule fork.threads)"
)]

mod support;

use php_ext::test::Capture;

/// What the engine wrote and what ended it, for one source, under an
/// engine of this extension alone.
fn served(source: &str) -> (String, Option<String>) {
    let engine = support::engine();
    let served = engine.serve_capture(source, Capture::default());
    (served.stdout_text(), served.fatal)
}

#[test]
fn the_extension_is_loaded() {
    let (stdout, _) = served("<?php var_dump(extension_loaded('standard'));");
    assert_eq!(stdout, "bool(true)\n");
}

/// `str_repeat` reaches the engine's table through the bundle's
/// declaration, and a 3 MB string is one value it hands back whole.
#[test]
fn str_repeat_is_declared_and_builds_a_string_past_two_mebibytes() {
    let (stdout, fatal) = served("<?php echo str_repeat('ab', 2), str_repeat('x', 3000000);");
    assert_eq!(fatal, None);
    assert!(stdout.starts_with("ababx"), "{}", &stdout[..8]);
    assert_eq!(stdout.len(), 4 + 3_000_000);
}

/// php's own text, from the pinned oracle, held here without it; the
/// trace the first of these prints, arguments and all, is the witness
/// beside `str_repeat`:
///
/// ```text
/// ValueError: str_repeat(): Argument #2 ($times) must be greater than or equal to 0
/// ArgumentCountError: str_repeat() expects exactly 2 arguments, 1 given
/// ArgumentCountError: str_repeat() expects exactly 2 arguments, 3 given
/// ```
///
/// The argument's name in the `ValueError` is #175's; what is held here
/// is the class, the position and the tail.
#[test]
fn the_count_below_zero_and_the_arity_are_php_s() {
    for (source, class, text) in [
        ("<?php str_repeat('x', -1);", "ValueError", "Argument #2"),
        (
            "<?php str_repeat('x', -1);",
            "ValueError",
            "must be greater than or equal to 0",
        ),
        (
            "<?php str_repeat('x');",
            "ArgumentCountError",
            "str_repeat() expects exactly 2 arguments, 1 given",
        ),
        (
            "<?php str_repeat('x', 1, 2);",
            "ArgumentCountError",
            "str_repeat() expects exactly 2 arguments, 3 given",
        ),
    ] {
        let (_, fatal) = served(source);
        let fatal = fatal.unwrap_or_default();
        assert!(
            fatal.contains(class) && fatal.contains(text),
            "{source} came to {fatal:?}"
        );
    }
}

/// `zend_string_safe_alloc`'s guard, before a byte is asked for: php's
/// fatal for a product that overflows, with the header's 32 bytes in the
/// text as php prints them.
#[test]
fn a_product_that_overflows_is_php_s_fatal_and_no_allocation() {
    let (_, fatal) = served("<?php str_repeat('xxx', 9223372036854775807);");
    let fatal = fatal.unwrap_or_default();
    assert!(
        fatal.contains(
            "Possible integer overflow in memory allocation (3 * 9223372036854775807 + 32)"
        ),
        "{fatal:?}"
    );
}

/// The walks php leaves unguarded, which recurse in php until the
/// thread's stack runs out and fault, are held to the thread's floor
/// here (`php_ext::raising::call_stack_end`), and those php 8.5.12
/// guards ask its limit (`call_stack_limit`): each over a nest deeper
/// than the test harness's 2 MiB thread is the catchable `Error`, and
/// the request goes on to the next. `debug_zval_dump`'s rows are
/// guarded the way php 8.5.12 guards them, `nesting level too deep` in
/// place of the row and no `Error`.
#[test]
fn a_walk_deeper_than_the_thread_is_refused_at_its_limit_or_its_floor() {
    let source = r#"<?php
$a = []; for ($i = 0; $i < 200000; $i++) { $a = ['k' => $a]; }
$b = $a;
$walks = [
    fn () => print_r($a, true),
    fn () => var_export($a, true),
    function () use ($a) { $c = $a; array_walk_recursive($c, fn () => null); },
    fn () => array_merge_recursive($a, $b),
    fn () => array_replace_recursive($a, $b),
    fn () => count($a, COUNT_RECURSIVE),
    fn () => compact([$a]),
    fn () => unserialize(str_repeat('a:1:{i:0;', 200000) . 'i:0;' . str_repeat('}', 200000), ['max_depth' => 0]),
];
foreach ($walks as $walk) {
    try { $walk(); echo "whole\n"; } catch (Error $e) { echo strstr($e->getMessage(), ' of ', true), "\n"; }
}
ob_start(); debug_zval_dump($a); echo str_contains(ob_get_clean(), 'nesting level too deep') ? "nesting level too deep\n" : "whole\n";
echo 'done';
"#;
    let (stdout, fatal) = served(source);
    assert_eq!(fatal, None);
    assert_eq!(
        stdout,
        format!(
            "{}nesting level too deep\ndone",
            "Maximum call stack size\n".repeat(8)
        )
    );
}

/// `extract` holds an owner of the array it walks, as php's copies it
/// into a local ("The array might be stored in a local variable that
/// will be overwritten"): the element `arr` assigns over the variable
/// that held the array's only owner, and the elements after it are
/// still the array's. Without the owner the array was freed mid-walk,
/// `$kkkkkkkkkk` was handed the freed value's block, and the string of
/// that size allocated next was handed it again.
#[test]
fn extract_holds_the_array_it_walks_when_an_element_overwrites_its_variable() {
    let (stdout, fatal) = served(
        "<?php function f() { $k = 'kkkkk'; $v = 'vvvvv'; $z = 'ZZZZZ';\n$arr = ['arr' => 1, $k . $k => $v . $v];\necho extract($arr), ',';\n$x = $z . $z;\necho $arr, ',', $kkkkkkkkkk; }\nf();",
    );
    assert_eq!(fatal, None);
    assert_eq!(stdout, "2,1,vvvvvvvvvv");
}

/// `extract` under `EXTR_REFS` walks the array its reference holds live,
/// as php's ref helpers walk its buckets (`ZEND_HASH_MAP_FOREACH`): a
/// destructor served as a variable is overwritten that unsets a row
/// ahead leaves it unvisited, and one that rewrites the row in place
/// leaves the walk reading the new value (#2042).
#[test]
fn extract_refs_walks_the_array_live_under_a_destructor() {
    let script = |body: &str| {
        format!(
            "<?php class D {{ function __destruct() {{ {body} echo \"d,\"; }} }}\n$o = new D; $k = 'kkkkk'; $v = 'vvvvv'; $z = 'ZZZZZ';\n$arr = ['o' => 1, $k . $k => $v . $v];\necho extract($arr, EXTR_REFS), ',';\n$x = $z . $z; echo $kkkkkkkkkk ?? 'unset';"
        )
    };
    let (stdout, fatal) = served(&script("unset($GLOBALS['arr']['kkkkkkkkkk']);"));
    assert_eq!(fatal, None);
    assert_eq!(stdout, "d,1,unset");
    let (stdout, fatal) = served(&script("$GLOBALS['arr']['kkkkkkkkkk'] = 'new';"));
    assert_eq!(fatal, None);
    assert_eq!(stdout, "d,2,new");
}

/// `source` served by an engine of this extension alone with the leak
/// sensor armed where the build has one, a debug build's: what it wrote,
/// and the fatal that ended it, if any.
fn armed(source: &str) -> (String, Option<String>) {
    let served = support::engine().serve_capture(source, Capture::default());
    (served.stdout_text(), served.fatal)
}

/// `serialize` walks a nested array with the foreach walk holding
/// nothing, as `php_var_serialize_nested_data` holds nothing. An
/// element's `__serialize` that assigns over the reference holding the
/// array's only owner destroys it mid-walk, and the walk ends there: the
/// element it was lent is written, no row after it is read, and the
/// brace is closed though the count said two. The strings allocated in
/// `__serialize` are the size of the rows after it, so a walk that read
/// on would read their blocks. Without the walk's end the debug build
/// read the freed rows (#1989, #2150).
///
/// Here dax departs from php on purpose. php reads the freed table: the
/// pinned php prints `s:10:"ZZZZZZZZZZ"` for the last row, and what it
/// prints changes with the allocator; on #1989's own input it dies on a
/// 4 GB allocation. No divergence row records it:
/// `docs/divergence.toml` is keyed by a compared site, and no witness or
/// phpt compares this input. A row is added if one ever does (#1989).
#[test]
fn serialize_ends_a_nested_array_s_walk_where_a_serialize_method_destroys_it() {
    let (stdout, fatal) = armed(
        "<?php class C { function __serialize(): array { $GLOBALS['r'] = 0; $GLOBALS['x'] = $GLOBALS['z'] . $GLOBALS['z']; $GLOBALS['y'] = $GLOBALS['z'] . $GLOBALS['z']; return []; } }\n$v = 'vvvvv'; $z = 'ZZZZZ';\n$r = [new C, $v . $v];\n$outer = [&$r];\necho serialize($outer);",
    );
    assert_eq!(fatal, None);
    assert_eq!(stdout, "a:1:{i:0;a:2:{i:0;O:1:\"C\":0:{}}}");
}

/// `print_r` walks a nested array with the foreach walk holding nothing,
/// as `print_hash` holds nothing. An element's `__debugInfo` that
/// assigns over the reference holding the array's only owner destroys
/// it mid-walk, and the walk ends there: the element it was lent is
/// printed, no row after it is read, and the `)` is written (#2044,
/// #2150).
///
/// No divergence row records the walk's end: no witness or phpt
/// compares this input.
#[test]
fn print_r_ends_a_nested_array_s_walk_where_a_debug_info_method_destroys_it() {
    let (stdout, fatal) = armed(
        "<?php class C { function __debugInfo() { $GLOBALS['r'] = 0; $GLOBALS['x'] = $GLOBALS['z'] . $GLOBALS['z']; return []; } }\n$v = 'vvvvv'; $z = 'ZZZZZ';\n$r = [new C, $v . $v];\n$outer = [&$r];\nprint_r($outer);",
    );
    assert_eq!(fatal, None);
    assert_eq!(
        stdout,
        "Array\n(\n    [0] => Array\n        (\n            [0] => C Object\n                (\n                )\n\n        )\n\n)\n"
    );
}

/// `print_r` over an object whose property holds a reference to a
/// nested array: an element's `__debugInfo` that assigns over the
/// variable the reference names destroys the array mid-walk, and the
/// foreach walk, holding nothing as `print_hash` holds nothing, ends
/// there, so the strings allocated after are not read as its rows. The
/// debug build read the freed rows and faulted before the foreach walk
/// (#2105, #2150).
///
/// No divergence row records the walk's end: no witness or phpt
/// compares this input.
#[test]
fn print_r_ends_an_object_s_nested_array_where_a_debug_info_method_destroys_it() {
    let (stdout, fatal) = armed(
        "<?php class C { function __debugInfo() { $GLOBALS['r'] = 0; $GLOBALS['x'] = $GLOBALS['z'] . $GLOBALS['z']; return []; } }\nclass O { public $p; }\n$v = 'vvvvv'; $z = 'ZZZZZ';\n$r = [new C, $v . $v]; $o = new O; $o->p = &$r;\nprint_r($o);",
    );
    assert_eq!(fatal, None);
    assert_eq!(
        stdout,
        "O Object\n(\n    [p] => Array\n        (\n            [0] => C Object\n                (\n                )\n\n        )\n\n)\n"
    );
}

/// `print_r` over an object whose property alone holds a nested array:
/// the walk over the object's table pins each row for its step (#2197),
/// and a pin keeps only a row's bytes (#2230), so an element's
/// `__debugInfo` that assigns over the property destroys the array
/// there, as php destroys it, and the walk over it ends at its
/// destruction, its later rows unprinted.
///
/// No divergence row records the walk's end: no witness or phpt
/// compares this input.
#[test]
fn print_r_ends_an_object_s_nested_array_its_row_held_where_it_is_destroyed() {
    let (stdout, fatal) = armed(
        "<?php class C { function __debugInfo() { $GLOBALS['o']->p = 0; $GLOBALS['x'] = $GLOBALS['z'] . $GLOBALS['z']; return []; } }\nclass O { public $p; }\n$v = 'vvvvv'; $z = 'ZZZZZ';\n$o = new O; $o->p = [new C, $v . $v];\nprint_r($o);",
    );
    assert_eq!(fatal, None);
    assert_eq!(
        stdout,
        "O Object\n(\n    [p] => Array\n        (\n            [0] => C Object\n                (\n                )\n\n        )\n\n)\n"
    );
}

/// What `conn` sent until it went quiet for a moment.
fn sent(conn: &mut std::net::TcpStream) -> Vec<u8> {
    use std::io::Read;
    conn.set_read_timeout(Some(std::time::Duration::from_millis(200)))
        .unwrap_or_default();
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 64];
    while let Ok(n) = conn.read(&mut chunk) {
        if n == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..n]);
    }
    bytes
}

/// The connections a listener was handed since it was last asked, each
/// kept open so a pooled socket stays alive, with what it sent.
fn accepted(listener: &std::net::TcpListener) -> Vec<(std::net::TcpStream, Vec<u8>)> {
    listener.set_nonblocking(true).unwrap_or_default();
    let mut conns = Vec::new();
    while let Ok((mut conn, _)) = listener.accept() {
        conn.set_nonblocking(false).unwrap_or_default();
        let bytes = sent(&mut conn);
        conns.push((conn, bytes));
    }
    conns
}

/// The persistent pool (design §4.10, #1172) across requests: two
/// requests served in turn on one engine share one connection, as two
/// requests of one php-fpm worker do, the second taking what the first
/// gave back; two served at once hold two, as two workers do, since an
/// entry is lent to one request at a time.
#[test]
fn a_persistent_socket_is_lent_to_one_request_at_a_time() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap_or_else(|e| panic!("{e}"));
    let port = listener.local_addr().map_or(0, |a| a.port());
    let engine = support::engine();
    let source = |tag: &str| {
        format!("<?php $s = pfsockopen('127.0.0.1', {port}); fwrite($s, '{tag}'); usleep(300000);")
    };
    for tag in ["a", "b"] {
        let served = engine.serve_capture(&source(tag), Capture::default());
        assert_eq!(served.fatal, None);
    }
    let mut first = accepted(&listener);
    assert_eq!(first.len(), 1);
    assert_eq!(first[0].1, b"ab");
    std::thread::scope(|scope| {
        for tag in ["c", "d"] {
            let (engine, source) = (&engine, source(tag));
            scope.spawn(move || engine.serve_capture(&source, Capture::default()));
        }
    });
    // One of the two took the idle connection and the other opened its
    // own.
    let second = accepted(&listener);
    assert_eq!(second.len(), 1);
    let mut both = vec![sent(&mut first[0].0), second[0].1.clone()];
    both.sort();
    assert_eq!(both, vec![b"c".to_vec(), b"d".to_vec()]);
}

/// `Zend Multibyte Support` is `disabled` where no extension set the
/// encoding functions, as php's is without mbstring
/// (`Z/ext/standard/info.c:872-881`); the witness beside `general`
/// holds `provided by mbstring` in the bundle.
#[test]
fn zend_multibyte_support_is_disabled_without_a_provider() {
    let (stdout, fatal) = served("<?php phpinfo(INFO_GENERAL);");
    assert_eq!(fatal, None);
    assert!(
        stdout
            .lines()
            .any(|line| line == "Zend Multibyte Support => disabled"),
        "{stdout}"
    );
}

/// #1556: argument #5's bound is the integer php-8.5.12 prints,
/// `PHP_TIMEOUT_ULL_MAX / 1000000` (`Z/ext/standard/fsock.c:110`), on
/// every platform, as the witness beside `fsockopen` holds it at the
/// oracle's.
#[test]
fn fsockopen_s_timeout_bound_is_the_integer_php_s_format_names() {
    let (stdout, fatal) = served(
        "<?php try { fsockopen('127.0.0.1', 1, $c, $m, 1e20); } catch (ValueError $e) { echo $e->getMessage(), \"\\n\"; }\ntry { pfsockopen('udp://127.0.0.1', 9, $c, $m, -2.0); } catch (ValueError $e) { echo $e->getMessage(); }",
    );
    assert_eq!(fatal, None);
    assert_eq!(
        stdout,
        "fsockopen(): Argument #5 ($timeout) must be -1 or between 0 and 18446744073709\npfsockopen(): Argument #5 ($timeout) must be -1 or between 0 and 18446744073709"
    );
}

/// #1986: `array_walk` holds no key of an element it has yet to visit.
/// The callback unsets the elements ahead, which frees their runtime
/// keys, and then builds keys of the same size in the reverse order, so
/// each takes the slot a freed key had. php's iterator walks the
/// buckets, the new ones in the order they were appended; a walk that
/// kept the freed keys read them as the new ones, in the old order.
#[test]
fn array_walk_holds_no_key_of_an_element_the_callback_unset() {
    let (stdout, fatal) = served(
        r#"<?php
$a = [];
for ($i = 0; $i < 4; $i++) { $a["kkk" . $i] = $i; }
array_walk($a, function ($v, $k) use (&$a) {
    echo $k, " ";
    if ($v === 0) {
        unset($a["kkk1"], $a["kkk2"], $a["kkk3"]);
        for ($n = 3; $n >= 1; $n--) { $a["ZZZ" . $n] = $n; }
    }
});
echo implode(",", array_keys($a));"#,
    );
    assert_eq!(fatal, None);
    assert_eq!(stdout, "kkk0 ZZZ3 ZZZ2 ZZZ1 kkk0,ZZZ3,ZZZ2,ZZZ1");
}

/// #1987: `array_walk` over an object holds no name of a property it has
/// yet to visit. The callback unsets the dynamic properties ahead, which
/// frees their runtime-built names, and then writes properties under
/// names of the same size in the reverse order, so each takes the slot a
/// freed name had; php hands the callback the new properties in the
/// order they were written, each with its own name and value.
#[test]
fn array_walk_holds_no_name_of_a_property_the_callback_unset() {
    let (stdout, fatal) = served(
        r#"<?php
$o = new stdClass;
for ($i = 0; $i < 4; $i++) { $name = "ppp" . $i; $o->$name = $i; }
array_walk($o, function ($v, $k) use ($o) {
    echo "$k=$v ";
    if ($v === 0) {
        unset($o->ppp1, $o->ppp2, $o->ppp3);
        for ($n = 3; $n >= 1; $n--) { $name = "QQQ" . $n; $o->$name = $n * 10; }
    }
});"#,
    );
    assert_eq!(fatal, None);
    assert_eq!(stdout, "ppp0=0 QQQ3=30 QQQ2=20 QQQ1=10 ");
}

/// #1987: a callback that lets go of the object the walk is over
/// destroys it there, and the walk reads the variable again and raises
/// php's `TypeError`, never reading the object's freed table.
#[test]
fn array_walk_over_an_object_the_callback_let_go_of_is_php_s_type_error() {
    let (stdout, fatal) = served(
        r#"<?php
class D { public $a = 1; public $b = 2; function __destruct() { echo "dtor "; } }
$o = new D;
try { array_walk($o, function ($v, $k) { global $o; echo "$k "; $o = null; echo "after "; }); } catch (TypeError $e) { echo $e->getMessage(); }"#,
    );
    assert_eq!(fatal, None);
    assert_eq!(
        stdout,
        "a dtor after Iterated value is no longer an array or object"
    );
}
