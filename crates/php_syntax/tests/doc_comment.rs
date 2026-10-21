//! The doc comment at every position php's grammar takes one (design §4.9;
//! `tree::builder`'s docs derive each): parsed through the real entry and
//! read off the typed nodes, held to what `Reflection*::getDocComment()`
//! answered under the pin at 8.5.9 for the same source. Every expectation
//! here is a measurement, `/tmp/dc.php` under `php -n`, never a reading of
//! `zend_language_parser.y` alone, since which token bison had read when
//! it reduces `backup_doc_comment` is a fact of its tables.

mod support;

use php_syntax::Parsed;
use php_syntax::tree::{
    AnonClassDecl, ArrowFnExpr, ClassDecl, ClosureExpr, ConstDeclarator, EnumCaseDecl, EnumDecl,
    FunctionDecl, HasDocComment, InterfaceDecl, MethodDecl, Node, Param, PropertyDeclarator,
    PropertyHook, TraitDecl,
};
use support::parse_clean;

/// The doc comments of every node of `T`, in arena order, as text.
fn docs<'p, T: Node + HasDocComment>(parsed: &'p Parsed<'_>) -> Vec<Option<&'p str>> {
    parsed
        .tree
        .nodes::<T>()
        .iter()
        .map(|node| {
            node.doc_comment().map(|id| {
                let token = parsed.tokens[id.0 as usize];
                std::str::from_utf8(
                    &parsed.source[token.offset as usize..(token.offset + token.len) as usize],
                )
                .expect("a doc comment is text")
            })
        })
        .collect()
}

/// `ReflectionClass::getDocComment()`: a class takes before its `{`, so a
/// comment anywhere in the head is the class's; an interface and an enum
/// read the same way, and a trait takes before its name.
#[test]
fn a_class_like_takes_inside_its_head() {
    let p = parse_clean(
        "<?php
class F extends /** D5 */ A {}
class F2 extends A /** D6 */ {}
class F3 /** D6b */ {}
class F4 extends A implements /** D6c */ I {}
class F5 {}
enum E2: /** EB */ int { case A = 1; }
enum E3 /** EB2 */ : int { case A = 1; }
interface I2 extends /** I2 */ I {}
interface I3 extends I /** I3 */ {}
/** T */ trait T1 /** T2 */ {}
trait T3 {}
",
    );
    assert_eq!(
        docs::<ClassDecl>(&p),
        [
            Some("/** D5 */"),
            Some("/** D6 */"),
            Some("/** D6b */"),
            Some("/** D6c */"),
            None
        ]
    );
    assert_eq!(
        docs::<EnumDecl>(&p),
        [Some("/** EB */"), Some("/** EB2 */")]
    );
    assert_eq!(
        docs::<InterfaceDecl>(&p),
        [Some("/** I2 */"), Some("/** I3 */")]
    );
    // T2 stays pending past the trait's take and its `}` clears it.
    assert_eq!(docs::<TraitDecl>(&p), [Some("/** T */"), None]);
}

/// `ReflectionFunction::getDocComment()` and
/// `ReflectionMethod::getDocComment()`: a function or a method takes
/// before its name and after nothing else in its head.
#[test]
fn a_function_or_method_takes_before_its_name() {
    let p = parse_clean(
        "<?php
function /** X */ f() {}
function f2 /** X2 */ () {}
function f3() /** X3 */ {}
function f4(): /** X4 */ int {}
function f5(): int /** X5 */ {}
class M {
  function /** M1 */ m1() {}
  function m2 /** M2 */ () {}
  public function m3 () /** M3 */ {}
  public function m5 (): int /** M5 */ {}
}
",
    );
    assert_eq!(
        docs::<FunctionDecl>(&p),
        [Some("/** X */"), None, None, None, None]
    );
    assert_eq!(
        docs::<MethodDecl>(&p),
        [Some("/** M1 */"), None, None, None]
    );
}

/// `ReflectionProperty::getDocComment()`: a declarator takes after its
/// last token, so the comment before the statement is the first
/// declarator's alone, and one before `,`, `;` or `=` is that declarator's.
#[test]
fn a_property_declarator_takes_after_its_last_token() {
    let p = parse_clean(
        "<?php
class M {
  /** P */ public $x, $y;
  /** Q */ public $a = 1, /** Q2 */ $b = 2;
  public $z /** P3 */;
  public $w = 1 /** P4 */;
  public $u /** P5 */ = 2;
  public $p /** HP */ { get { return 1; } }
  public $q { /** HG */ get { return 1; } }
  public $r { get /** HG2 */ { return 1; } }
}
",
    );
    assert_eq!(
        docs::<PropertyDeclarator>(&p),
        [
            Some("/** P */"),
            None,
            Some("/** Q */"),
            Some("/** Q2 */"),
            Some("/** P3 */"),
            Some("/** P4 */"),
            Some("/** P5 */"),
            Some("/** HP */"),
            None,
            None,
        ]
    );
}

/// `ReflectionClassConstant::getDocComment()`: as a property's, per
/// declarator, and a top-level `const` takes the same way.
#[test]
fn a_const_declarator_takes_after_its_expression() {
    let p = parse_clean(
        "<?php
class M {
  const A = 1, B = 2;
  /** K */ const C = 1, /** K2 */ D = 2;
  const E = 1 /** K3 */, F = 2;
}
/** G */ const G3 = 1, G4 = 2;
class Y6 {}
",
    );
    assert_eq!(
        docs::<ConstDeclarator>(&p),
        [
            None,
            None,
            Some("/** K */"),
            Some("/** K2 */"),
            Some("/** K3 */"),
            None,
            Some("/** G */"),
            None,
        ]
    );
    assert_eq!(docs::<ClassDecl>(&p), [None, None]);
}

