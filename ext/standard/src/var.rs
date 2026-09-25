//! The dumpers of `var.c` and the renderer under them. `print_r` is
//! `ext/standard`'s native (`PHP_FUNCTION(print_r)`,
//! `Z/ext/standard/basic_functions.c:2093`); what it renders with is
//! Zend's, `zend_print_zval_r_to_buf` and `print_hash`
//! (`Z/Zend/zend.c:380-612`), where 8.5 keeps the renderer php's
//! `ReflectionExtension::info` and `var.c` both call. The rendering is
//! the same walk `php_var_dump` does over the same rows and lays them
//! out differently: `Array\n(\n    [k] => v\n)\n` and `Class Object\n`
//! over four spaces of indent per level, a property's visibility in its
//! name rather than beside it, and no type or length anywhere.

use php_ext::abi::PropertyPurpose;
use php_ext::coerce::{Exponent, float_format, to_string};
use php_ext::decl::{
    ClassKind, ConstExpr, EnumBacking, FunctionDecl, Parameter, ParameterDefault, Type,
};
use php_ext::raising::{Thrown, warning};
use php_ext::request::Request;
use php_ext::values::{Key, PhpString, Value};

use crate::output::Entry;
use crate::{argument_count_between, bool_argument, declared, parameter};

mod debug;
mod serialize;
mod unserialize;

pub use serialize::Serializer;
pub use unserialize::Unserializer;

/// `PHP_IC_ENTRY`'s name, the class an object of a class `unserialize()`
/// could not load becomes.
pub(crate) const INCOMPLETE_CLASS: &[u8] = b"__PHP_Incomplete_Class";

/// `MAGIC_MEMBER`, the property an incomplete object keeps its class's
/// name in.
pub(crate) const MAGIC_MEMBER: &[u8] = b"__PHP_Incomplete_Class_Name";

/// The object's class name as the linked table spells it.
fn class_name(req: &Request, object: php_ext::values::PhpObject) -> Vec<u8> {
    req.class_decl(object.class())
        .map(|decl| decl.name.0.to_vec())
        .unwrap_or_default()
}

/// Whether the class, or a class it extends, declares `method`, compared
/// without case as php's function table is keyed.
fn has_method(req: &mut Request, class: php_ext::values::ClassId, method: &[u8]) -> bool {
    let mut at = Some(class);
    for _ in 0..64 {
        let Some(here) = at else { return false };
        let Some(decl) = req.class_decl(here) else {
            return false;
        };
        if decl
            .methods
            .iter()
            .any(|m| !m.is_abstract && m.function.name.0.eq_ignore_ascii_case(method))
        {
            return true;
        }
        let parent = decl.parent.as_ref().map(|p| p.0.to_vec());
        at = match parent {
            Some(parent) => php_ext::call::lookup_class(req, &parent).ok().flatten(),
            None => None,
        };
    }
    false
}

/// Whether the object is an enum case.
fn is_enum(req: &Request, object: php_ext::values::PhpObject) -> bool {
    is_enum_class(req, object.class())
}

fn is_enum_class(req: &Request, class: php_ext::values::ClassId) -> bool {
    req.class_decl(class)
        .is_some_and(|decl| matches!(decl.kind, ClassKind::Enum))
}

/// `__PHP_Incomplete_Class`, what `unserialize()` makes an object of a
/// class it cannot load: no member, every property dynamic.
///
/// ```php
/// $o = unserialize('O:3:"Foo":1:{s:1:"a";i:1;}'); var_dump($o, get_class($o), serialize($o));
/// ```
/// ```expect
/// object(__PHP_Incomplete_Class)#1 (2) {
///   ["__PHP_Incomplete_Class_Name"]=>
///   string(3) "Foo"
///   ["a"]=>
///   int(1)
/// }
/// string(22) "__PHP_Incomplete_Class"
/// string(26) "O:3:"Foo":1:{s:1:"a";i:1;}"
/// ```
pub(crate) fn classes() -> Vec<(
    php_ext::decl::ClassDecl,
    Vec<(php_ext::decl::MethodDecl, Entry)>,
)> {
    use php_ext::decl::{ClassDecl, ClassFlags, DynamicProperties, EnumBacking, Name};
    vec![(
        ClassDecl {
            name: Name(INCOMPLETE_CLASS.into()),
            kind: ClassKind::Class,
            flags: ClassFlags {
                is_final: true,
                dynamic_properties: DynamicProperties::Allowed,
                ..ClassFlags::default()
            },
            parent: None,
            interfaces: Vec::new(),
            traits: Vec::new(),
            backing: EnumBacking::Pure,
            constants: Vec::new(),
            properties: Vec::new(),
            methods: Vec::new(),
            attributes: Vec::new(),
            doc_comment: None,
            file: Name(Box::new([])),
            line: 0,
            end_line: 0,
        },
        Vec::new(),
    )]
}

