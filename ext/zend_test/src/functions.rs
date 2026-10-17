//! The test functions of php's `ext/zend_test/test.c`, each the native
//! `test.stub.php` declares, in the stub's order: what a Zend-level
//! `.phpt` calls to reach one API of php's engine, here the face's
//! equivalent of it. The aliases and the type `gen_stub.php` cannot
//! write are laid onto the declarations the attributes make. Those that
//! reach past what a program sees are [`internals`]', laid into the list
//! at their places.

use php_ext::abi::{
    ExtensionBuilder, FromArgument, Instance, Iterable, NamedVariadic, Never, Passed, Path, Raw,
    Ref, Union, Untyped, Variadic,
};
use php_ext::attr::php_function;
use php_ext::call::{call, instantiate_named, known_method, lookup_class};
use php_ext::coerce::{callable_checked, value_name};
use php_ext::decl::{Attribute, FunctionDecl, Name, Type};
use php_ext::raising::{
    Thrown, arg_type_error, arg_value_error, fatal, throw, throw_error, warning,
};
use php_ext::request::{Request, Stream};
use php_ext::syntax::Position;
use php_ext::values::{
    Owner, PhpArray, PhpObject, PhpString, Resource, ResourceType, Value, ValueRef,
};

use crate::classes::{ZendTestUnitEnum, stdClass};
use crate::internals;

/// The functions onto the builder, in `ext_functions`' order, then
/// `zend_test_internal_dnf_arguments`, which `MINIT` registers by hand,
/// and `zend_iterable_legacy`, which it registers last.
pub(crate) fn declared(builder: ExtensionBuilder) -> ExtensionBuilder {
    let void = zend_test_aliased::declared;
    let ns2 = |name: &str| format!("ZendTestNS2\\{name}");
    let sub = |name: &str| format!("ZendTestNS2\\ZendSubNS\\{name}");
    builder.functions(vec![
        zend_trigger_bailout::declared(),
        zend_test_array_return::declared(),
        documented(zend_test_nullable_array_return::declared(), 4, "8.3"),
        zend_test_void_return::declared(),
        zend_test_compile_string::declared(),
        zend_test_deprecated::declared(),
        zend_test_deprecated_attr::declared(),
        zend_test_nodiscard::declared(),
        zend_test_deprecated_nodiscard::declared(),
        void(),
        alias(void(), "zend_test_deprecated_aliased", true),
        zend_create_unterminated_string::declared(),
        zend_terminate_string::declared(),
        zend_leak_variable::declared(),
        internals::zend_leak_bytes::declared(),
        zend_string_or_object::declared(),
        zend_string_or_object_or_null::declared(),
        zend_string_or_stdclass::declared(),
        zend_string_or_stdclass_or_null::declared(),
        zend_number_or_string::declared(),
        zend_number_or_string_or_null::declared(),
        zend_iterable::declared(),
        zend_weakmap_attach::declared(),
        zend_weakmap_remove::declared(),
        zend_weakmap_dump::declared(),
        zend_get_unit_enum::declared(),
        zend_test_parameter_with_attribute::declared(),
        zend_test_attribute_with_named_argument::declared(),
        zend_get_current_func_name::declared(),
        zend_call_method::declared(),
        zend_object_init_with_constructor::declared(),
        zend_call_method_if_exists::declared(),
        zend_test_zend_ini_parse_quantity::declared(),
        zend_test_zend_ini_parse_uquantity::declared(),
        zend_test_zend_ini_str::declared(),
        internals::zend_test_zend_call_stack_get::declared(),
        internals::zend_test_zend_call_stack_use_all::declared(),
        zend_test_is_string_marked_as_valid_utf8::declared(),
        internals::zend_get_map_ptr_last::declared(),
        zend_test_fill_packed_array::declared(),
        zend_test_create_throwing_resource::declared(),
        get_open_basedir::declared(),
        internals::zend_test_override_libxml_global_state::declared(),
        internals::zend_test_is_pcre_bundled::declared(),
        zend_test_cast_fread::declared(),
        internals::zend_test_is_zend_ptr::declared(),
        internals::zend_test_log_err_debug::declared(),
        zend_test_compile_to_ast::declared(),
        internals::zend_test_opcache_preloading::declared(),
        internals::zend_test_uri_parser::declared(),
        zend_test_gh19792::declared(),
        namespaced_func::declared(),
        alias(void(), &ns2("namespaced_deprecated_func"), true),
        alias(void(), &ns2("namespaced_aliased_func"), false),
        alias(void(), &ns2("namespaced_deprecated_aliased_func"), true),
        alias(namespaced_func::declared(), &sub("namespaced_func"), false),
        alias(void(), &sub("namespaced_deprecated_func"), true),
        alias(void(), &sub("namespaced_aliased_func"), false),
        alias(void(), &sub("namespaced_deprecated_aliased_func"), true),
        dnf(zend_test_internal_dnf_arguments::declared()),
        zend_iterable_legacy::declared(),
    ])
}

/// A function entry under another name, as `ZEND_RAW_FENTRY` lays one
/// down for an `@alias` or for a body two stub functions share: the
/// same body, the stub's own name, and `@deprecated` where the stub
/// says it.
fn alias<E>(
    (mut decl, entry): (FunctionDecl, E),
    name: &str,
    deprecated: bool,
) -> (FunctionDecl, E) {
    decl.name = Name(name.as_bytes().into());
    if deprecated {
        decl.attributes.push(Attribute {
            name: Name(b"Deprecated".as_slice().into()),
            arguments: Vec::new(),
            line: 0,
            strict: false,
        });
    }
    (decl, entry)
}

