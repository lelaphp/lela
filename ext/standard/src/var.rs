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

use php_ext::coerce::to_string;
use php_ext::decl::{ClassKind, ConstExpr, EnumBacking, FunctionDecl, Parameter, Type};
use php_ext::raising::Thrown;
use php_ext::request::Request;
use php_ext::values::{Key, PhpString, Value};

use crate::output::Entry;
use crate::{argument_count_between, bool_argument, declared, parameter};

/// `PRINT_ZVAL_INDENT` (`Z/Zend/zend.c:80`): the four spaces a level of
/// the rendering steps in by, taken twice between a key and the array
/// its value opens.
const INDENT: usize = 4;

/// The natives declared here, paired with their entries, with php's
/// arity and parameter names, so `ReflectionFunction::__toString`
/// renders them as the oracle does.
///
/// ```php
/// echo new ReflectionFunction('print_r');
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
/// ```
pub(crate) fn natives() -> Vec<(FunctionDecl, Entry)> {
    vec![(
        declared(
            b"print_r",
            &[
                parameter(b"value", Type::Mixed),
                Parameter {
                    default: Some(ConstExpr::Bool(false)),
                    ..parameter(b"return", Type::Bool)
                },
            ],
            Type::Union(vec![Type::String, Type::True]),
        ),
        print_r,
    )]
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
/// `$return = true` hands the same bytes back as a string rather than
/// writing them, which is what a caller that renders into a page does.
///
/// ```php
/// class C { private $p = 1; }
/// var_dump(print_r(new C, true), print_r([1 => [2]], true));
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
fn render(
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
        let rows: Vec<(Key, Value)> = object
            .debug_props(req)?
            .into_iter()
            .map(|(name, value)| (Key::from_string(name), value))
            .collect();
        object.protect();
        let rendered = hash(req, out, &rows, indent, true);
        object.unprotect();
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

/// `zend_unmangle_property_name_ex` as `print_hash` reads it: a name
/// mangled `\0*\0p` or `\0C\0p` is the name with `:protected` or
/// `:C:private` after it, and anything else is the name as it stands,
/// a public property's and a dynamic one's.
fn unmangled(out: &mut Vec<u8>, key: &[u8]) {
    let Some(rest) = key.strip_prefix(b"\0") else {
        out.extend_from_slice(key);
        return;
    };
    let Some(end) = rest.iter().position(|b| *b == 0) else {
        out.extend_from_slice(key);
        return;
    };
    let (class, name) = (&rest[..end], &rest[end + 1..]);
    out.extend_from_slice(name);
    if class == b"*" {
        out.extend_from_slice(b":protected");
    } else {
        out.push(b':');
        out.extend_from_slice(class);
        out.extend_from_slice(b":private");
    }
}
