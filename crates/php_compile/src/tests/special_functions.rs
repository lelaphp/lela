//! The special functions: the names php folds, and each row of
//! `special::TABLE` compiled to the opcode php emits.

use super::*;

/// The oracle's dump under the pin's `dump` regime of #162's third
/// program, `echo 1 + strlen('a');`:
///
/// ```text
/// 0000 ECHO int(2)
/// 0001 RETURN int(1)
/// ```
///
/// `zend_try_compile_special_func` reaches `zend_compile_func_strlen`,
/// which leaves the constant string's length where the call stood,
/// and `zend_try_ct_eval_binary_op` folds the sum over it.
#[test]
fn strlen_over_a_constant_string_is_its_length_and_the_sum_folds_over_it() {
    let (unit, _strings) = unit_and_strings(b"<?php echo 1 + strlen('a');");
    assert_eq!(
        unit.dump(&[]).text(),
        main_dump(2, 0, 0, "0000 ECHO int(2)\n0001 RETURN int(1)\n"),
        "one ECHO of a constant, no INIT_FCALL and no ADD"
    );
    assert_eq!(unit.main.literals[0].as_long(), Some(2));
}

/// The four rows of php's table that fold to a constant with no
/// opcode, each held to what the oracle dumps: `strlen` over a
/// constant string, `chr` over a long written 0 to 255, `ord` over a
/// one-byte string, `defined` over a name the compiler substitutes,
/// and `.` over two literals counting as written, since php's parser
/// folds it before the compiler looks (`zend_ast_create_concat_op`).
#[test]
fn the_four_special_names_fold_to_the_constant_php_dumps() {
    for (source, long) in [
        (&b"<?php echo strlen('a' . 'b');"[..], 2),
        (b"<?php echo strlen('a' . 1);", 2),
        (b"<?php echo strlen('');", 0),
        (b"<?php echo \\strlen('ab');", 2),
        (b"<?php echo STRLEN('ab');", 2),
        (b"<?php echo strlen(chr(65));", 1),
        (b"<?php echo ord('a');", 97),
        (b"<?php echo ord('a' . '');", 97),
    ] {
        assert_eq!(
            folded(source).and_then(php_heap::Zval::as_long),
            Some(long),
            "{}",
            String::from_utf8_lossy(source)
        );
    }
    for (source, text) in [
        (&b"<?php echo chr(65);"[..], &b"A"[..]),
        (b"<?php echo chr(0);", b"\0"),
        (b"<?php echo chr(255);", b"\xff"),
    ] {
        assert_eq!(
            echoed_string(source),
            text,
            "{}",
            String::from_utf8_lossy(source)
        );
    }
    for source in [
        &b"<?php echo defined('INF');"[..],
        b"<?php echo defined('NAN');",
        b"<?php echo defined('true');",
        b"<?php echo defined('TRUE');",
        b"<?php echo defined('null');",
        b"<?php echo defined('IN' . 'F');",
    ] {
        assert_eq!(
            folded(source).map(php_heap::Zval::kind),
            Some(php_heap::ZvalType::True),
            "{}",
            String::from_utf8_lossy(source)
        );
    }
}

/// Whether the source compiles to the ordinary call of `name`: an
/// `INIT_FCALL` first, over that lowered name, and a `DO_ICALL`.
fn ordinary_call(source: &[u8], name: &[u8]) -> bool {
    let (unit, _strings) = unit_and_strings(source);
    let oplines = &unit.main.oplines;
    oplines.first().map(|op| op.opcode) == Some(Opcode::InitFcall)
        && literal_text(&unit, 0).as_deref() == Some(name)
        && oplines.iter().any(|op| op.opcode == Opcode::DoIcall)
}