/// The ini entries the unserializer reads, `basic_functions.c`'s
/// `unserialize_callback_func` and `unserialize_max_depth`.
///
/// ```php
/// var_dump(ini_get("unserialize_max_depth"), ini_get("unserialize_callback_func"));
/// ```
/// ```expect
/// string(4) "4096"
/// string(0) ""
/// ```
pub(crate) fn ini() -> Vec<php_ext::decl::IniDecl> {
    let entry = |name: &[u8], default: &[u8]| php_ext::decl::IniDecl {
        name: php_ext::decl::Name(name.into()),
        default: default.into(),
        user: true,
        per_dir: true,
        system: true,
    };
    vec![
        entry(b"unserialize_callback_func", b""),
        entry(b"unserialize_max_depth", b"4096"),
    ]
}

/// `PRINT_ZVAL_INDENT` (`Z/Zend/zend.c:80`): the four spaces a level of
/// the rendering steps in by, taken twice between a key and the array
/// its value opens.
const INDENT: usize = 4;

/// The natives declared here, paired with their entries, with php's
/// arity and parameter names, so `ReflectionFunction::__toString`
/// renders them as the oracle does.
///
/// ```php
/// echo new ReflectionFunction('print_r'), new ReflectionFunction('var_export');
/// ```
/// ```expect
/// Function [ <internal:standard> function print_r ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <required> mixed $value ]
///     Parameter #1 [ <optional> bool $return = false ]
///   }
///   - Return [ string|true ]
/// }
/// Function [ <internal:standard> function var_export ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <required> mixed $value ]
///     Parameter #1 [ <optional> bool $return = false ]
///   }
///   - Return [ ?string ]
/// }
/// ```
pub(crate) fn natives() -> Vec<(FunctionDecl, Entry)> {
    vec![
        (
            declared(
                b"print_r",
                &[
                    parameter(b"value", Type::Mixed),
                    Parameter {
                        default: ParameterDefault::Expr(ConstExpr::Bool(false)),
                        ..parameter(b"return", Type::Bool)
                    },
                ],
                Type::Union(vec![Type::String, Type::True]),
            ),
            print_r,
        ),
        (
            declared(
                b"var_export",
                &[
                    parameter(b"value", Type::Mixed),
                    Parameter {
                        default: ParameterDefault::Expr(ConstExpr::Bool(false)),
                        ..parameter(b"return", Type::Bool)
                    },
                ],
                Type::Nullable(Box::new(Type::String)),
            ),
            var_export,
        ),
        (
            declared(
                b"debug_zval_dump",
                &[
                    parameter(b"value", Type::Mixed),
                    Parameter {
                        variadic: true,
                        ..parameter(b"values", Type::Mixed)
                    },
                ],
                Type::Void,
            ),
            debug::debug_zval_dump,
        ),
        (
            declared(
                b"serialize",
                &[parameter(b"value", Type::Mixed)],
                Type::String,
            ),
            serialize::serialize,
        ),
        (
            declared(
                b"unserialize",
                &[
                    parameter(b"data", Type::String),
                    Parameter {
                        default: ParameterDefault::Expr(ConstExpr::Array(Vec::new())),
                        ..parameter(b"options", Type::Array)
                    },
                ],
                Type::Mixed,
            ),
            unserialize::unserialize,
        ),
    ]
}

