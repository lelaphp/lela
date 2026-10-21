//! Variables: a CV, an assignment and what `zend_do_free` frees, live
//! ranges, silence, `global`, by-reference assignment, `list`, and a
//! variable fetched by name or from the global table.

use super::*;

/// The oracle's dump under the pin's `dump` regime of
/// `$a = 1; echo $a, -$a, $A; $c = $b = $a + 1;`, `vars=4, tmps=5`:
///
/// ```text
/// 0000 ASSIGN CV0($a) int(1)
/// 0001 ECHO CV0($a)
/// 0002 T5 = MUL CV0($a) int(-1)
/// 0003 ECHO T5
/// 0004 ECHO CV1($A)
/// 0005 T6 = ADD CV0($a) int(1)
/// 0006 T7 = ASSIGN CV3($b) T6
/// 0007 ASSIGN CV2($c) T7
/// 0008 RETURN int(1)
/// ```
///
/// `lookup_cv` numbers a name when it is first met, `$a` and `$A`
/// apart, and `zend_compile_assign` looks the variable up before it
/// compiles the value, so `$c` is met before `$b`. Each `ASSIGN`
/// takes a temporary, read or not, which is the five php counts.
#[test]
fn a_variable_is_its_cv_and_an_assignment_is_php_s_assign_in_php_s_order() {
    use OperandKind::{Const, Cv, Tmp, Unused};
    let unit = unit_of(b"<?php $a = 1; echo $a, -$a, $A; $c = $b = $a + 1;");
    assert_eq!(
        unit.main.vars.iter().map(|v| &*v.0).collect::<Vec<_>>(),
        [b"a".as_slice(), b"A", b"c", b"b"]
    );
    assert_eq!(unit.main.num_cvs, 4);
    assert_eq!(unit.main.num_tmps, 5);
    let shape: Vec<_> = unit
        .main
        .oplines
        .iter()
        .map(|op| (op.opcode, op.op1_type, op.op1, op.op2_type, op.op2, op.out))
        .collect();
    assert_eq!(
        shape,
        [
            (Opcode::Assign, Cv, 0, Const, 0, 0),
            (Opcode::Echo, Cv, 0, Unused, 0, 0),
            (Opcode::Mul, Cv, 0, Const, 1, 5),
            (Opcode::Echo, Tmp, 5, Unused, 0, 0),
            (Opcode::Echo, Cv, 1, Unused, 0, 0),
            (Opcode::Add, Cv, 0, Const, 2, 6),
            (Opcode::Assign, Cv, 3, Tmp, 6, 7),
            (Opcode::Assign, Cv, 2, Tmp, 7, 4),
            (Opcode::Return, Const, 3, Unused, 0, 1),
        ]
    );
}