/// What php leaves to the call itself is the ordinary call here,
/// and never a constant or an opcode it is not: a `chr` whose long
/// is not written as such or is outside the byte, an `ord` over any
/// other string, a `defined` over a namespaced or a class name or
/// over no written value, a wrong count of arguments, an unpacked or
/// a named argument, and `func_num_args` and `array_slice` over
/// `func_get_args` outside a function.
#[test]
fn a_special_name_php_leaves_to_the_call_is_the_ordinary_call() {
    for (source, name) in [
        (&b"<?php echo strlen();"[..], &b"strlen"[..]),
        (b"<?php echo strlen('a', 'b');", b"strlen"),
        (b"<?php echo chr(1 + 64);", b"chr"),
        (b"<?php echo chr(-0);", b"chr"),
        (b"<?php echo chr(-1);", b"chr"),
        (b"<?php echo chr(256);", b"chr"),
        (b"<?php echo chr('65');", b"chr"),
        (b"<?php echo ord('ab');", b"ord"),
        (b"<?php echo ord('');", b"ord"),
        (b"<?php echo ord(65);", b"ord"),
        (b"<?php echo defined('A\\\\B');", b"defined"),
        (b"<?php echo defined('A::B');", b"defined"),
        (b"<?php echo defined(null);", b"defined"),
        (b"<?php echo defined(INF);", b"defined"),
        (b"<?php echo defined();", b"defined"),
        (b"<?php echo count($a, 1);", b"count"),
        (b"<?php echo is_int();", b"is_int"),
        (b"<?php echo is_int($a, $b);", b"is_int"),
        (b"<?php echo intval($a, 8);", b"intval"),
        (b"<?php echo intval(value: $a);", b"intval"),
        (b"<?php echo boolval(...$a);", b"boolval"),
        (b"<?php echo gettype();", b"gettype"),
        (b"<?php echo get_class($a, $b);", b"get_class"),
        (b"<?php echo get_called_class(1);", b"get_called_class"),
        (b"<?php echo func_num_args();", b"func_num_args"),
        (
            b"<?php echo array_slice(func_get_args(), 1);",
            b"array_slice",
        ),
        (b"<?php echo array_key_exists($k);", b"array_key_exists"),
    ] {
        assert!(
            ordinary_call(source, name),
            "{}",
            String::from_utf8_lossy(source)
        );
    }
}

/// `zend_compile_call` reads `fbc` off `CG(function_table)` before
/// `zend_try_compile_special_func` reads its table, so under a bundle
/// that declares none of the names, which `Options::default` is,
/// every one is the call by name, the four constant shapes included.
#[test]
fn a_special_name_the_bundle_has_not_is_the_call_by_name() {
    for (source, name) in [
        (&b"<?php echo strlen($a);"[..], &b"strlen"[..]),
        (b"<?php echo strlen('a');", b"strlen"),
        (b"<?php echo chr(65);", b"chr"),
        (b"<?php echo ord('a');", b"ord"),
        (b"<?php echo defined('INF');", b"defined"),
        (b"<?php echo defined('NOPE');", b"defined"),
        (b"<?php echo is_int($a);", b"is_int"),
        (b"<?php echo count($a);", b"count"),
    ] {
        let mut strings = php_heap::Strings::new();
        let unit =
            compile_parsed(source, &Options::default(), &mut strings).expect("the source compiles");
        let oplines = &unit.main.oplines;
        assert!(
            oplines.first().map(|op| op.opcode) == Some(Opcode::InitFcallByName)
                && literal_text(&unit, 0).as_deref() == Some(name)
                && oplines.iter().any(|op| op.opcode == Opcode::DoFcallByName),
            "{}",
            String::from_utf8_lossy(source)
        );
    }
}

