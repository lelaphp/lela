//! Objects and methods: `new`, a class named by an expression, method
//! and static calls and their known callees, `instanceof`, `$this` and
//! the nullsafe chain.

use super::*;

/// A class written in parentheses is an expression, `'(' expr ')'` in
/// `class_name_reference` and `fully_dereferenceable`, so `new (C)`,
/// `(C)::X` and `instanceof (C)` read the constant `C` and fetch the
/// class it names (`FETCH_CONSTANT`, `FETCH_CLASS`) where `new C` names
/// the class `C`; a string written where the class goes, folded by the
/// parser from `"A" . "B"` too, is the class it names as written
/// (`ZEND_NAME_FQ`). The dump is the oracle's under `[regime.dump]` at
/// 8.5.9 (#2680).
#[test]
fn a_class_in_parentheses_is_the_expression_and_a_string_the_class_it_names() {
    expect![[r#"

        $_main:
             ; (lines=24, args=0, vars=1, tmps=15)
             ; (before optimizer)
             ; :1-1
             ; return  [] RANGE[0..0]
        0000 T1 = FETCH_CONSTANT string("C")
        0001 V2 = FETCH_CLASS (exception) T1
        0002 V3 = NEW 0 V2
        0003 DO_FCALL
        0004 FREE V3
        0005 V5 = NEW 0 string("C")
        0006 DO_FCALL
        0007 FREE V5
        0008 T7 = FETCH_CONSTANT string("C")
        0009 V8 = FETCH_CLASS (exception) T7
        0010 T9 = FETCH_CLASS_CONSTANT V8 string("X")
        0011 ECHO T9
        0012 ECHO string("AB")
        0013 T10 = FETCH_CLASS_CONSTANT string("Foo") string("X")
        0014 ECHO T10
        0015 T11 = FETCH_CLASS_CONSTANT string("Foo") string("X")
        0016 ECHO T11
        0017 INIT_FCALL 1 96 string("var_dump")
        0018 T12 = FETCH_CONSTANT string("C")
        0019 V13 = FETCH_CLASS (no-autoload) (silent) (exception) T12
        0020 T14 = INSTANCEOF CV0($a) V13
        0021 SEND_VAL T14 1
        0022 DO_ICALL
        0023 RETURN int(1)
        LIVE RANGES:
             3: 0003 - 0004 (new)
             5: 0006 - 0007 (new)
    "#]].assert_eq(&dump_of(b"<?php new (C); new C; echo (C)::X, (\"A\" . \"B\")::class, \"Foo\"::X, (\"\\\\Foo\")::X; var_dump($a instanceof (C));"));
}

/// `zend_compile_new`, `zend_compile_static_call`,
/// `zend_compile_method_call` and `zend_compile_instanceof` as the
/// oracle at 8.5.9 dumps them under `opcache.opt_debug_level=0x10000`,
/// each source read once: `NEW` into a `VAR` with its `DO_FCALL`
/// unused and the `FREE` or the live range `zend_do_free` leaves;
/// the two `INIT_*_METHOD_CALL`s with their arguments sent for a
/// callee nothing is known of; `INSTANCEOF` into a temporary, a
/// chain of them grouped to the left with the second over the
/// first's temporary, and `false` where the left side is a constant.
#[test]
fn new_compiles_to_php_s_op_array() {
    let cases: &[(&[u8], Expect)] = &[
        (
            b"<?php $o = new E;",
            expect![[r#"

                $_main:
                     ; (lines=4, args=0, vars=1, tmps=3)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 V1 = NEW 0 string("E")
                0001 DO_FCALL
                0002 ASSIGN CV0($o) V1
                0003 RETURN int(1)
                LIVE RANGES:
                     1: 0001 - 0002 (new)
            "#]],
        ),
        (
            b"<?php new E;",
            expect![[r#"

                $_main:
                     ; (lines=4, args=0, vars=0, tmps=2)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 V0 = NEW 0 string("E")
                0001 DO_FCALL
                0002 FREE V0
                0003 RETURN int(1)
                LIVE RANGES:
                     0: 0001 - 0002 (new)
            "#]],
        ),
        (
            b"<?php echo (new E(1))->m();",
            expect![[r#"

                $_main:
                     ; (lines=7, args=0, vars=0, tmps=3)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 V0 = NEW 1 string("E")
                0001 SEND_VAL_EX int(1) 1
                0002 DO_FCALL
                0003 INIT_METHOD_CALL 0 V0 string("m")
                0004 V2 = DO_FCALL
                0005 ECHO V2
                0006 RETURN int(1)
                LIVE RANGES:
                     0: 0001 - 0003 (new)
            "#]],
        ),
    ];
    for (source, expected) in cases {
        expected.assert_eq(&dump_of(source));
    }
    let (unit, _strings) = unit_and_strings(b"<?php new E; $o instanceof E;");
    assert_eq!(
        unit.main.cache_size, 16,
        "NEW and INSTANCEOF take one cache slot each, eight bytes a slot"
    );
}

/// `zend_get_compatible_func_or_null` under a namespace (#322): the
/// callee is known by the name the fetch resolves to, so `M::f(1)`
/// under `namespace A;` finds the public `f` of the `A\M` the engine
/// declared and sends `SEND_VAL`, where `\M::f(1)` names the global
/// `M` the table has not.
#[test]
fn a_method_of_a_namespaced_class_is_a_known_callee_in_its_namespace() {
    use php_unit::decl::{ClassDecl, ClassFlags, ClassKind, EnumBacking, MethodDecl};
    let options = Options {
        classes: indexed(vec![ClassDecl {
            name: Name(b"A\\M".as_slice().into()),
            kind: ClassKind::Class,
            flags: ClassFlags::default(),
            parent: None,
            interfaces: Vec::new(),
            implementors: Vec::new(),
            aliases: Vec::new(),
            traits: Vec::new(),
            adaptations: Vec::new(),
            backing: EnumBacking::Pure,
            constants: Vec::new(),
            properties: Vec::new(),
            methods: vec![MethodDecl {
                function: php_unit::decl::native_function(b"f"),
                visibility: Visibility::Public,
                is_instance: false,
                is_abstract: false,
                is_final: false,
                tentative_return: false,
            }],
            attributes: Vec::new(),
            doc_comment: None,
            file: Name::default(),
            line: 0,
            end_line: 0,
            object_size: 0,
            object_beside: Box::default(),
            attribute_validator: None,
        }]),
        ..Options::default()
    };
    let mut strings = php_heap::Strings::new();
    let dump = compile_parsed(
        b"<?php namespace A; M::f(1); \\M::f(1);",
        &options,
        &mut strings,
    )
    .expect("the source compiles")
    .dump(&[])
    .text();
    expect![[r#"

        $_main:
             ; (lines=7, args=0, vars=0, tmps=2)
             ; (before optimizer)
             ; :1-1
             ; return  [] RANGE[0..0]
        0000 INIT_STATIC_METHOD_CALL 1 string("A\\M") string("f")
        0001 SEND_VAL int(1) 1
        0002 DO_FCALL
        0003 INIT_STATIC_METHOD_CALL 1 string("M") string("f")
        0004 SEND_VAL_EX int(1) 1
        0005 DO_FCALL
        0006 RETURN int(1)
    "#]]
    .assert_eq(&dump);
}

/// `zend_compile_method_call` and `zend_compile_static_call` against
/// the class being compiled (#2675): `$this->p()` to a private and
/// `$this->f()` to a final method the class declared before the call
/// are `DO_UCALL` with the arguments sent by the parameters, by
/// reference to `&$a`, where a public one stays `DO_FCALL` and so
/// does the method calling itself, which is public; `self::s()` and
/// `C::s()` inside `class C` are `DO_UCALL` too, the second in `s`'s
/// own body, which `zend_begin_method_decl` put in the table before
/// it compiled.
#[test]
fn a_call_the_class_being_compiled_pins_down_is_do_ucall() {
    expect![[r#"

        $_main:
             ; (lines=1, args=0, vars=0, tmps=0)
             ; (before optimizer)
             ; :1-15
             ; return  [] RANGE[0..0]
        0000 RETURN int(1)

        C::p:
             ; (lines=2, args=1, vars=1, tmps=0)
             ; (before optimizer)
             ; :3-3
             ; return  [] RANGE[0..0]
        0000 CV0($a) = RECV 1
        0001 RETURN null

        C::f:
             ; (lines=2, args=1, vars=1, tmps=0)
             ; (before optimizer)
             ; :4-4
             ; return  [] RANGE[0..0]
        0000 CV0($a) = RECV 1
        0001 RETURN null

        C::g:
             ; (lines=2, args=1, vars=1, tmps=0)
             ; (before optimizer)
             ; :5-5
             ; return  [] RANGE[0..0]
        0000 CV0($a) = RECV 1
        0001 RETURN null

        C::s:
             ; (lines=6, args=1, vars=1, tmps=1)
             ; (before optimizer)
             ; :6-6
             ; return  [] RANGE[0..0]
        0000 CV0($a) = RECV 1
        0001 INIT_STATIC_METHOD_CALL 1 string("C") string("s")
        0002 SEND_VAR CV0($a) 1
        0003 V1 = DO_UCALL
        0004 RETURN V1
        0005 RETURN null

        C::m:
             ; (lines=18, args=1, vars=1, tmps=5)
             ; (before optimizer)
             ; :7-13
             ; return  [] RANGE[0..0]
        0000 CV0($a) = RECV 1
        0001 INIT_METHOD_CALL 1 THIS string("p")
        0002 SEND_VAR CV0($a) 1
        0003 DO_UCALL
        0004 INIT_METHOD_CALL 1 THIS string("f")
        0005 SEND_REF CV0($a) 1
        0006 DO_UCALL
        0007 INIT_METHOD_CALL 1 THIS string("g")
        0008 SEND_VAR_EX CV0($a) 1
        0009 DO_FCALL
        0010 INIT_STATIC_METHOD_CALL 1 (self) (exception) string("s")
        0011 SEND_VAL int(1) 1
        0012 DO_UCALL
        0013 INIT_METHOD_CALL 1 THIS string("m")
        0014 SEND_VAR_EX CV0($a) 1
        0015 V5 = DO_FCALL
        0016 RETURN V5
        0017 RETURN null
    "#]]
    .assert_eq(&dump_of(
        b"<?php
class C {
    private function p($a) {}
    final public function f(&$a) {}
    public function g($a) {}
    public static function s($a) { return C::s($a); }
    public function m($a) {
        $this->p($a);
        $this->f($a);
        $this->g($a);
        self::s(1);
        return $this->m($a);
    }
}
",
    ));
}

/// `zend_compile_static_call` over a class bound early (#2016):
/// `zend_get_compatible_func_or_null` finds the method in the linked
/// class's `function_table`, which holds what it inherited, so
/// `B::foo($x)` over `A::foo(&$a)` is `DO_UCALL` with `$x` sent by
/// reference, as `Zend/tests/lsb/lsb_013.phpt` dumps, and a private
/// method inherited stays `DO_FCALL`.
#[test]
fn a_static_call_to_an_inherited_method_is_do_ucall() {
    expect![[r#"

        $_main:
             ; (lines=6, args=0, vars=1, tmps=2)
             ; (before optimizer)
             ; :1-6
             ; return  [] RANGE[0..0]
        0000 INIT_STATIC_METHOD_CALL 1 string("B") string("foo")
        0001 SEND_REF CV0($x) 1
        0002 DO_UCALL
        0003 INIT_STATIC_METHOD_CALL 0 string("B") string("p")
        0004 DO_FCALL
        0005 RETURN int(1)

        A::foo:
             ; (lines=2, args=1, vars=1, tmps=0)
             ; (before optimizer)
             ; :2-2
             ; return  [] RANGE[0..0]
        0000 CV0($a) = RECV 1
        0001 RETURN null

        A::p:
             ; (lines=1, args=0, vars=0, tmps=0)
             ; (before optimizer)
             ; :2-2
             ; return  [] RANGE[0..0]
        0000 RETURN null
    "#]]
    .assert_eq(&dump_of(
        b"<?php
class A { static function foo(&$a) {} private static function p() {} }
class B extends A {}
B::foo($x);
B::p();
",
    ));
}

/// `zend_get_compatible_func_or_null`: a public method of a class
/// the engine declared is a callee the compiler knows, so its
/// arguments are sent as `SEND_VAL` and `SEND_VAR`; a private one,
/// a class the table has not and a class the file declares with no
/// such method are sent for a callee nothing is known of. The
/// class's name is matched whatever its case, as `CG(class_table)`
/// is keyed by the lowered name.
#[test]
fn a_public_method_of_a_declared_class_is_a_known_callee() {
    use php_unit::decl::{ClassDecl, ClassFlags, ClassKind, EnumBacking, MethodDecl, Visibility};
    let method = |name: &'static [u8], visibility| MethodDecl {
        function: php_unit::decl::native_function(name),
        visibility,
        is_instance: false,
        is_abstract: false,
        is_final: false,
        tentative_return: false,
    };
    let options = Options {
        classes: indexed(vec![ClassDecl {
            name: Name(b"PhpToken".as_slice().into()),
            kind: ClassKind::Class,
            flags: ClassFlags::default(),
            parent: None,
            interfaces: Vec::new(),
            implementors: Vec::new(),
            aliases: Vec::new(),
            traits: Vec::new(),
            adaptations: Vec::new(),
            backing: EnumBacking::Pure,
            constants: Vec::new(),
            properties: Vec::new(),
            methods: vec![
                method(b"tokenize", Visibility::Public),
                method(b"hidden", Visibility::Private),
            ],
            attributes: Vec::new(),
            doc_comment: None,
            file: Name::default(),
            line: 0,
            end_line: 0,
            object_size: 0,
            object_beside: Box::default(),
            attribute_validator: None,
        }]),
        ..Options::default()
    };
    let dump = |source: &[u8]| {
        let mut strings = php_heap::Strings::new();
        compile_parsed(source, &options, &mut strings)
            .expect("the source compiles")
            .dump(&[])
            .text()
    };
    expect![[r#"

        $_main:
             ; (lines=6, args=0, vars=1, tmps=2)
             ; (before optimizer)
             ; :1-1
             ; return  [] RANGE[0..0]
        0000 ASSIGN CV0($a) int(1)
        0001 INIT_STATIC_METHOD_CALL 2 string("phptoken") string("TOKENIZE")
        0002 SEND_VAL string("<?php ") 1
        0003 SEND_VAR CV0($a) 2
        0004 DO_FCALL
        0005 RETURN int(1)
    "#]]
    .assert_eq(&dump(b"<?php $a = 1; phptoken::TOKENIZE('<?php ', $a);"));
    expect![[r#"

        $_main:
             ; (lines=10, args=0, vars=0, tmps=3)
             ; (before optimizer)
             ; :1-1
             ; return  [] RANGE[0..0]
        0000 INIT_STATIC_METHOD_CALL 1 string("PhpToken") string("hidden")
        0001 SEND_VAL_EX int(1) 1
        0002 DO_FCALL
        0003 INIT_STATIC_METHOD_CALL 1 string("Nope") string("m")
        0004 SEND_VAL_EX int(1) 1
        0005 DO_FCALL
        0006 INIT_STATIC_METHOD_CALL 1 string("M") string("m")
        0007 SEND_VAL_EX int(1) 1
        0008 DO_FCALL
        0009 RETURN int(1)
    "#]]
    .assert_eq(&dump(
        b"<?php PhpToken::hidden(1); Nope::m(1); class M {} M::m(1);",
    ));
}

/// The two `INIT_*_METHOD_CALL`s, read as
/// [`new_compiles_to_php_s_op_array`]'s cases were, and `INSTANCEOF`
/// as an argument sent.
#[test]
fn the_method_calls_compile_to_php_s_op_arrays() {
    let cases: &[(&[u8], Expect)] = &[
        (
            b"<?php $x = E::m();",
            expect![[r#"

                $_main:
                     ; (lines=4, args=0, vars=1, tmps=2)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 INIT_STATIC_METHOD_CALL 0 string("E") string("m")
                0001 V1 = DO_FCALL
                0002 ASSIGN CV0($x) V1
                0003 RETURN int(1)
            "#]],
        ),
        (
            b"<?php E::__construct();",
            expect![[r#"

                $_main:
                     ; (lines=3, args=0, vars=0, tmps=1)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 INIT_STATIC_METHOD_CALL 0 string("E") CONSTRUCTOR
                0001 DO_FCALL
                0002 RETURN int(1)
            "#]],
        ),
        (
            b"<?php $o = 1; $o->m(1, $o)->n();",
            expect![[r#"

                $_main:
                     ; (lines=8, args=0, vars=1, tmps=3)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 ASSIGN CV0($o) int(1)
                0001 INIT_METHOD_CALL 2 CV0($o) string("m")
                0002 SEND_VAL_EX int(1) 1
                0003 SEND_VAR_EX CV0($o) 2
                0004 V2 = DO_FCALL
                0005 INIT_METHOD_CALL 0 V2 string("n")
                0006 DO_FCALL
                0007 RETURN int(1)
            "#]],
        ),
        (
            b"<?php $o = 1; $o->m(f($o));",
            expect![[r#"

                $_main:
                     ; (lines=8, args=0, vars=1, tmps=3)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 ASSIGN CV0($o) int(1)
                0001 INIT_METHOD_CALL 1 CV0($o) string("m")
                0002 INIT_FCALL_BY_NAME 1 string("f")
                0003 SEND_VAR_EX CV0($o) 1
                0004 V2 = DO_FCALL_BY_NAME
                0005 SEND_VAR_NO_REF_EX V2 1
                0006 DO_FCALL
                0007 RETURN int(1)
            "#]],
        ),
        (
            b"<?php $o = 1; \\E::m($o instanceof \\E);",
            expect![[r#"

                $_main:
                     ; (lines=6, args=0, vars=1, tmps=3)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 ASSIGN CV0($o) int(1)
                0001 INIT_STATIC_METHOD_CALL 1 string("E") string("m")
                0002 T2 = INSTANCEOF CV0($o) string("E")
                0003 SEND_VAL_EX T2 1
                0004 DO_FCALL
                0005 RETURN int(1)
            "#]],
        ),
    ];
    for (source, expected) in cases {
        expected.assert_eq(&dump_of(source));
    }
    let (unit, _strings) = unit_and_strings(b"<?php E::m(); $o->n();");
    assert_eq!(
        unit.main.cache_size, 32,
        "each INIT takes two cache slots, the class and the method, eight bytes a slot"
    );
}

/// A member and a callee the program names, as the oracle's
/// `[regime.dump]` has them: the name's CV in `op2` of `FETCH_OBJ_R`,
/// `INIT_METHOD_CALL` and `INIT_STATIC_METHOD_CALL`, none of them
/// with a cache slot but the class's one, and a callee that is no
/// name, a parenthesised property among them, in `INIT_DYNAMIC_CALL`.
#[test]
fn a_member_and_a_callee_named_at_run_time_are_the_oracle_s_operands() {
    expect![[r#"

        $_main:
             ; (lines=12, args=0, vars=4, tmps=6)
             ; (before optimizer)
             ; :1-1
             ; return  [] RANGE[0..0]
        0000 T4 = FETCH_OBJ_R CV0($o) CV1($p)
        0001 ECHO T4
        0002 INIT_METHOD_CALL 0 CV0($o) CV2($m)
        0003 DO_FCALL
        0004 INIT_STATIC_METHOD_CALL 0 string("C") CV2($m)
        0005 DO_FCALL
        0006 INIT_DYNAMIC_CALL 0 CV3($f)
        0007 DO_FCALL
        0008 T8 = FETCH_OBJ_R CV0($o) string("p")
        0009 INIT_DYNAMIC_CALL 0 T8
        0010 DO_FCALL
        0011 RETURN int(1)
    "#]]
    .assert_eq(&dump_of(
        b"<?php echo $o->$p; $o->$m(); C::$m(); $f(); ($o->p)();",
    ));
}

/// What stands beside these four and is not read yet is refused as
/// such.
#[test]
fn what_stands_beside_new_and_the_calls_is_refused_as_not_yet_read() {
    let refused = |source: &[u8]| {
        let mut strings = php_heap::Strings::new();
        compile_parsed(source, &bundled(), &mut strings)
            .err()
            .map(|e| e.message.to_string())
    };
    assert_eq!(refused(b"<?php echo E::class;").as_deref(), None);
    assert_eq!(refused(b"<?php f($o->p, name: $o->q);").as_deref(), None);
}

/// `INSTANCEOF` as the oracle at 8.5.9 dumps it: into a temporary,
/// freed where the test is a statement; `false` where the left side
/// is a constant; a chain grouped to the left, the second over the
/// first's temporary.
#[test]
fn instanceof_compiles_to_php_s_op_array() {
    let cases: &[(&[u8], Expect)] = &[
        (
            b"<?php $o = 1; $o instanceof E;",
            expect![[r#"

                $_main:
                     ; (lines=4, args=0, vars=1, tmps=2)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 ASSIGN CV0($o) int(1)
                0001 T2 = INSTANCEOF CV0($o) string("E")
                0002 FREE T2
                0003 RETURN int(1)
            "#]],
        ),
        (
            b"<?php $o = 1; echo 1 instanceof E, strlen('a') instanceof E;",
            expect![[r#"

                $_main:
                     ; (lines=4, args=0, vars=1, tmps=1)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 ASSIGN CV0($o) int(1)
                0001 ECHO bool(false)
                0002 ECHO bool(false)
                0003 RETURN int(1)
            "#]],
        ),
        (
            b"<?php $o = 1; echo $o instanceof E instanceof F;",
            expect![[r#"

                $_main:
                     ; (lines=5, args=0, vars=1, tmps=3)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 ASSIGN CV0($o) int(1)
                0001 T2 = INSTANCEOF CV0($o) string("E")
                0002 T3 = INSTANCEOF T2 string("F")
                0003 ECHO T3
                0004 RETURN int(1)
            "#]],
        ),
    ];
    for (source, expected) in cases {
        expected.assert_eq(&dump_of(source));
    }
}

/// `FETCH_THIS` as the oracle dumps it: `$this` into a temporary
/// where it is read and into a `VAR` where it is a receiver or is
/// sent to a callee nothing is known of (`zend_compile_simple_var`,
/// `zend_compile_args`).
#[test]
fn this_compiles_to_fetch_this_as_php_emits_it() {
    let cases: &[(&[u8], Expect)] = &[
        (
            b"<?php echo $this;",
            expect![[r#"

                $_main:
                     ; (lines=3, args=0, vars=0, tmps=1)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 T0 = FETCH_THIS
                0001 ECHO T0
                0002 RETURN int(1)
            "#]],
        ),
        (
            b"<?php $this->p;",
            expect![[r#"

                $_main:
                     ; (lines=4, args=0, vars=0, tmps=2)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 V0 = FETCH_THIS
                0001 T1 = FETCH_OBJ_R V0 string("p")
                0002 FREE T1
                0003 RETURN int(1)
            "#]],
        ),
        (
            b"<?php $this->m();",
            expect![[r#"

                $_main:
                     ; (lines=4, args=0, vars=0, tmps=2)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 V0 = FETCH_THIS
                0001 INIT_METHOD_CALL 0 V0 string("m")
                0002 DO_FCALL
                0003 RETURN int(1)
            "#]],
        ),
        (
            b"<?php f($this);",
            expect![[r#"

                $_main:
                     ; (lines=5, args=0, vars=0, tmps=2)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 INIT_FCALL_BY_NAME 1 string("f")
                0001 V0 = FETCH_THIS
                0002 SEND_VAR_EX V0 1
                0003 DO_FCALL_BY_NAME
                0004 RETURN int(1)
            "#]],
        ),
    ];
    for (source, expected) in cases {
        expected.assert_eq(&dump_of(source));
    }
}

/// A class reference of the scope and a class the program computes,
/// which were refused as not read, as the oracle's `[regime.dump]`
/// has them in the main op array: `self`, `parent` and `static` are
/// their fetch type in an unused operand, the scope being the
/// includer's and so no compile-time error, and an expression is
/// `FETCH_CLASS` into a `VAR`, `instanceof`'s under
/// `ZEND_FETCH_CLASS_NO_AUTOLOAD` and `_SILENT` beside
/// `_EXCEPTION`.
#[test]
fn a_class_reference_of_the_scope_or_an_expression_compiles_as_php_emits_it() {
    expect![[r#"
        0000 T2 = FETCH_STATIC_PROP_R string("p") (self) (exception)
        0001 ECHO T2
        0002 V3 = FETCH_CLASS (exception) CV0($c)
        0003 V4 = NEW 0 V3
        0004 DO_FCALL
        0005 FREE V4
        0006 INIT_FCALL 1 96 string("var_dump")
        0007 V6 = FETCH_CLASS (no-autoload) (silent) (exception) CV0($c)
        0008 T7 = INSTANCEOF CV1($o) V6
        0009 SEND_VAL T7 1
        0010 DO_ICALL
        0011 V9 = FETCH_CLASS (exception) CV1($o)
        0012 T10 = FETCH_CLASS_CONSTANT V9 string("X")
        0013 ECHO T10
        0014 T11 = FETCH_CLASS_CONSTANT (static) (exception) string("Y")
        0015 ECHO T11
        0016 V12 = FETCH_CLASS (exception) CV1($o)
        0017 INIT_STATIC_METHOD_CALL 0 V12 string("m")
        0018 DO_FCALL
        0019 ASSIGN_STATIC_PROP string("q")
        0020 OP_DATA int(1)
        0021 V15 = NEW 0 (static) (exception)
        0022 DO_FCALL
        0023 FREE V15
        0024 RETURN int(1)
        LIVE RANGES:
             4: 0004 - 0005 (new)
             15: 0022 - 0023 (new)
    "#]].assert_eq(&oplines_of(
            b"<?php echo self::$p; new $c; var_dump($o instanceof $c); echo $o::X, static::Y; $o::m(); parent::$q = 1; new static;"
        ));
    let refused_message = |source: &[u8]| refused(source).map(|e| e.message.to_string());
    assert_eq!(
        refused_message(b"<?php function f() { return self::X; }").as_deref(),
        Some("Cannot use \"self\" when no class scope is active")
    );
    assert_eq!(
        refused_message(b"<?php class A { function f() { new parent; } }").as_deref(),
        Some("Cannot use \"parent\" when current class scope has no parent")
    );
    assert_eq!(
        refused_message(b"<?php class A { const X = static::Y; }").as_deref(),
        Some("\"static::\" is not allowed in compile-time constants")
    );
}

/// #531's program under the oracle's `[regime.dump]`: a `JMP_NULL`
/// over the object before each `?->`, its target the opline after
/// the chain and its `result` the chain's own, a call's arguments
/// inside the jump, and `"$a?->b"` a chain of its own inside the
/// string.
#[test]
fn a_nullsafe_chain_is_jmp_null_over_each_link_as_php_emits_it() {
    expect![[r#"

        $_main:
             ; (lines=16, args=0, vars=1, tmps=6)
             ; (before optimizer)
             ; :1-1
             ; return  [] RANGE[0..0]
        0000 ASSIGN CV0($a) null
        0001 T2 = JMP_NULL CV0($a) 0003
        0002 T2 = FETCH_OBJ_R CV0($a) string("b")
        0003 ECHO T2
        0004 T4 = JMP_NULL CV0($a) 0010
        0005 INIT_METHOD_CALL 1 CV0($a) string("m")
        0006 SEND_VAL_EX int(1) 1
        0007 V3 = DO_FCALL
        0008 T4 = JMP_NULL V3 0010
        0009 T4 = FETCH_OBJ_R V3 string("c")
        0010 ECHO T4
        0011 T5 = JMP_NULL CV0($a) 0013
        0012 T5 = FETCH_OBJ_R CV0($a) string("b")
        0013 T6 = CAST (string) T5
        0014 ECHO T6
        0015 RETURN int(1)
        LIVE RANGES:
             3: 0008 - 0009 (tmp/var)
    "#]]
    .assert_eq(&dump_of(
        b"<?php $a = null; echo $a?->b, $a?->m(1)?->c, \"$a?->b\";",
    ));
}

/// php's compile errors where a `?->` chain is written to or bound.
#[test]
fn a_nullsafe_chain_written_to_is_phps_compile_error() {
    let cases: &[(&[u8], &str)] = &[
        (
            b"<?php $a?->b = 1;",
            "Can't use nullsafe operator in write context",
        ),
        (
            b"<?php $a?->b++;",
            "Can't use nullsafe operator in write context",
        ),
        (
            b"<?php $a?->b .= 1;",
            "Can't use nullsafe operator in write context",
        ),
        (
            b"<?php unset($a?->b);",
            "Can't use nullsafe operator in write context",
        ),
        (
            b"<?php $a?->b[0] = 1;",
            "Can't use nullsafe operator in write context",
        ),
        (
            b"<?php foreach ($x as $a?->b) {}",
            "Can't use nullsafe operator in write context",
        ),
        (
            b"<?php $x = &$a?->b;",
            "Cannot take reference of a nullsafe chain",
        ),
        (
            b"<?php function &f() { return $a?->b[0]; }",
            "Cannot take reference of a nullsafe chain",
        ),
        (
            b"<?php [$a?->b] = $x;",
            "Assignments can only happen to writable values",
        ),
        (
            b"<?php $a?->m()++;",
            "Can't use method return value in write context",
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
