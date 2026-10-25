//! The classes `zend_register_attribute_ce` registers
//! (`Z/Zend/zend_attributes.c:575-607` at the tag), each as
//! `zend_attributes.stub.php` declares it: `Attribute` with its
//! `TARGET_*` and `IS_REPEATABLE` constants, the seven attributes php
//! marks internal, and `SensitiveParameterValue`, which is no attribute
//! and the value a trace shows in place of a parameter
//! `#[\SensitiveParameter]` marks. Every attribute carries its own
//! `#[Attribute(...)]` marker, the one `zend_mark_internal_attribute`
//! reads its targets from and `ReflectionAttribute::newInstance` reads
//! them from too.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use php_macros::php_class;
use php_unit::decl::Attribute as AttributeDecl;
use php_unit::decl::Bare;

use php_unit::Level;

use crate::api::abi::{ObjectHandlers, PropertiesFor, PropertyPurpose, This};
use crate::api::call::{Function, attribute_object_made};
use crate::api::raising::{Thrown, none_pending, raise};
use crate::api::request::Request;
use crate::api::values::{ClassId, Owner, PhpArray, PhpObject, PhpString, Value};

/// `fn_flags & (ZEND_ACC_DEPRECATED | ZEND_ACC_NODISCARD)` of a
/// function, and `CONST_DEPRECATED` of a constant: what the
/// `#[\Deprecated]` and `#[\NoDiscard]` attributes among a declaration's
/// attributes raise, decoded once where the declaration is linked or
/// registered, so a call and a fetch read a bit and never the
/// attributes. php's compiler raises `ZEND_ACC_DEPRECATED` from the
/// attribute's presence (`zend_compile_func_decl`) and
/// `validate_nodiscard` raises `ZEND_ACC_NODISCARD`; `gen_stub.php`
/// writes both from a native's stub.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Flags {
    pub(crate) deprecated: bool,
    pub(crate) no_discard: bool,
}

impl Flags {
    /// The flags `attributes` raise, each attribute by its resolved name
    /// as `zend_get_attribute_str` finds it.
    pub(crate) fn of(attributes: &[AttributeDecl]) -> Flags {
        Flags {
            deprecated: find(attributes, b"Deprecated").is_some(),
            no_discard: find(attributes, b"NoDiscard").is_some(),
        }
    }
}

/// `zend_get_attribute_str`: the attribute of a lowered name among a
/// declaration's, each named as resolved ([`php_unit::decl::Name`]).
fn find<'a>(attributes: &'a [AttributeDecl], name: &[u8]) -> Option<&'a AttributeDecl> {
    attributes
        .iter()
        .find(|one| one.name.0.eq_ignore_ascii_case(name))
}

/// The suffix php prints after a `#[\Deprecated]` or `#[\NoDiscard]`
/// diagnostic, as `get_deprecation_suffix_from_attribute` and
/// `get_nodiscard_suffix_from_attribute` hand it back: `None` is
/// `ZSTR_EMPTY_ALLOC`, which costs nothing, and otherwise the string
/// `zend_strpprintf` laid down on the request's heap, which the caller
/// holds while its message is raised and gives up after it
/// (`zend_string_release(message_suffix)`).
pub(crate) struct Suffix(Option<PhpString>);

impl Suffix {
    /// The string given up.
    pub(crate) fn give_up(self, req: &mut Request) {
        if let Some(text) = self.0 {
            req.release(Value::string(text));
        }
    }
}

impl std::fmt::Display for Suffix {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.0 {
            Some(text) => f.write_str(&String::from_utf8_lossy(text.as_bytes())),
            None => Ok(()),
        }
    }
}