/// The oracle's dump under the pin's `dump` regime of #219's program
/// and of its kin: `STRLEN` over the CV, over a constant
/// that is not a string and over what a `CONCAT` left, `COUNT` for
/// `count` and `sizeof` alike, `TYPE_CHECK` with the mask each
/// `is_*` writes, and `DEFINED` over the name as a string with its
/// cache slot, `1.5` written as `"1.5"`; and the name in any case,
/// with or without a leading separator.
#[test]
fn the_opcode_shapes_of_the_special_table_are_php_s() {
    assert_eq!(
        dump_of(b"<?php echo strlen($a), count($a), is_int($a), defined('NOPE');"),
        main_dump(
            9,
            1,
            4,
            "0000 T1 = STRLEN CV0($a)\n\
             0001 ECHO T1\n\
             0002 T2 = COUNT CV0($a)\n\
             0003 ECHO T2\n\
             0004 T3 = TYPE_CHECK (long) CV0($a)\n\
             0005 ECHO T3\n\
             0006 T4 = DEFINED string(\"NOPE\")\n\
             0007 ECHO T4\n\
             0008 RETURN int(1)\n"
        )
    );
    assert_eq!(
        dump_of(b"<?php echo strlen(1), strlen(INF), strlen(null), strlen($a . $b), STRLEN('ab');"),
        main_dump(
            11,
            2,
            5,
            "0000 T2 = STRLEN int(1)\n\
             0001 ECHO T2\n\
             0002 T3 = STRLEN float(inf)\n\
             0003 ECHO T3\n\
             0004 T4 = STRLEN null\n\
             0005 ECHO T4\n\
             0006 T5 = CONCAT CV0($a) CV1($b)\n\
             0007 T6 = STRLEN T5\n\
             0008 ECHO T6\n\
             0009 ECHO int(2)\n\
             0010 RETURN int(1)\n"
        )
    );
    assert_eq!(
        dump_of(b"<?php echo defined('inf'), defined(1.5), defined('NOPE'), $x = defined('NOPE');"),
        main_dump(
            10,
            1,
            5,
            "0000 T1 = DEFINED string(\"inf\")\n\
             0001 ECHO T1\n\
             0002 T2 = DEFINED string(\"1.5\")\n\
             0003 ECHO T2\n\
             0004 T3 = DEFINED string(\"NOPE\")\n\
             0005 ECHO T3\n\
             0006 T4 = DEFINED string(\"NOPE\")\n\
             0007 T5 = ASSIGN CV0($x) T4\n\
             0008 ECHO T5\n\
             0009 RETURN int(1)\n"
        )
    );
    let (unit, _strings) = unit_and_strings(b"<?php echo sizeof($a), defined('A'), defined('B');");
    let extended: Vec<_> = unit
        .main
        .oplines
        .iter()
        .filter(|op| matches!(op.opcode, Opcode::Count | Opcode::Defined))
        .map(|op| (op.opcode, op.extended_value))
        .collect();
    assert_eq!(
        extended,
        [
            (Opcode::Count, 1),
            (Opcode::Defined, 0),
            (Opcode::Defined, 8)
        ],
        "sizeof's bit, and one cache slot per DEFINED, eight bytes each"
    );
    assert_eq!(unit.main.cache_size, 16);
}

/// The oracle's dump of every `is_*` row and of `count` and `sizeof`
/// over a constant and a CV: `TYPE_CHECK` with the mask each name
/// writes, `is_bool` two bits and `is_scalar` five, over a constant
/// where one is written, and `COUNT` the same for both names.
#[test]
fn the_type_check_and_count_rows_are_php_s_masks() {
    assert_eq!(
        dump_of(
            b"<?php echo sizeof($a), is_scalar($a), is_bool($a), is_int(1), count(\"x\"), is_null($u), \\is_integer($a), Is_Float($a), is_double($a), is_string($a), is_array($a), is_object($a), is_resource($a), is_long($a);"
        ),
        main_dump(
            29,
            2,
            14,
            "0000 T2 = COUNT CV0($a)\n\
             0001 ECHO T2\n\
             0002 T3 = TYPE_CHECK TYPE [bool, long, double, string] CV0($a)\n\
             0003 ECHO T3\n\
             0004 T4 = TYPE_CHECK (bool) CV0($a)\n\
             0005 ECHO T4\n\
             0006 T5 = TYPE_CHECK (long) int(1)\n\
             0007 ECHO T5\n\
             0008 T6 = COUNT string(\"x\")\n\
             0009 ECHO T6\n\
             0010 T7 = TYPE_CHECK (null) CV1($u)\n\
             0011 ECHO T7\n\
             0012 T8 = TYPE_CHECK (long) CV0($a)\n\
             0013 ECHO T8\n\
             0014 T9 = TYPE_CHECK (double) CV0($a)\n\
             0015 ECHO T9\n\
             0016 T10 = TYPE_CHECK (double) CV0($a)\n\
             0017 ECHO T10\n\
             0018 T11 = TYPE_CHECK (string) CV0($a)\n\
             0019 ECHO T11\n\
             0020 T12 = TYPE_CHECK (array) CV0($a)\n\
             0021 ECHO T12\n\
             0022 T13 = TYPE_CHECK (object) CV0($a)\n\
             0023 ECHO T13\n\
             0024 T14 = TYPE_CHECK (resource) CV0($a)\n\
             0025 ECHO T14\n\
             0026 T15 = TYPE_CHECK (long) CV0($a)\n\
             0027 ECHO T15\n\
             0028 RETURN int(1)\n"
        )
    );
}

