//! Array literals: constant keys, `init_array` and its elements, the
//! spread, the long spelling, and what php refuses in an array.

use super::*;

/// `zend_try_ct_eval_array`'s keys: a decimal string is its integer,
/// a whole double is truncated, `false` and `true` are 0 and 1, a
/// repeated key keeps its first place with the last value, and the
/// keyless element takes the next index past the largest so far, as
/// `nNextFreeElement` has it since php 8.3 for a negative one too.
#[test]
fn a_constant_array_s_keys_are_the_ones_php_writes() {
    let (unit, _strings) = unit_and_strings(
        b"<?php echo ['a' => 1, '1' => 2, 1.0 => 3, true => 4, -5 => 6, 7, '08' => 8, '-0' => 9, false => 10, -7 => 11, 12];",
    );
    let array = unit.main.literals[0].as_arr().map(|a| {
        a.iter()
            .map(|(k, v)| {
                let key = match k {
                    php_heap::Key::Long(n) => n.to_string(),
                    php_heap::Key::Str(s) => String::from_utf8_lossy(s.bytes()).into_owned(),
                };
                (key, v.as_long().unwrap_or(-1))
            })
            .collect::<Vec<_>>()
    });
    assert_eq!(
        array,
        Some(vec![
            ("a".to_owned(), 1),
            ("1".to_owned(), 4),
            ("-5".to_owned(), 6),
            ("2".to_owned(), 7),
            ("08".to_owned(), 8),
            ("-0".to_owned(), 9),
            ("0".to_owned(), 10),
            ("-7".to_owned(), 11),
            ("3".to_owned(), 12),
        ])
    );
}

#[test]
fn what_php_s_compiler_refuses_in_a_foreach_and_an_array_is_refused_in_its_words() {
    for (source, message) in [
        (
            &b"<?php\nforeach ($a as &$k => $v) {}\n"[..],
            "Key element cannot be a reference",
        ),
        (
            b"<?php\nforeach ($a as $this) {}\n",
            "Cannot re-assign $this",
        ),
        (
            b"<?php\nforeach ($a as $this => $v) {}\n",
            "Cannot re-assign $this",
        ),
        (
            b"<?php\nforeach ([1, 2] as $v) { break 2; }\n",
            "Cannot 'break' 2 levels",
        ),
        (
            b"<?php\nforeach (strlen($a) as &$v) {}\n",
            "Cannot use result of built-in function in write context",
        ),
        (b"<?php\necho [[] => 2];\n", "Illegal offset type"),
    ] {
        let error = refused(source).unwrap_or_else(|| {
            Refused {
                level: Level::Parse,
                message: String::from("compiled").into(),
                line: 0,
                raised: Vec::new(),
                thrown: false,
                stage: crate::Stage::Compiling,
                declared: None,
                keys_made: 0,
            }
            .into()
        });
        assert_eq!(error.level, Level::CompileError, "{message}");
        assert_eq!(error.message, message);
        assert_eq!(error.line, 2, "{message}");
    }
    // `[[] => 2]` under a variable is not php's compile error: the
    // first pass of `zend_try_ct_eval_array` finds the variable and
    // leaves the literal to the request, where it is a `TypeError`.
    assert_eq!(refused(b"<?php echo [[] => 2, $a];"), None);
}