/// The two `get_*_suffix_from_attribute`s over the internal attribute
/// `class` names among `attributes`: none where the list carries no
/// such attribute or one with no argument; otherwise its object made
/// with no call frame of its own (`zend_get_attribute_object`), whose
/// constructor holds each argument to its `?string` under the caller's
/// `strict_types` and whose throw is the `Err`, the string each of
/// `properties` holds read off it, and the suffix `worded` makes of
/// them laid down by `zend_strpprintf` while the object stands, the
/// object given up after it.
fn attribute_suffix(
    req: &mut Request,
    attributes: &[AttributeDecl],
    class: &[u8],
    properties: &[&[u8]],
    scope: Option<ClassId>,
    worded: impl FnOnce(&[&[u8]]) -> Vec<u8>,
) -> Result<Suffix, Thrown> {
    let Some(attribute) = find(attributes, class).filter(|one| !one.arguments.is_empty()) else {
        return Ok(Suffix(None));
    };
    let Some(class) = req.class_named(Bare::resolved(class)) else {
        return Ok(Suffix(None));
    };
    let object = attribute_object_made(req, class, attribute, scope, None)?;
    let read: Vec<Value> = properties
        .iter()
        .map(|name| object.prop(req, name).unwrap_or_default())
        .collect();
    let read: Vec<&[u8]> = read
        .iter()
        .map(|value| value.as_str_bytes().unwrap_or_default())
        .collect();
    let text = worded(&read);
    let suffix = crate::api::raising::strpprintf(req, &text);
    req.release(object.as_value());
    Ok(Suffix(Some(suffix?)))
}

/// `Method C::m()` or `Function f()`, as `zend_deprecated_function` and
/// `zend_nodiscard_function` name the callee: the class that declares
/// it, `common.scope`, and the name it was declared under.
fn callee_named(req: &Request, function: Function, method: &str, function_word: &str) -> String {
    let name = req.declared_name(function);
    match req.method_class(function) {
        Some(class) => format!("{method} {}::{name}()", req.symbols().class_name(class)),
        None => format!("{function_word} {name}()"),
    }
}

/// `zend_deprecated_function` (`Z/Zend/zend_execute.c:1866`), raised
/// where `fn_flags` carries `ZEND_ACC_DEPRECATED`: by `DO_FCALL` and
/// `DO_FCALL_BY_NAME` before the callee runs and by
/// `zend_call_function` before its arguments are placed. The suffix is
/// the `Deprecated` object's `since` and `message`, the object made
/// from the attribute's arguments in the callee's scope
/// (`get_deprecation_suffix_from_attribute`), and the level
/// `E_DEPRECATED` for a native and `E_USER_DEPRECATED` for a program's.
/// What the raise left pending, or the object's constructor threw, is
/// the `Err`.
///
/// ```php
/// #[\Deprecated]
/// function test() {}
/// #[\Deprecated("use test() instead", since: "1.0")]
/// function test2() {}
/// class Clazz {
///     #[\Deprecated] function m() {}
///     #[\Deprecated(since: "2.0")] static function s() {}
/// }
/// test();
/// test2();
/// (new Clazz)->m();
/// Clazz::s();
/// call_user_func('test');
/// set_error_handler(function ($errno, $errstr) { var_dump($errno === E_USER_DEPRECATED); });
/// test();
/// ```
/// ```expect
/// Deprecated: Function test() is deprecated in %s on line 10
///
/// Deprecated: Function test2() is deprecated since 1.0, use test() instead in %s on line 11
///
/// Deprecated: Method Clazz::m() is deprecated in %s on line 12
///
/// Deprecated: Method Clazz::s() is deprecated since 2.0 in %s on line 13
///
/// Deprecated: Function test() is deprecated in %s on line 14
/// bool(true)
/// ```
///
/// The message is the object's, so an argument is held to `?string`
/// under the file's `strict_types`, and a handler that throws stops the
/// call before the callee runs:
///
/// ```php
/// #[\Deprecated(1234)]
/// function test() { echo "not run\n"; }
/// test();
/// set_error_handler(function ($errno, $errstr) { throw new Exception($errstr); });
/// try { test(); } catch (Exception $e) { echo "Caught: ", $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Deprecated: Function test() is deprecated, 1234 in %s on line 4
/// not run
/// Caught: Function test() is deprecated, 1234
/// ```
///
/// The suffix is a string of the request's, laid down as
/// `zend_strpprintf` lays it while the object stands and given up after
/// the message, and the object's constructor is served with php's
/// argument array and named table ([`attribute_object_made`]), so a
/// silenced call peaks where php's does, a native's as a program's:
///
/// ```php
/// function pk($f) { $k = $f(); unset($k); $s = memory_get_usage(); memory_reset_peak_usage(); $k = $f(); echo memory_get_peak_usage() - $s, ' ', memory_get_usage() - $s, "\n"; }
/// pk(fn () => @strftime('', 0));
/// pk(fn () => @mhash_count());
/// #[\Deprecated] function dep() { return 1; }
/// #[\Deprecated(since: "8.1")] function d3() { return false; }
/// #[\Deprecated("use IntlDateFormatter::format() instead", "8.1")] function d2() { return false; }
/// pk(fn () => @dep());
/// pk(fn () => @d3());
/// pk(fn () => @d2());
/// ```
/// ```expect
/// 552 32
/// 472 0
/// 320 0
/// 472 0
/// 464 0
/// ```
#[cold] // a flagged callee alone; out of the call path's frames (#3022)
pub(crate) fn deprecated_function(req: &mut Request, function: Function) -> Result<(), Thrown> {
    let attributes = req
        .symbols()
        .function_decl(function)
        .map(|decl| decl.attributes.clone())
        .unwrap_or_default();
    let scope = req.method_class(function);
    let suffix = deprecation_suffix(req, &attributes, scope)?;
    let level = if req.user_function(function).is_some() {
        Level::UserDeprecated
    } else {
        Level::Deprecated
    };
    let named = callee_named(req, function, "Method", "Function");
    raise(req, level, &format!("{named} is deprecated{suffix}"));
    suffix.give_up(req);
    none_pending(req)
}