/// `zend_compile_func_cast` as the oracle at 8.5.9 dumps it (#1251):
/// `BOOL` for `boolval`, `CAST` with its type for `intval`,
/// `floatval`, `doubleval` and `strval`, a fully qualified name
/// alike; a second argument, an unpacked one or none is the call.
#[test]
fn the_cast_rows_are_bool_and_cast() {
    assert_eq!(
        dump_of(
            b"<?php echo boolval($a), intval($a), floatval($a), doubleval($a), strval($a), intval($a, 8), boolval(...$a), \\strval(1.5), strval();"
        ),
        main_dump(
            26,
            1,
            9,
            "0000 T1 = BOOL CV0($a)\n\
             0001 ECHO T1\n\
             0002 T2 = CAST (long) CV0($a)\n\
             0003 ECHO T2\n\
             0004 T3 = CAST (double) CV0($a)\n\
             0005 ECHO T3\n\
             0006 T4 = CAST (double) CV0($a)\n\
             0007 ECHO T4\n\
             0008 T5 = CAST (string) CV0($a)\n\
             0009 ECHO T5\n\
             0010 INIT_FCALL 2 112 string(\"intval\")\n\
             0011 SEND_VAR CV0($a) 1\n\
             0012 SEND_VAL int(8) 2\n\
             0013 V6 = DO_ICALL\n\
             0014 ECHO V6\n\
             0015 INIT_FCALL 0 80 string(\"boolval\")\n\
             0016 SEND_UNPACK CV0($a)\n\
             0017 CHECK_UNDEF_ARGS\n\
             0018 V7 = DO_ICALL\n\
             0019 ECHO V7\n\
             0020 T8 = CAST (string) float(1.5)\n\
             0021 ECHO T8\n\
             0022 INIT_FCALL 0 80 string(\"strval\")\n\
             0023 V9 = DO_ICALL\n\
             0024 ECHO V9\n\
             0025 RETURN int(1)\n"
        )
    );
}

/// `zend_compile_cast` folds nothing, so the unary operators over a
/// cast fold nothing either and `-(float)"0"` is the `CAST` and the
/// `MUL` over it (#1251, `Zend/tests/bug52355.phpt`), while an array
/// of constants, which `zend_eval_const_expr` folds, folds the cast
/// inside it; as the oracle at 8.5.9 dumps it.
#[test]
fn a_cast_under_a_unary_operator_is_its_opline() {
    assert_eq!(
        dump_of(
            b"<?php var_dump(-(float)\"0\", +(float)\"-0\", !(bool)\"1\", ~(int)\"1\", -(int)1.5, (int)\"1\" + 1, [(int)\"2\"]);"
        ),
        main_dump(
            22,
            0,
            13,
            "0000 INIT_FCALL 7 192 string(\"var_dump\")\n\
             0001 T0 = CAST (double) string(\"0\")\n\
             0002 T1 = MUL T0 int(-1)\n\
             0003 SEND_VAL T1 1\n\
             0004 T2 = CAST (double) string(\"-0\")\n\
             0005 T3 = MUL T2 int(1)\n\
             0006 SEND_VAL T3 2\n\
             0007 T4 = BOOL string(\"1\")\n\
             0008 T5 = BOOL_NOT T4\n\
             0009 SEND_VAL T5 3\n\
             0010 T6 = CAST (long) string(\"1\")\n\
             0011 T7 = BW_NOT T6\n\
             0012 SEND_VAL T7 4\n\
             0013 T8 = CAST (long) float(1.5)\n\
             0014 T9 = MUL T8 int(-1)\n\
             0015 SEND_VAL T9 5\n\
             0016 T10 = CAST (long) string(\"1\")\n\
             0017 T11 = ADD T10 int(1)\n\
             0018 SEND_VAL T11 6\n\
             0019 SEND_VAL array(...) 7\n\
             0020 DO_ICALL\n\
             0021 RETURN int(1)\n"
        )
    );
}

