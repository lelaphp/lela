//! Class declarations: a class bound with no opline, in a namespace, an
//! enum, property hooks, `self` and `parent` while binding, and what
//! php refuses in a class.

use super::*;

/// A hook with no body, an interface's or an abstract property's, is
/// an op array `zend_foreach_op_array` walks, since it walks a
/// property's hooks by scope and holds only the function table to
/// `ZEND_ACC_ABSTRACT`: `get` prints `VERIFY_RETURN_TYPE` where the
/// property is typed and `RETURN null`, `set` its `RECV` and
/// `RETURN null`, and the abstract method prints nothing. The dump
/// is the oracle's under `[regime.dump]` at 8.5.9 (#2674).
#[test]
fn an_abstract_hook_is_dumped_and_an_abstract_method_is_not() {
    assert_eq!(
        dump_of(
            b"<?php interface I { public mixed $i { get; } } \
              abstract class A { abstract public int $p { get; set; } abstract function m(); }"
        ),
        "\n$_main:\n\
         \x20    ; (lines=1, args=0, vars=0, tmps=0)\n\
         \x20    ; (before optimizer)\n\
         \x20    ; :1-1\n\
         \x20    ; return  [] RANGE[0..0]\n\
         0000 RETURN int(1)\n\
         \n\
         I::$i::get:\n\
         \x20    ; (lines=2, args=0, vars=0, tmps=0)\n\
         \x20    ; (before optimizer)\n\
         \x20    ; :1-1\n\
         \x20    ; return  [] RANGE[0..0]\n\
         0000 VERIFY_RETURN_TYPE\n\
         0001 RETURN null\n\
         \n\
         A::$p::get:\n\
         \x20    ; (lines=2, args=0, vars=0, tmps=0)\n\
         \x20    ; (before optimizer)\n\
         \x20    ; :1-1\n\
         \x20    ; return  [] RANGE[0..0]\n\
         0000 VERIFY_RETURN_TYPE\n\
         0001 RETURN null\n\
         \n\
         A::$p::set:\n\
         \x20    ; (lines=2, args=1, vars=1, tmps=0)\n\
         \x20    ; (before optimizer)\n\
         \x20    ; :1-1\n\
         \x20    ; return  [] RANGE[0..0]\n\
         0000 CV0($value) = RECV 1\n\
         0001 RETURN null\n"
    );
}

/// A class completes after the classes its hooks declare, so php's
/// `CG(class_table)` holds an anonymous class written in a hook ahead
/// of the class around it, and the dump prints the anonymous class's
/// hook and method before the hook it stands in. The dump is the
/// oracle's under `[regime.dump]` at 8.5.9 (#2690).
#[test]
fn a_class_declared_in_a_hook_is_dumped_before_the_hook_s_class() {
    assert_eq!(
        dump_of(
            b"<?php class T { public $p { get => new class { \
              public $q { get => 1; } function m() {} }; } }"
        ),
        "\n$_main:\n\
         \x20    ; (lines=1, args=0, vars=0, tmps=0)\n\
         \x20    ; (before optimizer)\n\
         \x20    ; :1-1\n\
         \x20    ; return  [] RANGE[0..0]\n\
         0000 RETURN int(1)\n\
         \n\
         class@anonymous::m:\n\
         \x20    ; (lines=1, args=0, vars=0, tmps=0)\n\
         \x20    ; (before optimizer)\n\
         \x20    ; :1-1\n\
         \x20    ; return  [] RANGE[0..0]\n\
         0000 RETURN null\n\
         \n\
         class@anonymous::$q::get:\n\
         \x20    ; (lines=2, args=0, vars=0, tmps=0)\n\
         \x20    ; (before optimizer)\n\
         \x20    ; :1-1\n\
         \x20    ; return  [] RANGE[0..0]\n\
         0000 RETURN int(1)\n\
         0001 RETURN null\n\
         \n\
         T::$p::get:\n\
         \x20    ; (lines=5, args=0, vars=0, tmps=3)\n\
         \x20    ; (before optimizer)\n\
         \x20    ; :1-1\n\
         \x20    ; return  [] RANGE[0..0]\n\
         0000 V0 = DECLARE_ANON_CLASS string(\"class@anonymous\\x00:1$0\")\n\
         0001 V1 = NEW 0 V0\n\
         0002 DO_FCALL\n\
         0003 RETURN V1\n\
         0004 RETURN null\n\
         LIVE RANGES:\n\
         \x20    1: 0002 - 0003 (new)\n"
    );
}