/// `ReflectionClassConstant::getDocComment()` on a case: the take reduces
/// once `case` is shifted, so a comment after `case` is the next case's.
#[test]
fn an_enum_case_takes_before_case() {
    let p = parse_clean(
        "<?php
enum E: int {
  /** C1 */ case A = 0;
  case /** C2 */ B = 1;
  case C /** C3 */ = 2;
  case D = /** C4 */ 3;
}
",
    );
    assert_eq!(
        docs::<EnumCaseDecl>(&p),
        [
            Some("/** C1 */"),
            None,
            Some("/** C2 */"),
            Some("/** C3 */")
        ]
    );
}

/// The `attribute` rule takes at `#[` and puts it back after `]`, so
/// what a group contains, a closure's `}` included, is undone for the
/// declaration after, and a comment after the group stands.
#[test]
fn an_attribute_group_gives_back_what_it_took() {
    let p = parse_clean(
        "<?php
/** D4 */ #[Attr(static function(){})] class Y1 {}
/** D9 */ #[Attr(/** IN */ 1)] class Y2 {}
#[Attr] /** D8 */ class Y3 {}
/** D10 */ #[Attr] /** D11 */ class Y4 {}
#[Attr(/** IN2 */ 1)] class Y5 {}
",
    );
    assert_eq!(
        docs::<ClassDecl>(&p),
        [
            Some("/** D4 */"),
            Some("/** D9 */"),
            Some("/** D8 */"),
            Some("/** D11 */"),
            None
        ]
    );
}

/// `ReflectionProperty::getDocComment()` on a promoted property: a
/// parameter takes before its variable, so a comment after one is the
/// next parameter's; `ReflectionFunction::getDocComment()` on a closure:
/// a closure or an arrow function takes before its `(`, and what stands
/// after the head is nobody's. The state after each is the next
/// declaration's, so the classes hold that too.
#[test]
fn a_parameter_and_a_closure_take_too() {
    let p = parse_clean(
        "<?php
class C { function __construct(/** PA */ public $a, public $b /** PB */, public $c = 1) {} }
$g1 = fn($x /** B */) => 1; class X1 {}
$g2 = fn(/** A */ $x) => 1; class X2 {}
$g3 = fn /** C */ ($x) => 1; class X3 {}
$g4 = fn ($x) /** E */ => 1; class X4 {}
$g5 = function /** F */ () { }; class X5 {}
$g6 = function () /** G */ { }; class X6 {}
",
    );
    assert_eq!(
        docs::<Param>(&p),
        [
            Some("/** PA */"),
            None,
            Some("/** PB */"),
            None,
            Some("/** A */"),
            None,
            None,
        ]
    );
    assert_eq!(
        docs::<ArrowFnExpr>(&p),
        [None, None, Some("/** C */"), None]
    );
    assert_eq!(docs::<ClosureExpr>(&p), [Some("/** F */"), None]);
    assert_eq!(
        docs::<ClassDecl>(&p),
        [
            None,
            Some("/** B */"),
            None,
            None,
            Some("/** E */"),
            None,
            None
        ]
    );
}

/// `ReflectionClass::getDocComment()` on an anonymous class: it takes
/// before its `{` as a named class does, so a comment anywhere in the
/// head, the arguments included, is the class's (probed 8.5.9: `new
/// class /** N */ {}` N, `new class /** N2 */ extends stdClass {}` N2,
/// `new class extends stdClass /** N3 */ {}` N3, `new /** N4 */ class
/// {}` N4, `new class(/** N5 */ 1) {}` N5).
#[test]
fn an_anonymous_class_takes_before_its_brace() {
    let p = parse_clean(
        "<?php
$o = new class /** N */ {};
$o2 = new class /** N2 */ extends stdClass {};
$o3 = new class extends stdClass /** N3 */ {};
$o4 = new /** N4 */ class {};
$o5 = new class(/** N5 */ 1) { function __construct($x) {} };
$o6 = new class {};
",
    );
    assert_eq!(
        docs::<AnonClassDecl>(&p),
        [
            Some("/** N */"),
            Some("/** N2 */"),
            Some("/** N3 */"),
            Some("/** N4 */"),
            Some("/** N5 */"),
            None
        ]
    );
}

/// `ReflectionMethod::getDocComment()` on what
/// `ReflectionProperty::getHooks()` hands back: an accessor takes before its
/// name, after its modifiers, as a method does (probed 8.5.9: `{ /** HG
/// */ get { … } }` HG, `{ get /** HG2 */ { … } }` none, `{ /** HS */ set
/// { } get { … } }` HS on `set` and none on `get`, `{ /** HT */ final
/// get { … } }` HT).
#[test]
fn a_property_hook_takes_before_its_name() {
    let p = parse_clean(
        "<?php
class H {
  public $q { /** HG */ get { return 1; } }
  public $r { get /** HG2 */ { return 1; } }
  public $s { /** HS */ set { } get { return 1; } }
  public $t { /** HT */ final get { return 1; } }
}
",
    );
    assert_eq!(
        docs::<PropertyHook>(&p),
        [
            Some("/** HG */"),
            None,
            Some("/** HS */"),
            None,
            Some("/** HT */")
        ]
    );
}