/// The doc comment `@genstubs-expose-comment-block` keeps on a
/// declaration: "Lorem ipsum", `@see` and `@since`, indented `indent`
/// spaces as the stub indents it.
pub(crate) fn lorem_ipsum(indent: usize, since: &str) -> Box<[u8]> {
    let pad = " ".repeat(indent + 1);
    format!(
        "/**\n{pad}* \"Lorem ipsum\"\n{pad}* @see https://www.php.net\n{pad}* @since {since}\n{pad}*/"
    )
    .into_bytes()
    .into()
}

fn documented<E>(
    (mut decl, entry): (FunctionDecl, E),
    indent: usize,
    since: &str,
) -> (FunctionDecl, E) {
    decl.doc_comment = Some(lorem_ipsum(indent, since));
    (decl, entry)
}

/// `Iterator|(Traversable&Countable)`, `create_test_dnf_type`: the type
/// `gen_stub.php` cannot write, laid on by hand.
pub(crate) fn dnf_type() -> Type {
    let named = |name: &[u8]| Type::Named(Name(name.into()));
    Type::Union(vec![
        named(b"Iterator"),
        Type::Intersection(vec![named(b"Traversable"), named(b"Countable")]),
    ])
}

fn dnf<E>((mut decl, entry): (FunctionDecl, E)) -> (FunctionDecl, E) {
    decl.return_type = Some(dnf_type());
    for parameter in &mut decl.parameters {
        parameter.ty = Some(dnf_type());
    }
    (decl, entry)
}

/// `zend_trigger_bailout(): never`: the fatal `E_ERROR` it raises.
///
/// ```php
/// echo "before\n";
/// zend_trigger_bailout();
/// ```
/// ```expect exit=255
/// before
///
/// Fatal error: Bailout in %s on line 3
/// ```
#[php_function]
fn zend_trigger_bailout(req: &mut Request) -> Result<Never, Thrown> {
    Err(fatal(req, "Bailout"))
}

/// `zend_test_array_return(): array`, whose body returns nothing: php
/// verifies a native's return only in a debug build.
///
/// ```php
/// var_dump(zend_test_array_return());
/// ```
/// ```expect
/// NULL
/// ```
#[php_function]
fn zend_test_array_return(req: &mut Request) -> Result<Raw<PhpArray>, Thrown> {
    Raw::from_value(req, Value::null())
}

/// `zend_test_nullable_array_return(): ?array`, null, with the doc
/// comment its stub exposes.
///
/// ```php
/// var_dump(zend_test_nullable_array_return());
/// echo (new ReflectionFunction('zend_test_nullable_array_return'))->getDocComment(), "\n";
/// ```
/// ```expect
/// NULL
/// /**
///      * "Lorem ipsum"
///      * @see https://www.php.net
///      * @since 8.3
///      */
/// ```
#[php_function]
fn zend_test_nullable_array_return() -> Option<PhpArray> {
    None
}

/// `zend_test_void_return(): void`, and `zend_test_aliased`, its alias;
/// the deprecated aliases and the namespaced ones are laid down by
/// [`alias`].
///
/// ```php
/// var_dump(zend_test_void_return(), zend_test_aliased(), ZendTestNS2\namespaced_aliased_func());
/// zend_test_deprecated_aliased();
/// ZendTestNS2\ZendSubNS\namespaced_deprecated_func();
/// ```
/// ```expect
/// NULL
/// NULL
/// NULL
///
/// Deprecated: Function zend_test_deprecated_aliased() is deprecated in %s on line 3
///
/// Deprecated: Function ZendTestNS2\ZendSubNS\namespaced_deprecated_func() is deprecated in %s on line 4
/// ```
#[php_function(aliases(zend_test_aliased))]
fn zend_test_void_return() {}

/// `zend_test_compile_string(string $source_string, string $filename,
/// int $position): void`: `compile_string` from `ZEND_COMPILE_POSITION_*`
/// (0 at a shebang, 1 at an open tag, 2 after one) under the name, and
/// `zend_execute` of what it made.
///
/// ```php
/// zend_test_compile_string("#!/bin/php\n<?php echo __FILE__, \"\\n\";", 'Source string', 0);
/// zend_test_compile_string("<?php echo 'open';?>\n", 'Source string', 1);
/// $x = 'caller';
/// zend_test_compile_string('echo $x, "\n";', 'Source string', 2);
/// try { zend_test_compile_string('', "a\0b", 1); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// zend_test_compile_string('<?php }', 'Source string', 1);
/// ```
/// ```expect exit=255
/// Source string
/// opencaller
/// zend_test_compile_string(): Argument #2 ($filename) must not contain any null bytes
///
/// Parse error: Unmatched '}' in Source string on line 1
/// ```
#[php_function]
fn zend_test_compile_string(
    req: &mut Request,
    source_string: PhpString,
    filename: Path,
    position: i64,
) -> Result<(), Thrown> {
    let position = match position {
        0 => Position::AtShebang,
        2 => Position::AfterOpenTag,
        _ => Position::AtOpenTag,
    };
    req.serve_string(source_string.as_bytes(), filename.0.as_bytes(), position)
}

/// `zend_test_deprecated(mixed $arg = null): void`, `@deprecated`.
///
/// ```php
/// zend_test_deprecated(1);
/// ```
/// ```expect
/// Deprecated: Function zend_test_deprecated() is deprecated in %s on line 2
/// ```
#[php_function(deprecated)]
fn zend_test_deprecated(#[php(default = null)] arg: Value) {
    let _ = arg;
}