/// `print_r(mixed $value, bool $return = false): string|true`, php's
/// `PHP_FUNCTION(print_r)`: the rendering written where `echo` writes it
/// and `true` answered, or, under `$return`, the rendering itself and
/// nothing written. A scalar is what `zval_get_string` makes of it, so
/// `null` and `false` render as nothing at all and a float renders at
/// `precision` rather than at `serialize_precision`.
///
/// ```php
/// print_r("s"); print_r(1); print_r(1.5); print_r(true); print_r(false); print_r(null); echo "|\n";
/// var_dump(print_r(0.1, true), print_r("x", false));
/// ```
/// ```expect
/// s11.51|
/// xstring(3) "0.1"
/// bool(true)
/// ```
///
/// An array is `Array` and its elements under it, each key in brackets
/// and each value four spaces further in, and an element that opens an
/// array of its own leaves the blank line php's `print_hash` writes
/// after the `)` of a nested one.
///
/// ```php
/// print_r([1, "two" => [3, [], "k" => "v"]]);
/// ```
/// ```expect
/// Array
/// (
///     [0] => 1
///     [two] => Array
///         (
///             [0] => 3
///             [1] => Array
///                 (
///                 )
///
///             [k] => v
///         )
///
/// )
/// ```
///
/// An object is its class and `Object`, its properties under it with the
/// visibility php mangles into the name unmangled back out,
/// `[p:protected]` and `[p:C:private]` naming the class that declared
/// the private one, an inherited property before the class's own, and
/// the dynamic ones after. A value that holds itself is ` *RECURSION*`
/// and not an unbounded walk.
///
/// ```php
/// class A { public $pub = 1; protected $prot = "p"; private $priv = 2.5; }
/// class B extends A { public $own = [1]; }
/// $b = new B; $b->dyn = "d"; $b->self = $b;
/// print_r($b);
/// print_r(new stdClass);
/// ```
/// ```expect
/// Deprecated: Creation of dynamic property B::$dyn is deprecated in %s on line 4
///
/// Deprecated: Creation of dynamic property B::$self is deprecated in %s on line 4
/// B Object
/// (
///     [pub] => 1
///     [prot:protected] => p
///     [priv:A:private] => 2.5
///     [own] => Array
///         (
///             [0] => 1
///         )
///
///     [dyn] => d
///     [self] => B Object
///  *RECURSION*
/// )
/// stdClass Object
/// (
/// )
/// ```
///
/// The array half of the guard is `GC_PROTECTED` on the array itself,
/// which a program reaches by binding an entry to the variable that
/// holds the array (`$a[] = &$a`): the entry is the cell the CV holds,
/// the cell holds the array, and the array is the one the walk is
/// already inside. php writes the marker after the `Array` it has
/// already printed, so it lands on the next line with one space before
/// it and the rows never open.
///
/// ```php
/// $a = []; $a[] = &$a; print_r($a);
/// $b = [1]; $b["self"] = &$b; print_r($b);
/// ```
/// ```expect
/// Array
/// (
///     [0] => Array
///  *RECURSION*
/// )
/// Array
/// (
///     [0] => 1
///     [self] => Array
///  *RECURSION*
/// )
/// ```
///
/// The object half is the same mark on the object itself, which the one
/// line `$a->prop = $a` sets up, and the rows of the second visit never
/// open.
///
/// ```php
/// $a = new stdClass; $a->prop = $a; print_r($a);
/// ```
/// ```expect
/// stdClass Object
/// (
///     [prop] => stdClass Object
///  *RECURSION*
/// )
/// ```
///
/// `$return = true` hands the same bytes back as a string rather than
/// writing them, which is what a caller that renders into a page does.
///
/// ```php
/// class C { private $p = 1; }
/// var_dump(print_r(new C, true), print_r([1 => [2]], true));
/// $v = ['k' => [1, 2]];
/// print_r($v);
/// var_dump(print_r($v, true) === "Array\n(\n    [k] => Array\n        (\n            [0] => 1\n            [1] => 2\n        )\n\n)\n");
/// ```
/// ```expect
/// string(36) "C Object
/// (
///     [p:C:private] => 1
/// )
/// "
/// string(69) "Array
/// (
///     [1] => Array
///         (
///             [0] => 2
///         )
///
/// )
/// "
/// Array
/// (
///     [k] => Array
///         (
///             [0] => 1
///             [1] => 2
///         )
///
/// )
/// bool(true)
/// ```
///
/// ```php
/// print_r();
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ArgumentCountError: print_r() expects at least 1 argument, 0 given in %s:2
/// Stack trace:
/// #0 %s(2): print_r()
/// #1 {main}
///   thrown in %s on line 2
/// ```
fn print_r(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 2)?;
    let value = req.argument(1).unwrap_or_default();
    let returned = if req.argument_count() == 2 {
        bool_argument(req, 2)?
    } else {
        false
    };
    let mut out = Vec::new();
    render(req, &mut out, &value, 0)?;
    if returned {
        return Ok(Value::string(PhpString::new(req, &out)?));
    }
    req.write(&out);
    Ok(Value::bool(true))
}

