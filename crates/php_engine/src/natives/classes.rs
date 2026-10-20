//! The natives of `zend_builtin_functions.c` that read `EG(class_table)`
//! and an object's members: the called and the parent class, `is_a`
//! and `is_subclass_of`, the defaults and the members a scope may see,
//! the methods and properties a class has, the `*_exists` family, the
//! aliases `class_alias` adds, and the declared lists. Visibility is
//! measured against `zend_get_executed_scope`, which is the scope the
//! native was called from ([`Request::scope`]).

use php_macros::php_function;
use php_unit::decl::{Bare, ClassKind, Visibility};

use crate::api::abi::{
    OrFalse, Passed, PropertiesFor, PropertyPurpose, PropertyTest, Raw, Union, Untyped,
};
use crate::api::raising::{Thrown, arg_type_error, deprecated, none_pending, throw_error, warning};
use crate::api::request::{HookedRow, PropertyInfo, Request};
use crate::api::values::walk::{self, Table};
use crate::api::values::{ClassId, Key, Owner, PhpArray, PhpObject, PhpString, Value, ValueRef};

/// `zend_lookup_class_ex`: one leading `\` dropped, the table under
/// the name whatever its case, and the autoloaders where the table has
/// none and `autoload` asks for them, each handed the name past that
/// one `\`, which a second `\` stays in:
///
/// ```php
/// spl_autoload_register(function ($c) { echo "load [$c]\n"; });
/// var_dump(class_exists("\\\\stdClass"), class_exists("\\STDCLASS"), class_exists("\\\\Nope"));
/// var_dump(interface_exists("\\\\Countable"), class_exists("\\"), class_exists(""));
/// ```
/// ```expect
/// load [\stdClass]
/// load [\Nope]
/// bool(false)
/// bool(true)
/// bool(false)
/// load [\Countable]
/// load []
/// bool(false)
/// bool(false)
/// bool(false)
/// ```
pub(super) fn lookup_class(
    req: &mut Request,
    name: &[u8],
    autoload: bool,
) -> Result<Option<ClassId>, Thrown> {
    if autoload {
        return crate::api::call::autoload(req, name);
    }
    Ok(req.class_named(Bare::of(name)))
}

/// One `Z_PARAM_OBJ_OR_CLASS_NAME` of a native: an object's class, or
/// the class a string names, looked up with the autoloaders; anything
/// else, and a name no class has, is php's `TypeError`.
fn object_or_class_name(req: &mut Request, given: Value, position: u32) -> Result<ClassId, Thrown> {
    match given.as_ref() {
        ValueRef::Object(object) => return Ok(object.class()),
        ValueRef::Str(name) => {
            let name = name.as_bytes().to_vec();
            if let Some(class) = lookup_class(req, &name, true)? {
                return Ok(class);
            }
        }
        _ => {}
    }
    let tail = format!(
        "be an object or a valid class name, {} given",
        crate::ops::value_name(req, &given)
    );
    Err(arg_type_error(req, position, &tail))
}

/// `ce->name`: the declared name of a class, the string php keeps on the
/// class entry, one more owner ([`Request::declared_string`]).
fn class_name(req: &mut Request, class: ClassId) -> Result<Owner<PhpString>, Thrown> {
    let name = req.class_name_bytes(class);
    req.declared_string(&name)
}

/// `get_called_class(): string`, php's `ZEND_FUNCTION(get_called_class)`
/// (`Z/Zend/zend_builtin_functions.c:654`): the class `static` names
/// where the native was called, `zend_get_called_scope`, and php's
/// `Error` outside a class. php's compiler serves a call of no argument
/// with `ZEND_GET_CALLED_CLASS` (`zend_compile_func_get_called_class`);
/// what reaches the native is a call by a name the program holds and a
/// callback, whose frame the `Error`'s trace shows.
///
/// ```php
/// class A { static function f() { $g = 'get_called_class'; return $g(); } function g() { return call_user_func('get_called_class'); } }
/// class B extends A {}
/// var_dump(A::f(), B::f(), (new B)->g());
/// $g = 'get_called_class';
/// try { $g(); } catch (Error $e) { echo $e->getMessage(), " at ", count($e->getTrace()), "\n"; }
/// ```
/// ```expect
/// string(1) "A"
/// string(1) "B"
/// string(1) "B"
/// get_called_class() must be called from within a class at 1
/// ```
#[php_function]
fn get_called_class(req: &mut Request) -> Result<Owner<PhpString>, Thrown> {
    match req.called_scope() {
        Some(class) => class_name(req, class),
        None => Err(throw_error(
            req,
            "Error",
            "get_called_class() must be called from within a class",
        )),
    }
}

/// `get_parent_class(object|string $object_or_class = UNKNOWN):
/// string|false`, php's `ZEND_FUNCTION(get_parent_class)`
/// (`Z/Zend/zend_builtin_functions.c:670`): the parent's name, and
/// `false` at a root; with no argument the executed scope's, deprecated
/// since 8.3.
///
/// ```php
/// class A { function p() { return get_parent_class(); } }
/// class B extends A {}
/// var_dump(get_parent_class(new B), get_parent_class('b'), get_parent_class('A'), (new B)->p());
/// get_parent_class('Nope');
/// ```
/// ```expect exit=255
/// Deprecated: Calling get_parent_class() without arguments is deprecated in %s on line 2
/// string(1) "A"
/// string(1) "A"
/// bool(false)
/// bool(false)
///
/// Fatal error: Uncaught TypeError: get_parent_class(): Argument #1 ($object_or_class) must be an object or a valid class name, string given in %s:5
/// Stack trace:
/// #0 %s(5): get_parent_class('Nope')
/// #1 {main}
///   thrown in %s on line 5
/// ```
#[php_function]
fn get_parent_class(
    req: &mut Request,
    object_or_class: Passed<Raw<Union<PhpObject, PhpString>>>,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let class = if let Passed::Given(given) = object_or_class {
        Some(object_or_class_name(req, given.value(), 1)?)
    } else {
        deprecated(
            req,
            "Calling get_parent_class() without arguments is deprecated",
        );
        none_pending(req)?;
        req.scope()
    };
    match class.and_then(|class| req.class_parent(class)) {
        Some(parent) => class_name(req, parent).map(OrFalse::Value),
        None => Ok(OrFalse::False),
    }
}

