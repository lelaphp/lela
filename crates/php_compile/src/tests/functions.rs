//! Functions and methods: their op arrays, their parameters and
//! defaults, return types and masks, and by-reference parameters and
//! returns.

use super::*;

/// The oracle's dump of two arrow functions returning `never`
/// (#1262): `zend_compile_func_decl` wraps neither body in a
/// `return`, so the `throw` stands as an expression statement and
/// `42`, a constant, leaves no opline; each ends in the implicit
/// `VERIFY_NEVER_TYPE`.
#[test]
fn an_arrow_function_returning_never_compiles_its_body_as_a_statement() {
    expect![[r#"

        $_main:
             ; (lines=5, args=0, vars=2, tmps=4)
             ; (before optimizer)
             ; :1-4
             ; return  [] RANGE[0..0]
        0000 T2 = DECLARE_LAMBDA_FUNCTION 0
        0001 ASSIGN CV0($a) T2
        0002 T4 = DECLARE_LAMBDA_FUNCTION 1
        0003 ASSIGN CV1($b) T4
        0004 RETURN int(1)

        {closure::2}:
             ; (lines=5, args=0, vars=0, tmps=2)
             ; (before optimizer)
             ; :2-2
             ; return  [] RANGE[0..0]
        0000 V0 = NEW 1 string("Exception")
        0001 SEND_VAL_EX string("Here") 1
        0002 DO_FCALL
        0003 THROW V0
        0004 VERIFY_NEVER_TYPE
        LIVE RANGES:
             0: 0001 - 0003 (new)

        {closure::3}:
             ; (lines=1, args=0, vars=0, tmps=0)
             ; (before optimizer)
             ; :3-3
             ; return  [] RANGE[0..0]
        0000 VERIFY_NEVER_TYPE
    "#]]
    .assert_eq(&dump_of(
        b"<?php\n$a = fn(): never => throw new Exception(\"Here\");\n$b = fn(): never => 42;\n",
    ));
}

/// `zend_compile_func_decl` as the oracle at 8.5.9 dumps it under
/// `opcache.opt_debug_level=0x10000` (#309): a function and a
/// class's methods each an op array of its own after the main one,
/// named `f` and `C::m`, with `RECV` and `RECV_INIT` for the
/// parameters, the body, and php's implicit `RETURN null` on the
/// closing brace's line; a call to a function the file has declared
/// is `INIT_FCALL` with `zend_vm_calc_used_stack`'s number and
/// `DO_UCALL`, a call on the class to a method it has is `DO_UCALL`
/// too, and a method call is `DO_FCALL` still; `$this` inside an
/// instance method is the frame's own, `THIS`, where it is a
/// receiver, and `FETCH_THIS` where it is read.
#[test]
fn a_function_and_a_method_compile_to_their_own_op_arrays() {
    let source: &[u8] = b"<?php
function f($a, $b = 2) {
    echo $a;
    return $a + $b;
}
class C {
    public $p = 1;
    public function m($x) {
        echo $this->p;
        return $this->m2();
    }
    public static function s() { return 3; }
    private function m2() {}
}
$o = new C;
echo f(1), C::s(), $o->m(1), $o->p;
";
    expect![[r#"

        $_main:
             ; (lines=17, args=0, vars=1, tmps=7)
             ; (before optimizer)
             ; :1-17
             ; return  [] RANGE[0..0]
        0000 V1 = NEW 0 string("C")
        0001 DO_FCALL
        0002 ASSIGN CV0($o) V1
        0003 INIT_FCALL 1 128 string("f")
        0004 SEND_VAL int(1) 1
        0005 V4 = DO_UCALL
        0006 ECHO V4
        0007 INIT_STATIC_METHOD_CALL 0 string("C") string("s")
        0008 V5 = DO_UCALL
        0009 ECHO V5
        0010 INIT_METHOD_CALL 1 CV0($o) string("m")
        0011 SEND_VAL_EX int(1) 1
        0012 V6 = DO_FCALL
        0013 ECHO V6
        0014 T7 = FETCH_OBJ_R CV0($o) string("p")
        0015 ECHO T7
        0016 RETURN int(1)
        LIVE RANGES:
             1: 0001 - 0002 (new)

        f:
             ; (lines=6, args=2, vars=2, tmps=1)
             ; (before optimizer)
             ; :2-5
             ; return  [] RANGE[0..0]
        0000 CV0($a) = RECV 1
        0001 CV1($b) = RECV_INIT 2 int(2)
        0002 ECHO CV0($a)
        0003 T2 = ADD CV0($a) CV1($b)
        0004 RETURN T2
        0005 RETURN null

        C::m:
             ; (lines=7, args=1, vars=1, tmps=2)
             ; (before optimizer)
             ; :8-11
             ; return  [] RANGE[0..0]
        0000 CV0($x) = RECV 1
        0001 T1 = FETCH_OBJ_R THIS string("p")
        0002 ECHO T1
        0003 INIT_METHOD_CALL 0 THIS string("m2")
        0004 V2 = DO_FCALL
        0005 RETURN V2
        0006 RETURN null

        C::s:
             ; (lines=2, args=0, vars=0, tmps=0)
             ; (before optimizer)
             ; :12-12
             ; return  [] RANGE[0..0]
        0000 RETURN int(3)
        0001 RETURN null

        C::m2:
             ; (lines=1, args=0, vars=0, tmps=0)
             ; (before optimizer)
             ; :13-13
             ; return  [] RANGE[0..0]
        0000 RETURN null
    "#]]
    .assert_eq(&dump_of(source));
    let (unit, _strings) = unit_and_strings(source);
    assert_eq!(
        unit.functions[0].1.safepoints.first(),
        Some(&OplineIdx(2)),
        "the entry safepoint stands after the RECV prologue"
    );
    assert_eq!(unit.functions[0].0.parameters.len(), 2);
    assert_eq!(
        unit.classes[0]
            .methods
            .iter()
            .map(|m| m.is_instance)
            .collect::<Vec<_>>(),
        vec![true, false, true]
    );
    assert_eq!(unit.methods.len(), 3);
}

/// `zend_compile_params` under
/// Every optional parameter before the last required one is a
/// required `RECV` with no default (`zend_compile_params`,
/// `Z/Zend/zend_compile.c:7886`), the `Type $x = null` spelling as
/// much as `?Type $x = null`: the first only skips the deprecation.
/// The dump is the oracle's under `[regime.dump]` at 8.5.9 (#2684).
#[test]
fn an_optional_parameter_before_a_required_one_is_a_recv_whatever_its_spelling() {
    expect![[r#"

        $_main:
             ; (lines=1, args=0, vars=0, tmps=0)
             ; (before optimizer)
             ; :1-1
             ; return  [] RANGE[0..0]
        0000 RETURN int(1)

        f:
             ; (lines=5, args=4, vars=4, tmps=0)
             ; (before optimizer)
             ; :1-1
             ; return  [] RANGE[0..0]
        0000 CV0($a) = RECV 1
        0001 CV1($b) = RECV 2
        0002 CV2($c) = RECV 3
        0003 CV3($d) = RECV 4
        0004 RETURN null
    "#]]
    .assert_eq(&dump_of(
        b"<?php function f(?int $a = null, $b, Foo $c = null, $d) {}",
    ));
}

/// A parameter's default is `zend_const_expr_to_zval`'s: under
/// `ZEND_COMPILE_NO_PERSISTENT_CONSTANT_SUBSTITUTION` no constant is
/// substituted, so `self::X` stays the `IS_CONSTANT_AST` the dump prints
/// as `zval(type=11)`, while a class's name is none and
/// `zend_try_compile_const_expr_resolve_class_name` resolves `self::class`
/// and `parent::class` in a class with a parent, and `Baz::class`, to
/// their strings. The dump is the oracle's under `[regime.dump]` at 8.5.9
/// (#2685).
#[test]
fn a_class_name_in_a_parameter_default_folds_to_its_string() {
    expect![[r#"

        $_main:
             ; (lines=1, args=0, vars=0, tmps=0)
             ; (before optimizer)
             ; :1-1
             ; return  [] RANGE[0..0]
        0000 RETURN int(1)

        C::f:
             ; (lines=5, args=4, vars=4, tmps=0)
             ; (before optimizer)
             ; :1-1
             ; return  [] RANGE[0..0]
        0000 CV0($a) = RECV_INIT 1 string("C")
        0001 CV1($b) = RECV_INIT 2 string("P")
        0002 CV2($c) = RECV_INIT 3 string("Baz")
        0003 CV3($d) = RECV_INIT 4 zval(type=11)
        0004 RETURN null
    "#]].assert_eq(&dump_of(b"<?php class P {} class C extends P { function f($a = self::class, $b = parent::class, $c = Baz::class, $d = self::X) {} }"));
}

/// `ZEND_COMPILE_NO_PERSISTENT_CONSTANT_SUBSTITUTION` (#384): a
/// default naming a constant is not folded, so its `RECV_INIT`
/// carries the `IS_CONSTANT_AST` value the oracle dumps as
/// `zval(type=11)`, the constant expression beside the op array's
/// constant table in the order the parameters were compiled, and the
/// name as written for Reflection. A default that folds is the value
/// it folds to, as it always was.
#[test]
fn a_parameter_default_naming_a_constant_is_an_unevaluated_constant() {
    let source: &[u8] =
        b"<?php\nclass A { const X = 5; }\nfunction f($a = PHP_INT_MAX, $b = A::X, $c = 1) { echo $a; }\n";
    expect![[r#"

        $_main:
             ; (lines=1, args=0, vars=0, tmps=0)
             ; (before optimizer)
             ; :1-4
             ; return  [] RANGE[0..0]
        0000 RETURN int(1)

        f:
             ; (lines=5, args=3, vars=3, tmps=0)
             ; (before optimizer)
             ; :3-3
             ; return  [] RANGE[0..0]
        0000 CV0($a) = RECV_INIT 1 zval(type=11)
        0001 CV1($b) = RECV_INIT 2 zval(type=11)
        0002 CV2($c) = RECV_INIT 3 int(1)
        0003 ECHO CV0($a)
        0004 RETURN null
    "#]]
    .assert_eq(&dump_of(source));
    let (unit, _strings) = unit_and_strings(source);
    let body = &unit.functions[0].1;
    let unevaluated: Vec<&ConstExpr> = body
        .literals
        .iter()
        .filter_map(|constant| constant.as_const_expr())
        .filter_map(|at| body.const_exprs.get(at as usize))
        .collect();
    assert_eq!(
        unevaluated,
        vec![
            &ConstExpr::Constant {
                name: ConstantName::Resolved(Name(b"PHP_INT_MAX".as_slice().into())),
                line: 3,
            },
            &ConstExpr::ClassConstant {
                class: Name(b"A".as_slice().into()),
                class_kind: NameKind::NotFullyQualified,
                name: Name(b"X".as_slice().into()),
                line: 3,
            },
        ]
    );
    assert_eq!(
        unit.functions[0]
            .0
            .parameters
            .iter()
            .map(|p| p.default.expr().cloned())
            .collect::<Vec<_>>(),
        vec![
            Some(ConstExpr::Constant {
                name: ConstantName::Resolved(Name(b"PHP_INT_MAX".as_slice().into())),
                line: 3,
            }),
            Some(ConstExpr::ClassConstant {
                class: Name(b"A".as_slice().into()),
                class_kind: NameKind::NotFullyQualified,
                name: Name(b"X".as_slice().into()),
                line: 3,
            }),
            Some(ConstExpr::long(1)),
        ]
    );
}

/// `zend_compile_params` and `zend_compile_return_type` as the
/// oracle at 8.5.9 dumps them under
/// `opcache.opt_debug_level=0x10000` (#385): a typed parameter is a
/// `RECV` still, one with a default a `RECV_INIT`, and a `...` a
/// `RECV_VARIADIC` numbered after the rest and left out of `args`;
/// a declared return type is a `VERIFY_RETURN_TYPE` before each
/// `RETURN` that needs one and before php's implicit final return,
/// `void` needs none, and `function &g` returns by reference, whose
/// implicit return carries `-1` and prints `(function)`; a promoted
/// parameter is the `ASSIGN_OBJ`/`OP_DATA` pair after the prologue.
#[test]
#[allow(clippy::too_many_lines)] // one op array per declaration: the dump is the test
fn typed_parameters_and_a_return_type_compile_to_php_s_op_arrays() {
    let source: &[u8] = b"<?php
function f(int $a, ?string $b = null, float ...$rest): int {
    return $a;
}
function &g(array &$x) { return $x; }
function h(): void {}
class C {
    public function __construct(public int $p, protected readonly string $q = \"s\") {}
}
";
    expect![[r#"

        $_main:
             ; (lines=1, args=0, vars=0, tmps=0)
             ; (before optimizer)
             ; :1-10
             ; return  [] RANGE[0..0]
        0000 RETURN int(1)

        f:
             ; (lines=7, args=2, vars=3, tmps=0)
             ; (before optimizer)
             ; :2-4
             ; return  [] RANGE[0..0]
        0000 CV0($a) = RECV 1
        0001 CV1($b) = RECV_INIT 2 null
        0002 CV2($rest) = RECV_VARIADIC 3
        0003 VERIFY_RETURN_TYPE CV0($a)
        0004 RETURN CV0($a)
        0005 VERIFY_RETURN_TYPE
        0006 RETURN null

        g:
             ; (lines=3, args=1, vars=1, tmps=0)
             ; (before optimizer)
             ; :5-5
             ; return  [] RANGE[0..0]
        0000 CV0($x) = RECV 1
        0001 RETURN_BY_REF CV0($x)
        0002 RETURN_BY_REF (function) null

        h:
             ; (lines=1, args=0, vars=0, tmps=0)
             ; (before optimizer)
             ; :6-6
             ; return  [] RANGE[0..0]
        0000 RETURN null

        C::__construct:
             ; (lines=7, args=2, vars=2, tmps=0)
             ; (before optimizer)
             ; :8-8
             ; return  [] RANGE[0..0]
        0000 CV0($p) = RECV 1
        0001 CV1($q) = RECV_INIT 2 string("s")
        0002 ASSIGN_OBJ THIS string("p")
        0003 OP_DATA CV0($p)
        0004 ASSIGN_OBJ THIS string("q")
        0005 OP_DATA CV1($q)
        0006 RETURN null
    "#]]
    .assert_eq(&dump_of(source));
    let (unit, _strings) = unit_and_strings(source);
    let f = &unit.functions[0].0;
    assert_eq!(f.return_type, Some(Type::Int));
    assert!(!f.returns_by_ref);
    assert_eq!(
        f.parameters
            .iter()
            .map(|p| (
                p.ty.clone(),
                p.by_ref,
                p.variadic,
                p.default.expr().is_some()
            ))
            .collect::<Vec<_>>(),
        vec![
            (Some(Type::Int), false, false, false),
            (
                Some(Type::Nullable(Box::new(Type::String))),
                false,
                false,
                true
            ),
            (Some(Type::Float), false, true, false),
        ],
        "the implicit `?string` of a default of null, and the variadic's own type"
    );
    let g = &unit.functions[1].0;
    assert!(g.returns_by_ref, "`function &g` returns by reference");
    assert!(g.parameters[0].by_ref, "`&$x` is taken by reference");
    assert_eq!(
        unit.functions[0].1.safepoints.first(),
        Some(&OplineIdx(3)),
        "the entry safepoint stands after the whole RECV prologue"
    );
    assert_eq!(
        unit.classes[0]
            .properties
            .iter()
            .map(|p| (
                String::from_utf8_lossy(&p.name.0).into_owned(),
                p.visibility,
                p.is_readonly
            ))
            .collect::<Vec<_>>(),
        vec![
            ("p".to_owned(), Visibility::Public, false),
            ("q".to_owned(), Visibility::Protected, true),
        ],
        "a promoted parameter is a declared property of the class"
    );
}

/// `ZEND_TYPE_PURE_MASK(arg_info[-1].type)` on every op array: the
/// declared return type's mask wherever one is declared, the
/// `string` `zend_compile_params` falls back to for `__toString` and
/// the `void` a `set` hook is given among them, and 0 where none is
/// declared, the main op array's included.
#[test]
fn an_op_array_s_return_mask_is_its_declared_return_type_s_mask() {
    let (unit, _strings) = unit_and_strings(
        b"<?php
function a(): int { return 1; }
function b(): ?int { return null; }
function c(): int|string|null { return 1; }
function d(): Countable { return new ArrayObject; }
function e() { return 1; }
class K {
    public int $p { get => 1; set { } }
    public function s(): static { return $this; }
    public function m(): self|false { return $this; }
    public function __toString() { return \"\"; }
}
$f = fn (): float => 1.5;
",
    );
    let mask = |decl: &FunctionDecl| decl.return_type.as_ref().map_or(0, Type::mask);
    let mut held: Vec<(String, u32, u32)> = Vec::new();
    let mut take = |decl: &FunctionDecl, code: &OpArray| {
        let name = String::from_utf8_lossy(&decl.name.0).into_owned();
        held.push((name, mask(decl), code.return_mask));
    };
    for (decl, code) in unit.functions.iter().chain(&unit.dynamic) {
        take(decl, code);
    }
    for body in &unit.methods {
        let class = &unit.classes[body.class as usize];
        take(&class.methods[body.method as usize].function, &body.body);
    }
    for body in &unit.hooks {
        let property = &unit.classes[body.class as usize].properties[body.property as usize];
        let hook = property
            .hooks
            .as_ref()
            .and_then(|hooks| hooks.of(body.kind))
            .expect("a hook's body has its declaration");
        take(&hook.function, &body.body);
    }
    for (name, declared, carried) in &held {
        assert_eq!(declared, carried, "{name}");
    }
    let carried = |name: &str| {
        let found = held.iter().find(|(held, ..)| held == name);
        found.map(|(.., carried)| *carried)
    };
    assert_eq!(unit.main.return_mask, 0, "the main op array declares none");
    assert_eq!(carried("a"), Some(1 << 4));
    assert_eq!(carried("b"), Some((1 << 4) | (1 << 1)));
    assert_eq!(carried("c"), Some((1 << 4) | (1 << 6) | (1 << 1)));
    assert_eq!(carried("d"), Some(0), "a class name contributes no bit");
    assert_eq!(carried("e"), Some(0), "no return type is declared");
    assert_eq!(carried("s"), Some(1 << 15), "`static` is a bit of its own");
    assert_eq!(carried("m"), Some(1 << 2), "`self` contributes no bit");
    assert_eq!(carried("__toString"), Some(1 << 6), "the fallback `string`");
    assert_eq!(carried("$p::get"), Some(1 << 4), "the property's type");
    assert_eq!(
        carried("$p::set"),
        Some(1 << 14),
        "the `void` of a `set` hook"
    );
    let closure = held.iter().find(|(name, ..)| name.starts_with("{closure:"));
    assert_eq!(closure.map(|(.., carried)| *carried), Some(1 << 5));
    assert_eq!(held.len(), 11, "every function, method, hook and closure");
}

/// An argument the callee the table has declares by reference, as the
/// oracle dumps it (`zend_compile_args` under
/// `ARG_SHOULD_BE_SENT_BY_REF`): `SEND_REF` over the variable's CV,
/// the variadic's bit for every argument past the declared ones, and
/// `SEND_VAL_EX` for an argument that is no variable, which php
/// leaves to the handler to refuse. A by-reference return is
/// `RETURN_BY_REF` at each `return` and at the end of the body, and
/// `$b = &f($a)` is `ASSIGN_REF` over the call's `VAR` carrying
/// `ZEND_RETURNS_FUNCTION`.
#[test]
fn a_by_reference_parameter_is_sent_by_ref_as_php_emits_it() {
    let cases: &[(&[u8], Expect)] = &[
        (
            b"<?php function f(&$x) {} $a = 1; f($a);",
            expect![[r#"

                $_main:
                     ; (lines=5, args=0, vars=1, tmps=2)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 ASSIGN CV0($a) int(1)
                0001 INIT_FCALL 1 96 string("f")
                0002 SEND_REF CV0($a) 1
                0003 DO_UCALL
                0004 RETURN int(1)

                f:
                     ; (lines=2, args=1, vars=1, tmps=0)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 CV0($x) = RECV 1
                0001 RETURN null
            "#]],
        ),
        (
            b"<?php function f(&...$r) {} $a = 1; f($a, $a);",
            expect![[r#"

                $_main:
                     ; (lines=6, args=0, vars=1, tmps=2)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 ASSIGN CV0($a) int(1)
                0001 INIT_FCALL 2 128 string("f")
                0002 SEND_REF CV0($a) 1
                0003 SEND_REF CV0($a) 2
                0004 DO_UCALL
                0005 RETURN int(1)

                f:
                     ; (lines=2, args=0, vars=1, tmps=0)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 CV0($r) = RECV_VARIADIC 1
                0001 RETURN null
            "#]],
        ),
        (
            b"<?php function f(&$x) {} f(1);",
            expect![[r#"

                $_main:
                     ; (lines=4, args=0, vars=0, tmps=1)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 INIT_FCALL 1 96 string("f")
                0001 SEND_VAL_EX int(1) 1
                0002 DO_UCALL
                0003 RETURN int(1)

                f:
                     ; (lines=2, args=1, vars=1, tmps=0)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 CV0($x) = RECV 1
                0001 RETURN null
            "#]],
        ),
        (
            b"<?php function &f(&$x) { return $x; } $a = 1; $b = &f($a);",
            expect![[r#"

                $_main:
                     ; (lines=6, args=0, vars=2, tmps=3)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 ASSIGN CV0($a) int(1)
                0001 INIT_FCALL 1 96 string("f")
                0002 SEND_REF CV0($a) 1
                0003 V3 = DO_UCALL
                0004 ASSIGN_REF (function) CV1($b) V3
                0005 RETURN int(1)

                f:
                     ; (lines=3, args=1, vars=1, tmps=0)
                     ; (before optimizer)
                     ; :1-1
                     ; return  [] RANGE[0..0]
                0000 CV0($x) = RECV 1
                0001 RETURN_BY_REF CV0($x)
                0002 RETURN_BY_REF (function) null
            "#]],
        ),
    ];
    for (source, expected) in cases {
        expected.assert_eq(&dump_of(source));
    }
}

/// The body of a function written `function &f`: `RETURN_BY_REF` at
/// each `return` and the one `zend_emit_final_return` closes it with,
/// which carries `ZEND_RETURNS_FUNCTION` as php's does.
#[test]
fn a_by_reference_return_is_return_by_ref_as_php_emits_it() {
    expect![[r#"

        $_main:
             ; (lines=1, args=0, vars=0, tmps=0)
             ; (before optimizer)
             ; :1-1
             ; return  [] RANGE[0..0]
        0000 RETURN int(1)

        f:
             ; (lines=3, args=1, vars=1, tmps=0)
             ; (before optimizer)
             ; :1-1
             ; return  [] RANGE[0..0]
        0000 CV0($x) = RECV 1
        0001 RETURN_BY_REF CV0($x)
        0002 RETURN_BY_REF (function) null
    "#]]
    .assert_eq(&dump_of(b"<?php function &f(&$x) { return $x; }"));
}
