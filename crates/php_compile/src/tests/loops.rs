//! Loops: `for`, `while` and `foreach`, their headers, lists, live
//! ranges and jumps.

use super::*;

/// `zend_compile_for` over `for (;;) {}`, php's own oplines under
/// the dump recipe: the `JMP` forward to the condition, and the
/// condition, the constant `true` for an empty one, as the `JMPNZ`
/// back to the body's first opline, which is itself here. php folds
/// nothing, so the back-edge is a `JMPNZ` and not a `JMP`.
#[test]
fn an_empty_for_is_php_s_forward_jump_and_a_conditional_jump_to_itself() {
    let (unit, _strings) = unit_and_strings(b"<?php set_time_limit(1); for (;;) {}");
    // the oracle's dump, byte for byte
    expect![[r#"

        $_main:
             ; (lines=6, args=0, vars=0, tmps=1)
             ; (before optimizer)
             ; :1-1
             ; return  [] RANGE[0..0]
        0000 INIT_FCALL 1 96 string("set_time_limit")
        0001 SEND_VAL int(1) 1
        0002 DO_ICALL
        0003 JMP 0004
        0004 JMPNZ bool(true) 0004
        0005 RETURN int(1)
    "#]]
    .assert_eq(&unit.dump(&[]).text());
    assert_eq!(
        unit.main.oplines[3].op1, 4,
        "the JMP lands on the condition"
    );
    assert_eq!(
        unit.main.oplines[4].op2, 4,
        "the JMPNZ lands on the body's first opline, which is itself"
    );
    assert_eq!(
        unit.main.literals[2],
        php_heap::Zval::bool(true),
        "an empty condition is php's constant true"
    );
    assert_eq!(
        &*unit.main.safepoints,
        &[OplineIdx(0), OplineIdx(4), OplineIdx(5)],
        "the entry, the header the back-edge lands on, which is itself, the return"
    );
    assert_eq!(
        unit.main.oplines[4].out, 1,
        "the back-edge carries the header's number"
    );
    assert_eq!(unit.main.oplines[5].out, 2, "the return's");
}

/// The three expression lists and the body, in the order php lays
/// them down: initialisers, the jump, the body, the step, then the
/// condition, whose last expression is what the `JMPNZ` tests. The
/// oracle's dump of this source, under the recipe, is the same
/// sixteen oplines with `V3 = DO_ICALL` at 13, the one call whose
/// value is read.
#[test]
fn a_for_lays_its_lists_down_in_php_s_order() {
    let (unit, _strings) = unit_and_strings(
        b"<?php for (extension_loaded('i'); extension_loaded('a'), extension_loaded('b'); extension_loaded('s')) echo 'body';",
    );
    expect![[r#"

        $_main:
             ; (lines=16, args=0, vars=0, tmps=4)
             ; (before optimizer)
             ; :1-1
             ; return  [] RANGE[0..0]
        0000 INIT_FCALL 1 96 string("extension_loaded")
        0001 SEND_VAL string("i") 1
        0002 DO_ICALL
        0003 JMP 0008
        0004 ECHO string("body")
        0005 INIT_FCALL 1 96 string("extension_loaded")
        0006 SEND_VAL string("s") 1
        0007 DO_ICALL
        0008 INIT_FCALL 1 96 string("extension_loaded")
        0009 SEND_VAL string("a") 1
        0010 DO_ICALL
        0011 INIT_FCALL 1 96 string("extension_loaded")
        0012 SEND_VAL string("b") 1
        0013 V3 = DO_ICALL
        0014 JMPNZ V3 0004
        0015 RETURN int(1)
    "#]]
    .assert_eq(&unit.dump(&[]).text());
    assert_eq!(
        unit.main.oplines[3].op1, 8,
        "the JMP lands on the condition"
    );
    assert_eq!(unit.main.oplines[14].op2, 4, "the JMPNZ lands on the body");
}

/// php's own loop, the shape #156's bench runs and the witness beside
/// `php_heap::Str::bytes` is, under the dump recipe: the `$i++` in
/// the step is `PRE_INC` with its temporary unread, which is
/// `zend_do_free`'s conversion, and the condition is `IS_SMALLER`
/// into the temporary the `JMPNZ` tests, `T3 = ` before it.
#[test]
fn php_s_own_for_loop_is_an_is_smaller_and_a_pre_inc() {
    let (unit, _strings) = unit_and_strings(b"<?php for ($i = 0; $i < 1000; $i++) { echo 'dax'; }");
    expect![[r#"

        $_main:
             ; (lines=7, args=0, vars=1, tmps=3)
             ; (before optimizer)
             ; :1-1
             ; return  [] RANGE[0..0]
        0000 ASSIGN CV0($i) int(0)
        0001 JMP 0004
        0002 ECHO string("dax")
        0003 PRE_INC CV0($i)
        0004 T3 = IS_SMALLER CV0($i) int(1000)
        0005 JMPNZ T3 0002
        0006 RETURN int(1)
    "#]]
    .assert_eq(&unit.dump(&[]).text());
    assert_eq!(unit.main.oplines[3].op2_type, OperandKind::Unused);
    assert_eq!(unit.main.oplines[3].out, 1, "the increment's temporary");
    assert_eq!(
        unit.main.oplines[4].out, 3,
        "the comparison's slot, which the JMPNZ tests"
    );
    assert_eq!(
        &*unit.main.safepoints,
        &[OplineIdx(0), OplineIdx(2), OplineIdx(6)],
        "the entry, the body's first opline as the header, the return"
    );
    assert_eq!(
        unit.main.oplines[5].out, 1,
        "the JMPNZ carries the header's number, not its own opline's"
    );
}

/// One rule for `OpArray.safepoints` whatever the loop (#353): the
/// entry a backward jump names holds the opline it lands on. A
/// `while`'s `JMPNZ` lands on the body's first opline and a
/// `foreach`'s `JMP`, and the `continue` inside it, land on the
/// `FE_FETCH_R`; each entry is that header and never the jump.
#[test]
fn a_while_and_a_foreach_with_a_continue_number_their_headers_under_one_rule() {
    let (unit, _strings) = unit_and_strings(b"<?php while ($a) { echo 1; }");
    assert_eq!(
        unit.main.oplines[2].opcode,
        Opcode::Jmpnz,
        "the condition is the back-edge"
    );
    assert_eq!(
        unit.main.oplines[2].op2, 1,
        "landing on the body's first opline"
    );
    assert_eq!(
        &*unit.main.safepoints,
        &[OplineIdx(0), OplineIdx(1), OplineIdx(3)],
        "the entry, the header at the ECHO, the return"
    );
    assert_eq!(
        unit.main.oplines[2].out, 1,
        "the JMPNZ carries the header's number"
    );

    let (unit, _strings) = unit_and_strings(b"<?php foreach ($a as $v) { continue; }");
    assert_eq!(
        &*unit.main.safepoints,
        &[OplineIdx(0), OplineIdx(1), OplineIdx(5)],
        "the entry, the header at the FE_FETCH_R, the return"
    );
    let back_edges: Vec<(Opcode, u32, u32)> = [2, 3]
        .iter()
        .map(|&at| {
            let op = unit.main.oplines[at];
            (op.opcode, op.op1, op.out)
        })
        .collect();
    assert_eq!(
        back_edges,
        vec![(Opcode::Jmp, 1, 1), (Opcode::Jmp, 1, 1)],
        "the continue and the loop's own JMP land on the header and carry its one number"
    );
}

/// `zend_compile_while`, and a block whose statements keep their own
/// lines while the loop's own oplines keep the statement's, as php's
/// `CG(zend_lineno)` is the condition's again once the condition is
/// compiled; the final return takes the line the last opline left,
/// as `zend_emit_final_return` does.
#[test]
fn a_while_is_the_same_shape_and_the_body_keeps_its_lines() {
    let (unit, _strings) =
        unit_and_strings(b"<?php\nwhile (extension_loaded('a')) {\n  echo 1;\n}");
    expect![[r#"

        $_main:
             ; (lines=7, args=0, vars=0, tmps=1)
             ; (before optimizer)
             ; :1-4
             ; return  [] RANGE[0..0]
        0000 JMP 0002
        0001 ECHO int(1)
        0002 INIT_FCALL 1 96 string("extension_loaded")
        0003 SEND_VAL string("a") 1
        0004 V0 = DO_ICALL
        0005 JMPNZ V0 0001
        0006 RETURN int(1)
    "#]]
    .assert_eq(&unit.dump(&[]).text());
    assert_eq!(unit.main.oplines[0].op1, 2);
    assert_eq!(unit.main.oplines[5].op2, 1);
    let lines: Vec<u32> = unit.main.oplines.iter().map(|op| op.lineno).collect();
    // The implicit return on the file's last line, as phpdbg's `print`
    // of the oracle shows it: `L0004 0006 RETURN int(1)`.
    assert_eq!(lines, vec![2, 3, 2, 2, 2, 2, 4]);
}

/// `zend_compile_foreach` over `$this` (#2687): by reference the
/// subject is compiled under `BP_VAR_W`, `FETCH_THIS` into a `VAR`
/// and `FE_RESET_RW` over it; by value under `BP_VAR_R`, into a
/// temporary.
#[test]
fn foreach_over_this_by_reference_fetches_it_into_a_var() {
    let dump = dump_of(
        b"<?php
class C {
    public function g() { foreach ($this as &$v) {} foreach ($this as $v) {} }
}
",
    );
    let g = "0000 V1 = FETCH_THIS\n\
             0001 V2 = FE_RESET_RW V1 0004\n\
             0002 FE_FETCH_RW V2 CV0($v) 0004\n\
             0003 JMP 0002\n\
             0004 FE_FREE V2\n\
             0005 T3 = FETCH_THIS\n\
             0006 V4 = FE_RESET_R T3 0009\n\
             0007 FE_FETCH_R V4 CV0($v) 0009\n\
             0008 JMP 0007\n\
             0009 FE_FREE V4\n\
             0010 RETURN null\n";
    assert!(dump.contains(g), "php's C::g, {dump}");
}

/// #301's program under the oracle's `[regime.dump]`: `foreach` as
/// `zend_compile_foreach` shapes it, the constant array folded by
/// `zend_try_ct_eval_array`, `FE_RESET_R` into a `VAR` whose `op2`
/// and the fetch's `extended_value` point past the loop, the `JMP`
/// back to the fetch, `FE_FREE` at the end, and the `loop` live range
/// from the fetch to the free.
#[test]
fn a_foreach_is_php_s_op_array_ending_in_its_loop_live_range() {
    let (unit, _strings) = unit_and_strings(b"<?php foreach ([1, 2] as $v) { echo $v; }");
    expect![[r#"

        $_main:
             ; (lines=6, args=0, vars=1, tmps=1)
             ; (before optimizer)
             ; :1-1
             ; return  [] RANGE[0..0]
        0000 V1 = FE_RESET_R array(...) 0004
        0001 FE_FETCH_R V1 CV0($v) 0004
        0002 ECHO CV0($v)
        0003 JMP 0001
        0004 FE_FREE V1
        0005 RETURN int(1)
        LIVE RANGES:
             1: 0001 - 0004 (loop)
    "#]]
    .assert_eq(&unit.dump(&[]).text());
    let array = unit.main.literals[0]
        .as_arr()
        .map(|a| a.iter().collect::<Vec<_>>());
    assert_eq!(
        array,
        Some(vec![
            (php_heap::Key::Long(0), php_heap::Zval::long(1)),
            (php_heap::Key::Long(1), php_heap::Zval::long(2)),
        ]),
        "the literal is the array php folds"
    );
    assert_eq!(
        &*unit.main.safepoints,
        &[OplineIdx(0), OplineIdx(1), OplineIdx(5)],
        "the entry, the header the back-edge lands on, the return"
    );
    assert_eq!(
        unit.main.oplines[3].out, 1,
        "the back-edge carries the header's number"
    );
    assert_eq!(unit.main.oplines[5].out, 2, "the return's");
}

/// The same program with `return` inside the loop, the oracle's
/// dump: `FE_FREE` flagged `ZEND_FREE_ON_RETURN` before the `RETURN`,
/// its `op2` the distance to the loop's own `FE_FREE`, and the `loop`
/// range split around the return (#255's split, read off a dump of
/// the oracle's for the first time).
#[test]
fn an_early_return_in_a_foreach_splits_the_loop_range_around_its_fe_free() {
    let (unit, _strings) =
        unit_and_strings(b"<?php\nforeach ([1, 2] as $v) {\n    echo $v;\n    return;\n}\n");
    expect![[r#"
        0000 V1 = FE_RESET_R array(...) 0006
        0001 FE_FETCH_R V1 CV0($v) 0006
        0002 ECHO CV0($v)
        0003 FE_FREE V1 loop-end(+3)
        0004 RETURN null
        0005 JMP 0001
        0006 FE_FREE V1
        0007 RETURN int(1)
        LIVE RANGES:
             1: 0001 - 0003 (loop)
             1: 0005 - 0006 (loop)
    "#]]
    .assert_eq(&oplines_of(
        b"<?php\nforeach ([1, 2] as $v) {\n    echo $v;\n    return;\n}\n",
    ));
    assert_eq!(
        unit.main.oplines[3].op2, 3,
        "the distance to the loop's end"
    );
    assert_eq!(
        unit.main.oplines[4].out, 2,
        "the early return is a site of its own"
    );
    assert_eq!(
        unit.main.oplines[5].out, 1,
        "the back-edge carries the header's number"
    );
    assert_eq!(unit.main.oplines[7].out, 3, "and the final return its own");
    let lines: Vec<u32> = unit.main.oplines[..7].iter().map(|op| op.lineno).collect();
    assert_eq!(
        lines,
        vec![2, 2, 3, 4, 4, 2, 2],
        "the JMP and the FE_FREE stand on the foreach's line"
    );
}

/// The rest of `zend_compile_foreach` against the oracle's dump: a
/// key is the temporary the fetch writes, assigned to its CV; a
/// `continue` in a `foreach` is a second backward `JMP` to the
/// header carrying the header's number, and a `break` a forward one;
/// `&$v` is `FE_RESET_RW` and `FE_FETCH_RW`, with `SEPARATE` after a
/// call's `VAR`; `break 2` out of the inner loop and `continue 2`
/// free the inner iterator on the way, and a `return` frees every
/// open loop's, innermost first, a `switch` subject among them.
#[test]
fn the_key_the_jumps_and_the_reference_of_a_foreach_are_php_s() {
    let source = b"<?php\nforeach ([1, 2] as $k => $v) {\n    echo $v;\n    continue;\n}\nforeach (f($a) as &$v) { $v = 1; break; }\nforeach ($a as $v) { switch (strlen($v)) { case 1: foreach ($a as $w) { return $v; } } continue 1; }\n";
    expect![[r#"
        0000 V4 = FE_RESET_R array(...) 0006
        0001 T5 = FE_FETCH_R V4 CV0($v) 0006
        0002 ASSIGN CV1($k) T5
        0003 ECHO CV0($v)
        0004 JMP 0001
        0005 JMP 0001
        0006 FE_FREE V4
        0007 INIT_FCALL_BY_NAME 1 string("f")
        0008 SEND_VAR_EX CV2($a) 1
        0009 V7 = DO_FCALL_BY_NAME
        0010 V7 = SEPARATE V7
        0011 V8 = FE_RESET_RW V7 0016
        0012 FE_FETCH_RW V8 CV0($v) 0016
        0013 ASSIGN CV0($v) int(1)
        0014 JMP 0016
        0015 JMP 0012
        0016 FE_FREE V8
        0017 V10 = FE_RESET_R CV2($a) 0034
        0018 FE_FETCH_R V10 CV0($v) 0034
        0019 T11 = STRLEN CV0($v)
        0020 T12 = CASE T11 int(1)
        0021 JMPNZ T12 0023
        0022 JMP 0031
        0023 V13 = FE_RESET_R CV2($a) 0030
        0024 FE_FETCH_R V13 CV3($w) 0030
        0025 FE_FREE V13 loop-end(+5)
        0026 FREE T11 loop-end(+5)
        0027 FE_FREE V10 loop-end(+7)
        0028 RETURN CV0($v)
        0029 JMP 0024
        0030 FE_FREE V13
        0031 FREE T11
        0032 JMP 0018
        0033 JMP 0018
        0034 FE_FREE V10
        0035 RETURN int(1)
        LIVE RANGES:
             4: 0001 - 0006 (loop)
             7: 0010 - 0011 (tmp/var)
             8: 0012 - 0016 (loop)
             10: 0018 - 0027 (loop)
             11: 0020 - 0026 (tmp/var)
             13: 0024 - 0025 (loop)
             13: 0029 - 0030 (loop)
             11: 0029 - 0031 (tmp/var)
             10: 0029 - 0034 (loop)
    "#]]
    .assert_eq(&oplines_of(source));
    let (unit, _strings) = unit_and_strings(source);
    let jumps: Vec<(u32, u32)> = [4, 5, 15, 29, 32, 33]
        .iter()
        .map(|&at| (unit.main.oplines[at].op1, unit.main.oplines[at].out))
        .collect();
    assert_eq!(
        jumps,
        vec![(1, 1), (1, 1), (12, 2), (24, 4), (18, 3), (18, 3)],
        "two back-edges to one header carry one number, the header's"
    );
    assert_eq!(
        &*unit.main.safepoints,
        &[
            OplineIdx(0),
            OplineIdx(1),
            OplineIdx(12),
            OplineIdx(18),
            OplineIdx(24),
            OplineIdx(28),
            OplineIdx(35),
        ],
        "the entry, each foreach's header as its loop begins, the two returns"
    );
    assert_eq!(
        unit.main.oplines[14].out, 0,
        "a break is a forward jump and no site"
    );
}