/// `zend_compile_class_decl` names a class `zend_prefix_with_ns` over
/// the written name, so `class M` under `namespace A;` declares
/// `A\M`, the name `zend_resolve_class_name` gives `new M` and `M::K`
/// in the same namespace (#322). The dump is the oracle's under
/// `[regime.dump]` at 8.5.9 of this program, `NEW 0 string("A\M")`
/// with its `DO_FCALL` and `FREE` (#322's own `echo M::K` stands
/// aside here and is the class constant test's in properties.rs); a
/// file written under `opcache.file_update_protection`'s two seconds
/// before the oracle reads it prints no dump at all, which is the
/// empty dump #322 measured, and not the class. `__CLASS__` in the
/// body folds to the prefixed name, and a fatal names it as php's
/// does.
#[test]
fn a_class_declared_in_a_namespace_takes_the_namespace_on_its_name() {
    let source: &[u8] = b"<?php namespace A; class M {} new M;";
    let (unit, _strings) = unit_and_strings(source);
    assert_eq!(
        unit.dump(&[]).text(),
        main_dump(
            4,
            0,
            2,
            "0000 V0 = NEW 0 string(\"A\\\\M\")\n\
             0001 DO_FCALL\n\
             0002 FREE V0\n\
             0003 RETURN int(1)\n\
             LIVE RANGES:\n\
             \x20    0: 0001 - 0002 (new)\n"
        )
    );
    let names: Vec<&[u8]> = unit.classes.iter().map(|c| c.name.0.as_ref()).collect();
    assert_eq!(
        names,
        [b"A\\M".as_slice()],
        "declared as A\\M, got {names:?}"
    );

    let (unit, _strings) = unit_and_strings(b"<?php namespace A; class M { const C = __CLASS__; }");
    let names: Vec<&[u8]> = unit.classes.iter().map(|c| c.name.0.as_ref()).collect();
    assert_eq!(names, [b"A\\M".as_slice()]);
    assert_eq!(
        unit.classes[0].constants[0].value,
        ConstExpr::Str(b"A\\M".as_slice().into())
    );
    let mut strings = php_heap::Strings::new();
    let refused = compile_parsed(
        b"<?php namespace A;\nclass M { const K = 1;\n const K = 2; }",
        &bundled(),
        &mut strings,
    )
    .err()
    .map(|e| (e.level, e.message.into_owned(), e.line));
    assert_eq!(
        refused,
        Some((
            Level::CompileError,
            "Cannot redefine class constant A\\M::K".to_string(),
            3
        ))
    );
}