/// `is_a_impl` (`Z/Zend/zend_builtin_functions.c:692`): the instance's
/// class, an object's or, where strings are allowed, the one a string
/// names; the class its own name when the names are the same bytes;
/// otherwise the class named looked up without the autoloaders and
/// `instanceof_function` over the two, which `is_subclass_of` refuses
/// for the class itself.
fn is_a_impl(
    req: &mut Request,
    (object, class_name, allow_string): (Value, PhpString, bool),
    only_subclass: bool,
) -> Result<bool, Thrown> {
    let instance = match object.as_ref() {
        ValueRef::Str(name) if allow_string => {
            let name = name.as_bytes().to_vec();
            match lookup_class(req, &name, true)? {
                Some(class) => class,
                None => return Ok(false),
            }
        }
        ValueRef::Object(object) => object.class(),
        _ => return Ok(false),
    };
    let wanted = class_name.as_bytes().to_vec();
    if !only_subclass && req.class_name_bytes(instance) == wanted {
        return Ok(true);
    }
    let answer = match lookup_class(req, &wanted, false)? {
        None => false,
        Some(class) if only_subclass && class == instance => false,
        Some(class) => req.instance_of(instance, class),
    };
    Ok(answer)
}

/// `is_subclass_of(mixed $object_or_class, string $class, bool
/// $allow_string = true): bool`, php's `ZEND_FUNCTION(is_subclass_of)`:
/// [`is_a_impl`] with the class itself refused and strings allowed by
/// default.
///
/// ```php
/// interface I {} class A implements I {} class B extends A {}
/// var_dump(is_subclass_of(new B, 'A'), is_subclass_of('B', 'a'), is_subclass_of('B', 'I'), is_subclass_of(new A, 'A'), is_subclass_of('B', 'A', false), is_subclass_of('Nope', 'A'), is_subclass_of(1, 'A'));
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(false)
/// ```
#[php_function]
fn is_subclass_of(
    req: &mut Request,
    object_or_class: Value,
    class: PhpString,
    #[php(default = true)] allow_string: bool,
) -> Result<bool, Thrown> {
    is_a_impl(req, (object_or_class, class, allow_string), true)
}

/// `is_a(mixed $object_or_class, string $class, bool $allow_string =
/// false): bool`, php's `ZEND_FUNCTION(is_a)`: [`is_a_impl`] with the
/// class itself taken and strings refused by default.
///
/// ```php
/// interface I {} class A implements I {} class B extends A {}
/// var_dump(is_a(new B, 'A'), is_a(new A, 'A'), is_a(new A, '\A'), is_a(new B, 'I'), is_a('B', 'A'), is_a('B', 'A', true), is_a(new A, 'B'));
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(false)
/// bool(true)
/// bool(false)
/// ```
#[php_function]
fn is_a(
    req: &mut Request,
    object_or_class: Value,
    class: PhpString,
    #[php(default = false)] allow_string: bool,
) -> Result<bool, Thrown> {
    is_a_impl(req, (object_or_class, class, allow_string), false)
}

/// `ce->properties_info` in its order, the linked record's property
/// rows (`LinkedClass::properties`): the class's own declarations,
/// instance and `static`, as written, then each one inherited from the
/// parent that the class does not declare again, in the parent's own
/// order, which is where `zend_do_inheritance` appends them. Each row
/// names the class that declared it, `property_info->ce`, and where
/// among its declarations it stands.
fn properties_info(req: &Request, class: ClassId) -> Vec<PropertyInfo> {
    req.linked_class(class).map_or_else(Vec::new, |record| {
        (record.properties.rows().iter())
            .map(|row| PropertyInfo {
                declaring: row.holder_in(class),
                at: row.at,
            })
            .collect()
    })
}

/// `zend_check_protected` or the class itself: whether a member of
/// `declaring` of that visibility is one `scope` sees.
fn visible(
    req: &Request,
    declaring: ClassId,
    visibility: Visibility,
    scope: Option<ClassId>,
) -> bool {
    match visibility {
        Visibility::Public => true,
        Visibility::Private => scope == Some(declaring),
        Visibility::Protected => scope == Some(declaring) || req.check_protected(declaring, scope),
    }
}

