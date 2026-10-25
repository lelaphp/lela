//! Interpolated strings: the cast, the fast concat and the rope, their
//! pieces, the string offset spellings and the variable variable.

use super::*;

/// #303's program under the oracle's `[regime.dump]`: a five-piece
/// interpolation as `zend_compile_encaps_list` shapes it, `ROPE_INIT`
/// with the count, three `ROPE_ADD` and `ROPE_END`, the rope's
/// temporary taken after the value's and two more slots reserved
/// for its five string pointers, and the `rope` live range from the
/// init to the end, read off a dump of the oracle's for the first
/// time.
#[test]
fn a_three_part_interpolation_is_php_s_op_array_ending_in_its_rope_live_range() {
    let (unit, _strings) = unit_and_strings(b"<?php $a = 1; $b = 2; echo \"$a-$b-$a\";");
    expect![[r#"

        $_main:
             ; (lines=9, args=0, vars=2, tmps=6)
             ; (before optimizer)
             ; :1-1
             ; return  [] RANGE[0..0]
        0000 ASSIGN CV0($a) int(1)
        0001 ASSIGN CV1($b) int(2)
        0002 T5 = ROPE_INIT 5 CV0($a)
        0003 T5 = ROPE_ADD 1 T5 string("-")
        0004 T5 = ROPE_ADD 2 T5 CV1($b)
        0005 T5 = ROPE_ADD 3 T5 string("-")
        0006 T4 = ROPE_END 4 T5 CV0($a)
        0007 ECHO T4
        0008 RETURN int(1)
        LIVE RANGES:
             5: 0002 - 0006 (rope)
    "#]]
    .assert_eq(&unit.dump(&[]).text());
}

/// The rest of `zend_compile_encaps_list` and `zend_compile_rope_finalize`
/// against the oracle's dump: one piece is a `CAST (string)` and no
/// rope; two are a `FAST_CONCAT` over the `NOP` the first piece's
/// opline becomes, and no live range; a constant piece is folded onto
/// the constant before it, so the escaped `\{` and the plain `$`,
/// `{` and `}` are one string; `$this` is a `FETCH_THIS` piece; a
/// rope's value nothing reads is freed after its end; and `${a}` is
/// php's deprecation at compile time. In a class constant an
/// interpolation is php's `Constant expression contains invalid
/// operations`, since `zend_is_allowed_in_const_expr` has no
/// `ZEND_AST_ENCAPS_LIST`.
#[test]
fn one_and_two_piece_interpolations_are_a_cast_and_a_fast_concat_and_the_rest_a_rope() {
    expect![[r#"
        0000 T4 = CAST (string) CV1($a)
        0001 INIT_FCALL 2 112 string("str_repeat")
        0002 NOP
        0003 T5 = FAST_CONCAT string("p") CV1($a)
        0004 SEND_VAL T5 1
        0005 NOP
        0006 T6 = FAST_CONCAT CV1($a) CV1($a)
        0007 SEND_VAL T6 2
        0008 V7 = DO_ICALL
        0009 T8 = CONCAT T4 V7
        0010 ASSIGN CV0($x) T8
        0011 T11 = ROPE_INIT 3 CV1($a)
        0012 T11 = ROPE_ADD 1 T11 CV2($b)
        0013 T10 = ROPE_END 2 T11 CV3($c)
        0014 FREE T10
        0015 NOP
        0016 T13 = FAST_CONCAT string("x") CV1($a)
        0017 FREE T13
        0018 T16 = ROPE_INIT 6 string("a\\{")
        0019 T16 = ROPE_ADD 1 T16 CV1($a)
        0020 T16 = ROPE_ADD 2 T16 string("}b$x{ ")
        0021 T16 = ROPE_ADD 3 T16 CV1($a)
        0022 T16 = ROPE_ADD 4 T16 string("}$ $1")
        0023 T14 = FETCH_THIS
        0024 T15 = ROPE_END 5 T16 T14
        0025 ECHO T15
        0026 RETURN int(1)
        LIVE RANGES:
             4: 0001 - 0009 (tmp/var)
             11: 0011 - 0013 (rope)
             16: 0018 - 0024 (rope)
    "#]].assert_eq(&oplines_of(
            b"<?php $x = \"{$a}\" . str_repeat(\"p$a\", \"$a$a\"); \"$a$b$c\"; \"x$a\"; echo \"a\\{$a}b\\$x{ $a}$ $1{$this}\";"
        ));
    let (unit, _strings) = unit_and_strings(b"<?php\necho \"{$a}x${a}\";");
    assert_eq!(
        unit.diagnostics,
        vec![Diagnostic {
            level: Level::Deprecated,
            line: 2,
            message: "Using ${var} in strings is deprecated, use {$var} instead".into(),
        }]
    );
    assert_eq!(
        refused(b"<?php class C { const X = \"$a\"; }"),
        Some(
            Refused {
                level: Level::CompileError,
                message: "Constant expression contains invalid operations".into(),
                line: 1,
                raised: Vec::new(),
                raise: crate::Raise::Fatal,
                stage: crate::Stage::Compiling,
                declared: None,
                keys_made: 0,
            }
            .into()
        ),
        "`zend_is_allowed_in_const_expr` has no `ZEND_AST_ENCAPS_LIST`"
    );
}

/// #356's program under the oracle's `[regime.dump]`: a dimension
/// piece is `zend_compile_dim` under `BP_VAR_R`, `FETCH_DIM_R` over
/// the CV and the offset as `encaps_var_offset` read it, an integer
/// for `0` and a string for `k`; a property piece is
/// `zend_compile_prop`, `FETCH_OBJ_R`; and `{$a[0]}` and `{$o->p}`
/// are the same two; each piece's opline before its `ROPE_ADD`, the
/// rope's temporary taken after every piece's and the live range
/// from the init to the end.
#[test]
fn a_dimension_and_a_property_piece_are_fetched_before_their_rope_add() {
    let (unit, _strings) = unit_and_strings(
        b"<?php $a = [1]; $o = new stdClass; echo \"$a[0]-$a[k]-$o->p-{$a[0]}-{$o->p}\";",
    );
    expect![[r#"

        $_main:
             ; (lines=20, args=0, vars=2, tmps=15)
             ; (before optimizer)
             ; :1-1
             ; return  [] RANGE[0..0]
        0000 ASSIGN CV0($a) array(...)
        0001 V3 = NEW 0 string("stdClass")
        0002 DO_FCALL
        0003 ASSIGN CV1($o) V3
        0004 T6 = FETCH_DIM_R CV0($a) int(0)
        0005 T12 = ROPE_INIT 9 T6
        0006 T12 = ROPE_ADD 1 T12 string("-")
        0007 T7 = FETCH_DIM_R CV0($a) string("k")
        0008 T12 = ROPE_ADD 2 T12 T7
        0009 T12 = ROPE_ADD 3 T12 string("-")
        0010 T8 = FETCH_OBJ_R CV1($o) string("p")
        0011 T12 = ROPE_ADD 4 T12 T8
        0012 T12 = ROPE_ADD 5 T12 string("-")
        0013 T9 = FETCH_DIM_R CV0($a) int(0)
        0014 T12 = ROPE_ADD 6 T12 T9
        0015 T12 = ROPE_ADD 7 T12 string("-")
        0016 T10 = FETCH_OBJ_R CV1($o) string("p")
        0017 T11 = ROPE_END 8 T12 T10
        0018 ECHO T11
        0019 RETURN int(1)
        LIVE RANGES:
             3: 0002 - 0003 (new)
             12: 0005 - 0017 (rope)
    "#]]
    .assert_eq(&unit.dump(&[]).text());
}

/// The rest of `encaps_var` against the oracle's dump: the offset
/// negated as `zend_negate_num_string` negates it, `-0` and a
/// decimal past `ZEND_LONG_MAX` string keys; a method call under
/// `{…}` its `INIT_METHOD_CALL` and `DO_FCALL` before the `CAST`;
/// `${a[0]}` a `FETCH_DIM_R` with php's `${var}` deprecation on the
/// unit; and `${expr}` `zend_compile_simple_var` with the variable
/// variables one: `${$b}` and `${$b[0]}` a `FETCH_R (local)` over
/// the value, `${'a'}`, `${1}` and `${'a' . 'b'}` the CV php's
/// parser names, `${'_GET'}` the auto-global's `FETCH_R (global)`.
#[test]
fn the_string_offset_spellings_and_the_variable_variable_are_php_s() {
    expect![[r#"
        0000 ASSIGN CV0($a) array(...)
        0001 T4 = FETCH_DIM_R CV0($a) int(-1)
        0002 T8 = ROPE_INIT 3 T4
        0003 T5 = FETCH_DIM_R CV0($a) string("-0")
        0004 T8 = ROPE_ADD 1 T8 T5
        0005 T6 = FETCH_DIM_R CV0($a) string("9223372036854775808")
        0006 T7 = ROPE_END 2 T8 T6
        0007 ECHO T7
        0008 INIT_METHOD_CALL 0 CV1($o) string("m")
        0009 V10 = DO_FCALL
        0010 T11 = CAST (string) V10
        0011 ECHO T11
        0012 T12 = FETCH_DIM_R CV0($a) CV2($i)
        0013 T13 = CAST (string) T12
        0014 ECHO T13
        0015 RETURN int(1)
        LIVE RANGES:
             8: 0002 - 0006 (rope)
    "#]].assert_eq(&oplines_of(
            b"<?php $a = [1]; echo \"$a[-1]$a[-0]$a[9223372036854775808]\"; echo \"{$o->m()}\"; echo \"$a[$i]\";"
        ));
    let source = b"<?php\necho \"${a[0]}\";";
    let (unit, _strings) = unit_and_strings(source);
    expect![[r#"
        0000 T1 = FETCH_DIM_R CV0($a) int(0)
        0001 T2 = CAST (string) T1
        0002 ECHO T2
        0003 RETURN int(1)
    "#]]
    .assert_eq(&oplines_of(source));
    assert_eq!(
        unit.diagnostics,
        vec![Diagnostic {
            level: Level::Deprecated,
            line: 2,
            message: "Using ${var} in strings is deprecated, use {$var} instead".into(),
        }]
    );
    let source = b"<?php\necho \"${$b}${$b[0]}${'a'}${1}${'a' . 'b'}${'_GET'}${'this'}\";";
    let (unit, _strings) = unit_and_strings(source);
    expect![[r#"
        0000 T4 = FETCH_R (local) CV0($b)
        0001 T10 = ROPE_INIT 7 T4
        0002 T5 = FETCH_DIM_R CV0($b) int(0)
        0003 T6 = FETCH_R (local) T5
        0004 T10 = ROPE_ADD 1 T10 T6
        0005 T10 = ROPE_ADD 2 T10 CV1($a)
        0006 T10 = ROPE_ADD 3 T10 CV2($1)
        0007 T10 = ROPE_ADD 4 T10 CV3($ab)
        0008 T7 = FETCH_R (global) string("_GET")
        0009 T10 = ROPE_ADD 5 T10 T7
        0010 T8 = FETCH_THIS
        0011 T9 = ROPE_END 6 T10 T8
        0012 ECHO T9
        0013 RETURN int(1)
        LIVE RANGES:
             10: 0001 - 0011 (rope)
    "#]]
    .assert_eq(&oplines_of(source));
    assert_eq!(
        unit.diagnostics.len(),
        7,
        "one variable-variables deprecation per `${{expr}}` piece"
    );
    assert_eq!(
        unit.diagnostics.first(),
        Some(&Diagnostic {
            level: Level::Deprecated,
            line: 2,
            message:
                "Using ${expr} (variable variables) in strings is deprecated, use {${expr}} instead"
                    .into(),
        })
    );
}

/// `zend_delayed_compile_prop` under `zend_compile_encaps_list`
/// (#2676): `"$this->p"` inside a method reads the property off the
/// `IS_UNUSED` receiver, `FETCH_OBJ_R THIS`, as `"{$this->p}"` does.
#[test]
fn this_s_property_in_a_string_is_fetched_off_this() {
    expect![[r#"

        $_main:
             ; (lines=1, args=0, vars=0, tmps=0)
             ; (before optimizer)
             ; :1-5
             ; return  [] RANGE[0..0]
        0000 RETURN int(1)

        C::m:
             ; (lines=7, args=0, vars=0, tmps=5)
             ; (before optimizer)
             ; :3-3
             ; return  [] RANGE[0..0]
        0000 T0 = FETCH_OBJ_R THIS string("p")
        0001 T3 = ROPE_INIT 3 T0
        0002 T3 = ROPE_ADD 1 T3 string(" ")
        0003 T1 = FETCH_OBJ_R THIS string("p")
        0004 T2 = ROPE_END 2 T3 T1
        0005 RETURN T2
        0006 RETURN null
        LIVE RANGES:
             3: 0001 - 0004 (rope)
    "#]]
    .assert_eq(&dump_of(
        b"<?php
class C {
    public function m() { return \"$this->p {$this->p}\"; }
}
",
    ));
}