/// `zend_compile_class_decl` for the class the doc-comment witness
/// of `php_syntax` declares (#257): bound at compile time with no
/// opline, so the main op array is the oracle's `ECHO` and `RETURN`
/// alone; the declaration among the unit's classes with each
/// declarator's doc comment as `backup_doc_comment` took it (probed
/// 8.5.9: `/** P */ public $x, $y;` gives P to `$x` alone), the
/// constants' values as written, and a `}` clearing what is pending.
#[test]
fn a_class_with_properties_and_constants_is_bound_with_no_opline() {
    let (unit, _strings) = unit_and_strings(
        b"<?php\nclass M { /** P */ public $x, $y = 1 /** P2 */; /** K */ const A = 1, B = 2; static $s; /** F */ final protected const C = -A; }\n/** D */ { } class N /** D2 */ { var $v /** V */; }\necho 1;\n",
    );
    assert_eq!(
        unit.dump(&[]).text(),
        "\n$_main:\n     ; (lines=2, args=0, vars=0, tmps=0)\n     ; (before optimizer)\n     ; :1-5\n     ; return  [] RANGE[0..0]\n0000 ECHO int(1)\n0001 RETURN int(1)\n",
        "the class emits no opline; php binds it at compile time"
    );
    assert_eq!(unit.classes.len(), 2, "two classes, got {:?}", unit.classes);
    let (m, n) = (&unit.classes[0], &unit.classes[1]);
    assert_eq!(
        (m.name.0.as_ref(), m.line, m.doc_comment.as_deref()),
        (b"M".as_slice(), 2, None)
    );
    let props: Vec<_> = m
        .properties
        .iter()
        .map(|p| {
            (
                p.name.0.as_ref(),
                p.is_instance,
                p.default.clone(),
                p.doc_comment.as_deref(),
            )
        })
        .collect();
    assert_eq!(
        props,
        vec![
            (b"x".as_slice(), true, None, Some(b"/** P */".as_slice())),
            (b"y", true, Some(ConstExpr::long(1)), Some(b"/** P2 */")),
            (b"s", false, None, None),
        ]
    );
    let consts: Vec<_> = m
        .constants
        .iter()
        .map(|c| {
            (
                c.name.0.as_ref(),
                c.visibility,
                c.is_final,
                c.value.clone(),
                c.doc_comment.as_deref(),
            )
        })
        .collect();
    assert_eq!(
        consts,
        vec![
            (
                b"A".as_slice(),
                Visibility::Public,
                false,
                ConstExpr::long(1),
                Some(b"/** K */".as_slice())
            ),
            (b"B", Visibility::Public, false, ConstExpr::long(2), None),
            (
                b"C",
                Visibility::Protected,
                true,
                ConstExpr::Unary {
                    op: Name(b"-".as_slice().into()),
                    operand: Box::new(ConstExpr::Constant {
                        name: ConstantName::Resolved(Name(b"A".as_slice().into())),
                        line: 2,
                    }),
                    line: 2,
                },
                Some(b"/** F */"),
            ),
        ]
    );
    assert_eq!(
        (n.name.0.as_ref(), n.line, n.doc_comment.as_deref()),
        (b"N".as_slice(), 3, Some(b"/** D2 */".as_slice())),
        "the close brace before it cleared D, and a class takes before its open brace"
    );
    assert_eq!(
        n.properties[0].doc_comment.as_deref(),
        Some(b"/** V */".as_slice())
    );
    assert_eq!(
        n.properties[0].visibility,
        Visibility::Public,
        "`var` is public"
    );
}