/// `get_class_vars(string $class): array`, php's
/// `ZEND_FUNCTION(get_class_vars)` (`Z/Zend/zend_builtin_functions.c:810`)
/// over `add_class_vars`: the defaults of the properties the executed
/// scope sees, the instance ones and then the `static` ones, each in
/// `ce->properties_info` order, a typed one with no default as `null`
/// and a constant expression evaluated. A `static` property's row is its
/// default, whatever the class's slot holds now.
///
/// ```php
/// class P { public $a = 1; private $p = 2; protected $q = 3; public static $s = [1]; public int $t; }
/// class C extends P { public $b = 4; public $p = 5; const K = 7; public $k = 3 + 4; private static $ps = 6; function vars() { return get_class_vars("C"); } }
/// C::$s = 9;
/// var_dump(get_class_vars("C"), (new C)->vars());
/// get_class_vars("Nope");
/// ```
/// ```expect exit=255
/// array(6) {
///   ["b"]=>
///   int(4)
///   ["p"]=>
///   int(5)
///   ["k"]=>
///   int(7)
///   ["a"]=>
///   int(1)
///   ["t"]=>
///   NULL
///   ["s"]=>
///   array(1) {
///     [0]=>
///     int(1)
///   }
/// }
/// array(8) {
///   ["b"]=>
///   int(4)
///   ["p"]=>
///   int(5)
///   ["k"]=>
///   int(7)
///   ["a"]=>
///   int(1)
///   ["q"]=>
///   int(3)
///   ["t"]=>
///   NULL
///   ["ps"]=>
///   int(6)
///   ["s"]=>
///   array(1) {
///     [0]=>
///     int(1)
///   }
/// }
///
/// Fatal error: Uncaught TypeError: get_class_vars(): Argument #1 ($class) must be a valid class name, Nope given in %s:6
/// Stack trace:
/// #0 %s(6): get_class_vars('Nope')
/// #1 {main}
///   thrown in %s on line 6
/// ```
///
/// A virtual property is left out whether it has hooks or, as
/// `DatePeriod`'s do, none:
///
/// ```php
/// class H { public $v { get => 1; } public $b = 2; }
/// var_dump(get_class_vars('H'), get_class_vars('DatePeriod'));
/// ```
/// ```expect
/// array(1) {
///   ["b"]=>
///   int(2)
/// }
/// array(0) {
/// }
/// ```
///
/// The rows are the linked record's: a parent's private property the
/// child does not declare again stands in the child's table and is
/// listed for the parent's scope, after the child's own; one the child
/// declares again is the child's row, in the child's place and under
/// the child's visibility:
///
/// ```php
/// class P { private $x = 'P x'; private $a = 'P a'; public $b = 'P b'; private static $s = 'P s'; static function vars($c) { return get_class_vars($c); } }
/// class C extends P { public $b = 'C b'; private $a = 'C a'; public $d = 'C d'; static function own() { return get_class_vars('C'); } }
/// var_dump(P::vars('C'), C::own());
/// ```
/// ```expect
/// array(4) {
///   ["b"]=>
///   string(3) "C b"
///   ["d"]=>
///   string(3) "C d"
///   ["x"]=>
///   string(3) "P x"
///   ["s"]=>
///   string(3) "P s"
/// }
/// array(3) {
///   ["b"]=>
///   string(3) "C b"
///   ["a"]=>
///   string(3) "C a"
///   ["d"]=>
///   string(3) "C d"
/// }
/// ```
#[php_function]
fn get_class_vars(req: &mut Request, class: PhpString) -> Result<Owner<PhpArray>, Thrown> {
    let bytes = class.as_bytes().to_vec();
    let Some(class) = lookup_class(req, &bytes, true)? else {
        let tail = format!(
            "be a valid class name, {} given",
            String::from_utf8_lossy(&bytes)
        );
        return Err(arg_type_error(req, 1, &tail));
    };
    let scope = req.scope();
    // A virtual property has no default to list, having no slot.
    let rows: Vec<(PropertyInfo, bool)> = properties_info(req, class)
        .into_iter()
        .filter_map(|info| {
            let p = req.property_decl(info)?;
            let listed = visible(req, info.declaring, p.visibility, scope)
                && !req.property_virtual(class, &p.name.0);
            listed.then_some((info, p.is_instance))
        })
        .collect();
    let mut array = PhpArray::new(req)?;
    for statics in [false, true] {
        for &(info, _) in rows.iter().filter(|(_, instance)| *instance != statics) {
            // The default expression alone is handed on past what writes
            // the request, so it is what is cloned.
            let default = req.property_decl(info).and_then(|p| p.default.clone());
            let value = match &default {
                Some(expr) => crate::api::call::eval_declared(req, expr, Some(info.declaring))?,
                None => Owner::null(),
            };
            let name = (req.property_decl(info)).map_or_else(Vec::new, |p| p.name.0.to_vec());
            let key = match PhpString::new(req, &name) {
                Ok(key) => key.into_key(),
                Err(thrown) => {
                    value.give_up(req);
                    return Err(thrown);
                }
            };
            array.set(req, key, value)?;
        }
    }
    Ok(array)
}

/// One row of a hooked object's table into `get_object_vars`' answer:
/// an integer key php's table holds already is skipped, a declared row
/// the scope cannot see is left out, and a dynamic row's name is keyed
/// as php's `zend_symtable_add_new` keys it. The row's owners are the
/// array's or given up here.
fn add_hooked_var(
    req: &mut Request,
    array: &mut Owner<PhpArray>,
    row: HookedRow,
    scope: Option<ClassId>,
) -> Result<(), Thrown> {
    let HookedRow {
        key,
        value,
        declared,
    } = row;
    let name = match &*key {
        Key::Int(_) if array.contains(&key) => {
            value.give_up(req);
            key.give_up(req);
            return Ok(());
        }
        Key::Int(_) => return array.add_new(req, key, value),
        Key::Str(name) => *name,
    };
    let seen =
        declared.is_none_or(|(declaring, visibility)| visible(req, declaring, visibility, scope));
    if !seen {
        value.give_up(req);
        key.give_up(req);
        return Ok(());
    }
    if declared.is_some() {
        return array.add_new(req, key, value);
    }
    let name = Owner::take(req, name);
    key.give_up(req);
    let key = Key::from_string(req, name);
    array.add_new(req, key, value)
}