/// `zend_compile_func_get_class`, `_get_called_class` and `_gettype`
/// as the oracle at 8.5.9 dumps them (#1251): `GET_CLASS` with no
/// operand or over the one argument, `GET_CALLED_CLASS` and
/// `GET_TYPE`, in a method and in the main op array alike, since
/// none of the three reads the op array; a count they refuse is the
/// call.
#[test]
fn get_class_get_called_class_and_gettype_are_their_opcodes() {
    assert_eq!(
        dump_of(
            b"<?php class C { function f() { return [get_class(), get_class($this), get_called_class(), gettype($x), get_class($a, $b), get_called_class(1), gettype()]; } } echo get_class(), get_called_class(), gettype(1);"
        ),
        "\n\
         $_main:\n\
         \x20    ; (lines=7, args=0, vars=0, tmps=3)\n\
         \x20    ; (before optimizer)\n\
         \x20    ; :1-1\n\
         \x20    ; return  [] RANGE[0..0]\n\
         0000 T0 = GET_CLASS\n\
         0001 ECHO T0\n\
         0002 T1 = GET_CALLED_CLASS\n\
         0003 ECHO T1\n\
         0004 T2 = GET_TYPE int(1)\n\
         0005 ECHO T2\n\
         0006 RETURN int(1)\n\
         \n\
         C::f:\n\
         \x20    ; (lines=23, args=0, vars=3, tmps=9)\n\
         \x20    ; (before optimizer)\n\
         \x20    ; :1-1\n\
         \x20    ; return  [] RANGE[0..0]\n\
         0000 T3 = GET_CLASS\n\
         0001 T4 = INIT_ARRAY 7 (packed) T3 NEXT\n\
         0002 T5 = FETCH_THIS\n\
         0003 T6 = GET_CLASS T5\n\
         0004 T4 = ADD_ARRAY_ELEMENT T6 NEXT\n\
         0005 T7 = GET_CALLED_CLASS\n\
         0006 T4 = ADD_ARRAY_ELEMENT T7 NEXT\n\
         0007 T8 = GET_TYPE CV0($x)\n\
         0008 T4 = ADD_ARRAY_ELEMENT T8 NEXT\n\
         0009 INIT_FCALL 2 112 string(\"get_class\")\n\
         0010 SEND_VAR CV1($a) 1\n\
         0011 SEND_VAR CV2($b) 2\n\
         0012 V9 = DO_ICALL\n\
         0013 T4 = ADD_ARRAY_ELEMENT V9 NEXT\n\
         0014 INIT_FCALL 1 96 string(\"get_called_class\")\n\
         0015 SEND_VAL int(1) 1\n\
         0016 V10 = DO_ICALL\n\
         0017 T4 = ADD_ARRAY_ELEMENT V10 NEXT\n\
         0018 INIT_FCALL 0 80 string(\"gettype\")\n\
         0019 V11 = DO_ICALL\n\
         0020 T4 = ADD_ARRAY_ELEMENT V11 NEXT\n\
         0021 RETURN T4\n\
         0022 RETURN null\n\
         LIVE RANGES:\n\
         \x20    4: 0002 - 0021 (tmp/var)\n"
    );
}