/// `zend_nodiscard_function` (`Z/Zend/zend_execute.c:1947`), raised by
/// `DO_FCALL` and `DO_FCALL_BY_NAME` where `fn_flags` carries
/// `ZEND_ACC_NODISCARD` and the opline's `result` operand is `UNUSED`: php's
/// warning, `E_USER_WARNING` for a program's function, with the
/// `NoDiscard` object's `message` after it.
///
/// ```php
/// #[\NoDiscard]
/// function test(): int { return 0; }
/// #[\NoDiscard("this is important")]
/// function test2(): int { return 0; }
/// class Clazz { #[\NoDiscard] static function m(): int { return 0; } }
/// test();
/// test2();
/// Clazz::m();
/// $x = test();
/// (void) test();
/// call_user_func('test');
/// ```
/// ```expect
/// Warning: The return value of function test() should either be used or intentionally ignored by casting it as (void) in %s on line 7
///
/// Warning: The return value of function test2() should either be used or intentionally ignored by casting it as (void), this is important in %s on line 8
///
/// Warning: The return value of method Clazz::m() should either be used or intentionally ignored by casting it as (void) in %s on line 9
///
/// Warning: The return value of function test() should either be used or intentionally ignored by casting it as (void) in %s on line 12
/// ```
#[cold] // a flagged callee alone; out of the call path's frames (#3022)
pub(crate) fn no_discard_function(req: &mut Request, function: Function) -> Result<(), Thrown> {
    let attributes = req
        .symbols()
        .function_decl(function)
        .map(|decl| decl.attributes.clone())
        .unwrap_or_default();
    let scope = req.method_class(function);
    let suffix = attribute_suffix(
        req,
        &attributes,
        b"NoDiscard",
        &[b"message"],
        scope,
        |read| joined_suffix(read[0], b""),
    )?;
    let level = if req.user_function(function).is_some() {
        Level::UserWarning
    } else {
        Level::Warning
    };
    let named = callee_named(req, function, "method", "function");
    raise(
        req,
        level,
        &format!(
            "The return value of {named} should either be used or intentionally ignored by casting it as (void){suffix}"
        ),
    );
    suffix.give_up(req);
    none_pending(req)
}