/// `get_object_vars(object $object): array`, php's
/// `ZEND_FUNCTION(get_object_vars)` (`Z/Zend/zend_builtin_functions.c:832`):
/// the object's properties the executed scope may read, each named as
/// declared, `zend_check_property_access` deciding each row: a dynamic
/// one is everyone's, a declared one is what `zend_get_property_info`
/// reaches from the scope, and a slot nothing wrote is no row. A
/// reference only the object holds is handed out as its value.
///
/// ```php
/// class P { private $p = 1; protected $q = 2; public $r = 3; function mine() { return get_object_vars($this); } }
/// class C extends P { public $p = 9; private $own = 4; public int $u; function here() { return get_object_vars($this); } }
/// $c = new C; $c->dyn = 5;
/// var_dump(get_object_vars($c), $c->mine(), $c->here(), get_object_vars((object)['a' => 1, '7' => 2]));
/// ```
/// ```expect
/// Deprecated: Creation of dynamic property C::$dyn is deprecated in %s on line 4
/// array(3) {
///   ["r"]=>
///   int(3)
///   ["p"]=>
///   int(9)
///   ["dyn"]=>
///   int(5)
/// }
/// array(4) {
///   ["p"]=>
///   int(1)
///   ["q"]=>
///   int(2)
///   ["r"]=>
///   int(3)
///   ["dyn"]=>
///   int(5)
/// }
/// array(5) {
///   ["q"]=>
///   int(2)
///   ["r"]=>
///   int(3)
///   ["p"]=>
///   int(9)
///   ["own"]=>
///   int(4)
///   ["dyn"]=>
///   int(5)
/// }
/// array(2) {
///   ["a"]=>
///   int(1)
///   [7]=>
///   int(2)
/// }
/// ```
///
/// Each row is laid down as `zend_symtable_add_new` lays it, without
/// the lookup, so over a hooked class whose table holds copies of its
/// declared rows (a lazy ghost's, separated by its initializer) a name
/// the hooks answer and the table holds as a copy is two rows, and a
/// lookup finds the later.
///
/// ```php
/// #[AllowDynamicProperties]
/// class C { public int $b = 1; public $h = 5 { get => $this->h * 10; } function __construct() { $this->a = 1; $this->b = 3; } }
/// $o = (new ReflectionClass(C::class))->newLazyGhost(function ($o) { $o->__construct(); });
/// var_dump($o); $o->a += 1;
/// $vars = get_object_vars($o);
/// var_dump($vars, $vars['b'], count($vars));
/// ```
/// ```expect
/// lazy ghost object(C)#3 (0) {
///   ["b"]=>
///   uninitialized(int)
/// }
/// array(5) {
///   ["b"]=>
///   int(3)
///   ["h"]=>
///   int(50)
///   ["b"]=>
///   int(1)
///   ["h"]=>
///   int(5)
///   ["a"]=>
///   int(2)
/// }
/// int(1)
/// int(5)
/// ```
///
/// The table it reads is `zend_get_properties_for`'s, which lays the
/// object's own table down where it has none, as php's
/// `rebuild_object_properties` does, and `memory_get_usage()` counts it:
///
/// ```php
/// class K { public $d = 1; }
/// $k = new K; $m = memory_get_usage(); $a = get_object_vars($k); echo memory_get_usage() - $m, "\n";
/// ```
/// ```expect
/// 752
/// ```
///
/// The array is laid down at the size php's is, never grown from eight
/// while it fills ([`vars_laid`]): the element count of the table read,
/// and the size of the object's own table where php copies it whole, so
/// `memory_get_peak_usage()` reads no table the copy outgrew:
///
/// ```php
/// function pk($f) { $k = $f(); unset($k); $s = memory_get_usage(); memory_reset_peak_usage(); $k = $f(); echo memory_get_peak_usage() - $s, ' ', memory_get_usage() - $s, "\n"; }
/// class Nine { public $a=1; public $b=1; public $c=1; public $d=1; public $e=1; public $f=1; public $g=1; public $h=1; public $i=1; }
/// $n = new Nine;
/// pk(fn () => get_object_vars($n));
/// $i = new DateInterval('P1D'); get_object_vars($i);
/// pk(fn () => get_object_vars($i));
/// $p = DatePeriod::createFromISO8601String('R2/2012-07-01T00:00:00Z/P7D');
/// pk(fn () => get_object_vars($p));
/// ```
/// ```expect
/// 696 728
/// 696 696
/// 1504 1128
/// ```
#[php_function]
fn get_object_vars(req: &mut Request, object: PhpObject) -> Result<Owner<PhpArray>, Thrown> {
    let class = object.class();
    let scope = req.scope();
    // Over a class with a hooked property whose handlers are php's own,
    // `zend_std_get_properties_for` answers the table its hooks answer,
    // each row one owner of its own, read here with the slot each hooked
    // row stands for, php's `IS_PTR`.
    let overloaded = req.object_handlers(object.heap()).properties_overloaded();
    if !overloaded && let Some(rows) = req.hooked_object_rows(object)? {
        let mut rows = rows.into_iter();
        let mut array = match PhpArray::new(req) {
            Ok(array) => array,
            Err(thrown) => {
                HookedRow::give_up_all(req, rows);
                return Err(thrown);
            }
        };
        while let Some(row) = rows.next() {
            if let Err(thrown) = add_hooked_var(req, &mut array, row, scope) {
                HookedRow::give_up_all(req, rows);
                array.give_up(req);
                return Err(thrown);
            }
        }
        return Ok(array);
    }
    // `zend_get_properties_for(ZEND_PROP_PURPOSE_GET_OBJECT_VARS)`: a
    // class that answers the purpose itself hands back its table.
    let table = match req.properties_answered(object, PropertyPurpose::GetObjectVars)? {
        PropertiesFor::Table(Table::Object(table)) => table,
        PropertiesFor::Table(Table::Array(array)) => return answered_vars(req, object, array),
        PropertiesFor::Temporary(table) => {
            let made = answered_vars(req, object, *table);
            table.give_up(req);
            return made;
        }
        PropertiesFor::Debug(info) => {
            let made = answered_vars(req, object, *info.table);
            info.table.give_up(req);
            return made;
        }
        // No table: `RETURN_EMPTY_ARRAY` (`Z/Zend/zend_builtin_functions.c:849`).
        PropertiesFor::Null => return Ok(Owner::take(req, PhpArray::empty())),
    };
    // The fast copy (`Z/Zend/zend_builtin_functions.c:852`): a class
    // that declares nothing hands its own table on.
    if let Some(array) = crate::ops::object_symtable(req, object)? {
        return Ok(Owner::adopt(array));
    }
    let rows = req.property_rows(&table)?;
    let numeric = || {
        rows.iter().any(|(key, _, declared)| {
            declared.is_none() && matches!(key, Key::Str(name) if numeric_name(*name))
        })
    };
    let elements = table.positions().len();
    let mut array = vars_laid(req, object, table.dynamic(), elements, numeric)?;
    for (key, value, declared) in rows {
        // An integer key, which only a loophole such as
        // `object_properties_load` leaves in the table, is no name the
        // access rule is asked about (`zend_hash_index_add`).
        let name = match key {
            Key::Int(n) => {
                if !array.contains(&key) {
                    let value = req.dup_value(value, None);
                    let value = Owner::adopt(value);
                    array.add_new(req, Owner::index(n), value)?;
                }
                continue;
            }
            Key::Str(name) => name,
        };
        let seen = req.property_row_visible(
            class,
            name.as_bytes(),
            declared.map(|(declaring, _)| declaring),
        );
        if !seen {
            continue;
        }
        let key = if declared.is_some() {
            Key::Str(name)
        } else {
            Key::lookup(name)
        };
        let key = Owner::take(req, key);
        let value = req.dup_value(value, None);
        array.add_new(req, key, Owner::adopt(value))?;
    }
    Ok(array)
}