/// `zend_test_deprecated_attr(): void`, `#[\Deprecated]` with a message.
///
/// ```php
/// zend_test_deprecated_attr();
/// var_dump((new ReflectionFunction('zend_test_deprecated_attr'))->getAttributes()[0]->getArguments());
/// ```
/// ```expect
/// Deprecated: Function zend_test_deprecated_attr() is deprecated, custom message in %s on line 2
/// array(1) {
///   ["message"]=>
///   string(14) "custom message"
/// }
/// ```
#[php_function(deprecated(message = "custom message"))]
fn zend_test_deprecated_attr() {}

/// `zend_test_nodiscard(): int`, `#[\NoDiscard]` with a message.
///
/// ```php
/// var_dump(zend_test_nodiscard());
/// zend_test_nodiscard();
/// ```
/// ```expect
/// int(1)
///
/// Warning: The return value of function zend_test_nodiscard() should either be used or intentionally ignored by casting it as (void), custom message in %s on line 3
/// ```
#[php_function(no_discard(message = "custom message"))]
fn zend_test_nodiscard() -> i64 {
    1
}

/// `zend_test_deprecated_nodiscard(): int`, both attributes.
///
/// ```php
/// zend_test_deprecated_nodiscard();
/// ```
/// ```expect
/// Deprecated: Function zend_test_deprecated_nodiscard() is deprecated, custom message in %s on line 2
///
/// Warning: The return value of function zend_test_deprecated_nodiscard() should either be used or intentionally ignored by casting it as (void), custom message 2 in %s on line 2
/// ```
#[php_function(
    deprecated(message = "custom message"),
    no_discard(message = "custom message 2")
)]
fn zend_test_deprecated_nodiscard() -> i64 {
    1
}

/// `zend_create_unterminated_string(string $str): string`: a copy, which
/// php leaves without its terminating byte for the tests of functions
/// that would read past a string's end; a string here is its length.
///
/// ```php
/// $s = zend_create_unterminated_string("abc");
/// var_dump($s, strcmp($s, "abc"));
/// ```
/// ```expect
/// string(3) "abc"
/// int(0)
/// ```
#[php_function]
fn zend_create_unterminated_string(
    req: &mut Request,
    str: PhpString,
) -> Result<Owner<PhpString>, Thrown> {
    PhpString::new(req, str.as_bytes())
}

/// `zend_terminate_string(string &$str): void`: the byte php writes
/// past the end, which a string here does not need.
///
/// ```php
/// $s = zend_create_unterminated_string("abc");
/// zend_terminate_string($s);
/// var_dump($s);
/// ```
/// ```expect
/// string(3) "abc"
/// ```
#[php_function]
fn zend_terminate_string(str: Ref<PhpString>) {
    let _ = str;
}

/// `zend_leak_variable(mixed $variable): void`: one owner taken and
/// never given up by the program, or a warning for a value no owner
/// counts. php's leak lasts until its heap is freed whole at the
/// request's end; here the request state keeps the owner and gives it
/// up at `RSHUTDOWN`, so nothing the program sees tells the two apart.
///
/// ```php
/// zend_leak_variable(1);
/// $o = new stdClass;
/// zend_leak_variable($o);
/// unset($o);
/// echo "leaked\n";
/// ```
/// ```expect
/// Warning: Cannot leak variable that is not refcounted in %s on line 2
/// leaked
/// ```
#[php_function]
fn zend_leak_variable(req: &mut Request, variable: Value) {
    if !variable.is_refcounted() {
        warning(req, "Cannot leak variable that is not refcounted");
        return;
    }
    let leaked = Owner::take(req, variable);
    req.state::<crate::State>().leaked.push(leaked);
}

/// `zend_string_or_object(object|string $param): object|string`,
/// `Z_PARAM_OBJ_OR_STR`.
///
/// ```php
/// var_dump(zend_string_or_object("a"), zend_string_or_object(new stdClass));
/// try { zend_string_or_object([]); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(1) "a"
/// object(stdClass)#1 (0) {
/// }
/// zend_string_or_object(): Argument #1 ($param) must be of type object|string, array given
/// ```
#[php_function]
fn zend_string_or_object(param: Union<PhpObject, PhpString>) -> Union<PhpObject, PhpString> {
    param
}

/// `zend_string_or_object_or_null()`, `object|string|null` taken and
/// returned, held by `Z_PARAM_OBJ_OR_STR_OR_NULL`.
///
/// ```php
/// var_dump(zend_string_or_object_or_null(null), zend_string_or_object_or_null("a"));
/// ```
/// ```expect
/// NULL
/// string(1) "a"
/// ```
#[php_function]
fn zend_string_or_object_or_null(
    param: Option<Union<PhpObject, PhpString>>,
) -> Option<Union<PhpObject, PhpString>> {
    param
}

/// `zend_string_or_stdclass($param): stdClass|string`, its parameter
/// undeclared and held by `Z_PARAM_OBJ_OF_CLASS_OR_STR`
/// ([`stdclass_or_string`]).
///
/// ```php
/// class S { function __toString(): string { return "s"; } }
/// var_dump(zend_string_or_stdclass("a"), zend_string_or_stdclass(new stdClass), zend_string_or_stdclass(1), zend_string_or_stdclass(new S), zend_string_or_stdclass(null));
/// try { zend_string_or_stdclass(new ArrayObject); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Deprecated: zend_string_or_stdclass(): Passing null to parameter #1 ($param) of type string is deprecated in %s on line 3
/// string(1) "a"
/// object(stdClass)#1 (0) {
/// }
/// string(1) "1"
/// string(1) "s"
/// string(0) ""
/// zend_string_or_stdclass(): Argument #1 ($param) must be of type stdClass|string, ArrayObject given
/// ```
#[php_function]
fn zend_string_or_stdclass(
    req: &mut Request,
    param: Untyped<Value>,
) -> Result<StdClassOrString, Thrown> {
    match stdclass_or_string(req, param.0, false)? {
        Some(held) => Ok(held),
        None => Err(fatal(
            req,
            "InternalError: null held where null is not taken",
        )),
    }
}