/// `zend_deprecated_class_constant` (`Z/Zend/zend_execute.c:1966`):
/// `Constant C::K is deprecated`, `Enum case` for a case, with the
/// `Deprecated` object's suffix read in the declaring class's scope,
/// `E_USER_DEPRECATED` for a program's class.
///
/// ```php
/// class Clazz {
///     #[\Deprecated] const A = 1;
///     #[\Deprecated("use B", since: "1.0")] const B = 2;
///     #[\Deprecated(self::C)] const C = "from itself";
/// }
/// enum E { #[\Deprecated] case Test; }
/// echo Clazz::A, Clazz::B, Clazz::C, "\n";
/// var_dump(E::Test, constant('Clazz::A'));
/// ```
/// ```expect
/// Deprecated: Constant Clazz::A is deprecated in %s on line 8
/// 1
/// Deprecated: Constant Clazz::B is deprecated since 1.0, use B in %s on line 8
/// 2
/// Deprecated: Constant Clazz::C is deprecated, from itself in %s on line 8
/// from itself
///
/// Deprecated: Enum case E::Test is deprecated in %s on line 9
///
/// Deprecated: Constant Clazz::A is deprecated in %s on line 9
/// enum(E::Test)
/// int(1)
/// ```
pub(crate) fn deprecated_class_constant(
    req: &mut Request,
    declaring: ClassId,
    name: &[u8],
    constant: &php_unit::decl::ClassConstDecl,
    native: bool,
) -> Result<(), Thrown> {
    let suffix = deprecation_suffix(req, &constant.attributes, Some(declaring))?;
    let level = if native {
        Level::Deprecated
    } else {
        Level::UserDeprecated
    };
    let what = if constant.is_case {
        "Enum case"
    } else {
        "Constant"
    };
    let message = format!(
        "{what} {}::{} is deprecated{suffix}",
        req.symbols().class_name(declaring),
        String::from_utf8_lossy(name)
    );
    raise(req, level, &message);
    suffix.give_up(req);
    none_pending(req)
}

/// `get_deprecation_suffix_from_attribute` (`Z/Zend/zend_execute.c:1801`):
/// what php prints after `is deprecated` for a declaration carrying
/// `attributes`, read off the `Deprecated` object their `#[\Deprecated]`
/// makes in `scope`; empty where the attribute takes no argument or
/// is not there. What the object's constructor threw is the `Err`.
pub(crate) fn deprecation_suffix(
    req: &mut Request,
    attributes: &[AttributeDecl],
    scope: Option<ClassId>,
) -> Result<Suffix, Thrown> {
    attribute_suffix(
        req,
        attributes,
        b"Deprecated",
        &[b"message", b"since"],
        scope,
        |read| joined_suffix(read[0], read[1]),
    )
}

/// What php prints after `is deprecated`, `"%s%S%s%S"`: ` since <since>`
/// and `, <message>`, each only where it is no empty string; the
/// `NoDiscard` suffix is the message's half.
fn joined_suffix(message: &[u8], since: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    if !since.is_empty() {
        out.extend_from_slice(b" since ");
        out.extend_from_slice(since);
    }
    if !message.is_empty() {
        out.extend_from_slice(b", ");
        out.extend_from_slice(message);
    }
    out
}

/// `attributes_object_handlers_sensitive_parameter_value`:
/// `std_object_handlers` with `get_properties_for` answering nothing
/// for every purpose, so no dumper, cast or encoder shows the value.
struct SensitiveValueHandlers;

