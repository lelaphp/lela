//! Calls: a nested call, a call by name, include and eval, frameless
//! calls, a call in a namespace, `exit` and `assert`.

use super::*;

#[test]
fn a_nested_call_sends_what_the_inner_call_returned() {
    let (unit, _strings) = unit_and_strings(b"<?php var_dump(extension_loaded('demo'));");
    // php's, the inner call's `V0 = ` and the outer's bare
    expect![[r#"

        $_main:
             ; (lines=7, args=0, vars=0, tmps=2)
             ; (before optimizer)
             ; :1-1
             ; return  [] RANGE[0..0]
        0000 INIT_FCALL 1 96 string("var_dump")
        0001 INIT_FCALL 1 96 string("extension_loaded")
        0002 SEND_VAL string("demo") 1
        0003 V0 = DO_ICALL
        0004 SEND_VAR V0 1
        0005 DO_ICALL
        0006 RETURN int(1)
    "#]]
    .assert_eq(&unit.dump(&[]).text());
    assert_eq!(unit.main.oplines[0].extended_value, 1, "one argument");
    assert_eq!(
        unit.main.oplines[0].op1, 96,
        "zend_vm_calc_used_stack: five header slots and one argument, sixteen bytes each"
    );
    assert_eq!(unit.main.oplines[1].op1, 96);
    assert_eq!(
        unit.main.cache_size, 16,
        "one cache slot per INIT_FCALL, a pointer each"
    );
    assert_eq!(
        unit.main.num_tmps, 2,
        "one temporary per call, as php counts them here"
    );
}

/// `zend_get_call_op` under each of php's hooks ([`Hooked`]): a
/// user call, a native one, a call by name and a frameless one, as
/// php compiles them with neither hook replaced, under phpdbg's
/// `zend_execute_ex`, and with `zend_execute_internal` set beside it.
#[test]
fn a_replaced_hook_makes_every_call_it_would_not_see_do_fcall() {
    use Opcode::{DoFcall, DoFcallByName, DoIcall, DoUcall, FramelessIcall2};
    let source = b"<?php function foo($b) {} foo(1); var_dump($a); bar(); substr($a, 1);";
    let calls = |hooked: Hooked| {
        let mut strings = php_heap::Strings::new();
        let options = Options {
            hooked,
            ..bundled()
        };
        compile_parsed(source, &options, &mut strings)
            .expect("the source compiles")
            .main
            .oplines
            .iter()
            .map(|op| op.opcode)
            .filter(|op| {
                matches!(
                    op,
                    Opcode::DoUcall
                        | Opcode::DoIcall
                        | Opcode::DoFcall
                        | Opcode::DoFcallByName
                        | Opcode::FramelessIcall2
                )
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(
        calls(Hooked::NONE),
        [DoUcall, DoIcall, DoFcallByName, FramelessIcall2]
    );
    let phpdbg = Hooked {
        user: true,
        natives: false,
    };
    assert_eq!(calls(phpdbg), [DoFcall, DoIcall, DoFcall, FramelessIcall2]);
    let both = Hooked {
        user: true,
        natives: true,
    };
    assert_eq!(calls(both), [DoFcall, DoFcall, DoFcall, DoFcall]);
}

/// The oracle's dump of the four file constructs (#396), each one
/// `INCLUDE_OR_EVAL` over its operand with the construct in
/// `extended_value`, into a `VAR` that `zend_do_free` marks unused
/// where nothing reads it, as it marks a call's; the operand of
/// `require_once` is the whole `$x . "b"`, so the `CONCAT` stands
/// before it.
#[test]
fn the_four_file_constructs_are_one_include_or_eval_each() {
    expect![[r#"

        $_main:
             ; (lines=9, args=0, vars=2, tmps=7)
             ; (before optimizer)
             ; :1-1
             ; return  [] RANGE[0..0]
        0000 V2 = INCLUDE_OR_EVAL (include) string("a.php")
        0001 ASSIGN CV0($r) V2
        0002 T4 = CONCAT CV1($x) string("b")
        0003 INCLUDE_OR_EVAL (require_once) T4
        0004 INCLUDE_OR_EVAL (include_once) string("c")
        0005 INCLUDE_OR_EVAL (require) string("d")
        0006 V8 = INCLUDE_OR_EVAL (include_once) string("e")
        0007 ECHO V8
        0008 RETURN int(1)
    "#]].assert_eq(&dump_of(
            b"<?php $r = include \"a.php\"; require_once $x . \"b\"; include_once \"c\"; require \"d\"; echo include_once \"e\";"
        ));
}

/// The oracle's dump of `eval` (#573), the fifth construct of
/// `zend_compile_include_or_eval`: one `INCLUDE_OR_EVAL (eval)` over
/// the string, into a `VAR` that `zend_do_free` marks unused where
/// nothing reads it, as it marks an `include`'s; the operand of the
/// second is the whole `$x . "b"`, so the `CONCAT` stands before it.
#[test]
fn eval_is_the_fifth_include_or_eval() {
    expect![[r#"

        $_main:
             ; (lines=5, args=0, vars=2, tmps=4)
             ; (before optimizer)
             ; :1-1
             ; return  [] RANGE[0..0]
        0000 V2 = INCLUDE_OR_EVAL (eval) string("return 1;")
        0001 ASSIGN CV0($r) V2
        0002 T4 = CONCAT CV1($x) string("b")
        0003 INCLUDE_OR_EVAL (eval) T4
        0004 RETURN int(1)
    "#]]
    .assert_eq(&dump_of(
        b"<?php $r = eval(\"return 1;\"); eval($x . \"b\");",
    ));
}

/// The string an `eval` hands over, compiled as `compile_string`
/// compiles it (#573): code from its first byte, where a file's is
/// inline HTML, and a main op array that ends in `RETURN null`,
/// where a file's ends in `RETURN int(1)`.
#[test]
fn an_eval_string_begins_in_code_and_returns_null() {
    let options = Options {
        kind: SourceKind::EVAL,
        ..bundled()
    };
    let mut strings = php_heap::Strings::new();
    let unit = compile_source(&mut Arena::new(), b"echo 1; ?>x", &options, &mut strings)
        .expect("the source compiles");
    expect![[r#"

        $_main:
             ; (lines=3, args=0, vars=0, tmps=0)
             ; (before optimizer)
             ; :1-1
             ; return  [] RANGE[0..0]
        0000 ECHO int(1)
        0001 ECHO string("x")
        0002 RETURN null
    "#]]
    .assert_eq(&unit.dump(&[]).text());
}

/// The oracle's dump of `echo nope($a);` (#252), a name php's
/// function table has not: `zend_compile_dynamic_call`'s
/// `INIT_FCALL_BY_NAME` with the argument count and no used stack,
/// `SEND_VAR_EX` for the CV and `DO_FCALL_BY_NAME` into the `VAR`.
/// The constant pair is `zend_add_func_name_literal`'s, the name as
/// written and then its lowered form in the slot after it, two slots
/// even where the two are one string.
#[test]
fn a_name_the_bundle_has_not_is_init_fcall_by_name_over_the_constant_pair() {
    let (unit, _strings) = unit_and_strings(b"<?php echo nope($a);");
    expect![[r#"

        $_main:
             ; (lines=5, args=0, vars=1, tmps=1)
             ; (before optimizer)
             ; :1-1
             ; return  [] RANGE[0..0]
        0000 INIT_FCALL_BY_NAME 1 string("nope")
        0001 SEND_VAR_EX CV0($a) 1
        0002 V1 = DO_FCALL_BY_NAME
        0003 ECHO V1
        0004 RETURN int(1)
    "#]]
    .assert_eq(&unit.dump(&[]).text());
    let init = unit.main.oplines[0];
    assert_eq!(
        init.op1, 0,
        "zend_compile_call_common writes used_stack for INIT_FCALL alone"
    );
    assert_eq!(init.op1_type, OperandKind::Unused);
    assert_eq!((init.op2_type, init.op2), (OperandKind::Const, 0));
    assert_eq!(init.out, 0, "one cache slot, zend_alloc_cache_slot");
    assert_eq!(unit.main.cache_size, 8);
    assert_eq!(literal_text(&unit, 0).as_deref(), Some(&b"nope"[..]));
    assert_eq!(
        literal_text(&unit, 1).as_deref(),
        Some(&b"nope"[..]),
        "the lowered name is the slot after the written one"
    );
    assert_eq!(
        unit.main.literals.len(),
        3,
        "the pair and the final return's 1"
    );
}

/// The oracle's dump of `echo NoPe(nope2($a), 1 + $a, 'x');`: the
/// pair keeps the case the name was written in and lowers it beside,
/// what the inner call returned is `SEND_VAR_NO_REF_EX`, a temporary
/// and a constant are `SEND_VAL_EX`, and the inner call is its own
/// by-name sequence inside the outer's.
#[test]
fn a_call_by_name_sends_each_argument_as_php_sends_it_for_an_unknown_callee() {
    let (unit, _strings) = unit_and_strings(b"<?php echo NoPe(nope2($a), 1 + $a, 'x');");
    expect![[r#"

        $_main:
             ; (lines=11, args=0, vars=1, tmps=3)
             ; (before optimizer)
             ; :1-1
             ; return  [] RANGE[0..0]
        0000 INIT_FCALL_BY_NAME 3 string("NoPe")
        0001 INIT_FCALL_BY_NAME 1 string("nope2")
        0002 SEND_VAR_EX CV0($a) 1
        0003 V1 = DO_FCALL_BY_NAME
        0004 SEND_VAR_NO_REF_EX V1 1
        0005 T2 = ADD int(1) CV0($a)
        0006 SEND_VAL_EX T2 2
        0007 SEND_VAL_EX string("x") 3
        0008 V3 = DO_FCALL_BY_NAME
        0009 ECHO V3
        0010 RETURN int(1)
    "#]]
    .assert_eq(&unit.dump(&[]).text());
    assert_eq!(literal_text(&unit, 0).as_deref(), Some(&b"NoPe"[..]));
    assert_eq!(literal_text(&unit, 1).as_deref(), Some(&b"nope"[..]));
    assert_eq!(literal_text(&unit, 2).as_deref(), Some(&b"nope2"[..]));
    assert_eq!(literal_text(&unit, 3).as_deref(), Some(&b"nope2"[..]));
    assert_eq!(
        unit.main.cache_size, 16,
        "one cache slot per INIT_FCALL_BY_NAME"
    );
}

/// A name the bundle has keeps `INIT_FCALL` and `DO_ICALL` whatever
/// its arguments: `strlen('a', 'b')` is in `special::TABLE` and takes
/// two arguments, which the fold refuses, so it is the ordinary call
/// over the lowered name with php's used stack, as the oracle dumps
/// it: `INIT_FCALL 2 112 string("strlen")`.
#[test]
fn a_name_the_bundle_has_keeps_init_fcall_and_do_icall() {
    let (unit, _strings) = unit_and_strings(b"<?php echo StrLen('a', 'b');");
    expect![[r#"

        $_main:
             ; (lines=6, args=0, vars=0, tmps=1)
             ; (before optimizer)
             ; :1-1
             ; return  [] RANGE[0..0]
        0000 INIT_FCALL 2 112 string("strlen")
        0001 SEND_VAL string("a") 1
        0002 SEND_VAL string("b") 2
        0003 V0 = DO_ICALL
        0004 ECHO V0
        0005 RETURN int(1)
    "#]]
    .assert_eq(&unit.dump(&[]).text());
    assert_eq!(
        unit.main.literals.len(),
        4,
        "the lowered name once, no pair"
    );
}

/// The oracle's dump under `[regime.dump]` of a call of each
/// frameless arity `ext/standard` declares: `str_contains` of two,
/// `trim` of one and of two, `substr` of two, and `str_contains`
/// over a temporary; each a `FRAMELESS_ICALL_<n>` into a temporary
/// with the table's row in `extended_value`, the one nothing reads
/// freed by a `FREE` (`zend_do_free`), and the temporaries numbered
/// as php numbers them, one skipped per assignment.
#[test]
fn a_call_a_frameless_entry_serves_is_frameless_icall_with_its_row() {
    let source = b"<?php $a = \"x\"; $b = str_contains($a, \"y\"); $t = trim($a); $u = trim($a, \"x\"); substr($a, 1); echo str_contains($a . \"z\", \"z\");";
    expect![[r#"

        $_main:
             ; (lines=13, args=0, vars=4, tmps=10)
             ; (before optimizer)
             ; :1-1
             ; return  [] RANGE[0..0]
        0000 ASSIGN CV0($a) string("x")
        0001 T5 = FRAMELESS_ICALL_2(str_contains) CV0($a) string("y")
        0002 ASSIGN CV1($b) T5
        0003 T7 = FRAMELESS_ICALL_1(trim) CV0($a)
        0004 ASSIGN CV2($t) T7
        0005 T9 = FRAMELESS_ICALL_2(trim) CV0($a) string("x")
        0006 ASSIGN CV3($u) T9
        0007 T11 = FRAMELESS_ICALL_2(substr) CV0($a) int(1)
        0008 FREE T11
        0009 T12 = CONCAT CV0($a) string("z")
        0010 T13 = FRAMELESS_ICALL_2(str_contains) T12 string("z")
        0011 ECHO T13
        0012 RETURN int(1)
    "#]]
    .assert_eq(&dump_of(source));
    let unit = unit_of(source);
    let rows: Vec<(Opcode, u32)> = unit
        .main
        .oplines
        .iter()
        .filter(|op| op.opcode.is_frameless_icall())
        .map(|op| (op.opcode, op.extended_value))
        .collect();
    assert_eq!(
        rows,
        [
            (Opcode::FramelessIcall2, 10),
            (Opcode::FramelessIcall1, 0),
            (Opcode::FramelessIcall2, 1),
            (Opcode::FramelessIcall2, 12),
            (Opcode::FramelessIcall2, 10),
        ],
        "zend_flf_functions in registration order: trim 1, trim 2, implode 1, implode 2, dirname 1, dirname 2, strstr 2, strstr 3, strpos 2, strpos 3, str_contains 2, str_starts_with 2, substr 2, substr 3"
    );
}

/// A call of each of the seven natives #2673 found with no entry, at
/// each arity php declares for it, is a `FRAMELESS_ICALL_<n>` with
/// its row of `zend_flf_functions` as `basic_functions.stub.php`
/// orders them; a call of `join` or `strchr`, aliases php declares
/// no entry for, and `str_replace` with its fourth argument, above
/// every arity, stay the ordinary call.
#[test]
fn each_arity_php_declares_is_a_frameless_call_with_its_row() {
    let source = b"<?php implode($a); implode($a, $b); dirname($a); dirname($a, 2); strstr($a, $b); strstr($a, $b, true); strpos($a, $b); strpos($a, $b, 1); str_starts_with($a, $b); strtr($a, $b); strtr($a, $b, $c); str_replace($a, $b, $c); join($a, $b); strchr($a, $b); str_replace($a, $b, $c, $d);";
    let unit = unit_of(source);
    let rows: Vec<(Opcode, u32)> = unit
        .main
        .oplines
        .iter()
        .filter(|op| op.opcode.is_frameless_icall())
        .map(|op| (op.opcode, op.extended_value))
        .collect();
    assert_eq!(
        rows,
        [
            (Opcode::FramelessIcall1, 2),
            (Opcode::FramelessIcall2, 3),
            (Opcode::FramelessIcall1, 4),
            (Opcode::FramelessIcall2, 5),
            (Opcode::FramelessIcall2, 6),
            (Opcode::FramelessIcall3, 7),
            (Opcode::FramelessIcall2, 8),
            (Opcode::FramelessIcall3, 9),
            (Opcode::FramelessIcall2, 11),
            (Opcode::FramelessIcall2, 14),
            (Opcode::FramelessIcall3, 15),
            (Opcode::FramelessIcall3, 16),
        ]
    );
    let ordinary = unit
        .main
        .oplines
        .iter()
        .filter(|op| op.opcode == Opcode::InitFcall)
        .count();
    assert_eq!(ordinary, 3, "join, strchr and str_replace of four");
}

/// A third argument rides in an `OP_DATA` after the opline
/// (`zend_emit_op_data`), and `zend_do_free` steps back over it to
/// the call that left the value. The oracle's dump prints a live
/// range for the temporary across the `OP_DATA`, #255's table, and
/// the dump here is the oracle's under `[regime.dump]`, live ranges
/// and all.
#[test]
fn a_third_argument_is_an_op_data_after_the_frameless_call() {
    let source = b"<?php $a = \"x\"; $s = substr($a, 1, 2); substr($a, 1, 2);";
    expect![[r#"

        $_main:
             ; (lines=8, args=0, vars=2, tmps=4)
             ; (before optimizer)
             ; :1-1
             ; return  [] RANGE[0..0]
        0000 ASSIGN CV0($a) string("x")
        0001 T3 = FRAMELESS_ICALL_3(substr) CV0($a) int(1)
        0002 OP_DATA int(2)
        0003 ASSIGN CV1($s) T3
        0004 T5 = FRAMELESS_ICALL_3(substr) CV0($a) int(1)
        0005 OP_DATA int(2)
        0006 FREE T5
        0007 RETURN int(1)
        LIVE RANGES:
             3: 0002 - 0003 (tmp/var)
             5: 0005 - 0006 (tmp/var)
    "#]]
    .assert_eq(&dump_of(source));
}

/// The oracle's dump of `zend_compile_func_in_array`'s shape:
/// `IN_ARRAY` with `$strict` before its operands, over a haystack
/// of non-numeric strings with no third argument, of strings and
/// longs under `true`, of nothing under `null`, of a numeric string
/// and a long under `\TRUE`, and of a string under `''`, each third
/// argument read by `zend_is_true`. The arms that are the call are the
/// witness's on [`special`], since php's call there is frameless.
#[test]
fn in_array_over_a_literal_haystack_is_in_array_with_its_flag() {
    expect![[r#"

        $_main:
             ; (lines=11, args=0, vars=1, tmps=5)
             ; (before optimizer)
             ; :1-1
             ; return  [] RANGE[0..0]
        0000 T1 = IN_ARRAY 0 CV0($a) array(...)
        0001 ECHO T1
        0002 T2 = IN_ARRAY 1 CV0($a) array(...)
        0003 ECHO T2
        0004 T3 = IN_ARRAY 0 CV0($a) array(...)
        0005 ECHO T3
        0006 T4 = IN_ARRAY 1 CV0($a) array(...)
        0007 ECHO T4
        0008 T5 = IN_ARRAY 0 CV0($a) array(...)
        0009 ECHO T5
        0010 RETURN int(1)
    "#]].assert_eq(&dump_of(
            b"<?php echo in_array($a, [\"x\", \"y\"]), in_array($a, [1, \"b\"], true), in_array($a, [], null), in_array($a, [\"1\", 2], \\TRUE), in_array($a, ['y'], '');"
        ));
}

/// A count no entry serves is the ordinary call, whatever the
/// name: `str_contains` of one argument is below its required
/// count, `trim` of three is above every arity, and a name the
/// bundle has not is the call by name.
#[test]
fn a_count_no_entry_serves_is_the_ordinary_call() {
    expect![[r#"

        $_main:
             ; (lines=13, args=0, vars=3, tmps=3)
             ; (before optimizer)
             ; :1-1
             ; return  [] RANGE[0..0]
        0000 INIT_FCALL 1 96 string("str_contains")
        0001 SEND_VAR CV0($a) 1
        0002 DO_ICALL
        0003 INIT_FCALL 3 128 string("trim")
        0004 SEND_VAR CV0($a) 1
        0005 SEND_VAR CV1($b) 2
        0006 SEND_VAR CV2($c) 3
        0007 DO_ICALL
        0008 INIT_FCALL_BY_NAME 2 string("nope")
        0009 SEND_VAR_EX CV0($a) 1
        0010 SEND_VAR_EX CV1($b) 2
        0011 DO_FCALL_BY_NAME
        0012 RETURN int(1)
    "#]]
    .assert_eq(&dump_of(
        b"<?php str_contains($a); trim($a, $b, $c); nope($a, $b);",
    ));
}

/// The oracle's dump of `zend_compile_ns_call`'s shape for an
/// unqualified name in a namespace: `JMP_FRAMELESS` over the
/// lowered name with its cache slot, printed in bytes, the call by
/// name through `INIT_NS_FCALL_BY_NAME`, `SEND_*_EX` and
/// `DO_FCALL_BY_NAME`, the `JMP` past the branch, and the
/// `FRAMELESS_ICALL_<n>` into the same `VAR`, freed by a `FREE`
/// where nothing reads it. A `\`-qualified name resolves at compile
/// time as outside a namespace, and a call nested in the branch
/// takes no branch of its own (`in_jmp_frameless_branch`).
#[test]
fn an_unqualified_name_in_a_namespace_is_jmp_frameless_around_the_call_by_name() {
    expect![[r#"

        $_main:
             ; (lines=25, args=0, vars=2, tmps=7)
             ; (before optimizer)
             ; :1-1
             ; return  [] RANGE[0..0]
        0000 ASSIGN CV0($a) string("x")
        0001 JMP_FRAMELESS 8 string("foo\\str_contains") 0007
        0002 INIT_NS_FCALL_BY_NAME 2 string("Foo\\str_contains")
        0003 SEND_VAR_EX CV0($a) 1
        0004 SEND_VAL_EX string("y") 2
        0005 V3 = DO_FCALL_BY_NAME
        0006 JMP 0008
        0007 V3 = FRAMELESS_ICALL_2(str_contains) CV0($a) string("y")
        0008 ASSIGN CV1($b) V3
        0009 T5 = FRAMELESS_ICALL_1(trim) CV0($a)
        0010 FREE T5
        0011 JMP_FRAMELESS 40 string("foo\\trim") 0019
        0012 INIT_NS_FCALL_BY_NAME 1 string("Foo\\trim")
        0013 INIT_NS_FCALL_BY_NAME 1 string("Foo\\nope")
        0014 SEND_VAR_EX CV0($a) 1
        0015 V6 = DO_FCALL_BY_NAME
        0016 SEND_VAR_NO_REF_EX V6 1
        0017 V7 = DO_FCALL_BY_NAME
        0018 JMP 0023
        0019 INIT_NS_FCALL_BY_NAME 1 string("Foo\\nope")
        0020 SEND_VAR_EX CV0($a) 1
        0021 V8 = DO_FCALL_BY_NAME
        0022 V7 = FRAMELESS_ICALL_1(trim) V8
        0023 FREE V7
        0024 RETURN int(1)
    "#]].assert_eq(&dump_of(b"<?php namespace Foo; $a = \"x\"; $b = str_contains($a, \"y\"); \\trim($a); trim(nope($a));"));
}

/// The three constants `zend_add_ns_func_name_literal` lays down,
/// in a row: the name with its namespace as written, lowered, and
/// the lowered name alone; a second `namespace` statement moves the
/// namespace, and a qualified name is the namespace before it,
/// resolved at compile time as `INIT_FCALL_BY_NAME`.
#[test]
fn a_namespace_call_carries_php_s_three_constants() {
    let (unit, _strings) = unit_and_strings(
        b"<?php namespace A; namespace B\\C; Str_Contains(\"a\", \"b\"); D\\e(); namespace\\f();",
    );
    let literal = |at: usize| {
        unit.main.literals[at]
            .as_str()
            .map(|s| String::from_utf8_lossy(s.bytes()).into_owned())
    };
    assert_eq!(
        unit.main.oplines[1].opcode,
        Opcode::InitNsFcallByName,
        "after the JMP_FRAMELESS"
    );
    let at = unit.main.oplines[1].op2 as usize;
    assert_eq!(literal(at).as_deref(), Some("B\\C\\Str_Contains"));
    assert_eq!(literal(at + 1).as_deref(), Some("b\\c\\str_contains"));
    assert_eq!(literal(at + 2).as_deref(), Some("str_contains"));
    let by_name: Vec<String> = unit
        .main
        .oplines
        .iter()
        .filter(|op| op.opcode == Opcode::InitFcallByName)
        .filter_map(|op| literal(op.op2 as usize))
        .collect();
    assert_eq!(by_name, ["B\\C\\D\\e", "B\\C\\f"]);
}

/// #399's program under the oracle's `[regime.dump]`, whose dump of
/// `try { throw new Exception("x"); } catch (ValueError $e) { echo 1; }
/// catch (TypeError|Error $e) { echo 2; }` and
/// `function f() { try { return 1; } finally { echo 2; } }` is:
///
/// ```text
/// 0000 V1 = NEW 1 string("Exception")
/// 0001 SEND_VAL_EX string("x") 1
/// 0002 DO_FCALL
/// 0003 THROW V1
/// 0004 JMP 0012
/// 0005 CV0($e) = CATCH string("ValueError") 0008
/// 0006 ECHO int(1)
/// 0007 JMP 0012
/// 0008 CV0($e) = CATCH string("TypeError") 0010
/// 0009 JMP 0011
/// 0010 CV0($e) = CATCH string("Error")
/// 0011 ECHO int(2)
/// 0012 RETURN int(1)
/// LIVE RANGES:
///      1: 0001 - 0003 (new)
/// EXCEPTION TABLE:
///      0000, 0005, -, -
///
/// f:
///      ; (lines=7, args=0, vars=0, tmps=1)
/// 0000 T0 = FAST_CALL 0004
/// 0001 RETURN int(1)
/// 0002 T0 = FAST_CALL 0004
/// 0003 JMP 0006
/// 0004 ECHO int(2)
/// 0005 FAST_RET T0
/// 0006 RETURN null
/// EXCEPTION TABLE:
///      0000, -, 0004, 0005
/// ```
///
/// The clause with two classes is two `CATCH`es, the first jumping
/// to the clause's body; the last `CATCH` of the `try` names no next
/// and carries `ZEND_LAST_CATCH`. The `return` inside a `try` with a
/// `finally` calls the block before the `RETURN`, so `f` carries two
/// `FAST_CALL`s into one block.
/// The oracle's dump under `[regime.dump]` of `exit`, `exit(0)`,
/// `exit("msg")` and `die`: each is `INIT_FCALL` over `exit`, the
/// function php 8.4 made of the keyword, with `die` given the same
/// name by the parser and no opcode of its own anywhere.
#[test]
fn an_exit_or_a_die_is_php_s_init_fcall_over_exit() {
    expect![[r#"

        $_main:
             ; (lines=11, args=0, vars=0, tmps=4)
             ; (before optimizer)
             ; :1-1
             ; return  [] RANGE[0..0]
        0000 INIT_FCALL 0 80 string("exit")
        0001 DO_ICALL
        0002 INIT_FCALL 1 96 string("exit")
        0003 SEND_VAL int(0) 1
        0004 DO_ICALL
        0005 INIT_FCALL 1 96 string("exit")
        0006 SEND_VAL string("msg") 1
        0007 DO_ICALL
        0008 INIT_FCALL 0 80 string("exit")
        0009 DO_ICALL
        0010 RETURN int(1)
    "#]]
    .assert_eq(&dump_of(b"<?php exit; exit(0); exit(\"msg\"); die;"));
}

/// [`Options::assertions`] off, php's `zend.assertions = -1`: an
/// `assert()` is the constant `true` and nothing of its arguments
/// compiles (`zend_compile_assert`), so `f()` is never called. The
/// entry moves only at startup, so no witness runs php under it; the
/// dump is the oracle's under `-d zend.assertions=-1` and the dump
/// regime, and its behaviour `Zend/tests/assert/expect_017.phpt` and
/// `expect_019.phpt`, whose `--INI--` sets it. The bare name in a
/// namespace is `zend_compile_assert`'s whatever the table holds.
#[test]
fn an_assertion_under_assertions_off_is_the_constant_true() {
    let mut strings = php_heap::Strings::new();
    let options = Options {
        assertions: false,
        ..bundled()
    };
    let dump = compile_source(
        &mut Arena::new(),
        b"<?php namespace A; $a = assert(f());",
        &options,
        &mut strings,
    )
    .expect("the source compiles")
    .dump(&[])
    .text();
    expect![[r#"

        $_main:
             ; (lines=2, args=0, vars=1, tmps=1)
             ; (before optimizer)
             ; :1-1
             ; return  [] RANGE[0..0]
        0000 ASSIGN CV0($a) bool(true)
        0001 RETURN int(1)
    "#]]
    .assert_eq(&dump);
}