/// `zend_string_or_stdclass_or_null($param): stdClass|string|null`,
/// `Z_PARAM_OBJ_OF_CLASS_OR_STR_OR_NULL`.
///
/// ```php
/// var_dump(zend_string_or_stdclass_or_null(null), zend_string_or_stdclass_or_null(1.5));
/// try { zend_string_or_stdclass_or_null([]); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// NULL
/// string(3) "1.5"
/// zend_string_or_stdclass_or_null(): Argument #1 ($param) must be of type stdClass|string|null, array given
/// ```
#[php_function]
fn zend_string_or_stdclass_or_null(
    req: &mut Request,
    param: Untyped<Value>,
) -> Result<Option<StdClassOrString>, Thrown> {
    stdclass_or_string(req, param.0, true)
}

/// What `Z_PARAM_OBJ_OF_CLASS_OR_STR` holds over `stdClass`.
type StdClassOrString = Union<Instance<stdClass>, Owner<PhpString>>;

/// `Z_PARAM_OBJ_OF_CLASS_OR_STR` and `_OR_NULL` over `stdClass`: an
/// object of the class as it is, null as itself where `nullable` says,
/// and every other value as `Z_PARAM_STR` holds it, `zend_parse_arg_str`
/// under the caller's `strict_types`, whose null deprecation names
/// `string`; what neither takes is php's `TypeError` naming the whole
/// type.
fn stdclass_or_string(
    req: &mut Request,
    value: Value,
    nullable: bool,
) -> Result<Option<StdClassOrString>, Thrown> {
    let strict = php_ext::coerce::strict_types(req);
    let stdclass = lookup_class(req, b"stdClass")?;
    let as_string = match value.as_ref() {
        ValueRef::Object(object) if stdclass.is_some_and(|class| object.class() == class) => {
            return Instance::from_value(req, value).map(|held| Some(Union::A(held)));
        }
        ValueRef::Str(string) => return Ok(Some(Union::B(Owner::take(req, *string)))),
        ValueRef::Null if nullable => return Ok(None),
        ValueRef::Null if !strict => {
            php_ext::raising::arg_null_deprecated(req, 1, "string");
            true
        }
        ValueRef::Int(_) | ValueRef::Float(_) | ValueRef::Bool(_) => !strict,
        ValueRef::Object(object) => {
            !strict && req.method_named(object.class(), b"__tostring").is_some()
        }
        _ => false,
    };
    if as_string {
        return php_ext::coerce::to_string(req, &value).map(|string| Some(Union::B(string)));
    }
    let expected = if nullable {
        "stdClass|string|null"
    } else {
        "stdClass|string"
    };
    let text = format!("be of type {expected}, {} given", value_name(req, &value));
    Err(arg_type_error(req, 1, &text))
}

/// A number or a string, `string|int|float`, `Z_PARAM_NUMBER_OR_STR`.
type NumberOrString = Union<PhpString, Union<i64, f64>>;

/// `zend_number_or_string(string|int|float $param): string|int|float`.
///
/// ```php
/// var_dump(zend_number_or_string("a"), zend_number_or_string(1), zend_number_or_string(1.5), zend_number_or_string(true));
/// try { zend_number_or_string([]); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(1) "a"
/// int(1)
/// float(1.5)
/// int(1)
/// zend_number_or_string(): Argument #1 ($param) must be of type string|int|float, array given
/// ```
#[php_function]
fn zend_number_or_string(param: NumberOrString) -> NumberOrString {
    param
}

/// `zend_number_or_string_or_null(string|int|float|null $param):
/// string|int|float|null`.
///
/// ```php
/// var_dump(zend_number_or_string_or_null(null), zend_number_or_string_or_null(2));
/// ```
/// ```expect
/// NULL
/// int(2)
/// ```
#[php_function]
fn zend_number_or_string_or_null(param: Option<NumberOrString>) -> Option<NumberOrString> {
    param
}

/// `zend_iterable(iterable $arg1, ?iterable $arg2 = null): void`,
/// `Z_PARAM_ITERABLE` and `Z_PARAM_ITERABLE_OR_NULL`.
///
/// ```php
/// zend_iterable([]);
/// zend_iterable(new ArrayIterator([]), null);
/// try { zend_iterable(1); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// zend_iterable(): Argument #1 ($arg1) must be of type Traversable|array, int given
/// ```
#[php_function]
fn zend_iterable(arg1: Iterable, #[php(default = null)] arg2: Option<Iterable>) {
    let _ = (arg1, arg2);
}

/// `zend_iterable_legacy(iterable $arg1, ?iterable $arg2 = null):
/// iterable`: the first argument back, registered from the arginfo
/// php keeps for an extension built before `iterable` was a union.
///
/// ```php
/// var_dump(zend_iterable_legacy([1], null));
/// ```
/// ```expect
/// array(1) {
///   [0]=>
///   int(1)
/// }
/// ```
#[php_function]
fn zend_iterable_legacy(
    arg1: Raw<Iterable>,
    #[php(default = null)] arg2: Option<Iterable>,
) -> Raw<Iterable> {
    let _ = arg2;
    arg1
}