impl ObjectHandlers for SensitiveValueHandlers {
    /// `attributes_sensitive_parameter_value_get_properties_for`.
    ///
    /// ```php
    /// $v = new SensitiveParameterValue('secret');
    /// var_dump($v, (array) $v);
    /// print_r($v);
    /// echo "\n", json_encode($v), "\n";
    /// var_export($v);
    /// echo "\n";
    /// var_dump($v->getValue(), $v->__debugInfo());
    /// ```
    /// ```expect
    /// object(SensitiveParameterValue)#1 (0) {
    /// }
    /// array(0) {
    /// }
    /// SensitiveParameterValue Object
    /// (
    /// )
    ///
    /// {}
    /// \SensitiveParameterValue::__set_state(array(
    /// ))
    /// string(6) "secret"
    /// array(0) {
    /// }
    /// ```
    ///
    /// No table for any purpose, so `get_object_vars()` and the cast
    /// answer the process's empty array and cost nothing, where
    /// `get_mangled_object_vars()` reads the property table itself.
    ///
    /// ```php
    /// $v = new SensitiveParameterValue(1);
    /// $m = memory_get_usage();
    /// $a = get_object_vars($v);
    /// $b = (array) $v;
    /// var_dump(memory_get_usage() - $m);
    /// debug_zval_dump($a, $b, get_mangled_object_vars($v));
    /// ```
    /// ```expect
    /// int(0)
    /// array(0) interned {
    /// }
    /// array(0) interned {
    /// }
    /// array(1) refcount(1){
    ///   ["%0SensitiveParameterValue%0value"]=>
    ///   int(1)
    /// }
    /// ```
    fn get_properties_for(
        &self,
        req: &mut Request,
        object: PhpObject,
        purpose: PropertyPurpose,
    ) -> Result<PropertiesFor, Thrown> {
        let _ = (req, object, purpose);
        Ok(PropertiesFor::Null)
    }

    fn properties_overloaded(&self) -> bool {
        true
    }
}

static SENSITIVE_VALUE: SensitiveValueHandlers = SensitiveValueHandlers;

/// `final class Attribute`, itself `#[Attribute(Attribute::TARGET_CLASS)]`,
/// its constants `ZEND_ATTRIBUTE_TARGET_*` and `ZEND_ATTRIBUTE_IS_REPEATABLE`
/// (`Z/Zend/zend_attributes.h:26-35`) under the names the stub gives
/// them, its `$flags` and its constructor. The nine classes are
/// registered in the order `zend_register_attribute_ce` registers them.
///
/// ```php
/// foreach (['Attribute', 'ReturnTypeWillChange', 'AllowDynamicProperties',
///     'SensitiveParameter', 'SensitiveParameterValue', 'Override', 'Deprecated',
///     'NoDiscard', 'DelayedTargetValidation'] as $name) {
///     $c = new ReflectionClass($name);
///     echo $name, ' ', (int) $c->isFinal(), ' ', count($c->getAttributes()), "\n";
/// }
/// $all = Attribute::TARGET_CLASS | Attribute::TARGET_FUNCTION | Attribute::TARGET_METHOD
///     | Attribute::TARGET_PROPERTY | Attribute::TARGET_CLASS_CONSTANT
///     | Attribute::TARGET_PARAMETER | Attribute::TARGET_CONSTANT;
/// var_dump($all === Attribute::TARGET_ALL, Attribute::IS_REPEATABLE);
/// ```
/// ```expect
/// Attribute 1 1
/// ReturnTypeWillChange 1 1
/// AllowDynamicProperties 1 1
/// SensitiveParameter 1 1
/// SensitiveParameterValue 1 0
/// Override 1 1
/// Deprecated 1 1
/// NoDiscard 1 1
/// DelayedTargetValidation 1 1
/// bool(true)
/// int(128)
/// ```
///
/// `Attribute` renders as `php --rc Attribute` renders it:
///
/// ```php
/// echo new ReflectionClass('Attribute');
/// ```
/// ```expect
/// Class [ <internal:Core> final class Attribute ] {
///
///   - Constants [9] {
///     Constant [ public int TARGET_CLASS ] { 1 }
///     Constant [ public int TARGET_FUNCTION ] { 2 }
///     Constant [ public int TARGET_METHOD ] { 4 }
///     Constant [ public int TARGET_PROPERTY ] { 8 }
///     Constant [ public int TARGET_CLASS_CONSTANT ] { 16 }
///     Constant [ public int TARGET_PARAMETER ] { 32 }
///     Constant [ public int TARGET_CONSTANT ] { 64 }
///     Constant [ public int TARGET_ALL ] { 127 }
///     Constant [ public int IS_REPEATABLE ] { 128 }
///   }
///
///   - Static properties [0] {
///   }
///
///   - Static methods [0] {
///   }
///
///   - Properties [1] {
///     Property [ public int $flags ]
///   }
///
///   - Methods [1] {
///     Method [ <internal:Core, ctor> public method __construct ] {
///
///       - Parameters [1] {
///         Parameter #0 [ <optional> int $flags = Attribute::TARGET_ALL ]
///       }
///     }
///   }
/// }
/// ```
///
/// `@strict-properties` is what refuses a dynamic property on five of
/// them; the other three take one with 8.2's deprecation:
///
/// ```php
/// foreach ([new Override, new Deprecated, new NoDiscard, new DelayedTargetValidation,
///     new SensitiveParameter, new SensitiveParameterValue(1)] as $o) {
///     try { $o->x = 1; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// }
/// $a = new AllowDynamicProperties;
/// $a->x = 1;
/// ```
/// ```expect
/// Cannot create dynamic property Override::$x
/// Cannot create dynamic property Deprecated::$x
/// Cannot create dynamic property NoDiscard::$x
/// Cannot create dynamic property DelayedTargetValidation::$x
/// Cannot create dynamic property SensitiveParameter::$x
/// Cannot create dynamic property SensitiveParameterValue::$x
///
/// Deprecated: Creation of dynamic property AllowDynamicProperties::$x is deprecated in %s on line 7
/// ```
#[php_class(final, attribute_class(Attribute::TARGET_CLASS))]
pub(crate) struct Attribute;

