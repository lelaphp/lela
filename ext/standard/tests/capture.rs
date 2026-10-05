//! The extension through the `Capture` sink (design §4.10, ADR-0002): a
//! fresh engine with this crate alone, never the bundle.

use php_ext::test::{Capture, Engine};

/// What the engine wrote and what ended it, for one source, under an
/// engine of this extension alone.
fn served(source: &str) -> (String, Option<String>) {
    let engine = Engine::builder()
        .extensions(&[php_ext_standard::extension()])
        .build();
    let served = engine.serve_capture(source, Capture::default());
    (
        String::from_utf8_lossy(&served.stdout).into_owned(),
        served.fatal,
    )
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
/// here (`php_ext::raising::call_stack_end`): each over a nest deeper
/// than the test harness's 2 MiB thread is the catchable `Error`, and
/// the request goes on to the next.
#[test]
fn an_unguarded_walk_deeper_than_the_thread_is_refused_at_its_floor() {
    let source = r#"<?php
$a = []; for ($i = 0; $i < 200000; $i++) { $a = ['k' => $a]; }
$b = $a;
$walks = [
    fn () => print_r($a, true),
    fn () => var_export($a, true),
    function () use ($a) { ob_start(); try { debug_zval_dump($a); } finally { ob_end_clean(); } },
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
echo 'done';
"#;
    let (stdout, fatal) = served(source);
    assert_eq!(fatal, None);
    assert_eq!(
        stdout,
        format!("{}done", "Maximum call stack size\n".repeat(9))
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
    let engine = Engine::builder()
        .extensions(&[php_ext_standard::extension()])
        .build();
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

/// #1556: argument #6's bound is the integer `ZEND_ULONG_FMT` names,
/// `PHP_TIMEOUT_ULL_MAX / 1000000`; the pinned php prints what a register
/// held there, so the witness beside `fsockopen` holds the digits
/// and this holds the number (`trace:fsockopen-timeout-bound`).
#[test]
fn fsockopen_s_timeout_bound_is_the_integer_php_s_format_names() {
    let (stdout, fatal) = served(
        "<?php try { fsockopen('127.0.0.1', 1, $c, $m, 1e20); } catch (ValueError $e) { echo $e->getMessage(), \"\\n\"; }\ntry { pfsockopen('udp://127.0.0.1', 9, $c, $m, -2.0); } catch (ValueError $e) { echo $e->getMessage(); }",
    );
    assert_eq!(fatal, None);
    assert_eq!(
        stdout,
        "fsockopen(): Argument #6 must be -1 or between 0 and 18446744073709\npfsockopen(): Argument #6 must be -1 or between 0 and 18446744073709"
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