/// `zend_compile_array` against the oracle's dump over the programs
/// of #304: an array expression with a variable in it is `INIT_ARRAY`
/// over the first element and `ADD_ARRAY_ELEMENT` over each after,
/// the temporary numbered after the first element's own and shared
/// by every opline, the count above `ZEND_ARRAY_SIZE_SHIFT`, and
/// the live range running from the `INIT_ARRAY` to the opline that
/// reads the temporary.
#[test]
fn an_array_with_a_variable_is_init_array_and_add_array_element_as_php_emits_them() {
    let (unit, _strings) = unit_and_strings(b"<?php $a = ['x', 'y']; $b = [1, $a, 'k' => 2];");
    expect![[r#"

        $_main:
             ; (lines=6, args=0, vars=2, tmps=3)
             ; (before optimizer)
             ; :1-1
             ; return  [] RANGE[0..0]
        0000 ASSIGN CV0($a) array(...)
        0001 T3 = INIT_ARRAY 3 int(1) NEXT
        0002 T3 = ADD_ARRAY_ELEMENT CV0($a) NEXT
        0003 T3 = ADD_ARRAY_ELEMENT int(2) string("k")
        0004 ASSIGN CV1($b) T3
        0005 RETURN int(1)
        LIVE RANGES:
             3: 0002 - 0004 (tmp/var)
    "#]]
    .assert_eq(&unit.dump(&[]).text());
    assert_eq!(
        unit.main.oplines.get(1).map(|op| op.extended_value),
        Some((3 << 2) | 2),
        "three elements, and ZEND_ARRAY_NOT_PACKED for the string key"
    );
    expect![[r#"
        0000 INIT_FCALL 2 112 string("var_dump")
        0001 SEND_VAL array(...) 1
        0002 T1 = INIT_ARRAY 3 int(1) NEXT
        0003 T2 = ASSIGN CV0($a) int(2)
        0004 T1 = ADD_ARRAY_ELEMENT T2 NEXT
        0005 T1 = ADD_ARRAY_ELEMENT int(3) string("k")
        0006 SEND_VAL T1 2
        0007 DO_ICALL
        0008 RETURN int(1)
        LIVE RANGES:
             1: 0003 - 0006 (tmp/var)
    "#]]
    .assert_eq(&oplines_of(
        b"<?php var_dump(['x', 'y'], [1, $a = 2, 'k' => 3]);",
    ));
}

/// `zend_compile_array`'s `...` and `&` arms and `zend_compile_args`'
/// unpack arm against the oracle's dump: a `...` first is an
/// `INIT_ARRAY` with no operand and no count, the string key still
/// marks it `ZEND_ARRAY_NOT_PACKED`, a `&` element is the write fetch
/// and `ZEND_ARRAY_ELEMENT_REF`, and an argument after an unpacked
/// one is sent by name for a callee nothing is known of, under a
/// `CHECK_UNDEF_ARGS`. A constant array spread into a constant array
/// folds.
#[test]
fn a_spread_a_reference_element_and_an_unpacked_argument_are_emitted_as_php_emits_them() {
    expect![[r#"
        0000 T3 = INIT_ARRAY 0 NEXT
        0001 T3 = ADD_ARRAY_UNPACK CV1($x)
        0002 V4 = FETCH_DIM_W CV1($x) int(0)
        0003 T3 = ADD_ARRAY_ELEMENT (ref) V4 string("k")
        0004 T3 = ADD_ARRAY_ELEMENT (ref) CV2($w) NEXT
        0005 ASSIGN CV0($z) T3
        0006 INIT_FCALL_BY_NAME 1 string("f")
        0007 SEND_VAL_EX int(1) 1
        0008 SEND_UNPACK CV1($x)
        0009 SEND_VAL_EX int(2) string("c")
        0010 CHECK_UNDEF_ARGS
        0011 DO_FCALL_BY_NAME
        0012 RETURN int(1)
        LIVE RANGES:
             3: 0001 - 0005 (tmp/var)
    "#]]
    .assert_eq(&oplines_of(
        b"<?php $z = [...$x, 'k' => &$x[0], &$w]; f(1, ...$x, c: 2);",
    ));
    expect![[r#"
        0000 ASSIGN CV0($q) array(...)
        0001 RETURN int(1)
    "#]]
    .assert_eq(&oplines_of(b"<?php $q = [...[1, 2], 3, ...['k' => 4]];"));
    assert_eq!(
        refused_message(b"<?php\nf(...$a, 1);\n").as_deref(),
        Some("Cannot use positional argument after argument unpacking")
    );
}

/// `T_ARRAY '(' array_pair_list ')'`
/// (`Z/Zend/zend_language_parser.y:1491`) is the same
/// `ZEND_AST_ARRAY` over the same `array_pair_list` as `[…]`, with
/// `ZEND_ARRAY_SYNTAX_LONG` in `attr`, which `zend_compile_array`
/// reads only to refuse a `list()` where an array is meant: the
/// long spelling emits what the short one above emits, opline for
/// opline, and the word is matched whatever its case.
#[test]
fn the_long_array_spelling_emits_what_the_short_one_emits() {
    let short = oplines_of(b"<?php var_dump(['x', 'y'], [1, $a = 2, 'k' => 3]);");
    assert_eq!(
        oplines_of(b"<?php var_dump(array('x', 'y'), array(1, $a = 2, 'k' => 3));"),
        short
    );
    assert_eq!(
        oplines_of(b"<?php var_dump(ARRAY('x', 'y'), Array(1, $a = 2, 'k' => 3));"),
        short
    );
    assert_eq!(
        oplines_of(b"<?php $x = array(); $y = array(1, 'k' => 2);"),
        oplines_of(b"<?php $x = []; $y = [1, 'k' => 2];")
    );
}

/// The rest of `zend_compile_array` against the oracle's dump: what
/// `zend_try_ct_eval_array` leaves to the request, a `null` key, a
/// float key that is no whole number, and a next index past
/// `ZEND_LONG_MAX`, is emitted with the key as written, since the
/// handler warns over it; a constant string key that is a decimal is
/// the long `zend_handle_numeric_op` makes of it, and one that is
/// not sets `ZEND_ARRAY_NOT_PACKED` wherever it stands; a variable
/// key is compiled before the value and sets nothing; and an array
/// expression nothing reads is freed after its last element.
#[test]
fn the_keys_of_an_array_with_a_variable_are_emitted_as_php_emits_them() {
    let (unit, _strings) = unit_and_strings(
        b"<?php $k = '2'; $x = [1, $k, '3' => 4, 'a' => 5, 1.5 => 6, null => 7, true => 8]; $y = [$k => 1, 5 => 2, 'b' => 3]; [9223372036854775807 => 1, 2];",
    );
    expect![[r#"

        $_main:
             ; (lines=17, args=0, vars=3, tmps=6)
             ; (before optimizer)
             ; :1-1
             ; return  [] RANGE[0..0]
        0000 ASSIGN CV0($k) string("2")
        0001 T4 = INIT_ARRAY 7 int(1) NEXT
        0002 T4 = ADD_ARRAY_ELEMENT CV0($k) NEXT
        0003 T4 = ADD_ARRAY_ELEMENT int(4) int(3)
        0004 T4 = ADD_ARRAY_ELEMENT int(5) string("a")
        0005 T4 = ADD_ARRAY_ELEMENT int(6) float(1.5)
        0006 T4 = ADD_ARRAY_ELEMENT int(7) null
        0007 T4 = ADD_ARRAY_ELEMENT int(8) bool(true)
        0008 ASSIGN CV1($x) T4
        0009 T6 = INIT_ARRAY 3 int(1) CV0($k)
        0010 T6 = ADD_ARRAY_ELEMENT int(2) int(5)
        0011 T6 = ADD_ARRAY_ELEMENT int(3) string("b")
        0012 ASSIGN CV2($y) T6
        0013 T8 = INIT_ARRAY 2 (packed) int(1) int(9223372036854775807)
        0014 T8 = ADD_ARRAY_ELEMENT int(2) NEXT
        0015 FREE T8
        0016 RETURN int(1)
        LIVE RANGES:
             4: 0002 - 0008 (tmp/var)
             6: 0010 - 0012 (tmp/var)
             8: 0014 - 0015 (tmp/var)
    "#]]
    .assert_eq(&unit.dump(&[]).text());
    let flags: Vec<u32> = [1usize, 9, 13]
        .iter()
        .filter_map(|&at| unit.main.oplines.get(at))
        .map(|op| op.extended_value & 3)
        .collect();
    assert_eq!(
        flags,
        vec![2, 2, 0],
        "ZEND_ARRAY_NOT_PACKED on the two with a constant string key"
    );
}