/// What `zend_compile_prop_decl`, `zend_compile_class_const_decl`,
/// `zend_assert_valid_class_name` and `zend_add_member_modifier`
/// refuse, in php's words at the declarator's line, and what this
/// compiler does not read yet, refused as such rather than compiled
/// to something else.
#[test]
#[allow(clippy::too_many_lines)] // one assertion per refusal: the table is the test
fn what_php_s_compiler_refuses_in_a_class_is_refused_in_its_words() {
    let refused = |source: &[u8]| {
        let mut strings = php_heap::Strings::new();
        compile_parsed(source, &bundled(), &mut strings)
            .err()
            .map(|e| (e.level, e.message.into_owned(), e.line))
    };
    let compile_error =
        |message: &str, line| Some((Level::CompileError, message.to_string(), line));
    assert_eq!(
        refused(b"<?php\nclass M {\n public $x;\n public $x; }"),
        compile_error("Cannot redeclare M::$x", 4)
    );
    assert_eq!(
        refused(b"<?php\nclass M { const A = 1;\n const A = 2; }"),
        compile_error("Cannot redefine class constant M::A", 3)
    );
    assert_eq!(
        refused(b"<?php\nclass Int {}"),
        compile_error("Cannot use \"Int\" as a class name as it is reserved", 2)
    );
    assert_eq!(
        refused(b"<?php\nclass M { public $x = $y; }"),
        compile_error("Constant expression contains invalid operations", 2)
    );
    assert_eq!(
        refused(b"<?php\nclass M { const A = f(); }"),
        compile_error("Constant expression contains invalid operations", 2)
    );
    assert_eq!(
        refused(b"<?php\nclass M { public private $x; }"),
        compile_error("Multiple access type modifiers are not allowed", 2)
    );
    assert_eq!(
        refused(b"<?php\nclass M {}\nclass m {}"),
        None,
        "a second declaration of a name is the DECLARE_CLASS that refuses it as the program serves"
    );
    assert_eq!(
        refused(b"<?php\nclass M extends N {} class N {}"),
        None,
        "a parent bound later in the file is the DECLARE_CLASS at the class's line"
    );
    assert_eq!(
        refused(b"<?php\nclass M extends self {}"),
        compile_error("Cannot use \"self\" as class name, as it is reserved", 2)
    );
    assert_eq!(
        refused(b"<?php\ninterface I {}\nclass M extends I {}"),
        compile_error("Class M cannot extend interface I", 3),
        "the early bind links the class, so what zend_do_inheritance_ex refuses is the file's"
    );
    assert_eq!(
        refused(b"<?php\nclass M { abstract function f(); }"),
        compile_error(
            "Class M declares abstract method f() and must therefore be declared abstract",
            2
        )
    );
    assert_eq!(
        refused(b"<?php\nabstract class M { abstract function f(); }"),
        None
    );
    assert_eq!(
        refused(b"<?php\nclass M { function f(): (A&B)|C {} }"),
        None
    );
    assert_eq!(
        refused(b"<?php\nclass M { function f(public int $p) {} }"),
        compile_error("Cannot declare promoted property outside a constructor", 2)
    );
    assert_eq!(
        refused(b"<?php\nclass M { function f() {} function F() {} }"),
        compile_error("Cannot redeclare M::F()", 2)
    );
    assert_eq!(refused(b"<?php\n#[A] #[B(1), C] class M {}"), None);
    assert_eq!(refused(b"<?php\n#[A(x: 1)] class M {}"), None);
    assert_eq!(
        refused(b"<?php\n#[\\AllowDynamicProperties] class M {}"),
        None,
        "the one attribute the compiler reads is a bit on the class"
    );
    assert_eq!(
        refused(b"<?php\n#[\\AllowDynamicProperties]\ninterface I {}"),
        compile_error("Cannot apply #[\\AllowDynamicProperties] to interface I", 3)
    );
    assert_eq!(refused(b"<?php\n#[A] function f() {}"), None);
    assert_eq!(refused(b"<?php\nclass M { #[A] public $x; }"), None);
    assert_eq!(
        refused(b"<?php\nclass M { #[\\Override] public $x; }"),
        compile_error(
            "M::$x has #[\\Override] attribute, but no matching parent property exists",
            2
        )
    );
}

/// What `zend_declare_typed_property` refuses in a property group's
/// modifiers and type, in php's words at the declarator's line.
#[test]
fn what_php_s_compiler_refuses_in_a_property_group_is_refused_in_its_words() {
    let refused = |source: &[u8]| {
        let mut strings = php_heap::Strings::new();
        compile_parsed(source, &bundled(), &mut strings)
            .err()
            .map(|e| (e.level, e.message.into_owned(), e.line))
    };
    let compile_error =
        |message: &str, line| Some((Level::CompileError, message.to_string(), line));
    assert_eq!(
        refused(b"<?php\nclass M { public int $x; }"),
        None,
        "a property group's type is read where a modifier stood before it"
    );
    assert_eq!(refused(b"<?php\nclass M { public readonly int $x; }"), None);
    assert_eq!(
        refused(b"<?php\nclass M { public readonly int $x = 1; }"),
        compile_error("Readonly property M::$x cannot have default value", 2)
    );
    assert_eq!(
        refused(b"<?php\nclass M { private(set) protected(set) int $x; }"),
        compile_error("Multiple access type modifiers are not allowed", 2)
    );
    assert_eq!(
        refused(b"<?php\nclass M { public private(set) $x; }"),
        compile_error(
            "Property with asymmetric visibility M::$x must have type",
            2
        )
    );
    assert_eq!(
        refused(b"<?php\nclass M { protected public(set) int $x; }"),
        compile_error(
            "Visibility of property M::$x must not be weaker than set visibility",
            2
        )
    );
    assert_eq!(
        refused(b"<?php\nclass M { public private(set) int $x; }"),
        None,
        "a set visibility stricter than the read visibility stands"
    );
}