/// `zend_compile_func_num_args` and `zend_compile_func_array_slice`
/// as the oracle at 8.5.9 dumps them (#1251): in a function,
/// `FUNC_NUM_ARGS`, and `FUNC_GET_ARGS` with the offset as a constant
/// for `array_slice(func_get_args(), n)`, `n` a long written as such
/// and not below zero, the inner name in any case and fully
/// qualified; an offset written as a string or negated, an argument
/// to `func_get_args`, a third argument, and the main op array are
/// the calls, `func_get_args` served inside `array_slice`'s as its
/// own row serves it.
#[test]
fn func_num_args_and_array_slice_of_func_get_args_are_their_opcodes() {
    assert_eq!(
        dump_of(
            b"<?php function f() { return [func_num_args(), func_num_args(1), array_slice(func_get_args(), 1), array_slice(FUNC_GET_ARGS(), 0), array_slice(func_get_args(), \"1\"), array_slice(func_get_args(), -1), array_slice(func_get_args(1), 1), array_slice(\\func_get_args(), 2), array_slice(func_get_args(), 1, 2)]; } echo func_num_args(), array_slice(func_get_args(), 1);"
        ),
        "\n\
         $_main:\n\
         \x20    ; (lines=11, args=0, vars=0, tmps=3)\n\
         \x20    ; (before optimizer)\n\
         \x20    ; :1-1\n\
         \x20    ; return  [] RANGE[0..0]\n\
         0000 INIT_FCALL 0 80 string(\"func_num_args\")\n\
         0001 V0 = DO_ICALL\n\
         0002 ECHO V0\n\
         0003 INIT_FCALL 2 112 string(\"array_slice\")\n\
         0004 INIT_FCALL 0 80 string(\"func_get_args\")\n\
         0005 V1 = DO_ICALL\n\
         0006 SEND_VAR V1 1\n\
         0007 SEND_VAL int(1) 2\n\
         0008 V2 = DO_ICALL\n\
         0009 ECHO V2\n\
         0010 RETURN int(1)\n\
         \n\
         f:\n\
         \x20    ; (lines=41, args=0, vars=0, tmps=14)\n\
         \x20    ; (before optimizer)\n\
         \x20    ; :1-1\n\
         \x20    ; return  [] RANGE[0..0]\n\
         0000 T0 = FUNC_NUM_ARGS\n\
         0001 T1 = INIT_ARRAY 9 (packed) T0 NEXT\n\
         0002 INIT_FCALL 1 96 string(\"func_num_args\")\n\
         0003 SEND_VAL int(1) 1\n\
         0004 V2 = DO_ICALL\n\
         0005 T1 = ADD_ARRAY_ELEMENT V2 NEXT\n\
         0006 T3 = FUNC_GET_ARGS int(1)\n\
         0007 T1 = ADD_ARRAY_ELEMENT T3 NEXT\n\
         0008 T4 = FUNC_GET_ARGS int(0)\n\
         0009 T1 = ADD_ARRAY_ELEMENT T4 NEXT\n\
         0010 INIT_FCALL 2 112 string(\"array_slice\")\n\
         0011 T5 = FUNC_GET_ARGS\n\
         0012 SEND_VAL T5 1\n\
         0013 SEND_VAL string(\"1\") 2\n\
         0014 V6 = DO_ICALL\n\
         0015 T1 = ADD_ARRAY_ELEMENT V6 NEXT\n\
         0016 INIT_FCALL 2 112 string(\"array_slice\")\n\
         0017 T7 = FUNC_GET_ARGS\n\
         0018 SEND_VAL T7 1\n\
         0019 SEND_VAL int(-1) 2\n\
         0020 V8 = DO_ICALL\n\
         0021 T1 = ADD_ARRAY_ELEMENT V8 NEXT\n\
         0022 INIT_FCALL 2 112 string(\"array_slice\")\n\
         0023 INIT_FCALL 1 96 string(\"func_get_args\")\n\
         0024 SEND_VAL int(1) 1\n\
         0025 V9 = DO_ICALL\n\
         0026 SEND_VAR V9 1\n\
         0027 SEND_VAL int(1) 2\n\
         0028 V10 = DO_ICALL\n\
         0029 T1 = ADD_ARRAY_ELEMENT V10 NEXT\n\
         0030 T11 = FUNC_GET_ARGS int(2)\n\
         0031 T1 = ADD_ARRAY_ELEMENT T11 NEXT\n\
         0032 INIT_FCALL 3 128 string(\"array_slice\")\n\
         0033 T12 = FUNC_GET_ARGS\n\
         0034 SEND_VAL T12 1\n\
         0035 SEND_VAL int(1) 2\n\
         0036 SEND_VAL int(2) 3\n\
         0037 V13 = DO_ICALL\n\
         0038 T1 = ADD_ARRAY_ELEMENT V13 NEXT\n\
         0039 RETURN T1\n\
         0040 RETURN null\n\
         LIVE RANGES:\n\
         \x20    1: 0002 - 0039 (tmp/var)\n"
    );
}

