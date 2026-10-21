//! `try`, `catch` and `finally`: the catch chain, the fast call a jump
//! out of the block makes, and php's compile errors.

use super::*;

#[test]
fn a_try_is_phps_catch_chain_and_a_finally_is_its_fast_call_and_fast_ret() {
    let source = b"<?php\ntry { throw new Exception(\"x\"); } catch (ValueError $e) { echo 1; } catch (TypeError|Error $e) { echo 2; }\n";
    let (unit, _strings) = unit_and_strings(source);
    assert_eq!(
        unit.dump(&[]).text(),
        "\n$_main:\n     ; (lines=13, args=0, vars=1, tmps=2)\n\
         \x20    ; (before optimizer)\n     ; :1-3\n     ; return  [] RANGE[0..0]\n\
         0000 V1 = NEW 1 string(\"Exception\")\n\
         0001 SEND_VAL_EX string(\"x\") 1\n\
         0002 DO_FCALL\n\
         0003 THROW V1\n\
         0004 JMP 0012\n\
         0005 CV0($e) = CATCH string(\"ValueError\") 0008\n\
         0006 ECHO int(1)\n\
         0007 JMP 0012\n\
         0008 CV0($e) = CATCH string(\"TypeError\") 0010\n\
         0009 JMP 0011\n\
         0010 CV0($e) = CATCH string(\"Error\")\n\
         0011 ECHO int(2)\n\
         0012 RETURN int(1)\n\
         LIVE RANGES:\n\
         \x20    1: 0001 - 0003 (new)\n\
         EXCEPTION TABLE:\n\
         \x20    0000, 0005, -, -\n"
    );
    let last = unit.main.oplines[10];
    assert_eq!(
        last.extended_value & 1,
        1,
        "ZEND_LAST_CATCH on the last CATCH of the try"
    );
    assert_eq!(
        unit.main.oplines[5].extended_value & 1,
        0,
        "and on no earlier one"
    );
    assert_eq!(
        unit.main.try_catch[0].try_op.0, 0,
        "the element opens at the body's first opline"
    );
    assert_eq!(unit.main.try_catch[0].catch_op.0, 5);
}

#[test]
fn a_return_inside_a_try_with_a_finally_calls_the_block_before_the_return() {
    let (unit, _strings) =
        unit_and_strings(b"<?php\nfunction f() { try { return 1; } finally { echo 2; } }\n");
    assert_eq!(
        unit.dump(&[]).text(),
        "\n$_main:\n     ; (lines=1, args=0, vars=0, tmps=0)\n\
         \x20    ; (before optimizer)\n     ; :1-3\n     ; return  [] RANGE[0..0]\n\
         0000 RETURN int(1)\n\
         \nf:\n     ; (lines=7, args=0, vars=0, tmps=1)\n\
         \x20    ; (before optimizer)\n     ; :2-2\n     ; return  [] RANGE[0..0]\n\
         0000 T0 = FAST_CALL 0004\n\
         0001 RETURN int(1)\n\
         0002 T0 = FAST_CALL 0004\n\
         0003 JMP 0006\n\
         0004 ECHO int(2)\n\
         0005 FAST_RET T0\n\
         0006 RETURN null\n\
         EXCEPTION TABLE:\n\
         \x20    0000, -, 0004, 0005\n"
    );
}

