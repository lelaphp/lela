//! Dimensions: reads and writes of an element, the delayed fetch at a
//! chain's base, compound assignments to a dimension and a property,
//! and what no write can stand on.

use super::*;

/// `++` and `--` over an element and over a property, which were
/// refused as over what is no variable, as the oracle's
/// `[regime.dump]` has them: an element is `FETCH_DIM_RW` per link
/// and the step over the last `VAR`, a property is the last fetch
/// turned into `PRE_INC_OBJ` and its kin, and a `POST_*` nothing
/// reads is the `PRE_*` `zend_do_free` makes of it.
#[test]
fn a_step_over_an_element_or_a_property_is_the_fetch_under_rw() {
    assert_eq!(
        oplines_of(
            b"<?php $a[0]++; $x = $a[0][1]--; $o->p++; $y = --$o->q->r; ++$a[0]->p; $b[]++;"
        ),
        "0000 V5 = FETCH_DIM_RW CV0($a) int(0)\n\
         0001 PRE_INC V5\n\
         0002 V7 = FETCH_DIM_RW CV0($a) int(0)\n\
         0003 V8 = FETCH_DIM_RW V7 int(1)\n\
         0004 T9 = POST_DEC V8\n\
         0005 ASSIGN CV1($x) T9\n\
         0006 PRE_INC_OBJ CV2($o) string(\"p\")\n\
         0007 V12 = FETCH_OBJ_RW CV2($o) string(\"q\")\n\
         0008 T13 = PRE_DEC_OBJ V12 string(\"r\")\n\
         0009 ASSIGN CV3($y) T13\n\
         0010 V15 = FETCH_DIM_RW CV0($a) int(0)\n\
         0011 PRE_INC_OBJ V15 string(\"p\")\n\
         0012 V17 = FETCH_DIM_RW CV4($b) NEXT\n\
         0013 PRE_INC V17\n\
         0014 RETURN int(1)\n"
    );
}

/// `zend_delayed_compile_dim` over `$this` as the oracle at 8.5.9
/// dumps it (#2574): the container is `zend_compile_simple_var`'s
/// `FETCH_THIS` into a temporary, in a method and in the main op
/// array alike, and never the `UNUSED` receiver a property fetch
/// takes, which `ISSET_ISEMPTY_DIM_OBJ` and `FETCH_DIM_IS` do not
/// list.
#[test]
fn this_under_a_silent_dimension_is_fetched_into_a_temporary() {
    assert_eq!(
        dump_of(
            b"<?php class A { function f() { return [isset($this[0]), empty($this[0]), isset($this[0][1]), $this[0] ?? 2]; } } var_dump(isset($this[0]));"
        ),
        "\n\
         $_main:\n\
         \x20    ; (lines=6, args=0, vars=0, tmps=3)\n\
         \x20    ; (before optimizer)\n\
         \x20    ; :1-1\n\
         \x20    ; return  [] RANGE[0..0]\n\
         0000 INIT_FCALL 1 96 string(\"var_dump\")\n\
         0001 T0 = FETCH_THIS\n\
         0002 T1 = ISSET_ISEMPTY_DIM_OBJ (isset) T0 int(0)\n\
         0003 SEND_VAL T1 1\n\
         0004 DO_ICALL\n\
         0005 RETURN int(1)\n\
         \n\
         A::f:\n\
         \x20    ; (lines=17, args=0, vars=0, tmps=11)\n\
         \x20    ; (before optimizer)\n\
         \x20    ; :1-1\n\
         \x20    ; return  [] RANGE[0..0]\n\
         0000 T0 = FETCH_THIS\n\
         0001 T1 = ISSET_ISEMPTY_DIM_OBJ (isset) T0 int(0)\n\
         0002 T2 = INIT_ARRAY 4 (packed) T1 NEXT\n\
         0003 T3 = FETCH_THIS\n\
         0004 T4 = ISSET_ISEMPTY_DIM_OBJ (empty) T3 int(0)\n\
         0005 T2 = ADD_ARRAY_ELEMENT T4 NEXT\n\
         0006 T5 = FETCH_THIS\n\
         0007 T6 = FETCH_DIM_IS T5 int(0)\n\
         0008 T7 = ISSET_ISEMPTY_DIM_OBJ (isset) T6 int(1)\n\
         0009 T2 = ADD_ARRAY_ELEMENT T7 NEXT\n\
         0010 T8 = FETCH_THIS\n\
         0011 T9 = FETCH_DIM_IS T8 int(0)\n\
         0012 T10 = COALESCE T9 0014\n\
         0013 T10 = QM_ASSIGN int(2)\n\
         0014 T2 = ADD_ARRAY_ELEMENT T10 NEXT\n\
         0015 RETURN T2\n\
         0016 RETURN null\n\
         LIVE RANGES:\n\
         \x20    2: 0003 - 0015 (tmp/var)\n"
    );
}