/// `get_object_vars()` over the table a class's `get_properties_for`
/// answered, a table of its own and never the object's, so each row is
/// dynamic to `zend_check_property_access`: a mangled name passes as it
/// is and stays mangled, and a plain one is refused only where it names
/// a declared property that is not public or that `scope` may not see;
/// a plain one is keyed as a symbol table keys it. The rows are views,
/// and each kept one takes its own owner. The witness is `DatePeriod`'s,
/// whose seven fields php adds to the table; a `DateTime`, whose class
/// hands this reader the standard table, and a `DateInterval`, whose
/// class sets only `get_properties`, are read as the object's own.
///
/// ```php
/// class P extends DatePeriod { public $own = 1; protected $prot = 2; }
/// $p = P::createFromISO8601String('R2/2012-07-01T00:00:00Z/P7D');
/// echo json_encode(array_keys(get_object_vars($p))), "\n";
/// echo count(get_object_vars(new DateTime)), ' ', json_encode(array_keys(get_object_vars(new DateInterval('P1D')))), "\n";
/// ```
/// ```expect
/// ["own","\u0000*\u0000prot","start","current","end","interval","recurrences","include_start_date","include_end_date"]
/// 0 ["y","m","d","h","i","s","f","invert","days","from_string"]
/// ```
fn answered_vars(
    req: &mut Request,
    object: PhpObject,
    table: PhpArray,
) -> Result<Owner<PhpArray>, Thrown> {
    let class = object.class();
    let rows: Vec<(Key, Value)> = walk::rows(req, table).collect();
    let numeric = || {
        rows.iter()
            .any(|(key, _)| matches!(key, Key::Str(name) if numeric_name(*name)))
    };
    let elements = u32::try_from(table.len()).unwrap_or(u32::MAX);
    let mut array = vars_laid(req, object, table, elements, numeric)?;
    for (key, value) in rows {
        // An integer key is an element the class shows among its
        // properties, which no access rule holds (`zend_hash_index_add`).
        let name = match key {
            Key::Int(n) => {
                let value = req.dup_value(value, None);
                let value = Owner::adopt(value);
                array.set(req, Owner::index(n), value)?;
                continue;
            }
            Key::Str(name) => name,
        };
        let key = if name.as_bytes().first() == Some(&0) {
            Key::Str(name)
        } else {
            if !req.property_row_visible(class, name.as_bytes(), None) {
                continue;
            }
            Key::lookup(name)
        };
        let key = Owner::take(req, key);
        let value = req.dup_value(value, None);
        array.add_new(req, key, Owner::adopt(value))?;
    }
    Ok(array)
}

/// The array `get_object_vars()` fills from `source`, a table of
/// `elements` rows, laid down where php lays its: the fast copy's
/// ([`crate::ops::symtable_laid`]) over the object's own table where its
/// class declares nothing (`Z/Zend/zend_builtin_functions.c:852`), and
/// `array_init_size(return_value, zend_hash_num_elements(properties))`
/// otherwise. `numeric` says whether a name of the table reads as an
/// integer, which only the fast copy asks.
fn vars_laid(
    req: &mut Request,
    object: PhpObject,
    source: PhpArray,
    elements: u32,
    numeric: impl FnOnce() -> bool,
) -> Result<Owner<PhpArray>, Thrown> {
    let o = object.heap();
    let own = o
        .properties()
        .is_some_and(|table| table.is_same(source.heap()));
    if o.slots() == 0 && own && !source.is_recursive() {
        return crate::ops::symtable_laid(req, source, elements, numeric());
    }
    PhpArray::init_size(req, elements)
}

/// Whether `name` reads as an integer key, `ZEND_HANDLE_NUMERIC`.
fn numeric_name(name: PhpString) -> bool {
    php_heap::canonical_int(name.as_bytes()).is_some()
}

/// `get_mangled_object_vars(object $object): array`, php's
/// `ZEND_FUNCTION(get_mangled_object_vars)`
/// (`Z/Zend/zend_builtin_functions.c:922`): the object's own table,
/// every row whatever its visibility, the names mangled as the
/// `(array)` cast mangles them; its witness is
/// [`crate::ops::mangled_array`]'s.
///
/// ```php
/// class A { private $x = 1; }
/// var_dump(get_mangled_object_vars(new A));
/// ```
/// ```expect
/// array(1) {
///   ["%0A%0x"]=>
///   int(1)
/// }
/// ```
#[php_function]
fn get_mangled_object_vars(
    req: &mut Request,
    object: PhpObject,
) -> Result<Owner<PhpArray>, Thrown> {
    // `zend_get_properties_no_lazy_init`: a lazy object's table as it
    // stands, nothing initialised for it.
    if let Some(array) = crate::ops::object_symtable(req, object)? {
        return Ok(Owner::adopt(array));
    }
    let table = req.properties_no_lazy_init(object)?;
    let made = crate::ops::mangled_array(req, object, table);
    crate::api::values::adopted(made)
}