#[php_class]
impl Attribute {
    const TARGET_CLASS: i64 = 1;
    const TARGET_FUNCTION: i64 = 1 << 1;
    const TARGET_METHOD: i64 = 1 << 2;
    const TARGET_PROPERTY: i64 = 1 << 3;
    const TARGET_CLASS_CONSTANT: i64 = 1 << 4;
    const TARGET_PARAMETER: i64 = 1 << 5;
    const TARGET_CONSTANT: i64 = 1 << 6;
    const TARGET_ALL: i64 = (1 << 7) - 1;
    const IS_REPEATABLE: i64 = 1 << 7;
    #[php(property)]
    const flags: i64;

    /// `Attribute::__construct(int $flags = Attribute::TARGET_ALL)`: the
    /// flags into the one property, unchecked, as php writes
    /// `OBJ_PROP_NUM(obj, 0)`; `ReflectionAttribute::newInstance` is what
    /// holds them to the targets.
    ///
    /// ```php
    /// var_dump(new Attribute, new Attribute(Attribute::TARGET_CLASS | Attribute::IS_REPEATABLE));
    /// try { new Attribute("x"); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// try { new Attribute(1, 2); } catch (ArgumentCountError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// object(Attribute)#1 (1) {
    ///   ["flags"]=>
    ///   int(127)
    /// }
    /// object(Attribute)#2 (1) {
    ///   ["flags"]=>
    ///   int(129)
    /// }
    /// Attribute::__construct(): Argument #1 ($flags) must be of type int, string given
    /// Attribute::__construct() expects at most 1 argument, 2 given
    /// ```
    fn __construct(
        req: &mut Request,
        this: This<Self>,
        #[php(default = Attribute::TARGET_ALL)] flags: i64,
    ) -> Result<(), Thrown> {
        let flags = Owner::int(flags);
        this.object().set_slot(req, 0, flags)
    }
}

/// `final class ReturnTypeWillChange`, `#[Attribute(Attribute::TARGET_METHOD)]`.
#[php_class(final, attribute_class(Attribute::TARGET_METHOD))]
pub(crate) struct ReturnTypeWillChange;

#[php_class]
impl ReturnTypeWillChange {
    /// The constructor of `ReturnTypeWillChange`, `AllowDynamicProperties`,
    /// `SensitiveParameter` and `Override`, `ZEND_PARSE_PARAMETERS_NONE`.
    ///
    /// ```php
    /// var_dump(new Override);
    /// try { new Override(1); } catch (ArgumentCountError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// object(Override)#1 (0) {
    /// }
    /// Override::__construct() expects exactly 0 arguments, 1 given
    /// ```
    fn __construct(this: This<Self>) {
        let _ = this;
    }
}