/// One value as `zend_print_zval_r_to_buf(buf, expr, indent)` renders
/// it: an array and an object open their name and hand their rows to
/// [`hash`], a reference is what the cell holds, and everything else is
/// `zval_get_string`, which is `1` for `true` and nothing at all for
/// `false` and `null`.
///
/// The array and the object arms carry php's recursion guard, the mark
/// set on what the walk is inside and cleared on the way out, and a
/// value already marked is ` *RECURSION*` where its rows would have
/// stood — after the `Array` or the `Object` php has already written,
/// which is why the marker lands on the next line with one space before
/// it. Without the guard a value that holds itself is unbounded
/// recursion, which is a panic reaching a request (design §10).
pub(super) fn render(
    req: &mut Request,
    out: &mut Vec<u8>,
    value: &Value,
    indent: usize,
) -> Result<(), Thrown> {
    // `case IS_REFERENCE`: php renders what the cell holds, at this
    // level and with no mark of its own.
    let value = value.deref();
    if let Some(array) = value.as_array() {
        out.extend_from_slice(b"Array\n");
        // `GC_IS_RECURSIVE` and `GC_PROTECT_RECURSION`, which php takes
        // only where the array is refcounted; an immutable one is marked
        // by neither, since nothing can make one hold itself.
        if array.is_recursive() {
            out.extend_from_slice(b" *RECURSION*");
            return Ok(());
        }
        array.protect();
        let rows = array.snapshot();
        let rendered = hash(req, out, &rows, indent, false);
        array.unprotect();
        return rendered;
    }
    if let Some(object) = value.as_object() {
        let Some(decl) = req.class_decl(object.class()) else {
            return Ok(());
        };
        let (name, kind, backing) = (decl.name.0.to_vec(), decl.kind, decl.backing);
        out.extend_from_slice(&name);
        // `ZEND_ACC_ENUM`: a case is `Suit Enum:string` and its `name`
        // and `value` under it, where an object is `C Object`.
        if matches!(kind, ClassKind::Enum) {
            out.extend_from_slice(b" Enum");
            match backing {
                EnumBacking::Int => out.extend_from_slice(b":int"),
                EnumBacking::String => out.extend_from_slice(b":string"),
                EnumBacking::Pure => {}
            }
            out.push(b'\n');
        } else {
            out.extend_from_slice(b" Object\n");
        }
        if object.is_recursive() {
            out.extend_from_slice(b" *RECURSION*");
            return Ok(());
        }
        // `zend_get_properties_for(struc, ZEND_PROP_PURPOSE_DEBUG)`: the
        // table a class shows in place of its properties, which is what
        // `__debugInfo` and a `Closure` show, and the members otherwise.
        // The table has one owner and is given up once its rows are
        // rendered, php's `zend_release_properties`.
        let own = object.debug_info(req)?;
        let rows: Vec<(Key, Value)> = match own {
            Some(own) => own.snapshot(),
            None => object
                .props_for(req, PropertyPurpose::Debug)?
                .into_iter()
                .map(|(name, value)| (Key::from_string(name), value))
                .collect(),
        };
        object.protect();
        let rendered = hash(req, out, &rows, indent, true);
        object.unprotect();
        if let Some(own) = own {
            req.release(Value::array(own));
        }
        return rendered;
    }
    // `zval_get_string_func`, whose string is the caller's to release:
    // the decimal of an int, the bytes of a string, `1` for `true`, the
    // empty string for `false` and for `null`, and a resource's
    // `Resource id #N`.
    let string = to_string(req, &value)?;
    out.extend_from_slice(string.as_bytes());
    req.release(Value::string(string));
    Ok(())
}