/// `zend_weakmap_attach(object $object, mixed $value): bool`: the value
/// kept under the object for as long as the object lives,
/// `zend_weakrefs_hash_add` on the request's map; false where the
/// object already has one.
///
/// ```php
/// $o = new stdClass;
/// var_dump(zend_weakmap_attach($o, 1), zend_weakmap_attach($o, 2), count(zend_weakmap_dump()));
/// unset($o);
/// var_dump(zend_weakmap_dump());
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// int(1)
/// array(0) {
/// }
/// ```
#[php_function]
fn zend_weakmap_attach(req: &mut Request, object: PhpObject, value: Value) -> Result<bool, Thrown> {
    crate::weakmap::attach(req, object, value)
}

/// `zend_weakmap_remove(object $object): bool`, `zend_weakrefs_hash_del`.
///
/// ```php
/// $o = new stdClass;
/// zend_weakmap_attach($o, 1);
/// var_dump(zend_weakmap_remove($o), zend_weakmap_remove($o));
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// ```
#[php_function]
fn zend_weakmap_remove(req: &mut Request, object: PhpObject) -> Result<bool, Thrown> {
    crate::weakmap::remove(req, object)
}

/// `zend_weakmap_dump(): array`: a copy of the map, keyed by object.
///
/// ```php
/// $o = new stdClass;
/// zend_weakmap_attach($o, "v");
/// var_dump(array_values(zend_weakmap_dump()));
/// ```
/// ```expect
/// array(1) {
///   [0]=>
///   string(1) "v"
/// }
/// ```
#[php_function]
fn zend_weakmap_dump(req: &mut Request) -> Result<Owner<PhpArray>, Thrown> {
    crate::weakmap::dump(req)
}

/// `zend_get_unit_enum(): ZendTestUnitEnum`, the case `Foo`.
///
/// ```php
/// var_dump(zend_get_unit_enum());
/// ```
/// ```expect
/// enum(ZendTestUnitEnum::Foo)
/// ```
#[php_function]
fn zend_get_unit_enum() -> ZendTestUnitEnum {
    ZendTestUnitEnum::Foo
}

/// `zend_test_parameter_with_attribute(#[ZendTestParameterAttribute(
/// "value1")] string $parameter): int`.
///
/// ```php
/// var_dump(zend_test_parameter_with_attribute("a"));
/// ```
/// ```expect
/// int(1)
/// ```
#[php_function]
fn zend_test_parameter_with_attribute(
    #[php(attribute(ZendTestParameterAttribute("value1")))] parameter: PhpString,
) -> i64 {
    let _ = parameter;
    1
}

/// `#[ZendTestAttributeWithArguments(arg: "foo")]
/// zend_test_attribute_with_named_argument(): void`.
///
/// ```php
/// var_dump(zend_test_attribute_with_named_argument());
/// ```
/// ```expect
/// NULL
/// ```
#[php_function(attribute(ZendTestAttributeWithArguments(arg = "foo")))]
fn zend_test_attribute_with_named_argument() {}

/// `zend_get_current_func_name(): string`: the function it was called
/// from, `get_function_or_method_name`.
///
/// ```php
/// class K { static function m() { return zend_get_current_func_name(); } }
/// function f() { return zend_get_current_func_name(); }
/// $cuf = 'call_user_func';
/// var_dump(zend_get_current_func_name(), f(), K::m(), $cuf('zend_get_current_func_name'));
/// ```
/// ```expect
/// string(4) "main"
/// string(1) "f"
/// string(4) "K::m"
/// string(14) "call_user_func"
/// ```
#[php_function]
fn zend_get_current_func_name(req: &mut Request) -> Result<Owner<PhpString>, Thrown> {
    let name = req.caller_function_name();
    PhpString::new(req, name.as_bytes())
}

/// `zend_call_method(object|string $obj_or_class, string $method, mixed
/// $arg1 = UNKNOWN, mixed $arg2 = UNKNOWN): mixed`: `zend_call_method`,
/// the method the class declares or inherits under the name, served
/// with the arguments given, on the object where one is given and with
/// the class as the called scope.
///
/// ```php
/// enum E: int { case A = 1; }
/// var_dump(zend_call_method(E::class, "from", 1), zend_call_method(new ArrayObject([1, 2]), "count"));
/// ```
/// ```expect
/// enum(E::A)
/// int(2)
/// ```
#[php_function]
fn zend_call_method(
    req: &mut Request,
    obj_or_class: Raw<Union<PhpObject, PhpString>>,
    method: PhpString,
    arg1: Passed<Value>,
    arg2: Passed<Value>,
) -> Result<Owner<Value>, Thrown> {
    let (object, class) = match obj_or_class.value().as_ref() {
        ValueRef::Object(object) => (Some(*object), object.class()),
        ValueRef::Str(name) => {
            let Some(class) = lookup_class(req, name.as_bytes())? else {
                let text = format!(
                    "Unknown class '{}'",
                    String::from_utf8_lossy(name.as_bytes())
                );
                return Err(fatal(req, &text));
            };
            (None, class)
        }
        _ => {
            let text = format!(
                "be of type object|string, {} given",
                value_name(req, &obj_or_class.value())
            );
            return Err(arg_type_error(req, 1, &text));
        }
    };
    let name = Name(method.as_bytes().into());
    let Some(callable) = known_method(req, class, &name, object) else {
        let text = format!(
            "Couldn't find implementation for method {}::{}",
            req.class_name(class),
            String::from_utf8_lossy(method.as_bytes())
        );
        return Err(fatal(req, &text));
    };
    let arguments: Vec<Value> = [arg1, arg2]
        .into_iter()
        .map_while(|arg| match arg {
            Passed::Given(value) => Some(value),
            Passed::Omitted => None,
        })
        .collect();
    call(req, &callable, &arguments)
}