/// `zend_enum_register_funcs` runs as the enum links (#2678): an
/// enum declaring its own `cases()`, or a backed one its own
/// `from()`, compiles, its `DECLARE_CLASS` and its method dumped as
/// `Zend/tests/enum/no-cases.phpt` and `no-from.phpt` dump them, and
/// the link refuses it when the opline runs.
#[test]
fn an_enum_declaring_its_own_cases_or_from_compiles() {
    let cases = dump_of(
        b"<?php
enum Foo {
    case Bar;
    public static function cases(): array { return []; }
}
",
    );
    assert!(
        cases.contains("0000 DECLARE_CLASS string(\"foo\")\n0001 RETURN int(1)\n"),
        "php's $_main, {cases}"
    );
    assert!(
        cases.contains(
            "\nFoo::cases:\n     ; (lines=3, args=0, vars=0, tmps=0)\n\
             \x20    ; (before optimizer)\n     ; :4-4\n     ; return  [] RANGE[0..0]\n\
             0000 RETURN array(...)\n\
             0001 VERIFY_RETURN_TYPE\n\
             0002 RETURN null\n"
        ),
        "php's Foo::cases, {cases}"
    );
    let from = dump_of(
        b"<?php
enum Foo: int {
    case Bar = 0;
    public static function from(string|int $value): self { return $this; }
}
",
    );
    assert!(
        from.contains("0000 DECLARE_CLASS string(\"foo\")\n0001 RETURN int(1)\n"),
        "php's $_main, {from}"
    );
    assert!(
        from.contains(
            "0000 CV0($value) = RECV 1\n\
             0001 T1 = FETCH_THIS\n\
             0002 VERIFY_RETURN_TYPE T1\n\
             0003 RETURN T1\n\
             0004 VERIFY_RETURN_TYPE\n\
             0005 RETURN null\n"
        ),
        "php's Foo::from, {from}"
    );
}

/// `zend_compile_class_decl` compiles the body in the order written
/// (#2797), so a property hook sees in `CG(active_class_entry)`'s
/// `function_table` the methods declared above its property and
/// none below: `$this->p()` to a private method above is php's
/// `DO_UCALL`, with its argument sent by the parameter, and
/// `$this->q($a)` to one below stays `DO_FCALL`.
#[test]
fn a_hook_knows_the_methods_declared_above_its_property() {
    let dump = dump_of(
        b"<?php
class C {
    private function p(&$r) { return 1; }
    public $x { get => $this->p($a); }
    public $y { get => $this->q($a); }
    private function q(&$r) { return 2; }
}
",
    );
    let x = "0000 INIT_METHOD_CALL 1 THIS string(\"p\")\n\
             0001 SEND_REF CV0($a) 1\n\
             0002 V1 = DO_UCALL\n";
    let y = "0000 INIT_METHOD_CALL 1 THIS string(\"q\")\n\
             0001 SEND_VAR_EX CV0($a) 1\n\
             0002 V1 = DO_FCALL\n";
    assert!(dump.contains(x), "php's C::$x::get, {dump}");
    assert!(dump.contains(y), "php's C::$y::get, {dump}");
    let call = |opcode: &str| {
        format!(
            "0000 INIT_METHOD_CALL 0 THIS string(\"p\")\n\
             0001 V0 = {opcode}\n\
             0002 RETURN V0\n"
        )
    };
    let above = dump_of(
        b"<?php
class C {
    private function p() { return 1; }
    public $x { get => $this->p(); }
}
",
    );
    assert!(above.contains(&call("DO_UCALL")), "php's dump, {above}");
    let below = dump_of(
        b"<?php
class C {
    public $x { get => $this->p(); }
    private function p() { return 1; }
}
",
    );
    assert!(below.contains(&call("DO_FCALL")), "php's dump, {below}");
    let scoped = dump_of(
        b"<?php
class C {
    private static function p() { return 1; }
    public $x { get => self::p() + C::p(); }
}
",
    );
    assert!(
        scoped.contains(
            "0000 INIT_STATIC_METHOD_CALL 0 (self) (exception) string(\"p\")\n\
             0001 V0 = DO_UCALL\n\
             0002 INIT_STATIC_METHOD_CALL 0 string(\"C\") string(\"p\")\n\
             0003 V1 = DO_UCALL\n"
        ),
        "php's dump, {scoped}"
    );
}