/// `final class AllowDynamicProperties`, `#[Attribute(Attribute::TARGET_CLASS)]`.
#[php_class(final, attribute_class(Attribute::TARGET_CLASS))]
pub(crate) struct AllowDynamicProperties;

#[php_class]
impl AllowDynamicProperties {
    #[php(implementation = ReturnTypeWillChange::__construct)]
    fn __construct(this: This<Self>);
}

/// `final class SensitiveParameter`, `@strict-properties`,
/// `#[Attribute(Attribute::TARGET_PARAMETER)]`.
#[php_class(
    final,
    dynamic_properties = forbidden,
    attribute_class(Attribute::TARGET_PARAMETER)
)]
pub(crate) struct SensitiveParameter;

#[php_class]
impl SensitiveParameter {
    #[php(implementation = ReturnTypeWillChange::__construct)]
    fn __construct(this: This<Self>);
}

/// `final class SensitiveParameterValue`, `@strict-properties` and
/// `@not-serializable`, no attribute, its private readonly `$value`
/// shown to no dumper through [`SensitiveValueHandlers`].
///
/// ```php
/// $v = new SensitiveParameterValue(['secret']);
/// var_dump($v->getValue(), $v->__debugInfo());
/// ```
/// ```expect
/// array(1) {
///   [0]=>
///   string(6) "secret"
/// }
/// array(0) {
/// }
/// ```
#[php_class(
    final,
    not_serializable,
    dynamic_properties = forbidden,
    handlers = SENSITIVE_VALUE
)]
pub(crate) struct SensitiveParameterValue;

#[php_class]
impl SensitiveParameterValue {
    #[php(property, private, readonly)]
    const value: Value;

    /// `SensitiveParameterValue::__construct(mixed $value)`.
    ///
    /// ```php
    /// $v = new SensitiveParameterValue([1]);
    /// try { $v->__construct(2); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// try { new SensitiveParameterValue; } catch (ArgumentCountError $e) { echo $e->getMessage(), "\n"; }
    /// var_dump($v->getValue());
    /// ```
    /// ```expect
    /// Cannot modify readonly property SensitiveParameterValue::$value
    /// SensitiveParameterValue::__construct() expects exactly 1 argument, 0 given
    /// array(1) {
    ///   [0]=>
    ///   int(1)
    /// }
    /// ```
    fn __construct(req: &mut Request, this: This<Self>, value: Value) -> Result<(), Thrown> {
        let value = Owner::take(req, value);
        initialise(req, this.object(), b"value", value)
    }

    /// `SensitiveParameterValue::getValue(): mixed`, the value the
    /// constructor kept.
    fn getValue(this: This<Self>) -> Value {
        this.object().slot(0).unwrap_or_default()
    }

    /// `SensitiveParameterValue::__debugInfo(): array`, empty.
    fn __debugInfo(this: This<Self>) -> PhpArray {
        let _ = this;
        PhpArray::empty()
    }
}

/// `final class Override`, `@strict-properties`,
/// `#[Attribute(Attribute::TARGET_METHOD|Attribute::TARGET_PROPERTY)]`.
#[php_class(
    final,
    dynamic_properties = forbidden,
    attribute_class(Attribute::TARGET_METHOD | Attribute::TARGET_PROPERTY)
)]
pub(crate) struct Override;

#[php_class]
impl Override {
    #[php(implementation = ReturnTypeWillChange::__construct)]
    fn __construct(this: This<Self>);
}

/// `final class Deprecated`, `@strict-properties`, its two readonly
/// properties.
#[php_class(
    final,
    dynamic_properties = forbidden,
    attribute_class(
        Attribute::TARGET_METHOD
            | Attribute::TARGET_FUNCTION
            | Attribute::TARGET_CLASS_CONSTANT
            | Attribute::TARGET_CONSTANT
            | Attribute::TARGET_CLASS
    )
)]
pub(crate) struct Deprecated;

#[php_class]
impl Deprecated {
    #[php(property, readonly)]
    const message: Option<PhpString>;
    #[php(property, readonly)]
    const since: Option<PhpString>;