/// `get_class_methods(object|string $object_or_class): array`, php's
/// `ZEND_FUNCTION(get_class_methods)` (`Z/Zend/zend_builtin_functions.c:946`):
/// the names of the methods in `ce->function_table` the executed scope
/// may call, `zend_check_method_accessible`, each as declared.
///
/// ```php
/// class P { function pub() {} private function priv() {} protected function prot() {} }
/// class C extends P { function own() {} function PUB() {} function list() { return get_class_methods($this); } }
/// var_dump(get_class_methods('C'), (new C)->list());
/// get_class_methods(1);
/// ```
/// ```expect exit=255
/// array(3) {
///   [0]=>
///   string(3) "own"
///   [1]=>
///   string(3) "PUB"
///   [2]=>
///   string(4) "list"
/// }
/// array(4) {
///   [0]=>
///   string(3) "own"
///   [1]=>
///   string(3) "PUB"
///   [2]=>
///   string(4) "list"
///   [3]=>
///   string(4) "prot"
/// }
///
/// Fatal error: Uncaught TypeError: get_class_methods(): Argument #1 ($object_or_class) must be an object or a valid class name, int given in %s:5
/// Stack trace:
/// #0 %s(5): get_class_methods(1)
/// #1 {main}
///   thrown in %s on line 5
/// ```
///
/// The table's order is php's: the class's own methods, a trait's
/// among them, then the parent's table, then the traits' abstract
/// methods it took, then its interfaces' methods nothing above holds:
///
/// ```php
/// interface I { function i(); }
/// trait T { function t() {} abstract function ta(); }
/// abstract class P implements I { function p() {} private function pp() {} }
/// abstract class C extends P implements Countable { use T; function c() {} }
/// echo implode(",", get_class_methods('C')), "\n";
/// ```
/// ```expect
/// c,t,p,i,ta,count
/// ```
#[php_function]
fn get_class_methods(
    req: &mut Request,
    object_or_class: Raw<Union<PhpObject, PhpString>>,
) -> Result<Owner<PhpArray>, Thrown> {
    let class = object_or_class_name(req, object_or_class.value(), 1)?;
    let scope = req.scope();
    let names: Vec<Box<[u8]>> = req
        .methods_in_order(class)
        .into_iter()
        .filter(|(declaring, method)| visible(req, *declaring, method.visibility, scope))
        .map(|(_, method)| method.function.name.0.clone())
        .collect();
    let mut array = PhpArray::new(req)?;
    for name in names {
        // `ZVAL_STR_COPY(&method_name, mptr->common.function_name)`.
        let name = req.declared_string(&name)?;
        array.append(req, name.into_value())?;
    }
    Ok(array)
}

/// The class a `$object_or_class` of `method_exists` and
/// `property_exists` names, php's two arms before `zend_argument_type_error`:
/// an object's, a string's looked up with the autoloaders, `None` for a
/// name no class has, and the `TypeError` for anything else.
fn class_of_argument(req: &mut Request, given: Value) -> Result<(Option<ClassId>, bool), Thrown> {
    match given.as_ref() {
        ValueRef::Object(object) => Ok((Some(object.class()), true)),
        ValueRef::Str(name) => {
            let name = name.as_bytes().to_vec();
            Ok((lookup_class(req, &name, true)?, false))
        }
        _ => {
            let tail = format!(
                "be of type object|string, {} given",
                crate::ops::value_name(req, &given)
            );
            Err(arg_type_error(req, 1, &tail))
        }
    }
}

/// `method_exists($object_or_class, string $method): bool`, php's
/// `ZEND_FUNCTION(method_exists)` (`Z/Zend/zend_builtin_functions.c:969`):
/// the method under the lowered name in `ce->function_table`, a parent's
/// private one only for an object; and past the table, `Closure`'s
/// `__invoke`, which its `get_method` makes.
///
/// ```php
/// class P { private function priv() {} } class C extends P { function m() {} }
/// var_dump(method_exists('C', 'M'), method_exists(new C, 'priv'), method_exists('C', 'priv'), method_exists('P', 'priv'), method_exists('Nope', 'm'), method_exists(fn () => 1, '__invoke'), method_exists('Closure', '__INVOKE'), method_exists('C', 'nope'));
/// method_exists(1, 'm');
/// ```
/// ```expect exit=255
/// bool(true)
/// bool(true)
/// bool(false)
/// bool(true)
/// bool(false)
/// bool(true)
/// bool(true)
/// bool(false)
///
/// Fatal error: Uncaught TypeError: method_exists(): Argument #1 ($object_or_class) must be of type object|string, int given in %s:4
/// Stack trace:
/// #0 %s(4): method_exists(1, 'm')
/// #1 {main}
///   thrown in %s on line 4
/// ```
///
/// An object's own `get_method` is asked past the table, so a method an
/// outer iterator hands to its inner one exists on the outer
/// (`spl_dual_it_get_method`), and a `__call` trampoline does not:
///
/// ```php
/// $it = new IteratorIterator(new ArrayIterator([1, 2, 3]));
/// class M { function __call($n, $a) {} }
/// var_dump(method_exists($it, 'offsetGet'), method_exists($it, 'nope'), method_exists(new M, 'any'));
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// bool(false)
/// ```
#[php_function]
fn method_exists(
    req: &mut Request,
    object_or_class: Untyped<Value>,
    method: PhpString,
) -> Result<bool, Thrown> {
    let object = object_or_class.0.deref().as_object();
    let (class, is_object) = class_of_argument(req, object_or_class.0)?;
    let Some(class) = class else {
        return Ok(false);
    };
    let lowered = crate::exec::Lowered::of(method.as_bytes());
    if let Some(function) = req.method_named(class, &lowered) {
        // An object's table answers whatever the method's visibility, so
        // the declaration is read only for a class named by string.
        if is_object {
            return Ok(true);
        }
        let private = req
            .method_decl(function)
            .is_some_and(|decl| decl.visibility == Visibility::Private);
        return Ok(!private || req.method_class(function) == Some(class));
    }
    // `Z_OBJ_HT_P(klass)->get_method`, and the object it hands an
    // unknown name to; a trampoline answers `false`.
    let mut on = object;
    while let Some(object) = on {
        let table = req.object_handlers(object.heap());
        // A function `get_method` made, `ZEND_ACC_CALL_VIA_TRAMPOLINE`,
        // answers `false`, as `FFI`'s C functions do.
        if let Some(function) = table.get_method_as(req, object, &lowered, method.as_bytes())? {
            return Ok(!req.via_native_trampoline(function));
        }
        if req
            .call_trampoline(object.class(), method.as_bytes(), false)
            .is_some()
        {
            break;
        }
        on = table.method_receiver(req, object);
    }
    let closure = req.class_named(Bare::resolved(b"Closure")) == Some(class);
    Ok(closure && *lowered == *b"__invoke")
}