/// `zend_compile_static_call`'s `self::` reads
/// `CG(active_class_entry)` itself, never the table's class of its
/// name: in a second `class A` declared under an `if`, `self::f($x)`
/// is the class being compiled's, which has no `f`, so `DO_FCALL`
/// with `$x` sent as to an unknown callee, though the `A` bound
/// early has `f(&$r)`; a hook's `self::h()` to its own private `h`
/// above is `DO_UCALL`.
#[test]
fn self_in_a_class_redeclaring_a_bound_name_is_the_class_compiling() {
    let dump = dump_of(
        b"<?php
class A { public static function f(&$r) {} }
if (true) {
    class A { function g() { self::f($x); } private function h() {} public $p { get => self::h(); } }
}
",
    );
    let g = "0000 INIT_STATIC_METHOD_CALL 1 (self) (exception) string(\"f\")\n\
             0001 SEND_VAR_EX CV0($x) 1\n\
             0002 DO_FCALL\n";
    let p = "0000 INIT_STATIC_METHOD_CALL 0 (self) (exception) string(\"h\")\n\
             0001 V0 = DO_UCALL\n";
    assert!(dump.contains(g), "php's A::g, {dump}");
    assert!(dump.contains(p), "php's A::$p::get, {dump}");
}

/// `zend_try_early_bind` while the file compiles (#2689): the class
/// being bound is in no table yet, so a type its parent's method
/// writes naming it finds nothing, `zend_can_early_bind` comes to
/// `INHERITANCE_UNRESOLVED`, and the class keeps its
/// `DECLARE_CLASS_DELAYED`, as
/// `Zend/tests/type_declarations/union_types/variance/valid.phpt`
/// dumps; `D`, which `A`'s `B` names neither, is left the same.
#[test]
fn a_parent_naming_the_class_being_bound_leaves_it_delayed() {
    let dump = dump_of(
        b"<?php
class A { public function m(B|string $a): A|string {} }
class B extends A { public function m(A|string $a): B|string {} }
class D extends A { public function m(A|string $a): D|string {} }
",
    );
    assert!(
        dump.contains(
            "0000 DECLARE_CLASS_DELAYED string(\"b\") string(\"a\")\n\
             0001 DECLARE_CLASS_DELAYED string(\"d\") string(\"a\")\n\
             0002 RETURN int(1)\n"
        ),
        "php's $_main, {dump}"
    );
}

/// `zend_compile_property_hooks`' `num_hooked_prop_variance_checks`
/// (#2688): a `set` hook whose parameter's type names a class not
/// declared yet keeps the class from the early bind and from
/// opcache's delayed one, so its opline is `DECLARE_CLASS` with no
/// parent operand, as
/// `Zend/tests/property_hooks/set_value_parameter_type_variance_003.phpt`
/// dumps.
#[test]
fn a_set_hook_unresolved_while_compiling_is_declare_class() {
    let dump = dump_of(
        b"<?php
class Test {
    public X $prop {
        set(Y $prop) {}
    }
}
interface X {}
interface Y {}
",
    );
    assert!(
        dump.contains("0000 DECLARE_CLASS string(\"test\")\n0001 RETURN int(1)\n"),
        "php's $_main, {dump}"
    );
}