/// `zend_delayed_compile_var` as the base of a chain of dimensions
/// (#451), the oracle's dump under `[regime.dump]` at 8.5.9: a
/// property is the one fetch php holds back, so its `FETCH_OBJ_R`
/// stands after the oplines of whatever the dimension compiled to,
/// which is what puts `f`'s call before the fetch of `p` in
/// `$o->p[f()]` while the fetch keeps the lower temporary; under a
/// write it is `FETCH_OBJ_W` carrying `ZEND_FETCH_DIM_WRITE`, the
/// bit the dump prints as `(dim write)`. A call is compiled where
/// it stands and takes `SEPARATE` over its own `VAR` under a write
/// (`zend_separate_if_call_and_write`), which the dump prints into
/// the same `VAR` it reads; `$g()` is php's `ZEND_AST_CALL` the
/// same as `f()` is, so `zend_is_call` separates it the same way
/// (#336).
#[test]
fn the_base_of_a_chain_of_dimensions_is_the_oracle_s_delayed_fetch() {
    let source: &[u8] = b"<?php $o = new stdClass; $x = $o->p[0]; $o->p[1] = 2; \
                          $y = f()[0]; f()[1] = 3; $z = $o->p[f()]; $g()[2] = 4; \
                          function f() { return [1]; }";
    assert_eq!(
        dump_of(source),
        main_dump(
            29,
            5,
            19,
            "0000 V5 = NEW 0 string(\"stdClass\")\n\
             0001 DO_FCALL\n\
             0002 ASSIGN CV0($o) V5\n\
             0003 T8 = FETCH_OBJ_R CV0($o) string(\"p\")\n\
             0004 T9 = FETCH_DIM_R T8 int(0)\n\
             0005 ASSIGN CV1($x) T9\n\
             0006 V11 = FETCH_OBJ_W (dim write) CV0($o) string(\"p\")\n\
             0007 ASSIGN_DIM V11 int(1)\n\
             0008 OP_DATA int(2)\n\
             0009 INIT_FCALL_BY_NAME 0 string(\"f\")\n\
             0010 V13 = DO_FCALL_BY_NAME\n\
             0011 T14 = FETCH_DIM_R V13 int(0)\n\
             0012 ASSIGN CV2($y) T14\n\
             0013 INIT_FCALL_BY_NAME 0 string(\"f\")\n\
             0014 V16 = DO_FCALL_BY_NAME\n\
             0015 V16 = SEPARATE V16\n\
             0016 ASSIGN_DIM V16 int(1)\n\
             0017 OP_DATA int(3)\n\
             0018 INIT_FCALL_BY_NAME 0 string(\"f\")\n\
             0019 V19 = DO_FCALL_BY_NAME\n\
             0020 T18 = FETCH_OBJ_R CV0($o) string(\"p\")\n\
             0021 T20 = FETCH_DIM_R T18 V19\n\
             0022 ASSIGN CV3($z) T20\n\
             0023 INIT_DYNAMIC_CALL 0 CV4($g)\n\
             0024 V22 = DO_FCALL\n\
             0025 V22 = SEPARATE V22\n\
             0026 ASSIGN_DIM V22 int(2)\n\
             0027 OP_DATA int(4)\n\
             0028 RETURN int(1)\n\
             LIVE RANGES:\n\
             \x20    5: 0001 - 0002 (new)\n\
             \x20    16: 0015 - 0016 (tmp/var)\n\
             \x20    19: 0020 - 0021 (tmp/var)\n\
             \x20    22: 0025 - 0026 (tmp/var)\n"
        ) + "\nf:\n     ; (lines=2, args=0, vars=0, tmps=0)\n\
             \x20    ; (before optimizer)\n     ; :1-1\n     ; return  [] RANGE[0..0]\n\
             0000 RETURN array(...)\n\
             0001 RETURN null\n"
    );
}