/// `zend_object_init_with_constructor(string $class, mixed ...$args):
/// mixed`, `object_init_with_constructor`.
///
/// ```php
/// class P { function __construct(public int $a, public string $b = "b") {} }
/// var_dump(zend_object_init_with_constructor("P", 1, b: "c"));
/// try { zend_object_init_with_constructor("Countable"); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// object(P)#1 (2) {
///   ["a"]=>
///   int(1)
///   ["b"]=>
///   string(1) "c"
/// }
/// Cannot instantiate interface Countable
/// ```
#[php_function]
fn zend_object_init_with_constructor(
    req: &mut Request,
    class: PhpString,
    args: NamedVariadic<Value>,
) -> Result<Owner<Value>, Thrown> {
    let Some(class) = lookup_class(req, class.as_bytes())? else {
        let text = format!(
            "be a valid class name, {} given",
            String::from_utf8_lossy(class.as_bytes())
        );
        return Err(arg_type_error(req, 1, &text));
    };
    Ok(instantiate_named(req, class, &args.positional, args.named())?.into_value())
}

/// `zend_call_method_if_exists(object $obj, string $method, mixed
/// ...$args): mixed`: the method served where `$obj` and `$method` name
/// a callable from the caller's scope, `__call` among them, and null
/// where they name none.
///
/// ```php
/// class A { function f($x) { return $x * 2; } private function p() { return 1; } }
/// var_dump(zend_call_method_if_exists(new A, 'f', 2), zend_call_method_if_exists(new A, 'p'), zend_call_method_if_exists(new A, 'g'));
/// ```
/// ```expect
/// int(4)
/// NULL
/// NULL
/// ```
#[php_function]
fn zend_call_method_if_exists(
    req: &mut Request,
    obj: PhpObject,
    method: PhpString,
    args: Variadic<Value>,
) -> Result<Owner<Value>, Thrown> {
    let mut pair = PhpArray::new(req)?;
    pair.append(req, Owner::take(req, obj.as_value()))?;
    pair.append(req, Owner::take(req, Value::string(method)))?;
    let served = match callable_checked(req, &pair.to_value()) {
        Ok(callable) => call(req, &callable, &args.0),
        Err(_) => Ok(Owner::null()),
    };
    pair.give_up(req);
    served
}

/// `zend_test_zend_ini_parse_quantity(string $str): int`,
/// `zend_ini_parse_quantity` with its sentence raised as a warning.
///
/// ```php
/// var_dump(zend_test_zend_ini_parse_quantity("1K"), zend_test_zend_ini_parse_quantity("1Q"));
/// ```
/// ```expect
/// Warning: Invalid quantity "1Q": unknown multiplier "Q", interpreting as "1" for backwards compatibility in %s on line 2
/// int(1024)
/// int(1)
/// ```
#[php_function]
fn zend_test_zend_ini_parse_quantity(req: &mut Request, str: PhpString) -> i64 {
    let (value, refused) = Request::ini_parse_quantity(str.as_bytes());
    if let Some(sentence) = refused {
        warning(req, &sentence);
    }
    value
}

/// `zend_test_zend_ini_parse_uquantity(string $str): int`,
/// `zend_ini_parse_uquantity`, the unsigned value as the int of its
/// bits.
///
/// ```php
/// var_dump(zend_test_zend_ini_parse_uquantity("2K"), zend_test_zend_ini_parse_uquantity("-1"), zend_test_zend_ini_parse_uquantity("-2"));
/// ```
/// ```expect
/// Warning: Invalid quantity "-2": value is out of range, using overflow result for backwards compatibility in %s on line 2
/// int(2048)
/// int(-1)
/// int(2)
/// ```
#[php_function]
fn zend_test_zend_ini_parse_uquantity(req: &mut Request, str: PhpString) -> i64 {
    let (value, refused) = Request::ini_parse_uquantity(str.as_bytes());
    if let Some(sentence) = refused {
        warning(req, &sentence);
    }
    i64::from_ne_bytes(value.to_ne_bytes())
}

/// `zend_test_zend_ini_str(): string`, `zend_test.str_test` as it
/// stands.
///
/// ```php
/// var_dump(zend_test_zend_ini_str());
/// ini_set('zend_test.str_test', 'x');
/// var_dump(zend_test_zend_ini_str());
/// ```
/// ```expect
/// string(0) ""
/// string(1) "x"
/// ```
#[php_function]
fn zend_test_zend_ini_str(req: &mut Request) -> Result<Owner<PhpString>, Thrown> {
    let value = req.ini_get(crate::STR_TEST).unwrap_or_default().to_vec();
    PhpString::new(req, &value)
}