/// Inside a namespace the unqualified names wait for the program
/// (`zend_compile_ns_call`) and reach no row, and the `func_get_args`
/// inside a fully qualified `array_slice` resolves into the
/// namespace, which `zend_compile_func_array_slice` refuses; a fully
/// qualified name is served, as the oracle at 8.5.9 dumps it (#1251).
#[test]
fn a_special_name_waiting_for_its_namespace_is_no_row() {
    assert_eq!(
        dump_of(
            b"<?php namespace N; function f() { return [\\array_slice(func_get_args(), 1), \\array_slice(\\func_get_args(), 1), intval($a), \\intval($a), \\get_class()]; }"
        ),
        "\n\
         $_main:\n\
         \x20    ; (lines=1, args=0, vars=0, tmps=0)\n\
         \x20    ; (before optimizer)\n\
         \x20    ; :1-1\n\
         \x20    ; return  [] RANGE[0..0]\n\
         0000 RETURN int(1)\n\
         \n\
         N\\f:\n\
         \x20    ; (lines=19, args=0, vars=1, tmps=7)\n\
         \x20    ; (before optimizer)\n\
         \x20    ; :1-1\n\
         \x20    ; return  [] RANGE[0..0]\n\
         0000 INIT_FCALL 2 112 string(\"array_slice\")\n\
         0001 INIT_NS_FCALL_BY_NAME 0 string(\"N\\\\func_get_args\")\n\
         0002 V1 = DO_FCALL_BY_NAME\n\
         0003 SEND_VAR V1 1\n\
         0004 SEND_VAL int(1) 2\n\
         0005 V2 = DO_ICALL\n\
         0006 T3 = INIT_ARRAY 5 (packed) V2 NEXT\n\
         0007 T4 = FUNC_GET_ARGS int(1)\n\
         0008 T3 = ADD_ARRAY_ELEMENT T4 NEXT\n\
         0009 INIT_NS_FCALL_BY_NAME 1 string(\"N\\\\intval\")\n\
         0010 SEND_VAR_EX CV0($a) 1\n\
         0011 V5 = DO_FCALL_BY_NAME\n\
         0012 T3 = ADD_ARRAY_ELEMENT V5 NEXT\n\
         0013 T6 = CAST (long) CV0($a)\n\
         0014 T3 = ADD_ARRAY_ELEMENT T6 NEXT\n\
         0015 T7 = GET_CLASS\n\
         0016 T3 = ADD_ARRAY_ELEMENT T7 NEXT\n\
         0017 RETURN T3\n\
         0018 RETURN null\n\
         LIVE RANGES:\n\
         \x20    3: 0007 - 0017 (tmp/var)\n"
    );
}