/// `print_hash(buf, ht, indent, is_object)`: the rows between a `(` and
/// a `)` at this level, each `[key] => ` one step in and its value one
/// step further, with the newline php writes after every row. An
/// object's key is `zend_unmangle_property_name_ex`'d back out, so a
/// protected property is `[p:protected]` and a private one
/// `[p:C:private]` under the class that declared it.
fn hash(
    req: &mut Request,
    out: &mut Vec<u8>,
    rows: &[(Key, Value)],
    indent: usize,
    is_object: bool,
) -> Result<(), Thrown> {
    out.extend(std::iter::repeat_n(b' ', indent));
    out.extend_from_slice(b"(\n");
    for (key, element) in rows {
        out.extend(std::iter::repeat_n(b' ', indent + INDENT));
        out.push(b'[');
        match key {
            Key::Int(n) => out.extend_from_slice(n.to_string().as_bytes()),
            Key::Str(s) if is_object => unmangled(out, s.as_bytes()),
            Key::Str(s) => out.extend_from_slice(s.as_bytes()),
        }
        out.extend_from_slice(b"] => ");
        render(req, out, element, indent + INDENT + INDENT)?;
        out.push(b'\n');
    }
    out.extend(std::iter::repeat_n(b' ', indent));
    out.extend_from_slice(b")\n");
    Ok(())
}

/// `zend_unmangle_property_name_ex`: the class a mangled name carries
/// and the name itself, `*` for the class of a protected property and
/// the declaring class for a private one; a name that is not mangled,
/// a public property's and a dynamic one's, carries no class.
fn unmangle(key: &[u8]) -> (Option<&[u8]>, &[u8]) {
    let Some(rest) = key.strip_prefix(b"\0") else {
        return (None, key);
    };
    let Some(end) = rest.iter().position(|b| *b == 0) else {
        return (None, key);
    };
    (Some(&rest[..end]), &rest[end + 1..])
}

/// The name `print_hash` prints for an object's row: the unmangled name,
/// and the visibility php spells after it where the key carried a class.
fn unmangled(out: &mut Vec<u8>, key: &[u8]) {
    let (class, name) = unmangle(key);
    out.extend_from_slice(name);
    match class {
        None => {}
        Some(b"*") => out.extend_from_slice(b":protected"),
        Some(class) => {
            out.push(b':');
            out.extend_from_slice(class);
            out.extend_from_slice(b":private");
        }
    }
}

