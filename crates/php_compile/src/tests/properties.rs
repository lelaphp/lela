//! Class constants and properties: a class constant's fetch, static and
//! instance property reads and writes, compound and by-reference writes
//! to them, and what php refuses beside them.

use super::*;

/// `zend_compile_class_const` under `ZEND_COMPILE_NO_CONSTANT_SUBSTITUTION`,
/// which opcache sets, so `zend_try_ct_eval_class_const` folds
/// nothing and `M::K` is fetched as the program serves: the
/// class's constant pair in `op1` (`zend_set_class_name_op1` over
/// `zend_resolve_class_name`, which takes the namespace as
/// `zend_compile_class_decl` took it on the declaration), the
/// constant's name in `op2`, into a temporary. The dump is the
/// oracle's under `[regime.dump]` at 8.5.9 of this program, which is
/// the namespaced class test's in classes.rs with the `echo M::K` that
/// test stands aside from.
#[test]
fn a_class_constant_is_fetch_class_constant_over_the_resolved_class_name() {
    let (unit, _strings) =
        unit_and_strings(b"<?php namespace A; class M { const K = 1; } new M; echo M::K;");
    expect![[r#"

        $_main:
             ; (lines=6, args=0, vars=0, tmps=3)
             ; (before optimizer)
             ; :1-1
             ; return  [] RANGE[0..0]
        0000 V0 = NEW 0 string("A\\M")
        0001 DO_FCALL
        0002 FREE V0
        0003 T2 = FETCH_CLASS_CONSTANT string("A\\M") string("K")
        0004 ECHO T2
        0005 RETURN int(1)
        LIVE RANGES:
             0: 0001 - 0002 (new)
    "#]]
    .assert_eq(&unit.dump(&[]).text());
}

/// The constant table `zend_compile_class_const` leaves:
/// `zend_emit_op_tmp` adds the constant's name, then
/// `zend_set_class_name_op1` the class's pair, so the name stands
/// before the pair, as it does under `zend_compile_static_prop`.
/// The dump prints values and not slots, so it reads no order; the
/// table is read here, whole: `NEW`'s pair, the fetch's name and
/// pair, and the final return's `1`, each slot its own as
/// `zend_add_literal` keeps them.
#[test]
fn a_class_constant_fetch_adds_the_constant_s_name_before_the_class_s_pair() {
    let (unit, _strings) =
        unit_and_strings(b"<?php namespace A; class M { const K = 1; } new M; echo M::K;");
    let table: Vec<String> = unit
        .main
        .literals
        .iter()
        .map(|z| match (z.as_str(), z.as_long()) {
            (Some(s), _) => String::from_utf8_lossy(s.bytes()).into_owned(),
            (None, Some(n)) => n.to_string(),
            (None, None) => String::from("?"),
        })
        .collect();
    assert_eq!(
        table,
        ["A\\M", "a\\m", "K", "A\\M", "a\\m", "1"],
        "php's table, the constant's name before the class's pair"
    );
    let fetch = &unit.main.oplines[3];
    assert_eq!(fetch.op2, 2, "the name is the slot added first");
    assert_eq!(
        fetch.op1, 3,
        "the pair's written name follows it, its lowered form after"
    );
}

/// `zend_compile_static_prop` under `BP_VAR_R`: the
/// property's name in `op1`, the class's constant pair in `op2`, into
/// a temporary (`zend_adjust_for_fetch_type`). The dump is the
/// oracle's under `[regime.dump]` at 8.5.9 of `<?php echo C::$p;`,
/// whose class the table has not, which php compiles all the same
/// and fails on only as it runs. Under a namespace the class takes
/// it, as every other class reference does.
#[test]
fn a_static_property_read_is_fetch_static_prop_r_over_the_resolved_class_name() {
    expect![[r#"

        $_main:
             ; (lines=3, args=0, vars=0, tmps=1)
             ; (before optimizer)
             ; :1-1
             ; return  [] RANGE[0..0]
        0000 T0 = FETCH_STATIC_PROP_R string("p") string("C")
        0001 ECHO T0
        0002 RETURN int(1)
    "#]]
    .assert_eq(&dump_of(b"<?php echo C::$p;"));
    expect![[r#"

        $_main:
             ; (lines=3, args=0, vars=0, tmps=1)
             ; (before optimizer)
             ; :1-1
             ; return  [] RANGE[0..0]
        0000 T0 = FETCH_STATIC_PROP_R string("p") string("A\\C")
        0001 ECHO T0
        0002 RETURN int(1)
    "#]]
    .assert_eq(&dump_of(b"<?php namespace A; class C { } echo C::$p;"));
}

/// `zend_compile_assign`'s `ZEND_AST_STATIC_PROP` arm: the
/// fetch's own opline turned into `ASSIGN_STATIC_PROP`, its
/// `result` a temporary and the value in the `OP_DATA` beside it.
/// The dump is the oracle's under `[regime.dump]` at 8.5.9; the
/// read's temporary is `T1` because `T0` is the write's, numbered
/// whether or not anything reads it and marked `UNUSED` here by
/// `zend_do_free`.
#[test]
fn a_static_property_write_is_assign_static_prop_over_the_op_data_beside_it() {
    expect![[r#"

        $_main:
             ; (lines=7, args=0, vars=0, tmps=3)
             ; (before optimizer)
             ; :1-1
             ; return  [] RANGE[0..0]
        0000 ASSIGN_STATIC_PROP string("a") string("T")
        0001 OP_DATA int(5)
        0002 INIT_FCALL 1 96 string("var_dump")
        0003 T1 = FETCH_STATIC_PROP_R string("a") string("T")
        0004 SEND_VAL T1 1
        0005 DO_ICALL
        0006 RETURN int(1)
    "#]]
    .assert_eq(&dump_of(
        b"<?php class T { public static $a = 1; } T::$a = 5; var_dump(T::$a);",
    ));
}

/// `zend_compile_compound_assign`, `zend_compile_pre_incdec`,
/// `zend_compile_post_incdec` and `zend_compile_assign_ref` over a
/// `ZEND_AST_STATIC_PROP`: one opline each, the fetch's own
/// with its opcode set, the operator where the cache slot stood for
/// the compound form and the slot moved onto its `OP_DATA`. The
/// dump is the oracle's under `[regime.dump]` at 8.5.9; `C::$p++`
/// and `C::$p--` written as statements are the `PRE_*` opcodes,
/// since `zend_do_free` turns a step nothing reads into the one
/// that leaves the value after it, and the one `var_dump` reads is
/// `POST_INC_STATIC_PROP` into `T8`.
#[test]
fn the_compound_step_and_reference_writes_to_a_static_property_are_the_oracle_s_oplines() {
    expect![[r#"

        $_main:
             ; (lines=14, args=0, vars=1, tmps=9)
             ; (before optimizer)
             ; :1-1
             ; return  [] RANGE[0..0]
        0000 ASSIGN_STATIC_PROP_OP (ADD) string("p") string("C")
        0001 OP_DATA int(2)
        0002 PRE_INC_STATIC_PROP string("p") string("C")
        0003 PRE_DEC_STATIC_PROP string("p") string("C")
        0004 PRE_INC_STATIC_PROP string("p") string("C")
        0005 PRE_DEC_STATIC_PROP string("p") string("C")
        0006 ASSIGN CV0($x) int(1)
        0007 ASSIGN_STATIC_PROP_REF string("p") string("C")
        0008 OP_DATA CV0($x)
        0009 INIT_FCALL 1 96 string("var_dump")
        0010 T8 = POST_INC_STATIC_PROP string("p") string("C")
        0011 SEND_VAL T8 1
        0012 DO_ICALL
        0013 RETURN int(1)
    "#]]
    .assert_eq(&dump_of(
        b"<?php class C { public static $p = 1; } C::$p += 2; ++C::$p; --C::$p; \
              C::$p++; C::$p--; $x = 1; C::$p = &$x; var_dump(C::$p++);",
    ));
}

/// What stands beside a `C::$p` read and is not read yet is refused
/// as such: a compound assignment over what is no variable,
/// `self`, `parent` and `static` as the class, and such a fetch in a
/// constant expression, which php refuses outright. `C::$p = &$o->q`
/// was one of them until the source of a `= &` was made
/// `zend_compile_var`'s, `C::$p()`, whose method the local `$p`
/// names, until it was read as `INIT_STATIC_METHOD_CALL` over the
/// CV, and the property the program names as it serves, `C::$$p`
/// and `C::${'p'}`, until that too was read.
#[test]
fn what_stands_beside_a_static_property_read_is_refused_as_not_yet_read() {
    let refused = |source: &[u8]| {
        let mut strings = php_heap::Strings::new();
        compile_parsed(source, &bundled(), &mut strings)
            .err()
            .map(|e| e.message.to_string())
    };
    assert_eq!(
        refused(b"<?php $this += 1;").as_deref(),
        Some("a compound assignment to what is no variable this compiler cannot read yet")
    );
    assert_eq!(refused(b"<?php C::$$p;").as_deref(), None);
    assert_eq!(refused(b"<?php C::${'p'};").as_deref(), None);
    assert_eq!(refused(b"<?php C::$p();").as_deref(), None);
    assert_eq!(refused(b"<?php echo self::$p;").as_deref(), None);
    assert_eq!(
        refused(b"<?php class D { const X = C::$p; }").as_deref(),
        Some("Constant expression contains invalid operations")
    );
}

/// `zend_is_assign_to_self` through a property: `$a->x[] =
/// $a` and `$a[0]->p[1] = $a` read `$a` into a temporary first,
/// numbered after the delayed fetches, and `OP_DATA` carries it.
#[test]
fn an_assignment_to_self_through_a_property_reads_the_value_first() {
    expect![[r#"

        $_main:
             ; (lines=13, args=0, vars=1, tmps=10)
             ; (before optimizer)
             ; :1-5
             ; return  [] RANGE[0..0]
        0000 V1 = NEW 0 string("stdClass")
        0001 DO_FCALL
        0002 ASSIGN CV0($a) V1
        0003 T6 = QM_ASSIGN CV0($a)
        0004 V4 = FETCH_OBJ_W (dim write) CV0($a) string("x")
        0005 ASSIGN_DIM V4 NEXT
        0006 OP_DATA T6
        0007 T10 = QM_ASSIGN CV0($a)
        0008 V7 = FETCH_DIM_W CV0($a) int(0)
        0009 V8 = FETCH_OBJ_W (dim write) V7 string("p")
        0010 ASSIGN_DIM V8 int(1)
        0011 OP_DATA T10
        0012 RETURN int(1)
        LIVE RANGES:
             1: 0001 - 0002 (new)
             6: 0004 - 0005 (tmp/var)
             10: 0008 - 0010 (tmp/var)
    "#]]
    .assert_eq(&dump_of(
        b"<?php
$a = new stdClass;
$a->x[] = $a;
$a[0]->p[1] = $a;
",
    ));
    // `zend_is_variable` takes no call, so the walk stops at
    // `$a->m()`, and a `static` property's class is walked into.
    expect![[r#"

        $_main:
             ; (lines=12, args=0, vars=1, tmps=7)
             ; (before optimizer)
             ; :1-4
             ; return  [] RANGE[0..0]
        0000 INIT_METHOD_CALL 0 CV0($a) string("m")
        0001 V1 = DO_FCALL
        0002 V1 = SEPARATE V1
        0003 V2 = FETCH_OBJ_W (dim write) V1 string("x")
        0004 ASSIGN_DIM V2 int(0)
        0005 OP_DATA CV0($a)
        0006 V4 = FETCH_CLASS (exception) CV0($a)
        0007 T7 = QM_ASSIGN CV0($a)
        0008 V5 = FETCH_STATIC_PROP_W (dim write) string("s") V4
        0009 ASSIGN_DIM V5 int(0)
        0010 OP_DATA T7
        0011 RETURN int(1)
        LIVE RANGES:
             1: 0002 - 0003 (tmp/var)
             7: 0008 - 0009 (tmp/var)
    "#]]
    .assert_eq(&dump_of(b"<?php\n$a->m()->x[0] = $a;\n$a::$s[0] = $a;\n"));
}

/// `zend_delayed_compile_prop` as the oracle at 8.5.9 dumps it under
/// `opcache.opt_debug_level=0x10000`: `FETCH_OBJ_R` into a temporary
/// with the name as one constant, the receiver's operand whatever it
/// is, the temporary freed where the fetch is a statement and sent
/// by `SEND_VAL` to a callee the table has.
#[test]
fn property_fetches_compile_to_php_s_op_arrays() {
    let cases: &[(&[u8], Expect)] = &[
        (
            b"<?php $x = $o->p;",
            expect![[r#"

                $_main:
                     ; (lines=3, args=0, vars=2, tmps=2)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 T2 = FETCH_OBJ_R CV1($o) string("p")
                0001 ASSIGN CV0($x) T2
                0002 RETURN int(1)
            "#]],
        ),
        (
            b"<?php $x = $o->p + $o->q;",
            expect![[r#"

                $_main:
                     ; (lines=5, args=0, vars=2, tmps=4)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 T2 = FETCH_OBJ_R CV1($o) string("p")
                0001 T3 = FETCH_OBJ_R CV1($o) string("q")
                0002 T4 = ADD T2 T3
                0003 ASSIGN CV0($x) T4
                0004 RETURN int(1)
                LIVE RANGES:
                     2: 0001 - 0002 (tmp/var)
            "#]],
        ),
        (
            b"<?php (new E)->p;",
            expect![[r#"

                $_main:
                     ; (lines=5, args=0, vars=0, tmps=3)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 V0 = NEW 0 string("E")
                0001 DO_FCALL
                0002 T2 = FETCH_OBJ_R V0 string("p")
                0003 FREE T2
                0004 RETURN int(1)
                LIVE RANGES:
                     0: 0001 - 0002 (new)
            "#]],
        ),
        (
            b"<?php $o->p->q()->r;",
            expect![[r#"

                $_main:
                     ; (lines=6, args=0, vars=1, tmps=3)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 T1 = FETCH_OBJ_R CV0($o) string("p")
                0001 INIT_METHOD_CALL 0 T1 string("q")
                0002 V2 = DO_FCALL
                0003 T3 = FETCH_OBJ_R V2 string("r")
                0004 FREE T3
                0005 RETURN int(1)
            "#]],
        ),
        (
            b"<?php var_dump($o->p);",
            expect![[r#"

                $_main:
                     ; (lines=5, args=0, vars=1, tmps=2)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 INIT_FCALL 1 96 string("var_dump")
                0001 T1 = FETCH_OBJ_R CV0($o) string("p")
                0002 SEND_VAL T1 1
                0003 DO_ICALL
                0004 RETURN int(1)
            "#]],
        ),
    ];
    for (source, expected) in cases {
        expected.assert_eq(&dump_of(source));
    }
    let (unit, _strings) = unit_and_strings(b"<?php $o->p;");
    assert_eq!(
        unit.main.cache_size, 24,
        "a property fetch takes three cache slots (zend_alloc_cache_slots(3)), eight bytes a slot"
    );
}

/// `$o->p = v` as the oracle dumps it (`zend_compile_assign`, the
/// `ZEND_AST_PROP` arm): the value's oplines, then `ASSIGN_OBJ` over
/// the receiver with the name as one constant and `OP_DATA` carrying
/// the value; a chain is `FETCH_OBJ_W` into a `VAR` per link but the
/// last, each `VAR` numbered before the value's temporaries; the
/// temporary `ASSIGN_OBJ` writes is marked unused where nothing
/// reads it and kept where an assignment does; `$this` under the chain is
/// `FETCH_THIS` into a `VAR` where it stands, before the value.
#[test]
fn a_property_assignment_compiles_to_assign_obj_as_php_emits_it() {
    let cases: &[(&[u8], Expect)] = &[
        (
            b"<?php $o->p = 1;",
            expect![[r#"

                $_main:
                     ; (lines=3, args=0, vars=1, tmps=1)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 ASSIGN_OBJ CV0($o) string("p")
                0001 OP_DATA int(1)
                0002 RETURN int(1)
            "#]],
        ),
        (
            b"<?php $x = $o->q->p = $y;",
            expect![[r#"

                $_main:
                     ; (lines=5, args=0, vars=3, tmps=3)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 V3 = FETCH_OBJ_W CV1($o) string("q")
                0001 T4 = ASSIGN_OBJ V3 string("p")
                0002 OP_DATA CV2($y)
                0003 ASSIGN CV0($x) T4
                0004 RETURN int(1)
                LIVE RANGES:
                     4: 0002 - 0003 (tmp/var)
            "#]],
        ),
        (
            b"<?php $this->p = 1;",
            expect![[r#"

                $_main:
                     ; (lines=4, args=0, vars=0, tmps=2)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 V0 = FETCH_THIS
                0001 ASSIGN_OBJ V0 string("p")
                0002 OP_DATA int(1)
                0003 RETURN int(1)
            "#]],
        ),
        (
            b"<?php $o->q->r->p = 1;",
            expect![[r#"

                $_main:
                     ; (lines=5, args=0, vars=1, tmps=3)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 V1 = FETCH_OBJ_W CV0($o) string("q")
                0001 V2 = FETCH_OBJ_W V1 string("r")
                0002 ASSIGN_OBJ V2 string("p")
                0003 OP_DATA int(1)
                0004 RETURN int(1)
            "#]],
        ),
    ];
    for (source, expected) in cases {
        expected.assert_eq(&dump_of(source));
    }
}

/// `zend_separate_if_call_and_write` under a property write,
/// the oracle's dump under `[regime.dump]` at 8.5.9: a call under
/// the chain is compiled where it stands and `SEPARATE` reads and
/// writes the one `VAR` it returned into, before the fetches the
/// chain delays, so the `VAR` the write lands in is the request's
/// own. Every shape `zend_is_call` names is separated the same way:
/// a call by name, a call through a value, a method call and a call
/// on a class, each with the live range the dump prints over the
/// `SEPARATE` and the fetch after it.
#[test]
fn a_call_under_a_property_write_is_the_oracle_s_separate() {
    expect![[r#"

        $_main:
             ; (lines=27, args=0, vars=2, tmps=11)
             ; (before optimizer)
             ; :1-1
             ; return  [] RANGE[0..0]
        0000 INIT_FCALL_BY_NAME 0 string("f")
        0001 V2 = DO_FCALL_BY_NAME
        0002 V2 = SEPARATE V2
        0003 ASSIGN_OBJ V2 string("p")
        0004 OP_DATA int(1)
        0005 INIT_FCALL_BY_NAME 0 string("f")
        0006 V4 = DO_FCALL_BY_NAME
        0007 V4 = SEPARATE V4
        0008 V5 = FETCH_OBJ_W V4 string("a")
        0009 ASSIGN_OBJ V5 string("b")
        0010 OP_DATA int(2)
        0011 INIT_METHOD_CALL 0 CV0($o) string("m")
        0012 V7 = DO_FCALL
        0013 V7 = SEPARATE V7
        0014 ASSIGN_OBJ V7 string("p")
        0015 OP_DATA int(3)
        0016 INIT_DYNAMIC_CALL 0 CV1($f)
        0017 V9 = DO_FCALL
        0018 V9 = SEPARATE V9
        0019 ASSIGN_OBJ V9 string("p")
        0020 OP_DATA int(4)
        0021 INIT_STATIC_METHOD_CALL 0 string("C") string("m")
        0022 V11 = DO_FCALL
        0023 V11 = SEPARATE V11
        0024 ASSIGN_OBJ V11 string("p")
        0025 OP_DATA int(5)
        0026 RETURN int(1)
        LIVE RANGES:
             2: 0002 - 0003 (tmp/var)
             4: 0007 - 0008 (tmp/var)
             7: 0013 - 0014 (tmp/var)
             9: 0018 - 0019 (tmp/var)
             11: 0023 - 0024 (tmp/var)
    "#]]
    .assert_eq(&dump_of(
        b"<?php f()->p = 1; f()->a->b = 2; $o->m()->p = 3; $f()->p = 4; C::m()->p = 5;",
    ));
}

/// These shapes under the oracle's `[regime.dump]`: a property as
/// the target of `= &` is `ASSIGN_OBJ_REF` over its object with the
/// source in an `OP_DATA`, `(function)` and a `MAKE_REF` over a call.
#[test]
fn a_property_bound_by_reference_is_assign_obj_ref_as_php_emits_it() {
    let cases: &[(&[u8], Expect)] = &[
        (
            b"<?php $o->p = &$x;",
            expect![[r#"

                $_main:
                     ; (lines=3, args=0, vars=2, tmps=1)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 ASSIGN_OBJ_REF CV0($o) string("p")
                0001 OP_DATA CV1($x)
                0002 RETURN int(1)
            "#]],
        ),
        (
            b"<?php $o->p = &f();",
            expect![[r#"

                $_main:
                     ; (lines=6, args=0, vars=1, tmps=3)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 INIT_FCALL_BY_NAME 0 string("f")
                0001 V2 = DO_FCALL_BY_NAME
                0002 V3 = MAKE_REF V2
                0003 ASSIGN_OBJ_REF (function) CV0($o) string("p")
                0004 OP_DATA V3
                0005 RETURN int(1)
            "#]],
        ),
    ];
    for (source, expected) in cases {
        expected.assert_eq(&dump_of(source));
    }
}