/// The bases php refuses under a write, `zend_compile_var`'s
/// default arm and `zend_separate_if_call_and_write`: a temporary
/// expression and a call php answered in one, each php's own
/// compile error, under a dimension and under a property alike
/// (#336); and the one this compiler has not, a `static`
/// property under a write, whose fetch is `FETCH_STATIC_PROP_W`
/// (#507). `new C` is no call however its `VAR` is written, since
/// `zend_is_call` reads the shape of the expression and not where
/// its value landed.
#[test]
fn a_base_no_write_can_stand_on_is_php_s_compile_error() {
    let refused = |source: &[u8]| refused_message(source);
    assert_eq!(
        refused(b"<?php (new C)[0] = 1;").as_deref(),
        Some("Cannot use temporary expression in write context")
    );
    assert_eq!(
        refused(b"<?php \"ab\"[0] = 1;").as_deref(),
        Some("Cannot use temporary expression in write context")
    );
    assert_eq!(
        refused(b"<?php strlen(\"a\")[0] = 1;").as_deref(),
        Some("Cannot use result of built-in function in write context")
    );
    assert_eq!(
        refused(b"<?php (new C)->p = 1;").as_deref(),
        Some("Cannot use temporary expression in write context")
    );
    assert_eq!(
        refused(b"<?php (1)->p = 1;").as_deref(),
        Some("Cannot use temporary expression in write context")
    );
    assert_eq!(
        refused(b"<?php [1, 2]->p = 1;").as_deref(),
        Some("Cannot use temporary expression in write context")
    );
    assert_eq!(
        refused(b"<?php strlen(\"a\")->p = 1;").as_deref(),
        Some("Cannot use result of built-in function in write context")
    );
    assert_eq!(
        oplines_of(b"<?php C::$s[0] += 1; C::$s[1]++;"),
        "0000 V0 = FETCH_STATIC_PROP_RW string(\"s\") string(\"C\")\n\
         0001 ASSIGN_DIM_OP (ADD) V0 int(0)\n\
         0002 OP_DATA int(1)\n\
         0003 V2 = FETCH_STATIC_PROP_RW string(\"s\") string(\"C\")\n\
         0004 V3 = FETCH_DIM_RW V2 int(1)\n\
         0005 PRE_INC V3\n\
         0006 RETURN int(1)\n"
    );
}