/// `var_export(mixed $value, bool $return = false): ?string`, php's
/// `PHP_FUNCTION(var_export)` over `php_var_export_ex`
/// (`Z/ext/standard/var.c:516`): the value as PHP source that evaluates
/// back to it, written where `echo` writes it and `null` answered, or,
/// under `$return`, the source itself. A string is quoted with `'` and
/// `\` escaped as `php_addcslashes` escapes them and a NUL spliced out
/// as `' . "\0" . '`; a float is `serialize_precision` with `.0` after a
/// whole one, so what is printed reads back as the same double;
/// `PHP_INT_MIN` is `-9223372036854775807-1`, since the number written
/// whole would read back as a float.
///
/// ```php
/// var_export(1); echo "|"; var_export(1.0); echo "|"; var_export(0.1); echo "|"; var_export(true); echo "|"; var_export(false); echo "|"; var_export(null); echo "|"; var_export("it's\\a"); echo "|"; var_export(PHP_INT_MIN); echo "|"; var_export("a\0b");
/// ```
/// ```expect
/// 1|1.0|0.1|true|false|NULL|'it\'s\\a'|-9223372036854775807-1|'a' . "\0" . 'b'
/// ```
///
/// An array is `array (` and its elements, each key two spaces in and
/// each nested array opening on a line of its own; an object is
/// `\Class::__set_state(array(` with its properties three spaces in
/// under their unmangled names, and a `stdClass` is `(object) array(`,
/// which is what php writes for the one class with no `__set_state`.
///
/// ```php
/// var_export([1, 'k' => [2, 3], 'x' => 'y']); echo "\n";
/// class A { public $a = 1; protected $b = 2; private $c = 3; }
/// var_export(new A); echo "\n";
/// var_export(new stdClass);
/// ```
/// ```expect
/// array (
///   0 => 1,
///   'k' =>%w
///   array (
///     0 => 2,
///     1 => 3,
///   ),
///   'x' => 'y',
/// )
/// \A::__set_state(array(
///    'a' => 1,
///    'b' => 2,
///    'c' => 3,
/// ))
/// (object) array(
/// )
/// ```
///
/// A value that holds itself is where the two dumpers part, and the
/// difference is php's: `print_r` prints ` *RECURSION*` and goes on,
/// `var_export` prints `NULL` in its place and raises the warning
/// `php_var_export_ex` raises there, pinned below, since what it writes
/// has to be source that evaluates.
///
/// ```php
/// $o = new stdClass; $o->p = $o; var_export($o); echo "\n";
/// $a = [1]; $a[] = &$a; var_export($a); echo "\n";
/// var_dump(var_export([1], true), var_export(1, true));
/// ```
/// ```expect
/// Warning: var_export does not handle circular references in %s on line 2
/// (object) array(
///    'p' => NULL,
/// )
///
/// Warning: var_export does not handle circular references in %s on line 3
/// array (
///   0 => 1,
///   1 => NULL,
/// )
/// string(19) "array (
///   0 => 1,
/// )"
/// string(1) "1"
/// ```
fn var_export(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 2)?;
    let value = req.argument(1).unwrap_or_default();
    let returned = if req.argument_count() == 2 {
        bool_argument(req, 2)?
    } else {
        false
    };
    let mut out = Vec::new();
    export(req, &mut out, &value, 1)?;
    if returned {
        return Ok(Value::string(PhpString::new(req, &out)?));
    }
    req.write(&out);
    Ok(Value::null())
}

/// One value as `php_var_export_ex(struc, level, buf)` writes it, and a
/// reference as what the cell holds, php's `goto again`. `level` is
/// php's: the top level is one, an array's elements are exported at two
/// more than their array's, and what is written before the `array (` or
/// the `(object) array(` of a nested one is the newline and the indent
/// php only writes past level one.
///
/// Both compound arms carry php's `EXPORT` guard rather than the
/// `DEBUG` one `print_r` takes, and answer a value already inside the
/// walk with `NULL` and the warning, which is what keeps the source it
/// writes evaluable.
fn export(req: &mut Request, out: &mut Vec<u8>, value: &Value, level: usize) -> Result<(), Thrown> {
    let value = value.deref();
    if let Some(b) = value.as_bool() {
        out.extend_from_slice(if b { b"true" } else { b"false" });
    } else if let Some(n) = value.as_int() {
        // `ZEND_LONG_MIN` written whole would read back as a float.
        if n == i64::MIN {
            out.extend_from_slice(format!("{}-1", i64::MIN + 1).as_bytes());
        } else {
            out.extend_from_slice(n.to_string().as_bytes());
        }
    } else if let Some(f) = value.as_float() {
        // `smart_str_append_double` with `zero_fraction`: the text, and
        // `.0` after a finite one that carries no point of its own.
        let text = float_format(f, req.serialize_precision(), Exponent::Upper);
        out.extend_from_slice(text.as_bytes());
        if f.is_finite() && !text.contains('.') {
            out.extend_from_slice(b".0");
        }
    } else if let Some(s) = value.as_php_string() {
        quoted(out, s.as_bytes());
    } else if let Some(array) = value.as_array() {
        if array.is_recursive() {
            out.extend_from_slice(b"NULL");
            circular(req);
            return Ok(());
        }
        array.protect();
        if level > 1 {
            out.push(b'\n');
            out.extend(std::iter::repeat_n(b' ', level - 1));
        }
        out.extend_from_slice(b"array (\n");
        let mut written = Ok(());
        for (key, element) in array.snapshot() {
            out.extend(std::iter::repeat_n(b' ', level + 1));
            match key {
                Key::Int(n) => out.extend_from_slice(n.to_string().as_bytes()),
                Key::Str(s) => quoted(out, s.as_bytes()),
            }
            out.extend_from_slice(b" => ");
            written = export(req, out, &element, level + 2);
            if written.is_err() {
                break;
            }
            out.extend_from_slice(b",\n");
        }
        array.unprotect();
        written?;
        if level > 1 {
            out.extend(std::iter::repeat_n(b' ', level - 1));
        }
        out.push(b')');
    } else if let Some(object) = value.as_object() {
        return export_object(req, out, *object, level);
    } else {
        out.extend_from_slice(b"NULL");
    }
    Ok(())
}