/// #421's programs under the oracle's `[regime.dump]`: a `break`
/// out of a `try` with a `finally` is the block's `FAST_CALL` before
/// the `JMP`, and a `break 2` out of two is both blocks' `FAST_CALL`s
/// innermost first, then the inner `foreach`'s `FE_FREE` flagged
/// `ZEND_FREE_ON_RETURN`, then the `JMP` to the outer loop's end,
/// the walk of `zend_handle_loops_and_finally_ex` to a depth of two.
#[test]
fn a_break_out_of_a_try_with_a_finally_calls_the_block_before_the_jump() {
    let (unit, _strings) = unit_and_strings(
        b"<?php\nforeach ([1,2] as $v) { try { break; } finally { echo \"f\"; } }\n",
    );
    assert_eq!(
        unit.dump(&[]).text(),
        "\n$_main:\n     ; (lines=11, args=0, vars=1, tmps=2)\n\
         \x20    ; (before optimizer)\n     ; :1-3\n     ; return  [] RANGE[0..0]\n\
         0000 V1 = FE_RESET_R array(...) 0009\n\
         0001 FE_FETCH_R V1 CV0($v) 0009\n\
         0002 T2 = FAST_CALL 0006\n\
         0003 JMP 0009\n\
         0004 T2 = FAST_CALL 0006\n\
         0005 JMP 0008\n\
         0006 ECHO string(\"f\")\n\
         0007 FAST_RET T2\n\
         0008 JMP 0001\n\
         0009 FE_FREE V1\n\
         0010 RETURN int(1)\n\
         LIVE RANGES:\n\
         \x20    1: 0001 - 0009 (loop)\n\
         EXCEPTION TABLE:\n\
         \x20    0002, -, 0006, 0007\n"
    );
    let (unit, _strings) = unit_and_strings(
        b"<?php\nforeach ([1,2] as $v) { foreach ([3] as $w) { try { try { break 2; } finally { echo \"i\"; } } finally { echo \"o\"; } } }\n",
    );
    assert_eq!(
        unit.dump(&[]).text(),
        "\n$_main:\n     ; (lines=21, args=0, vars=2, tmps=4)\n\
         \x20    ; (before optimizer)\n     ; :1-3\n     ; return  [] RANGE[0..0]\n\
         0000 V2 = FE_RESET_R array(...) 0019\n\
         0001 FE_FETCH_R V2 CV0($v) 0019\n\
         0002 V3 = FE_RESET_R array(...) 0017\n\
         0003 FE_FETCH_R V3 CV1($w) 0017\n\
         0004 T5 = FAST_CALL 0010\n\
         0005 T4 = FAST_CALL 0014\n\
         0006 FE_FREE V3 loop-end(+11)\n\
         0007 JMP 0019\n\
         0008 T5 = FAST_CALL 0010\n\
         0009 JMP 0012\n\
         0010 ECHO string(\"i\")\n\
         0011 FAST_RET T5 try-catch(0)\n\
         0012 T4 = FAST_CALL 0014\n\
         0013 JMP 0016\n\
         0014 ECHO string(\"o\")\n\
         0015 FAST_RET T4\n\
         0016 JMP 0003\n\
         0017 FE_FREE V3\n\
         0018 JMP 0001\n\
         0019 FE_FREE V2\n\
         0020 RETURN int(1)\n\
         LIVE RANGES:\n\
         \x20    2: 0001 - 0019 (loop)\n\
         \x20    3: 0003 - 0006 (loop)\n\
         \x20    3: 0008 - 0017 (loop)\n\
         EXCEPTION TABLE:\n\
         \x20    0004, -, 0014, 0015\n\
         \x20    0004, -, 0010, 0011\n"
    );
}