/// `zend_compile_compound_assign`, the `ZEND_AST_DIM` and
/// `ZEND_AST_PROP` arms (#498): the chain's last delayed opline and
/// the delayed property fetch, each with its opcode set to
/// `ASSIGN_DIM_OP` or `ASSIGN_OBJ_OP`, the operator where the
/// dimension has no cache slot and where the property's slot stood,
/// and an `OP_DATA` after it carrying the value; the property's
/// three slots move onto that `OP_DATA`, which the dump does not
/// print. `$a[] .= "x"` is the append, `NEXT` in the dump, and the
/// one the `$b` reads is the temporary `zend_do_free` leaves
/// standing. The dump is the oracle's under `[regime.dump]` at
/// 8.5.9.
#[test]
fn a_compound_assignment_to_a_dimension_and_to_a_property_are_the_oracle_s_oplines() {
    assert_eq!(
        dump_of(b"<?php $a = [1]; $a[0] += 1; $a[] .= \"x\"; $o->p |= 3; $b = $a[2] **= 2;"),
        main_dump(
            11,
            3,
            6,
            "0000 ASSIGN CV0($a) array(...)\n\
             0001 ASSIGN_DIM_OP (ADD) CV0($a) int(0)\n\
             0002 OP_DATA int(1)\n\
             0003 ASSIGN_DIM_OP (CONCAT) CV0($a) NEXT\n\
             0004 OP_DATA string(\"x\")\n\
             0005 ASSIGN_OBJ_OP (BW_OR) CV1($o) string(\"p\")\n\
             0006 OP_DATA int(3)\n\
             0007 T7 = ASSIGN_DIM_OP (POW) CV0($a) int(2)\n\
             0008 OP_DATA int(2)\n\
             0009 ASSIGN CV2($b) T7\n\
             0010 RETURN int(1)\n\
             LIVE RANGES:\n\
             \x20    7: 0008 - 0009 (tmp/var)\n"
        )
    );
}

/// `zend_adjust_for_fetch_type` under `BP_VAR_RW` (#504): every
/// link of a compound assignment's target but the last is
/// `FETCH_DIM_RW` or `FETCH_OBJ_RW`, the `_W` opcode plus one, into
/// a `VAR` the link over it stands on. The dump is the oracle's
/// under `[regime.dump]` at 8.5.9.
#[test]
fn the_links_under_a_compound_assignment_s_last_are_the_oracle_s_rw_fetches() {
    assert_eq!(
        dump_of(b"<?php $a = [[1]]; $a[0][1] *= 2; $o->p->q += 1;"),
        main_dump(
            8,
            2,
            5,
            "0000 ASSIGN CV0($a) array(...)\n\
             0001 V3 = FETCH_DIM_RW CV0($a) int(0)\n\
             0002 ASSIGN_DIM_OP (MUL) V3 int(1)\n\
             0003 OP_DATA int(2)\n\
             0004 V5 = FETCH_OBJ_RW CV1($o) string(\"p\")\n\
             0005 ASSIGN_OBJ_OP (ADD) V5 string(\"q\")\n\
             0006 OP_DATA int(1)\n\
             0007 RETURN int(1)\n"
        )
    );
}

/// `zend_delayed_compile_var`'s last two bases under a write
/// (#507): a `static` property is `FETCH_STATIC_PROP_W` carrying
/// `ZEND_FETCH_DIM_WRITE`, the bit the dump prints as `(dim write)`,
/// and `$this` is `FETCH_THIS` into the `VAR` the dimension stands
/// on, never the `UNUSED` receiver a property fetch takes. The dump
/// is the oracle's under `[regime.dump]` at 8.5.9.
#[test]
fn a_static_property_and_this_under_a_written_dimension_are_the_oracle_s_bases() {
    assert_eq!(
        dump_of(b"<?php class C { public static $s = [1]; } C::$s[0] = 1;"),
        main_dump(
            4,
            0,
            2,
            "0000 V0 = FETCH_STATIC_PROP_W (dim write) string(\"s\") string(\"C\")\n\
             0001 ASSIGN_DIM V0 int(0)\n\
             0002 OP_DATA int(1)\n\
             0003 RETURN int(1)\n"
        )
    );
    assert_eq!(
        dump_of(b"<?php $this[0] = 1;"),
        main_dump(
            4,
            0,
            2,
            "0000 V0 = FETCH_THIS\n\
             0001 ASSIGN_DIM V0 int(0)\n\
             0002 OP_DATA int(1)\n\
             0003 RETURN int(1)\n"
        )
    );
}