/// `php_var_export_ex`'s `IS_OBJECT` arm: the guard first, so a class
/// whose rows are built on the way out is not asked for them inside its
/// own cycle; then `\Class::__set_state(array(` and each property under
/// its unmangled name, `(object) array(` for the one class with no
/// `__set_state`, and `\Suit::Hearts` for an enum case, which carries
/// neither its properties nor the closing pair.
fn export_object(
    req: &mut Request,
    out: &mut Vec<u8>,
    object: php_ext::values::PhpObject,
    level: usize,
) -> Result<(), Thrown> {
    if object.is_recursive() {
        out.extend_from_slice(b"NULL");
        circular(req);
        return Ok(());
    }
    object.protect();
    let rows = object.props_for(req, PropertyPurpose::VarExport);
    let Some(decl) = req.class_decl(object.class()) else {
        object.unprotect();
        return Ok(());
    };
    let (name, is_enum) = (decl.name.0.to_vec(), matches!(decl.kind, ClassKind::Enum));
    let standard = name == b"stdClass";
    if level > 1 {
        out.push(b'\n');
        out.extend(std::iter::repeat_n(b' ', level - 1));
    }
    if standard {
        out.extend_from_slice(b"(object) array(\n");
    } else {
        out.push(b'\\');
        out.extend_from_slice(&name);
        if is_enum {
            out.extend_from_slice(b"::");
            let case = object
                .prop(req, b"name")
                .and_then(|name| name.as_str_bytes().map(<[u8]>::to_vec))
                .unwrap_or_default();
            out.extend_from_slice(&case);
        } else {
            out.extend_from_slice(b"::__set_state(array(\n");
        }
    }
    let mut written = Ok(());
    if !is_enum {
        for (key, element) in rows? {
            out.extend(std::iter::repeat_n(b' ', level + 2));
            let (_, name) = unmangle(key.as_bytes());
            out.push(b'\'');
            escaped(out, name);
            out.push(b'\'');
            out.extend_from_slice(b" => ");
            written = export(req, out, &element, level + 2);
            if written.is_err() {
                break;
            }
            out.extend_from_slice(b",\n");
        }
    }
    object.unprotect();
    written?;
    if level > 1 && !is_enum {
        out.extend(std::iter::repeat_n(b' ', level - 1));
    }
    if standard {
        out.push(b')');
    } else if !is_enum {
        out.extend_from_slice(b"))");
    }
    Ok(())
}

/// `var_export does not handle circular references`, the warning php
/// raises where the walk meets a value it is already inside.
fn circular(req: &mut Request) {
    warning(req, "var_export does not handle circular references");
}

/// A string as php writes it into exported source: `php_addcslashes`
/// over `'` and `\`, then `php_str_to_str` splicing a NUL out of the
/// quoted text as `' . "\0" . '`, between the quotes php puts around it.
fn quoted(out: &mut Vec<u8>, bytes: &[u8]) {
    out.push(b'\'');
    escaped(out, bytes);
    out.push(b'\'');
}

/// The escaping itself, which a property name takes without the NUL
/// splice php leaves to a value's string.
fn escaped(out: &mut Vec<u8>, bytes: &[u8]) {
    for byte in bytes {
        match byte {
            b'\'' | b'\\' => {
                out.push(b'\\');
                out.push(*byte);
            }
            0 => out.extend_from_slice(b"' . \"\\0\" . '"),
            _ => out.push(*byte),
        }
    }
}