/// `zend_test_is_string_marked_as_valid_utf8(string $string): bool`,
/// `ZSTR_IS_VALID_UTF8`.
///
/// ```php
/// var_dump(zend_test_is_string_marked_as_valid_utf8(""), zend_test_is_string_marked_as_valid_utf8(str_repeat("\xff", 2)));
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// ```
///
/// The engine marks what php's `zend_long_to_str` and
/// `zend_double_to_str` make, a concatenation of two marked strings
/// where the program makes it and as the compiler folds it (save where interning finds the
/// bytes already held, as `"f" . "o" . "o"` finds the `Foo` class's
/// lowered name), a rope of marked pieces, `str_repeat` of a marked
/// string and `implode` of marked pieces with a marked glue:
///
/// ```php
/// $f = fn ($s) => zend_test_is_string_marked_as_valid_utf8($s) ? 'y' : 'n';
/// $a = 'a'; $b = 'b'; $x = "\xff"; $i = 12; $d = 1.5;
/// echo $f((string) $i), $f((string) $d), $f($a . $b), $f($a . $x), $f("f" . "x" . "o"), $f("f" . "o" . "o"), "\n";
/// echo $f("$a$b$a"), $f("$a$x"), $f(str_repeat($a, 3)), $f(str_repeat($x, 3)), "\n";
/// echo $f(implode(",", [$a, $b])), $f(implode(",", [$a, 1, 2.5])), $f(implode(",", [$a, $x])), $f(implode($x, [$a, $b])), $f(implode([$i])), "\n";
/// ```
/// ```expect exit=0
/// yyynyn
/// ynyn
/// yynny
/// ```
#[php_function]
fn zend_test_is_string_marked_as_valid_utf8(string: PhpString) -> bool {
    string.is_valid_utf8()
}

/// `ZendTestNS2\namespaced_func(): bool`, and the same under
/// `ZendTestNS2\ZendSubNS`, laid down by [`alias`].
///
/// ```php
/// var_dump(ZendTestNS2\namespaced_func(), ZendTestNS2\ZendSubNS\namespaced_func());
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// ```
#[php_function(namespace(ZendTestNS2))]
fn namespaced_func() -> bool {
    true
}

/// `zend_test_fill_packed_array(array &$array): void`: ten integers,
/// `0` to `9`, appended to a packed array, as `ZEND_HASH_FILL_PACKED`
/// lays them past its last slot.
///
/// ```php
/// $a = [1];
/// zend_test_fill_packed_array($a);
/// echo implode(",", $a), " ", count($a), "\n";
/// $h = ['k' => 1];
/// try { zend_test_fill_packed_array($h); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// 1,0,1,2,3,4,5,6,7,8,9 11
/// zend_test_fill_packed_array(): Argument #1 ($array) must be a packed array
/// ```
#[php_function]
fn zend_test_fill_packed_array(req: &mut Request, array: Ref<PhpArray>) -> Result<(), Thrown> {
    let Some(mut held) = array.separated(req)? else {
        return Ok(());
    };
    if !held.is_packed() {
        return Err(arg_value_error(req, 1, "be a packed array"));
    }
    for i in 0..10 {
        held.append(req, Owner::int(i))?;
    }
    Ok(())
}

/// `zend_test_create_throwing_resource(): resource`, of the type
/// `throwing resource`, whose destructor throws.
///
/// ```php
/// try { $r = zend_test_create_throwing_resource(); var_dump(get_resource_type($r)); unset($r); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(17) "throwing resource"
/// Throwing resource destructor called
/// ```
#[php_function]
fn zend_test_create_throwing_resource(
    req: &mut Request,
) -> Result<Untyped<Owner<Resource>>, Thrown> {
    let kind = throwing_resource(req);
    Resource::bare(req, kind).map(Untyped)
}

/// The `throwing resource` type, `le_throwing_resource`, registered on
/// first use in the request.
fn throwing_resource(req: &mut Request) -> ResourceType {
    if let Some(kind) = req.state::<crate::State>().throwing_resource {
        return kind;
    }
    let kind = req
        .resources()
        .register_type(b"throwing resource", Some(throw_from_dtor));
    req.state::<crate::State>().throwing_resource = Some(kind);
    kind
}

/// `le_throwing_resource_dtor`: `Exception`, "Throwing resource
/// destructor called", left pending where the release stood.
///
/// Released at the request's end, by `zend_close_rsrc_list`, no frame
/// stands to take it: the handler `set_exception_handler()` set is
/// handed it, as `zend_throw_exception_internal` hands it with
/// `EG(current_execute_data)` empty, and the throwable names no file.
///
/// ```php
/// set_exception_handler(function (Throwable $e) {
///     echo $e::class, ": ", $e->getMessage(), " in ", $e->getFile(), ":", $e->getLine(), "\n";
/// });
/// $a = zend_test_create_throwing_resource();
/// $b = zend_test_create_throwing_resource();
/// echo "end\n";
/// ```
/// ```expect
/// end
/// Exception: Throwing resource destructor called in [no active file]:0
/// Exception: Throwing resource destructor called in [no active file]:0
/// ```
///
/// A handler that throws is the uncaught fatal, and php's
/// `zend_bailout` out of it leaves the handler taken down: the next resource's throwable
/// is rendered uncaught, and the walk goes on after each.
///
/// ```php
/// set_exception_handler(function (Throwable $e) {
///     echo "handler: ", $e->getMessage(), "\n";
///     throw new Exception('Caught');
/// });
/// $a = zend_test_create_throwing_resource();
/// $b = zend_test_create_throwing_resource();
/// ```
/// ```expect exit=255
/// handler: Throwing resource destructor called
///
/// Fatal error: Uncaught Exception: Caught in %s:4
/// Stack trace:
/// #0 [internal function]: {closure:%s:2}(Object(Exception))
/// #1 {main}
///   thrown in %s on line 4
///
/// Fatal error: Uncaught Exception: Throwing resource destructor called in [no active file]:0
/// Stack trace:
/// #0 {main}
///   thrown in [no active file] on line 0
/// ```
///
/// Released with a returning or an unwinding frame's variables, it is
/// raised with the caller current, as `zend_leave_helper` moves
/// `EG(current_execute_data)` before `i_free_compiled_variables`: the
/// caller's line, and the frame left off the trace.
///
/// ```php
/// function f() { $r = zend_test_create_throwing_resource(); return 1; }
/// function u() { $r = zend_test_create_throwing_resource(); throw new LogicException("first"); }
/// try { f(); } catch (Exception $e) { echo $e->getLine(), " ", $e->getTraceAsString(), "\n"; }
/// try { u(); } catch (Exception $e) { echo $e->getLine(), " ", $e->getTraceAsString(), " ", $e->getPrevious()->getLine(), "\n"; }
/// ```
/// ```expect
/// 4 #0 {main}
/// 5 #0 {main} 3
/// ```
///
/// Released by the unwinding of an `exit`, it is given up: what is
/// unwinding is not replaced by a different throwable, and the request
/// ends as the exit ends it, its shutdown functions served.
///
/// ```php
/// function f() { $r = zend_test_create_throwing_resource(); exit("x\n"); }
/// register_shutdown_function(function () { echo "shutdown\n"; });
/// f();
/// ```
/// ```expect
/// x
/// shutdown
/// ```
fn throw_from_dtor(req: &mut Request, _resource: Resource) -> Result<(), Thrown> {
    Err(throw(
        req,
        "Exception",
        "Throwing resource destructor called",
    ))
}