/// `property_exists($object_or_class, string $property): bool`, php's
/// `_property_exists` (`Z/Zend/zend_builtin_functions.c:1021`): the
/// name in `ce->properties_info`, a private one only where the class
/// itself declared it; and for an object, a property its table has,
/// `has_property` under `ZEND_PROPERTY_EXISTS`, which a `null` passes.
///
/// A class that answers its own properties answers here too:
/// `DateInterval`'s fields exist on an interval (`bug69587.phpt`).
///
/// ```php
/// class P { private $pp; } class C extends P { public $a; protected $b; private $c; static $s; }
/// $c = new C; $c->dyn = null;
/// var_dump(property_exists('C', 'a'), property_exists('C', 'b'), property_exists('C', 'c'), property_exists('C', 's'), property_exists('C', 'pp'), property_exists('P', 'pp'), property_exists($c, 'dyn'), property_exists('C', 'dyn'), property_exists('Nope', 'a'));
/// $i = new DateInterval('P1D'); var_dump(property_exists($i, 'd'), property_exists($i, 'nope'));
/// property_exists(null, 'a');
/// ```
/// ```expect exit=255
/// Deprecated: Creation of dynamic property C::$dyn is deprecated in %s on line 3
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(false)
/// bool(true)
/// bool(true)
/// bool(false)
/// bool(false)
/// bool(true)
/// bool(false)
///
/// Fatal error: Uncaught TypeError: property_exists(): Argument #1 ($object_or_class) must be of type object|string, null given in %s:6
/// Stack trace:
/// #0 %s(6): property_exists(NULL, 'a')
/// #1 {main}
///   thrown in %s on line 6
/// ```
///
/// Declared frameless for its two arguments
/// (`frameless_function_infos_property_exists`), so each call above is a
/// `FRAMELESS_ICALL_2` over this body, the refusal's among them.
#[php_function(frameless(2))]
fn property_exists(
    req: &mut Request,
    object_or_class: Untyped<Value>,
    property: PhpString,
) -> Result<bool, Thrown> {
    let given = object_or_class.0;
    let (class, is_object) = class_of_argument(req, given)?;
    let Some(class) = class else {
        return Ok(false);
    };
    let name = property.as_bytes();
    if req.property_info(class, name).is_some_and(|info| {
        info.declaring == class
            || req
                .property_decl(info)
                .is_some_and(|p| p.visibility != Visibility::Private)
    }) {
        return Ok(true);
    }
    if is_object && let Some(object) = given.as_object() {
        let table = req.object_handlers(object.heap());
        let name = name.to_vec();
        return table.has_property(req, object, &name, PropertyTest::Exists);
    }
    Ok(false)
}

/// `class_exists_impl` (`Z/Zend/zend_builtin_functions.c:1115`): the
/// class the name finds, with or without the autoloaders, answered by
/// whether its kind is one `kinds` takes.
pub(super) fn exists(
    req: &mut Request,
    name: PhpString,
    autoload: bool,
    kinds: &[ClassKind],
) -> Result<bool, Thrown> {
    let found = lookup_class(req, name.as_bytes(), autoload)?;
    let answer = found
        .and_then(|class| req.class_decl(class))
        .is_some_and(|decl| kinds.contains(&decl.kind));
    Ok(answer)
}

/// `interface_exists(string $interface, bool $autoload = true): bool`,
/// php's `ZEND_FUNCTION(interface_exists)`: `ZEND_ACC_INTERFACE`.
///
/// ```php
/// interface I {} class C {}
/// var_dump(interface_exists('i'), interface_exists('C'), interface_exists('Throwable'), interface_exists('\I', false), interface_exists('Nope'));
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// bool(true)
/// bool(true)
/// bool(false)
/// ```
#[php_function]
fn interface_exists(
    req: &mut Request,
    interface: PhpString,
    #[php(default = true)] autoload: bool,
) -> Result<bool, Thrown> {
    exists(req, interface, autoload, &[ClassKind::Interface])
}