/// `zend_do_free` over the oracle's dump of `$a = 1; $a + 1;
/// str_repeat("a", 3) . "b"; extension_loaded('Core'); $a++;`,
/// `vars=1, tmps=6`:
///
/// ```text
/// 0000 ASSIGN CV0($a) int(1)
/// 0001 T2 = ADD CV0($a) int(1)
/// 0002 FREE T2
/// 0003 INIT_FCALL 2 112 string("str_repeat")
/// 0004 SEND_VAL string("a") 1
/// 0005 SEND_VAL int(3) 2
/// 0006 V3 = DO_ICALL
/// 0007 T4 = CONCAT V3 string("b")
/// 0008 FREE T4
/// 0009 INIT_FCALL 1 96 string("extension_loaded")
/// 0010 SEND_VAL string("Core") 1
/// 0011 DO_ICALL
/// 0012 PRE_INC CV0($a)
/// 0013 RETURN int(1)
/// ```
///
/// The `ASSIGN`, the call and the `PRE_INC` are marked `UNUSED` in
/// `out_type` and keep their numbers; the `ADD` and the `CONCAT`,
/// which php does not mark, are followed by a `FREE` of their
/// temporary.
#[test]
fn zend_do_free_marks_an_assign_a_call_and_a_pre_inc_and_frees_the_rest() {
    use OperandKind::{Tmp, Unused, Var};
    let (unit, _strings) = unit_and_strings(
        b"<?php $a = 1; $a + 1; str_repeat(\"a\", 3) . \"b\"; extension_loaded('Core'); $a++;",
    );
    expect![[r#"

        $_main:
             ; (lines=14, args=0, vars=1, tmps=6)
             ; (before optimizer)
             ; :1-1
             ; return  [] RANGE[0..0]
        0000 ASSIGN CV0($a) int(1)
        0001 T2 = ADD CV0($a) int(1)
        0002 FREE T2
        0003 INIT_FCALL 2 112 string("str_repeat")
        0004 SEND_VAL string("a") 1
        0005 SEND_VAL int(3) 2
        0006 V3 = DO_ICALL
        0007 T4 = CONCAT V3 string("b")
        0008 FREE T4
        0009 INIT_FCALL 1 96 string("extension_loaded")
        0010 SEND_VAL string("Core") 1
        0011 DO_ICALL
        0012 PRE_INC CV0($a)
        0013 RETURN int(1)
    "#]]
    .assert_eq(&unit.dump(&[]).text());
    let marks: Vec<_> = unit
        .main
        .oplines
        .iter()
        .map(|op| (op.opcode, op.out_type, op.out))
        .collect();
    assert_eq!(marks[0], (Opcode::Assign, Unused, 0), "numbered, unread");
    assert_eq!(
        marks[1],
        (Opcode::Add, Tmp, 2),
        "the slot, as php prints it"
    );
    assert_eq!(marks[2], (Opcode::Free, Unused, 0));
    assert_eq!(unit.main.oplines[2].op1_type, Tmp);
    assert_eq!(unit.main.oplines[2].op1, 2, "the ADD's temporary");
    assert_eq!(marks[6], (Opcode::DoIcall, Var, 3));
    assert_eq!(marks[11], (Opcode::DoIcall, Unused, 4), "numbered, unread");
    assert_eq!(marks[12], (Opcode::PreInc, Unused, 5), "numbered, unread");
    assert_eq!(
        unit.main.oplines[13].out_type, Unused,
        "a site keeps its number bare"
    );
}

/// `zend_calc_live_ranges` over the oracle's dump of
/// `echo str_repeat("a", 2) . str_repeat("b", 2);`, `vars=0,
/// tmps=3`, which ends in the live-range table:
///
/// ```text
/// 0000 INIT_FCALL 2 112 string("str_repeat")
/// 0001 SEND_VAL string("a") 1
/// 0002 SEND_VAL int(2) 2
/// 0003 V0 = DO_ICALL
/// 0004 INIT_FCALL 2 112 string("str_repeat")
/// 0005 SEND_VAL string("b") 1
/// 0006 SEND_VAL int(2) 2
/// 0007 V1 = DO_ICALL
/// 0008 T2 = CONCAT V0 V1
/// 0009 ECHO T2
/// 0010 RETURN int(1)
/// LIVE RANGES:
///      0: 0004 - 0008 (tmp/var)
/// ```
///
/// The first call's `V0` is defined at 0003 and read at the `CONCAT`
/// four oplines on, so it is live from the opline after its
/// definition to its use; `V1` and `T2` are each read by the next
/// opline, the trivial range php stores nothing for.
#[test]
fn a_temporary_read_past_the_next_opline_has_php_s_live_range() {
    use php_unit::{LiveKind, LiveRange};
    let (unit, _strings) =
        unit_and_strings(b"<?php echo str_repeat(\"a\", 2) . str_repeat(\"b\", 2);");
    expect![[r#"

        $_main:
             ; (lines=11, args=0, vars=0, tmps=3)
             ; (before optimizer)
             ; :1-1
             ; return  [] RANGE[0..0]
        0000 INIT_FCALL 2 112 string("str_repeat")
        0001 SEND_VAL string("a") 1
        0002 SEND_VAL int(2) 2
        0003 V0 = DO_ICALL
        0004 INIT_FCALL 2 112 string("str_repeat")
        0005 SEND_VAL string("b") 1
        0006 SEND_VAL int(2) 2
        0007 V1 = DO_ICALL
        0008 T2 = CONCAT V0 V1
        0009 ECHO T2
        0010 RETURN int(1)
        LIVE RANGES:
             0: 0004 - 0008 (tmp/var)
    "#]]
    .assert_eq(&unit.dump(&[]).text());
    assert_eq!(
        *unit.main.live_ranges,
        [LiveRange {
            var: 0,
            kind: LiveKind::TmpVar,
            start: OplineIdx(4),
            end: OplineIdx(8),
        }]
    );
}

/// A CV is sent by `SEND_VAR` as a call's `VAR` is
/// (`zend_compile_args`), and the value a native returned is
/// assigned from its `VAR`, which is the shape #174's witness
/// compiles to.
#[test]
fn a_cv_is_sent_by_send_var_and_assigned_from_a_call_s_var() {
    use OperandKind::{Const, Cv, Unused, Var};
    let unit = unit_of(b"<?php $b = memory_get_usage(); var_dump(memory_get_usage() - $b);");
    let shape: Vec<_> = unit
        .main
        .oplines
        .iter()
        .map(|op| (op.opcode, op.op1_type, op.op2_type))
        .collect();
    assert_eq!(
        shape,
        [
            (Opcode::InitFcall, Unused, Const),
            (Opcode::DoIcall, Unused, Unused),
            (Opcode::Assign, Cv, Var),
            (Opcode::InitFcall, Unused, Const),
            (Opcode::InitFcall, Unused, Const),
            (Opcode::DoIcall, Unused, Unused),
            (Opcode::Sub, Var, Cv),
            (Opcode::SendVal, OperandKind::Tmp, Unused),
            (Opcode::DoIcall, Unused, Unused),
            (Opcode::Return, Const, Unused),
        ]
    );
    let sent = unit_of(b"<?php $a = 1; var_dump($a);");
    assert_eq!(sent.main.oplines[2].opcode, Opcode::SendVar);
    assert_eq!(sent.main.oplines[2].op1_type, Cv);
}

/// #302's program under the oracle's `[regime.dump]`: `@` as
/// `zend_compile_silence` shapes it, `BEGIN_SILENCE` into a
/// temporary, the call, `END_SILENCE` over that temporary, and the
/// `silence` live range from the opline after the begin to the end,
/// read off a dump of the oracle's for the first time.
#[test]
fn a_silenced_call_is_php_s_op_array_ending_in_its_silence_live_range() {
    let (unit, _strings) = unit_and_strings(b"<?php echo @str_repeat(\"a\", 2);");
    expect![[r#"

        $_main:
             ; (lines=8, args=0, vars=0, tmps=2)
             ; (before optimizer)
             ; :1-1
             ; return  [] RANGE[0..0]
        0000 T0 = BEGIN_SILENCE
        0001 INIT_FCALL 2 112 string("str_repeat")
        0002 SEND_VAL string("a") 1
        0003 SEND_VAL int(2) 2
        0004 V1 = DO_ICALL
        0005 END_SILENCE T0
        0006 ECHO V1
        0007 RETURN int(1)
        LIVE RANGES:
             0: 0001 - 0005 (silence)
             1: 0005 - 0006 (tmp/var)
    "#]]
    .assert_eq(&unit.dump(&[]).text());
}

/// The rest of `zend_compile_silence` against the oracle's dump: a
/// variable under `@` is `zend_compile_simple_var_no_cv`'s
/// `FETCH_R (local)` over its name into a temporary and no CV, `$this`
/// among them; and `zend_do_free` steps back over the `END_SILENCE`,
/// so a call's `VAR` nothing reads is marked unused, a `$a++` becomes
/// `PRE_INC`, an `ASSIGN`'s temporary is marked unused, and a fetch's
/// temporary is freed after the end. Two `@` nest, each its own range;
/// `$b`, the one CV, moves every temporary's number up by one.
#[test]
fn a_variable_under_a_silence_is_fetched_by_name_and_its_value_freed_past_the_end() {
    expect![[r#"
        0000 T1 = BEGIN_SILENCE
        0001 T2 = FETCH_R (local) string("a")
        0002 END_SILENCE T1
        0003 ECHO T2
        0004 T3 = BEGIN_SILENCE
        0005 T4 = FETCH_R (local) string("this")
        0006 END_SILENCE T3
        0007 ECHO T4
        0008 T5 = BEGIN_SILENCE
        0009 T6 = FETCH_R (local) string("a")
        0010 END_SILENCE T5
        0011 FREE T6
        0012 T7 = BEGIN_SILENCE
        0013 INIT_FCALL 2 112 string("str_repeat")
        0014 SEND_VAL string("a") 1
        0015 SEND_VAL int(2) 2
        0016 DO_ICALL
        0017 END_SILENCE T7
        0018 T9 = BEGIN_SILENCE
        0019 PRE_INC CV0($b)
        0020 END_SILENCE T9
        0021 T11 = BEGIN_SILENCE
        0022 ASSIGN CV0($b) int(1)
        0023 END_SILENCE T11
        0024 T13 = BEGIN_SILENCE
        0025 T14 = BEGIN_SILENCE
        0026 T15 = FETCH_R (local) string("c")
        0027 END_SILENCE T14
        0028 END_SILENCE T13
        0029 FREE T15
        0030 RETURN int(1)
        LIVE RANGES:
             1: 0001 - 0002 (silence)
             2: 0002 - 0003 (tmp/var)
             3: 0005 - 0006 (silence)
             4: 0006 - 0007 (tmp/var)
             5: 0009 - 0010 (silence)
             6: 0010 - 0011 (tmp/var)
             7: 0013 - 0017 (silence)
             9: 0019 - 0020 (silence)
             11: 0022 - 0023 (silence)
             13: 0025 - 0028 (silence)
             14: 0026 - 0027 (silence)
             15: 0027 - 0029 (tmp/var)
    "#]]
    .assert_eq(&oplines_of(
        b"<?php echo @$a, @$this; @$a; @str_repeat(\"a\", 2); @$b++; @$b = 1; @@$c;",
    ));
    assert_eq!(
        refused(b"<?php class C { const X = @1; }"),
        Some(
            Refused {
                level: Level::CompileError,
                message: "Constant expression contains invalid operations".into(),
                line: 1,
                raised: Vec::new(),
                thrown: false,
                stage: crate::Stage::Compiling,
                declared: None,
                keys_made: 0,
            }
            .into()
        ),
        "`zend_is_allowed_in_const_expr` has no `ZEND_AST_SILENCE`"
    );
}

/// `global $a, $b;` is `zend_compile_global_var`'s `BIND_GLOBAL` per
/// variable, the CV in `op1` and its name in `op2`, where the
/// statement was refused as not read before; a name written out
/// under `${…}` is the same CV.
#[test]
fn a_global_statement_binds_each_cv_by_name() {
    expect![[r#"
        0000 BIND_GLOBAL CV0($a) string("a")
        0001 BIND_GLOBAL CV1($b) string("b")
        0002 ECHO CV0($a)
        0003 RETURN int(1)
    "#]]
    .assert_eq(&oplines_of(b"<?php global $a, ${'b'}; echo $a;"));
}

/// `zend_compile_compound_assign`, the `ZEND_AST_VAR` arm (#479):
/// the twelve operators over a plain variable, each one `ASSIGN_OP`
/// with the binary operator's own opcode in `extended_value`, which
/// the dump prints in brackets. The temporary is numbered for every
/// one of them and `zend_do_free` marks all but the last `UNUSED`,
/// so the one `$b` reads is `T15`. The dump is the oracle's under
/// `[regime.dump]` at 8.5.9.
#[test]
fn the_twelve_compound_assignments_to_a_variable_are_the_oracle_s_oplines() {
    expect![[r#"

        $_main:
             ; (lines=16, args=0, vars=2, tmps=15)
             ; (before optimizer)
             ; :1-1
             ; return  [] RANGE[0..0]
        0000 ASSIGN CV0($a) int(6)
        0001 ASSIGN_OP (ADD) CV0($a) int(1)
        0002 ASSIGN_OP (SUB) CV0($a) int(1)
        0003 ASSIGN_OP (MUL) CV0($a) int(2)
        0004 ASSIGN_OP (DIV) CV0($a) int(3)
        0005 ASSIGN_OP (MOD) CV0($a) int(5)
        0006 ASSIGN_OP (POW) CV0($a) int(2)
        0007 ASSIGN_OP (CONCAT) CV0($a) string("x")
        0008 ASSIGN_OP (BW_OR) CV0($a) int(3)
        0009 ASSIGN_OP (BW_XOR) CV0($a) int(1)
        0010 ASSIGN_OP (BW_AND) CV0($a) int(6)
        0011 ASSIGN_OP (SL) CV0($a) int(2)
        0012 ASSIGN_OP (SR) CV0($a) int(1)
        0013 T15 = ASSIGN_OP (ADD) CV0($a) int(2)
        0014 ASSIGN CV1($b) T15
        0015 RETURN int(1)
    "#]]
    .assert_eq(&dump_of(
        b"<?php $a = 6; $a += 1; $a -= 1; $a *= 2; $a /= 3; $a %= 5; $a **= 2; \
              $a .= \"x\"; $a |= 3; $a ^= 1; $a &= 6; $a <<= 2; $a >>= 1; $b = $a += 2;",
    ));
}

/// `$b = &$a` as the oracle dumps it (`zend_compile_assign_ref`):
/// `ASSIGN_REF` over the target's CV and the source's, the target
/// numbered first, its `VAR` marked unused where nothing reads it
/// and kept where an assignment, an `ECHO` or an operator does; the
/// same with the `&` spaced either way and with one CV as both.
///
/// What stands on the right is `zend_compile_var` under `BP_VAR_W`
/// with `by_ref`, so a dimension is the `FETCH_DIM_W` chain, a
/// property and a `static` property their `_W` fetches carrying
/// `ZEND_FETCH_REF`, and `$this` a `FETCH_THIS` into a `VAR`. The
/// target is held back while that compiles, which is why
/// `$a[1] = &$a[0]` fetches the source first and why `MAKE_REF`
/// stands between them wherever the target is no plain `$name`.
#[test]
#[allow(clippy::too_many_lines)] // one case per shape: the table is the test
fn a_by_reference_assignment_compiles_to_assign_ref_as_php_emits_it() {
    let cases: &[(&[u8], Expect)] = &[
        (
            b"<?php $b = &$a;",
            expect![[r#"

                $_main:
                     ; (lines=2, args=0, vars=2, tmps=1)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 ASSIGN_REF CV0($b) CV1($a)
                0001 RETURN int(1)
            "#]],
        ),
        (
            b"<?php $b =& $a;",
            expect![[r#"

                $_main:
                     ; (lines=2, args=0, vars=2, tmps=1)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 ASSIGN_REF CV0($b) CV1($a)
                0001 RETURN int(1)
            "#]],
        ),
        (
            b"<?php $a = &$a;",
            expect![[r#"

                $_main:
                     ; (lines=2, args=0, vars=1, tmps=1)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 ASSIGN_REF CV0($a) CV0($a)
                0001 RETURN int(1)
            "#]],
        ),
        (
            b"<?php $c = $b = &$a;",
            expect![[r#"

                $_main:
                     ; (lines=3, args=0, vars=3, tmps=2)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 V3 = ASSIGN_REF CV1($b) CV2($a)
                0001 ASSIGN CV0($c) V3
                0002 RETURN int(1)
            "#]],
        ),
        (
            b"<?php echo $b = &$a;",
            expect![[r#"

                $_main:
                     ; (lines=3, args=0, vars=2, tmps=1)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 V2 = ASSIGN_REF CV0($b) CV1($a)
                0001 ECHO V2
                0002 RETURN int(1)
            "#]],
        ),
        (
            b"<?php $x = ($b = &$a) + 1;",
            expect![[r#"

                $_main:
                     ; (lines=4, args=0, vars=3, tmps=3)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 V3 = ASSIGN_REF CV1($b) CV2($a)
                0001 T4 = ADD V3 int(1)
                0002 ASSIGN CV0($x) T4
                0003 RETURN int(1)
            "#]],
        ),
        (
            b"<?php $b = &$a; $a = 1; var_dump($b);",
            expect![[r#"

                $_main:
                     ; (lines=6, args=0, vars=2, tmps=3)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 ASSIGN_REF CV0($b) CV1($a)
                0001 ASSIGN CV1($a) int(1)
                0002 INIT_FCALL 1 96 string("var_dump")
                0003 SEND_VAR CV0($b) 1
                0004 DO_ICALL
                0005 RETURN int(1)
            "#]],
        ),
        // `zend_compile_var(BP_VAR_W, 1)` over the source: the
        // target's own fetch is held back until it has compiled, so
        // the source is numbered second and fetched first, with
        // `MAKE_REF` between the two (bug #71539).
        (
            b"<?php $a[1] = &$a[0];",
            expect![[r#"

                $_main:
                     ; (lines=5, args=0, vars=1, tmps=4)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 V2 = FETCH_DIM_W CV0($a) int(0)
                0001 V3 = MAKE_REF V2
                0002 V1 = FETCH_DIM_W CV0($a) int(1)
                0003 ASSIGN_REF V1 V3
                0004 RETURN int(1)
                LIVE RANGES:
                     3: 0002 - 0003 (tmp/var)
            "#]],
        ),
        (
            b"<?php $b = &$o->p;",
            expect![[r#"

                $_main:
                     ; (lines=3, args=0, vars=2, tmps=2)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 V2 = FETCH_OBJ_W (ref) CV1($o) string("p")
                0001 ASSIGN_REF CV0($b) V2
                0002 RETURN int(1)
            "#]],
        ),
        (
            b"<?php $b = &C::$s;",
            expect![[r#"

                $_main:
                     ; (lines=3, args=0, vars=1, tmps=2)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 V1 = FETCH_STATIC_PROP_W (ref) string("s") string("C")
                0001 ASSIGN_REF CV0($b) V1
                0002 RETURN int(1)
            "#]],
        ),
        // Only the outermost link of the chain is the reference's:
        // the `FETCH_OBJ_W` under it is `ZEND_FETCH_DIM_WRITE` as
        // any property under a dimension is.
        (
            b"<?php $b = &$o->p[0];",
            expect![[r#"

                $_main:
                     ; (lines=4, args=0, vars=2, tmps=3)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 V2 = FETCH_OBJ_W (dim write) CV1($o) string("p")
                0001 V3 = FETCH_DIM_W V2 int(0)
                0002 ASSIGN_REF CV0($b) V3
                0003 RETURN int(1)
            "#]],
        ),
        (
            b"<?php $b = &$this;",
            expect![[r#"

                $_main:
                     ; (lines=3, args=0, vars=1, tmps=2)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 V1 = FETCH_THIS
                0001 ASSIGN_REF CV0($b) V1
                0002 RETURN int(1)
            "#]],
        ),
        // A `static` property on the left is php's
        // `ASSIGN_STATIC_PROP_REF` arm, whose source rides in the
        // `OP_DATA` after it and is `MAKE_REF`'d like any other.
        (
            b"<?php C::$t = &$a[0];",
            expect![[r#"

                $_main:
                     ; (lines=5, args=0, vars=1, tmps=3)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 V2 = FETCH_DIM_W CV0($a) int(0)
                0001 V3 = MAKE_REF V2
                0002 ASSIGN_STATIC_PROP_REF string("t") string("C")
                0003 OP_DATA V3
                0004 RETURN int(1)
            "#]],
        ),
    ];
    for (source, expected) in cases {
        expected.assert_eq(&dump_of(source));
    }
    // `zend_is_call`: a call is the source php marks
    // `ZEND_RETURNS_FUNCTION`, and the name it resolves as the
    // program serves is `INIT_FCALL_BY_NAME`.
    expect![[r#"

        $_main:
             ; (lines=4, args=0, vars=1, tmps=2)
             ; (before optimizer)
             ; :1-1
             ; return  [] RANGE[0..0]
        0000 INIT_FCALL_BY_NAME 0 string("f")
        0001 V1 = DO_FCALL_BY_NAME
        0002 ASSIGN_REF (function) CV0($b) V1
        0003 RETURN int(1)
    "#]]
    .assert_eq(&dump_of(b"<?php $b = &f();"));
    // `zend_compile_var_inner`'s default arm under `BP_VAR_W`, and
    // the arm after `MAKE_REF` over a call php answered in a
    // temporary: php's own words for each.
    assert_eq!(
        refused_message(b"<?php $b = &count($x);").as_deref(),
        Some("Cannot use result of built-in function in write context")
    );
    for source in [
        b"<?php $b = &$a = 1;".as_slice(),
        b"<?php $b = &&$a;".as_slice(),
    ] {
        assert!(
            refused(source).is_some(),
            "{}",
            String::from_utf8_lossy(source)
        );
    }
}

/// `zend_compile_list_assign`'s oplines over the four shapes #583
/// names, as the oracle's `[regime.dump]` prints them: a plain list
/// reads the right side's copy once per element with `FETCH_LIST_R`
/// and frees it after the last, a keyed list reads by the key, a
/// nested list reads the element and then its elements, and an
/// element taken by reference is `FETCH_LIST_W` and `MAKE_REF` over
/// the right side made a reference; a `foreach` value that is a list
/// is fetched into a `VAR` and read the same way.
#[test]
#[allow(clippy::too_many_lines)] // one snapshot per case: the table is the test
fn a_list_compiles_to_what_zend_compile_list_assign_emits() {
    let cases: &[(&[u8], Expect)] = &[
        (
            b"<?php [$a, $b] = $x;",
            expect![[r#"

                $_main:
                     ; (lines=7, args=0, vars=3, tmps=5)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 T3 = QM_ASSIGN CV0($x)
                0001 V4 = FETCH_LIST_R T3 int(0)
                0002 ASSIGN CV1($a) V4
                0003 V6 = FETCH_LIST_R T3 int(1)
                0004 ASSIGN CV2($b) V6
                0005 FREE T3
                0006 RETURN int(1)
                LIVE RANGES:
                     3: 0001 - 0005 (tmp/var)
            "#]],
        ),
        (
            b"<?php list('k' => $a) = $x;",
            expect![[r#"

                $_main:
                     ; (lines=5, args=0, vars=2, tmps=3)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 T2 = QM_ASSIGN CV0($x)
                0001 V3 = FETCH_LIST_R T2 string("k")
                0002 ASSIGN CV1($a) V3
                0003 FREE T2
                0004 RETURN int(1)
                LIVE RANGES:
                     2: 0001 - 0003 (tmp/var)
            "#]],
        ),
        (
            b"<?php [$a, [$b, $c]] = $x;",
            expect![[r#"

                $_main:
                     ; (lines=11, args=0, vars=4, tmps=8)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 T4 = QM_ASSIGN CV0($x)
                0001 V5 = FETCH_LIST_R T4 int(0)
                0002 ASSIGN CV1($a) V5
                0003 V7 = FETCH_LIST_R T4 int(1)
                0004 V8 = FETCH_LIST_R V7 int(0)
                0005 ASSIGN CV2($b) V8
                0006 V10 = FETCH_LIST_R V7 int(1)
                0007 ASSIGN CV3($c) V10
                0008 FREE V7
                0009 FREE T4
                0010 RETURN int(1)
                LIVE RANGES:
                     4: 0001 - 0009 (tmp/var)
                     7: 0004 - 0008 (tmp/var)
            "#]],
        ),
        (
            b"<?php [&$a] = $x;",
            expect![[r#"

                $_main:
                     ; (lines=6, args=0, vars=2, tmps=4)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 V2 = MAKE_REF CV0($x)
                0001 V3 = FETCH_LIST_W V2 int(0)
                0002 V4 = MAKE_REF V3
                0003 ASSIGN_REF CV1($a) V4
                0004 FREE V2
                0005 RETURN int(1)
                LIVE RANGES:
                     2: 0001 - 0004 (tmp/var)
            "#]],
        ),
        (
            b"<?php foreach ($xs as $k => [$a, $b]) {}",
            expect![[r#"

                $_main:
                     ; (lines=11, args=0, vars=4, tmps=8)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 V4 = FE_RESET_R CV0($xs) 0009
                0001 T10 = FE_FETCH_R V4 V5 0009
                0002 V6 = FETCH_LIST_R V5 int(0)
                0003 ASSIGN CV1($a) V6
                0004 V8 = FETCH_LIST_R V5 int(1)
                0005 ASSIGN CV2($b) V8
                0006 FREE V5
                0007 ASSIGN CV3($k) T10
                0008 JMP 0001
                0009 FE_FREE V4
                0010 RETURN int(1)
                LIVE RANGES:
                     4: 0001 - 0009 (loop)
                     5: 0002 - 0006 (tmp/var)
                     10: 0002 - 0007 (tmp/var)
            "#]],
        ),
    ];
    for (source, expected) in cases {
        expected.assert_eq(&dump_of(source));
    }
}

/// php's compile errors over a list's shape, each in php's words.
#[test]
fn a_list_of_the_wrong_shape_is_phps_compile_error() {
    let cases: &[(&[u8], &str)] = &[
        (b"<?php [] = $x;", "Cannot use empty list"),
        (
            b"<?php [$a, 'k' => $b] = $x;",
            "Cannot mix keyed and unkeyed array entries in assignments",
        ),
        (
            b"<?php $x = [list($a)];",
            "Cannot use list() as standalone expression",
        ),
        (
            b"<?php [...$a] = $x;",
            "Spread operator is not supported in assignments",
        ),
        (
            b"<?php [array($a)] = $x;",
            "Cannot assign to array(), use [] instead",
        ),
        (b"<?php [list($a)] = $x;", "Cannot mix [] and list()"),
        (
            b"<?php ['k' => $a, , ] = $x;",
            "Cannot use empty array entries in keyed array assignment",
        ),
        (
            b"<?php [f()] = $x;",
            "Can't use function return value in write context",
        ),
        (
            b"<?php [1] = $x;",
            "Assignments can only happen to writable values",
        ),
        (
            b"<?php [&$a] = 1;",
            "Cannot assign reference to non referenceable value",
        ),
        (
            b"<?php foreach ($x as [$a] => $b) {}",
            "Cannot use list as key element",
        ),
    ];
    for (source, message) in cases {
        assert_eq!(
            refused(source).map(|e| e.message.to_string()),
            Some(String::from(*message)),
            "{}",
            String::from_utf8_lossy(source)
        );
    }
}

/// `zend_try_compile_cv` takes a CV only for a name php's tree holds as
/// `ZEND_AST_ZVAL`: `${1}` and `${'1'}` are the CV `$1`, while `${true}`
/// names it by `ZEND_AST_CONST`, which `zend_compile_simple_var_no_cv`
/// compiles, converts to a string and fetches by name, `FETCH_W (local)`
/// under the write and `FETCH_R (local)` under the read. The dump is the
/// oracle's under `[regime.dump]` at 8.5.9 (#2686).
#[test]
fn a_name_written_as_a_constant_other_than_a_string_is_fetched_by_name_not_a_cv() {
    expect![[r#"

        $_main:
             ; (lines=6, args=0, vars=1, tmps=3)
             ; (before optimizer)
             ; :1-1
             ; return  [] RANGE[0..0]
        0000 V1 = FETCH_W (local) string("1")
        0001 ASSIGN V1 int(1)
        0002 T3 = FETCH_R (local) string("1")
        0003 ECHO T3
        0004 ECHO CV0($1)
        0005 RETURN int(1)
    "#]]
    .assert_eq(&dump_of(
        b"<?php ${true} = 1; ${1}; ${'1'}; echo ${true}, ${1};",
    ));
}

/// #690's shapes under the oracle's `[regime.dump]`: a `global` over
/// a name no CV has is `FETCH_W (global+lock)` and an `ASSIGN_REF`
/// over a fetch by the same name, the local variable's or, for an
/// auto-global, the global's; `$$x++` is `FETCH_RW (local)` and the
/// step over the `VAR`.
#[test]
fn a_variable_named_at_run_time_is_fetched_by_name_as_php_emits_it() {
    let cases: &[(&[u8], Expect)] = &[
        (
            b"<?php global ${$x[0]};",
            expect![[r#"

                $_main:
                     ; (lines=5, args=0, vars=1, tmps=4)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 T1 = FETCH_DIM_R CV0($x) int(0)
                0001 V2 = FETCH_W (global+lock) T1
                0002 V3 = FETCH_W (local) T1
                0003 ASSIGN_REF V3 V2
                0004 RETURN int(1)
                LIVE RANGES:
                     1: 0001 - 0002 (tmp/var)
                     2: 0002 - 0003 (tmp/var)
            "#]],
        ),
        (
            b"<?php global $_SERVER;",
            expect![[r#"

                $_main:
                     ; (lines=4, args=0, vars=0, tmps=3)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 V0 = FETCH_W (global+lock) string("_SERVER")
                0001 V1 = FETCH_W (global) string("_SERVER")
                0002 ASSIGN_REF V1 V0
                0003 RETURN int(1)
                LIVE RANGES:
                     0: 0001 - 0002 (tmp/var)
            "#]],
        ),
        (
            b"<?php $$x++;",
            expect![[r#"

                $_main:
                     ; (lines=3, args=0, vars=1, tmps=2)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 V1 = FETCH_RW (local) CV0($x)
                0001 PRE_INC V1
                0002 RETURN int(1)
            "#]],
        ),
    ];
    for (source, expected) in cases {
        expected.assert_eq(&dump_of(source));
    }
}

/// #633's shapes under the oracle's `[regime.dump]`: an auto-global
/// under an element's write is `FETCH_W (global)`, under a compound
/// assignment `FETCH_RW (global)`, under an `unset` of its element
/// `FETCH_UNSET (global)`; `isset` and `empty` over a variable fetched
/// by name are `ISSET_ISEMPTY_VAR`, and `unset` over one `UNSET_VAR`.
#[test]
fn an_auto_global_is_fetched_by_name_from_the_global_table_as_php_emits_it() {
    let cases: &[(&[u8], Expect)] = &[
        (
            b"<?php $_SERVER[\"x\"] = 1;",
            expect![[r#"

                $_main:
                     ; (lines=4, args=0, vars=0, tmps=2)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 V0 = FETCH_W (global) string("_SERVER")
                0001 ASSIGN_DIM V0 string("x")
                0002 OP_DATA int(1)
                0003 RETURN int(1)
            "#]],
        ),
        (
            b"<?php $_SERVER[\"n\"] .= \"a\";",
            expect![[r#"

                $_main:
                     ; (lines=4, args=0, vars=0, tmps=2)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 V0 = FETCH_RW (global) string("_SERVER")
                0001 ASSIGN_DIM_OP (CONCAT) V0 string("n")
                0002 OP_DATA string("a")
                0003 RETURN int(1)
            "#]],
        ),
        (
            b"<?php unset($_SERVER[\"x\"]);",
            expect![[r#"

                $_main:
                     ; (lines=3, args=0, vars=0, tmps=1)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 V0 = FETCH_UNSET (global) string("_SERVER")
                0001 UNSET_DIM V0 string("x")
                0002 RETURN int(1)
            "#]],
        ),
        (
            b"<?php $a = isset($_SERVER) && empty($$a);",
            expect![[r#"

                $_main:
                     ; (lines=6, args=0, vars=1, tmps=3)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 T1 = ISSET_ISEMPTY_VAR (global) (isset) string("_SERVER")
                0001 T1 = JMPZ_EX T1 0004
                0002 T2 = ISSET_ISEMPTY_VAR (local) (empty) CV0($a)
                0003 T1 = BOOL T2
                0004 ASSIGN CV0($a) T1
                0005 RETURN int(1)
            "#]],
        ),
        (
            b"<?php unset($$x, $_ENV);",
            expect![[r#"

                $_main:
                     ; (lines=3, args=0, vars=1, tmps=0)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 UNSET_VAR (local) CV0($x)
                0001 UNSET_VAR (global) string("_ENV")
                0002 RETURN int(1)
            "#]],
        ),
    ];
    for (source, expected) in cases {
        expected.assert_eq(&dump_of(source));
    }
}

/// `$GLOBALS` under the oracle's `[regime.dump]`: read whole it is
/// `FETCH_GLOBALS` into a temporary; `$GLOBALS['a']` is the fetch by
/// name over `'a'` from the global table wherever it stands, the
/// variable of an `ASSIGN`, the base of a dimension's compound
/// assignment, `UNSET_VAR` and `ISSET_ISEMPTY_VAR`; `isset($GLOBALS)`
/// is `true` before the request runs.
#[test]
fn globals_is_a_copy_of_the_table_and_its_element_a_fetch_by_name_as_php_emits_it() {
    let cases: &[(&[u8], Expect)] = &[
        (
            b"<?php $x = $GLOBALS;",
            expect![[r#"

                $_main:
                     ; (lines=3, args=0, vars=1, tmps=2)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 T1 = FETCH_GLOBALS
                0001 ASSIGN CV0($x) T1
                0002 RETURN int(1)
            "#]],
        ),
        (
            b"<?php $GLOBALS[\"a\"] = 1;",
            expect![[r#"

                $_main:
                     ; (lines=3, args=0, vars=0, tmps=2)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 V0 = FETCH_W (global) string("a")
                0001 ASSIGN V0 int(1)
                0002 RETURN int(1)
            "#]],
        ),
        (
            b"<?php $GLOBALS[\"a\"][\"b\"] .= \"c\";",
            expect![[r#"

                $_main:
                     ; (lines=4, args=0, vars=0, tmps=2)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 V0 = FETCH_RW (global) string("a")
                0001 ASSIGN_DIM_OP (CONCAT) V0 string("b")
                0002 OP_DATA string("c")
                0003 RETURN int(1)
            "#]],
        ),
        (
            b"<?php unset($GLOBALS[\"a\"]);",
            expect![[r#"

                $_main:
                     ; (lines=2, args=0, vars=0, tmps=0)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 UNSET_VAR (global) string("a")
                0001 RETURN int(1)
            "#]],
        ),
        (
            b"<?php $b = isset($GLOBALS[\"a\"]) || isset($GLOBALS);",
            expect![[r#"

                $_main:
                     ; (lines=5, args=0, vars=1, tmps=2)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 T1 = ISSET_ISEMPTY_VAR (global) (isset) string("a")
                0001 T1 = JMPNZ_EX T1 0003
                0002 T1 = BOOL bool(true)
                0003 ASSIGN CV0($b) T1
                0004 RETURN int(1)
            "#]],
        ),
    ];
    for (source, expected) in cases {
        expected.assert_eq(&dump_of(source));
    }
}

/// `zend_is_auto_global` arms what the compiler meets, in the order
/// it meets it, once, whether or not the line runs: the unit carries
/// that order ([`Unit::arms`]), which is the order php lays the
/// variables made just in time into `EG(symbol_table)`.
#[test]
fn the_unit_arms_each_auto_global_it_names_in_the_order_it_met_them() {
    let (unit, _strings) = unit_and_strings(
        b"<?php if (0) { $_REQUEST; } $_ENV['k'] ?? 1; \"$_REQUEST[x]\"; $n = '_GET'; $$n;",
    );
    assert_eq!(unit.arms, [AutoGlobal::Request, AutoGlobal::Env]);
}