/// `get_open_basedir(): ?string`, `PG(open_basedir)`: the list the
/// guard reads, expanded where `ini_set()` set it, beside the value as
/// set that `ini_get()` answers.
///
/// ```php
/// var_dump(get_open_basedir());
/// ini_set('open_basedir', __DIR__);
/// var_dump(get_open_basedir() === __DIR__);
/// ini_set('open_basedir', __DIR__ . '/./a/..');
/// var_dump(get_open_basedir() === __DIR__, ini_get('open_basedir') === __DIR__ . '/./a/..');
/// ```
/// ```expect
/// string(0) ""
/// bool(true)
/// bool(true)
/// bool(true)
/// ```
#[php_function]
fn get_open_basedir(req: &mut Request) -> Result<Option<Owner<PhpString>>, Thrown> {
    match req.open_basedir() {
        Some(dir) => {
            let dir = dir.to_vec();
            PhpString::new(req, &dir).map(Some)
        }
        None => Ok(None),
    }
}

/// `zend_test_cast_fread($stream): void`: ten kilobytes read from the
/// stream as `fread` reads the `FILE *` `php_stream_cast` makes of it,
/// so a user wrapper's `stream_read` is asked for them.
///
/// ```php
/// $f = fopen('php://memory', 'w+');
/// fwrite($f, "abc");
/// rewind($f);
/// zend_test_cast_fread($f);
/// var_dump(feof($f));
/// ```
/// ```expect
/// bool(true)
/// ```
#[php_function]
fn zend_test_cast_fread(req: &mut Request, stream: Resource) -> Result<(), Thrown> {
    let Some(stream) = Stream::from_value(req, &stream.as_value()) else {
        return Ok(());
    };
    if let Some(read) = stream.read(req, 10240)? {
        read.give_up(req);
    }
    Ok(())
}

/// `zend_test_compile_to_ast(string $str): string`: the source parsed
/// and php's text of its tree, `zend_ast_export`.
///
/// ```php
/// echo zend_test_compile_to_ast("<?php\nnamespace N;\nuse A\\B as C;\n/** doc */\n#[A] function f( int \$a ): int { return \$a+1; }\nconst C = [1,'x'=>2];\nenum E: int { case One = 1; }\necho f(C);");
/// ```
/// ```expect
/// namespace N;
/// use A\B as C;
/// #[A]
/// function f(int $a): int {
///     return $a + 1;
/// }
///
/// const C = [1, 'x' => 2];
/// enum E: int {
///     case One = 1;
/// }
///
/// echo f(C);
/// ```
#[php_function]
fn zend_test_compile_to_ast(req: &mut Request, str: PhpString) -> Result<Owner<PhpString>, Thrown> {
    let exported = req.export_tree(str.as_bytes())?;
    PhpString::new(req, &exported)
}

/// `zend_test_gh19792(): void`, `@compile-time-eval`: a warning, then
/// an `Error`.
///
/// ```php
/// try { zend_test_gh19792(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Warning: a warning in %s on line 2
/// an exception
/// ```
#[php_function]
fn zend_test_gh19792(req: &mut Request) -> Result<(), Thrown> {
    warning(req, "a warning");
    Err(throw_error(req, "Error", "an exception"))
}

/// `zend_test_internal_dnf_arguments()`, taking and returning
/// `Iterator|(Traversable&Countable)`, the type laid on by [`dnf`] and
/// held by the body, as php's holds it.
///
/// ```php
/// var_dump(zend_test_internal_dnf_arguments(new ArrayIterator([])) instanceof ArrayIterator);
/// try { zend_test_internal_dnf_arguments(new stdClass); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// zend_test_internal_dnf_arguments(): Argument #1 ($arg) must be of type Iterator|(Traversable&Countable), stdClass given
/// ```
#[php_function]
fn zend_test_internal_dnf_arguments(
    req: &mut Request,
    arg: PhpObject,
) -> Result<PhpObject, Thrown> {
    let mut is = |name: &[u8]| -> Result<bool, Thrown> {
        Ok(lookup_class(req, name)?.is_some_and(|class| arg.instance_of(req, class)))
    };
    if is(b"Iterator")? || (is(b"Traversable")? && is(b"Countable")?) {
        return Ok(arg);
    }
    let text = format!(
        "be of type Iterator|(Traversable&Countable), {} given",
        req.class_name(arg.class())
    );
    Err(arg_type_error(req, 1, &text))
}