/// `$a[k] = v`, `$a[] = v` and `$a[] = &$x` as the oracle dumps them
/// (`zend_compile_assign`'s and `zend_compile_assign_ref`'s
/// `ZEND_AST_DIM` arms): the value's oplines, then `ASSIGN_DIM` over
/// the container with the dimension in `op2` and `OP_DATA` carrying
/// the value, the append an `UNUSED` `op2` the dump prints as `NEXT`;
/// a chain is `FETCH_DIM_W` into a `VAR` per link but the last, each
/// `VAR` numbered after its own dimension and before the value's
/// temporaries; a constant string dimension that is a canonical
/// decimal integer is that integer (`zend_handle_numeric_dim`); the
/// temporary `ASSIGN_DIM` writes is marked unused where nothing reads
/// it and kept where an assignment does; `$a[0] = $a` reads the right
/// `$a` first through a `QM_ASSIGN`; and a by-reference write leaves
/// the last link a `FETCH_DIM_W` with `ASSIGN_REF` over its `VAR`.
#[test]
fn a_dimension_assignment_compiles_to_assign_dim_as_php_emits_it() {
    let cases: &[(&[u8], usize, u32, u32, &str)] = &[
        (
            b"<?php $a[0] = 2;",
            3,
            1,
            1,
            "0000 ASSIGN_DIM CV0($a) int(0)\n\
             0001 OP_DATA int(2)\n\
             0002 RETURN int(1)\n",
        ),
        (
            b"<?php $a[] = 3;",
            3,
            1,
            1,
            "0000 ASSIGN_DIM CV0($a) NEXT\n\
             0001 OP_DATA int(3)\n\
             0002 RETURN int(1)\n",
        ),
        (
            b"<?php $a[\"10\"] = 1;",
            3,
            1,
            1,
            "0000 ASSIGN_DIM CV0($a) int(10)\n\
             0001 OP_DATA int(1)\n\
             0002 RETURN int(1)\n",
        ),
        (
            b"<?php $x = $a[$k] = $v;",
            4,
            4,
            2,
            "0000 T4 = ASSIGN_DIM CV1($a) CV2($k)\n\
             0001 OP_DATA CV3($v)\n\
             0002 ASSIGN CV0($x) T4\n\
             0003 RETURN int(1)\n\
             LIVE RANGES:\n\
             \x20    4: 0001 - 0002 (tmp/var)\n",
        ),
        (
            b"<?php $a[1][2] = 3;",
            4,
            1,
            2,
            "0000 V1 = FETCH_DIM_W CV0($a) int(1)\n\
             0001 ASSIGN_DIM V1 int(2)\n\
             0002 OP_DATA int(3)\n\
             0003 RETURN int(1)\n",
        ),
        (
            b"<?php $a[0] = $a;",
            4,
            1,
            2,
            "0000 T2 = QM_ASSIGN CV0($a)\n\
             0001 ASSIGN_DIM CV0($a) int(0)\n\
             0002 OP_DATA T2\n\
             0003 RETURN int(1)\n",
        ),
        (
            b"<?php $a[] = &$a;",
            3,
            1,
            2,
            "0000 V1 = FETCH_DIM_W CV0($a) NEXT\n\
             0001 ASSIGN_REF V1 CV0($a)\n\
             0002 RETURN int(1)\n",
        ),
        (
            b"<?php $a[0][] = &$b;",
            4,
            2,
            3,
            "0000 V2 = FETCH_DIM_W CV0($a) int(0)\n\
             0001 V3 = FETCH_DIM_W V2 NEXT\n\
             0002 ASSIGN_REF V3 CV1($b)\n\
             0003 RETURN int(1)\n",
        ),
    ];
    for (source, lines, vars, tmps, oplines) in cases {
        assert_eq!(
            dump_of(source),
            main_dump(*lines, *vars, *tmps, oplines),
            "{}",
            String::from_utf8_lossy(source)
        );
    }
}