/// `trait_exists(string $trait, bool $autoload = true): bool`, php's
/// `ZEND_FUNCTION(trait_exists)`: `ZEND_ACC_TRAIT`, which neither
/// `class_exists` nor `interface_exists` takes.
///
/// ```php
/// trait T {} class C {} interface I {}
/// var_dump(trait_exists('t'), trait_exists('C'), trait_exists('I'), trait_exists('Nope', false), class_exists('T'), interface_exists('T'));
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(false)
/// ```
#[php_function]
fn trait_exists(
    req: &mut Request,
    r#trait: PhpString,
    #[php(default = true)] autoload: bool,
) -> Result<bool, Thrown> {
    exists(req, r#trait, autoload, &[ClassKind::Trait])
}

/// `enum_exists(string $enum, bool $autoload = true): bool`, php's
/// `ZEND_FUNCTION(enum_exists)`: `ZEND_ACC_ENUM`, which `class_exists`
/// takes as well.
///
/// ```php
/// class C {}
/// var_dump(enum_exists('propertyhooktype'), enum_exists('C'), class_exists('PropertyHookType'), enum_exists('Nope'));
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// bool(true)
/// bool(false)
/// ```
///
/// The name is looked up as `class_exists` looks one up, a leading `\`
/// dropped and the case folded:
///
/// ```php
/// enum Suit { case Hearts; }
/// var_dump(enum_exists('Suit'), enum_exists('\suit'), enum_exists('stdClass'), enum_exists('PropertyHookType'), enum_exists('Nope', false));
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// bool(false)
/// bool(true)
/// bool(false)
/// ```
#[php_function]
fn enum_exists(
    req: &mut Request,
    r#enum: PhpString,
    #[php(default = true)] autoload: bool,
) -> Result<bool, Thrown> {
    exists(req, r#enum, autoload, &[ClassKind::Enum])
}

/// `zend_is_reserved_class_name` (`Z/Zend/zend_compile.c:224`): the
/// unqualified part of the name, any case, one of the type names a
/// class cannot take.
fn reserved_class_name(lowered: &[u8]) -> bool {
    let unqualified = lowered
        .iter()
        .rposition(|b| *b == b'\\')
        .map_or(lowered, |at| &lowered[at + 1..]);
    [
        &b"bool"[..],
        b"false",
        b"float",
        b"int",
        b"null",
        b"parent",
        b"self",
        b"static",
        b"string",
        b"true",
        b"void",
        b"never",
        b"iterable",
        b"object",
        b"mixed",
        b"array",
        b"callable",
    ]
    .contains(&unqualified)
}

/// `class_alias(string $class, string $alias, bool $autoload = true):
/// bool`, php's `ZEND_FUNCTION(class_alias)`
/// (`Z/Zend/zend_builtin_functions.c:1229`) over
/// `zend_register_class_alias_ex`: the class, looked up with the
/// autoloaders as `$autoload` says, added to `EG(class_table)` under the
/// lowered alias, a leading `\` dropped, so every lookup of the alias
/// finds the class itself; a name the table has already is
/// `zend_class_redeclaration_error_ex` as a warning, naming the aliased
/// class's kind and site, and `false`.
///
/// ```php
/// class A {}
/// var_dump(class_alias('A', 'B'), class_alias('A', '\B'), get_class(new B), new B instanceof A, class_exists('b'), get_declared_classes()[count(get_declared_classes()) - 1]);
/// var_dump(class_alias('Nope', 'X'), class_alias('stdClass', 'Exception'));
/// ```
/// ```expect
/// Warning: Cannot redeclare class \B (previously declared in %s:2) in %s on line 3
/// bool(true)
/// bool(false)
/// string(1) "A"
/// bool(true)
/// bool(true)
/// string(1) "b"
///
/// Warning: Class "Nope" not found in %s on line 4
///
/// Warning: Cannot redeclare class Exception in %s on line 4
/// bool(false)
/// bool(false)
/// ```
///
/// A reserved name is `zend_assert_valid_class_name`'s compile error,
/// and `_` its 8.4 deprecation.
///
/// ```php
/// class_alias('stdClass', '_'); var_dump(new _);
/// class_alias('stdClass', 'Int');
/// ```
/// ```expect exit=255
/// Deprecated: Using "_" as a class alias is deprecated since 8.4 in %s on line 2
/// object(stdClass)#1 (0) {
/// }
///
/// Fatal error: Cannot use "int" as a class alias as it is reserved in %s on line 3
/// ```
#[php_function]
fn class_alias(
    req: &mut Request,
    class: PhpString,
    alias: PhpString,
    #[php(default = true)] autoload: bool,
) -> Result<bool, Thrown> {
    let class_bytes = class.as_bytes().to_vec();
    let alias_bytes = alias.as_bytes().to_vec();
    let Some(class) = lookup_class(req, &class_bytes, autoload)? else {
        warning(
            req,
            &format!(
                "Class \"{}\" not found",
                String::from_utf8_lossy(&class_bytes)
            ),
        );
        return Ok(false);
    };
    let alias = Bare::of(&alias_bytes);
    let lowered = alias.bytes().to_ascii_lowercase();
    if reserved_class_name(&lowered) {
        let message = format!(
            "Cannot use \"{}\" as a class alias as it is reserved",
            String::from_utf8_lossy(&lowered)
        );
        req.note_error(php_unit::Level::CompileError, &message);
        req.bail_out();
        return Err(Thrown::of(None));
    }
    if lowered == b"_" {
        deprecated(req, "Using \"_\" as a class alias is deprecated since 8.4");
    }
    if req.class_alias(class, alias) {
        return Ok(true);
    }
    let message = req.alias_redeclaration(class, &alias_bytes);
    warning(req, &message);
    Ok(false)
}

/// `get_declared_class_impl` (`Z/Zend/zend_builtin_functions.c:1430`):
/// `EG(class_table)` in its order, each row whose kind `kinds` takes, by
/// the class's declared name, and an alias's row by the lowered name it
/// is keyed by.
fn declared(req: &mut Request, kinds: &[ClassKind]) -> Result<Owner<PhpArray>, Thrown> {
    let names: Vec<Vec<u8>> = req
        .class_keys()
        .into_iter()
        .filter_map(|(class, alias)| {
            let decl = req.class_decl(class)?;
            kinds
                .contains(&decl.kind)
                .then(|| alias.unwrap_or_else(|| decl.name.0.to_vec()))
        })
        // `ZSTR_VAL(key)[0] != 0`: a key with a leading `\0` is none a
        // program declared, the iterator wrapper's here.
        .filter(|name| name.first() != Some(&0))
        .collect();
    let mut array = PhpArray::new(req)?;
    for name in names {
        // `copy_class_or_interface_name`: `ce->name`, or the alias's key.
        let name = req.declared_string(&name)?;
        array.append(req, name.into_value())?;
    }
    Ok(array)
}

/// `get_declared_classes(): array`, php's
/// `ZEND_FUNCTION(get_declared_classes)`: the classes and enums, the
/// engine's first.
///
/// ```php
/// class A {} interface I {} class B {}
/// $all = get_declared_classes(); $seen = [];
/// foreach ($all as $name) { $seen[$name] = true; }
/// var_dump(isset($seen['Exception']), isset($seen['PropertyHookType']), isset($seen['Throwable']), $all[count($all) - 2], $all[count($all) - 1]);
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// bool(false)
/// string(1) "A"
/// string(1) "B"
/// ```
#[php_function]
fn get_declared_classes(req: &mut Request) -> Result<Owner<PhpArray>, Thrown> {
    declared(req, &[ClassKind::Class, ClassKind::Enum])
}

/// `get_declared_traits(): array`, php's
/// `ZEND_FUNCTION(get_declared_traits)`: the bundle's own traits first,
/// `zend_test`'s `_ZendTestTrait` where the engine links it, then the
/// program's.
///
/// ```php
/// trait T {} class C { use T; } trait U {}
/// var_dump(array_slice(get_declared_traits(), -2));
/// ```
/// ```expect
/// array(2) {
///   [0]=>
///   string(1) "T"
///   [1]=>
///   string(1) "U"
/// }
/// ```
#[php_function]
fn get_declared_traits(req: &mut Request) -> Result<Owner<PhpArray>, Thrown> {
    declared(req, &[ClassKind::Trait])
}

/// `get_declared_interfaces(): array`, php's
/// `ZEND_FUNCTION(get_declared_interfaces)`.
///
/// ```php
/// interface I {}
/// $all = get_declared_interfaces(); $seen = [];
/// foreach ($all as $name) { $seen[$name] = true; }
/// var_dump($all[count($all) - 1], isset($seen['Throwable']), isset($seen['stdClass']));
/// ```
/// ```expect
/// string(1) "I"
/// bool(true)
/// bool(false)
/// ```
#[php_function]
fn get_declared_interfaces(req: &mut Request) -> Result<Owner<PhpArray>, Thrown> {
    declared(req, &[ClassKind::Interface])
}