/// `zend_compile_func_array_key_exists` as the oracle at 8.5.9 dumps
/// it (#1251): `ARRAY_KEY_EXISTS` over the key and then the array,
/// each compiled first, read by a `JMPZ` and freed where nothing
/// reads it; one argument is the call.
#[test]
fn array_key_exists_of_two_arguments_is_its_opcode() {
    assert_eq!(
        dump_of(
            b"<?php echo array_key_exists($k, $a), array_key_exists(1, [1]), array_key_exists($k), array_key_exists($k . \"\", $a[\"x\"]); if (array_key_exists($k, $a)) echo 1; array_key_exists($k, $a);"
        ),
        main_dump(
            18,
            2,
            8,
            "0000 T2 = ARRAY_KEY_EXISTS CV0($k) CV1($a)\n\
             0001 ECHO T2\n\
             0002 T3 = ARRAY_KEY_EXISTS int(1) array(...)\n\
             0003 ECHO T3\n\
             0004 INIT_FCALL 1 96 string(\"array_key_exists\")\n\
             0005 SEND_VAR CV0($k) 1\n\
             0006 V4 = DO_ICALL\n\
             0007 ECHO V4\n\
             0008 T5 = CONCAT CV0($k) string(\"\")\n\
             0009 T6 = FETCH_DIM_R CV1($a) string(\"x\")\n\
             0010 T7 = ARRAY_KEY_EXISTS T5 T6\n\
             0011 ECHO T7\n\
             0012 T8 = ARRAY_KEY_EXISTS CV0($k) CV1($a)\n\
             0013 JMPZ T8 0015\n\
             0014 ECHO int(1)\n\
             0015 T9 = ARRAY_KEY_EXISTS CV0($k) CV1($a)\n\
             0016 FREE T9\n\
             0017 RETURN int(1)\n\
             LIVE RANGES:\n\
             \x20    5: 0009 - 0010 (tmp/var)\n"
        )
    );
}

/// `zend_do_free` over what the four opcodes leave: none of them is
/// one php marks `UNUSED`, so a value nothing reads is a `FREE`.
#[test]
fn a_special_opcode_nothing_reads_is_freed() {
    assert_eq!(
        dump_of(b"<?php $n = strlen($a); strlen($a); is_int($a); count($a); defined('NOPE');"),
        main_dump(
            11,
            2,
            6,
            "0000 T2 = STRLEN CV1($a)\n\
             0001 ASSIGN CV0($n) T2\n\
             0002 T4 = STRLEN CV1($a)\n\
             0003 FREE T4\n\
             0004 T5 = TYPE_CHECK (long) CV1($a)\n\
             0005 FREE T5\n\
             0006 T6 = COUNT CV1($a)\n\
             0007 FREE T6\n\
             0008 T7 = DEFINED string(\"NOPE\")\n\
             0009 FREE T7\n\
             0010 RETURN int(1)\n"
        )
    );
}

/// The table is `zend_try_compile_special_func_ex`'s names in its
/// order, `strlen` first and `clone` last, and a name is found by the
/// row it stands in.
#[test]
fn the_special_table_is_php_s_names_in_php_s_order() {
    use special::{Special, TABLE, row};
    assert_eq!(TABLE.len(), 35);
    assert_eq!(TABLE.first().map(|(n, _)| *n), Some(&b"strlen"[..]));
    assert_eq!(TABLE.last().map(|(n, _)| *n), Some(&b"clone"[..]));
    assert_eq!(row(b"defined"), Some(Special::Defined));
    assert_eq!(row(b"is_integer"), Some(Special::TypeCheck(0x10)));
    assert_eq!(row(b"is_scalar"), Some(Special::TypeCheck(0x7c)));
    assert_eq!(row(b"sizeof"), Some(Special::Count { sizeof: true }));
    assert_eq!(row(b"intval"), Some(Special::Cast(emit::CastTo::Long)));
    assert_eq!(row(b"doubleval"), Some(Special::Cast(emit::CastTo::Double)));
    assert_eq!(row(b"var_dump"), None);
    let names: Vec<&[u8]> = TABLE.iter().map(|(n, _)| *n).collect();
    let mut sorted = names.clone();
    sorted.sort_unstable();
    sorted.dedup();
    assert_eq!(sorted.len(), names.len(), "each name once");
}