/// `zend_handle_numeric_dim`'s pair (`Z/Zend/zend_compile.c:2788`):
/// a constant dimension whose string is a canonical decimal integer
/// is the long in the operand with the string as written in the slot
/// right after it, and the long's slot is that pair's alone,
/// so `$o[10]` beside `$o["10"]` reads the long. A dimension written
/// as an integer, and one that is no canonical integer, make no pair.
#[test]
fn a_constant_numeric_dimension_keeps_the_written_string_after_the_long() {
    use php_heap::Zval;
    use php_unit::ConstIdx;
    let (unit, _strings) = unit_and_strings(b"<?php $a[\"10\"] = 1; $a[10] = 2; $a[\"k\"] = 3;");
    let main = &unit.main;
    assert_eq!(
        main.numeric_dims.len(),
        1,
        "one pair, for the written \"10\""
    );
    let paired = main.numeric_dims[0];
    assert_eq!(
        main.dimension_written(paired)
            .and_then(Zval::as_str)
            .map(|s| s.bytes().to_vec()),
        Some(b"10".to_vec()),
        "the written form stands after the long"
    );
    let dims: Vec<Zval> = main
        .oplines
        .iter()
        .filter(|op| op.opcode == Opcode::AssignDim && op.op2_type == OperandKind::Const)
        .filter_map(|op| main.literals.get(op.op2 as usize).copied())
        .collect();
    assert_eq!(dims.len(), 3);
    assert_eq!(dims[0].as_long(), Some(10), "the written \"10\" folded");
    assert_eq!(dims[1].as_long(), Some(10), "10 as written");
    assert_eq!(
        main.dimension_written(ConstIdx(
            main.oplines
                .iter()
                .filter(|op| op.opcode == Opcode::AssignDim)
                .nth(1)
                .map_or(0, |op| op.op2)
        ))
        .and_then(Zval::as_long),
        Some(10),
        "a dimension written as an integer keeps no pair and reads as itself"
    );
    assert_eq!(
        dims[2].as_str().map(|s| s.bytes().to_vec()),
        Some(b"k".to_vec())
    );
}

/// php's `dereferenceable_scalar` under a read (#482): a string
/// written out, an array written out, a chain over one, a
/// parenthesised expression and the `BP_VAR_IS` chain a `??` puts
/// over one. Nothing is folded -- `"ab"[0]` is the `FETCH_DIM_R`
/// over the constant string and never `string("a")` -- and the
/// concatenation inside the parentheses is folded where it stands,
/// so `("a" . "b")[0]` fetches over `string("ab")`. The dump is the
/// oracle's under `[regime.dump]` at 8.5.9.
#[test]
fn a_dimension_behind_a_literal_is_the_oracle_s_oplines() {
    assert_eq!(
        dump_of(
            b"<?php $a = \"ab\"[0]; $b = [1, 2][0]; $c = [[1], 2][0][0]; \
              $d = \"ab\"[0] ?? \"d\"; $e = (\"a\" . \"b\")[0];"
        ),
        main_dump(
            14,
            5,
            12,
            "0000 T5 = FETCH_DIM_R string(\"ab\") int(0)\n\
             0001 ASSIGN CV0($a) T5\n\
             0002 T7 = FETCH_DIM_R array(...) int(0)\n\
             0003 ASSIGN CV1($b) T7\n\
             0004 T9 = FETCH_DIM_R array(...) int(0)\n\
             0005 T10 = FETCH_DIM_R T9 int(0)\n\
             0006 ASSIGN CV2($c) T10\n\
             0007 T12 = FETCH_DIM_IS string(\"ab\") int(0)\n\
             0008 T13 = COALESCE T12 0010\n\
             0009 T13 = QM_ASSIGN string(\"d\")\n\
             0010 ASSIGN CV3($d) T13\n\
             0011 T15 = FETCH_DIM_R string(\"ab\") int(0)\n\
             0012 ASSIGN CV4($e) T15\n\
             0013 RETURN int(1)\n"
        )
    );
}