    /// `Deprecated::__construct(?string $message = null, ?string $since =
    /// null)`: each argument held to `?string` under the caller's
    /// `strict_types`, then written into its readonly property.
    ///
    /// ```php
    /// var_dump(new Deprecated, new Deprecated(since: "8.0"), new Deprecated(null, 5));
    /// try { new Deprecated([]); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// $d = new Deprecated("foo");
    /// try { $d->message = 'bar'; } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// try { $d->__construct("bar"); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// object(Deprecated)#1 (2) {
    ///   ["message"]=>
    ///   NULL
    ///   ["since"]=>
    ///   NULL
    /// }
    /// object(Deprecated)#2 (2) {
    ///   ["message"]=>
    ///   NULL
    ///   ["since"]=>
    ///   string(3) "8.0"
    /// }
    /// object(Deprecated)#3 (2) {
    ///   ["message"]=>
    ///   NULL
    ///   ["since"]=>
    ///   string(1) "5"
    /// }
    /// Deprecated::__construct(): Argument #1 ($message) must be of type ?string, array given
    /// Cannot modify readonly property Deprecated::$message
    /// Cannot modify readonly property Deprecated::$message
    /// ```
    fn __construct(
        req: &mut Request,
        this: This<Self>,
        #[php(default = null)] message: Option<PhpString>,
        #[php(default = null)] since: Option<PhpString>,
    ) -> Result<(), Thrown> {
        let this = this.object();
        let message = Owner::take(req, message.map_or_else(Value::null, Value::string));
        initialise(req, this, b"message", message)?;
        let since = Owner::take(req, since.map_or_else(Value::null, Value::string));
        initialise(req, this, b"since", since)
    }
}

/// `final class NoDiscard`, `@strict-properties`, its one readonly
/// property.
#[php_class(
    final,
    dynamic_properties = forbidden,
    attribute_class(Attribute::TARGET_METHOD | Attribute::TARGET_FUNCTION)
)]
pub(crate) struct NoDiscard;

#[php_class]
impl NoDiscard {
    #[php(property, readonly)]
    const message: Option<PhpString>;

    /// `NoDiscard::__construct(?string $message = null)`.
    ///
    /// ```php
    /// var_dump(new NoDiscard, new NoDiscard("kept"));
    /// ```
    /// ```expect
    /// object(NoDiscard)#1 (1) {
    ///   ["message"]=>
    ///   NULL
    /// }
    /// object(NoDiscard)#2 (1) {
    ///   ["message"]=>
    ///   string(4) "kept"
    /// }
    /// ```
    fn __construct(
        req: &mut Request,
        this: This<Self>,
        #[php(default = null)] message: Option<PhpString>,
    ) -> Result<(), Thrown> {
        let message = Owner::take(req, message.map_or_else(Value::null, Value::string));
        initialise(req, this.object(), b"message", message)
    }
}

/// `final class DelayedTargetValidation`, `@strict-properties`,
/// `#[Attribute(Attribute::TARGET_ALL)]`, of no member.
#[php_class(
    final,
    dynamic_properties = forbidden,
    attribute_class(Attribute::TARGET_ALL)
)]
pub(crate) struct DelayedTargetValidation;

#[php_class]
impl DelayedTargetValidation {}

/// `zend_update_property_ex` over a readonly property from its class's
/// own constructor: the first write initialises it, and a second is
/// php's `Cannot modify readonly property`. The name is the property's
/// own, `ZSTR_KNOWN(ZEND_STR_MESSAGE)` and its kind, which costs the
/// request nothing.
fn initialise(
    req: &mut Request,
    this: PhpObject,
    name: &[u8],
    value: Owner<Value>,
) -> Result<(), Thrown> {
    if this.prop(req, name).is_some() {
        value.give_up(req);
        return Err(req.readonly_modification(this.class(), name, "modify"));
    }
    let name = match req.declared_string(name) {
        Ok(name) => name,
        Err(thrown) => {
            value.give_up(req);
            return Err(thrown);
        }
    };
    this.set_prop(req, name, value)
}