/// `function &f(array &$a): array { try { return $a; } finally { echo "f"; } }`
/// under the oracle's `[regime.dump]`: the CV made a reference by
/// `MAKE_REF` into a `VAR`, the return type checked, the block's
/// `FAST_CALL` carrying the `VAR`, and the type checked again after
/// it, since the block may have written the cell.
#[test]
fn a_by_reference_return_past_a_finally_is_a_make_ref_and_a_second_type_check() {
    let (unit, _strings) = unit_and_strings(
        b"<?php\nfunction &f(array &$a): array { try { return $a; } finally { echo \"f\"; } }\n",
    );
    assert_eq!(
        unit.dump(&[]).text(),
        "\n$_main:\n     ; (lines=1, args=0, vars=0, tmps=0)\n\
         \x20    ; (before optimizer)\n     ; :1-3\n     ; return  [] RANGE[0..0]\n\
         0000 RETURN int(1)\n\
         \nf:\n     ; (lines=12, args=1, vars=1, tmps=2)\n\
         \x20    ; (before optimizer)\n     ; :2-2\n     ; return  [] RANGE[0..0]\n\
         0000 CV0($a) = RECV 1\n\
         0001 V2 = MAKE_REF CV0($a)\n\
         0002 VERIFY_RETURN_TYPE V2\n\
         0003 T1 = FAST_CALL 0008 V2\n\
         0004 VERIFY_RETURN_TYPE V2\n\
         0005 RETURN_BY_REF V2\n\
         0006 T1 = FAST_CALL 0008\n\
         0007 JMP 0010\n\
         0008 ECHO string(\"f\")\n\
         0009 FAST_RET T1\n\
         0010 VERIFY_RETURN_TYPE\n\
         0011 RETURN_BY_REF (function) null\n\
         LIVE RANGES:\n\
         \x20    2: 0002 - 0005 (tmp/var)\n\
         EXCEPTION TABLE:\n\
         \x20    0001, -, 0008, 0009\n"
    );
}

#[test]
fn a_jump_out_of_a_finally_block_is_phps_compile_error() {
    let out = refused(b"<?php\nforeach ([1] as $v) { try { echo 1; } finally {\nbreak; } }\n");
    assert_eq!(
        out.map(|e| (e.message.into_owned(), e.line)),
        Some((String::from("jump out of a finally block is disallowed"), 3))
    );
    let inside = refused(
        b"<?php\nforeach ([1] as $v) { try { echo 1; } finally { while (true) { break; } } }\n",
    );
    assert!(
        inside.is_none(),
        "a loop inside the block is left by nothing"
    );
}

/// `function f() { try { try { goto x; } finally { echo "a"; } x: echo "b"; } finally { echo "c"; } }`
/// under the oracle's `[regime.dump]`: the `goto` emitted both
/// blocks' `FAST_CALL`s, and `zend_resolve_goto_label` made a `NOP`
/// of the outer one, whose `try` holds the label too.
#[test]
fn a_goto_that_stays_inside_a_try_has_that_finally_call_made_a_nop() {
    let (unit, _strings) = unit_and_strings(
        b"<?php\nfunction f() { try { try { goto x; } finally { echo \"a\"; } x: echo \"b\"; } finally { echo \"c\"; } }\n",
    );
    assert_eq!(
        unit.dump(&[]).text(),
        "\n$_main:\n     ; (lines=1, args=0, vars=0, tmps=0)\n\
         \x20    ; (before optimizer)\n     ; :1-3\n     ; return  [] RANGE[0..0]\n\
         0000 RETURN int(1)\n\
         \nf:\n     ; (lines=13, args=0, vars=0, tmps=2)\n\
         \x20    ; (before optimizer)\n     ; :2-2\n     ; return  [] RANGE[0..0]\n\
         0000 T1 = FAST_CALL 0005\n\
         0001 NOP\n\
         0002 JMP 0007\n\
         0003 T1 = FAST_CALL 0005\n\
         0004 JMP 0007\n\
         0005 ECHO string(\"a\")\n\
         0006 FAST_RET T1 try-catch(0)\n\
         0007 ECHO string(\"b\")\n\
         0008 T0 = FAST_CALL 0010\n\
         0009 JMP 0012\n\
         0010 ECHO string(\"c\")\n\
         0011 FAST_RET T0\n\
         0012 RETURN null\n\
         EXCEPTION TABLE:\n\
         \x20    0000, -, 0010, 0011\n\
         \x20    0000, -, 0005, 0006\n"
    );
}

#[test]
fn a_try_with_neither_a_catch_nor_a_finally_is_phps_compile_error() {
    let refused = refused(b"<?php\ntry { echo 1; }\n");
    assert_eq!(
        refused.map(|e| e.message.into_owned()),
        Some(String::from("Cannot use try without catch or finally"))
    );
}