/// What a dimension refuses: `[]` where a value is wanted, php's
/// `Cannot use [] for reading`; under a write the bases php
/// refuses outright, a temporary expression and a call answered in
/// one. `$this` and a `static` property under a write, a compound
/// assignment and a `++` (`FETCH_STATIC_PROP_RW`), a property and a
/// call under a write, and every base php compiles where it stands
/// under a read, compile since #451, #507 and #667.
#[test]
fn a_dimension_this_compiler_cannot_read_yet_is_refused() {
    for source in [
        b"<?php $a[] = $b[];".as_slice(),
        b"<?php echo $a[];".as_slice(),
        b"<?php \"ab\"[0] = 1;".as_slice(),
        b"<?php [1, 2][0] = 1;".as_slice(),
    ] {
        assert!(
            refused(source).is_some(),
            "{}",
            String::from_utf8_lossy(source)
        );
    }
}

/// `$a[k]` where a value is wanted as the oracle dumps it
/// (`zend_compile_dim` under `BP_VAR_R`): one `FETCH_DIM_R` per link
/// of the chain into a temporary, each reading the temporary the
/// link under it wrote; a constant string dimension that is a
/// canonical decimal integer is that integer
/// (`zend_handle_numeric_dim`) and one that is not stands as written;
/// a dimension that is itself a dimension is read before the chain
/// over it; and a read as the value of a write is the value's oplines
/// before the write's, the write's own temporary numbered first.
#[test]
fn a_dimension_read_compiles_to_fetch_dim_r_as_php_emits_it() {
    let cases: &[(&[u8], usize, u32, u32, &str)] = &[
        (
            b"<?php echo $a[0];",
            3,
            1,
            1,
            "0000 T1 = FETCH_DIM_R CV0($a) int(0)\n\
             0001 ECHO T1\n\
             0002 RETURN int(1)\n",
        ),
        (
            b"<?php echo $a[\"10\"];",
            3,
            1,
            1,
            "0000 T1 = FETCH_DIM_R CV0($a) int(10)\n\
             0001 ECHO T1\n\
             0002 RETURN int(1)\n",
        ),
        (
            b"<?php echo $a[\"k\"];",
            3,
            1,
            1,
            "0000 T1 = FETCH_DIM_R CV0($a) string(\"k\")\n\
             0001 ECHO T1\n\
             0002 RETURN int(1)\n",
        ),
        (
            b"<?php $x = $a[$k][1];",
            4,
            3,
            3,
            "0000 T3 = FETCH_DIM_R CV1($a) CV2($k)\n\
             0001 T4 = FETCH_DIM_R T3 int(1)\n\
             0002 ASSIGN CV0($x) T4\n\
             0003 RETURN int(1)\n",
        ),
        (
            b"<?php echo $a[$b[0]];",
            4,
            2,
            2,
            "0000 T2 = FETCH_DIM_R CV1($b) int(0)\n\
             0001 T3 = FETCH_DIM_R CV0($a) T2\n\
             0002 ECHO T3\n\
             0003 RETURN int(1)\n",
        ),
        (
            b"<?php $a[0] = $b[1];",
            4,
            2,
            2,
            "0000 T3 = FETCH_DIM_R CV1($b) int(1)\n\
             0001 ASSIGN_DIM CV0($a) int(0)\n\
             0002 OP_DATA T3\n\
             0003 RETURN int(1)\n",
        ),
    ];
    for (source, lines, vars, tmps, oplines) in cases {
        assert_eq!(
            dump_of(source),
            main_dump(*lines, *vars, *tmps, oplines),
            "{}",
            String::from_utf8_lossy(source)
        );
    }
}
