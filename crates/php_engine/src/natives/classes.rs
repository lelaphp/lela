//! The natives of `zend_builtin_functions.c` that read `EG(class_table)`
//! and an object's members: the called and the parent class, `is_a`
//! and `is_subclass_of`, the defaults and the members a scope may see,
//! the methods and properties a class has, the `*_exists` family, the
//! aliases `class_alias` adds, and the declared lists. Visibility is
//! measured against `zend_get_executed_scope`, which is the scope the
//! native was called from ([`Request::scope`]).

use php_macros::php_function;
use php_unit::decl::{ClassKind, PropertyDecl, Visibility};

use crate::api::abi::{
    OrFalse, Passed, PropertiesFor, PropertyPurpose, PropertyTest, Raw, Union, Untyped,
};
use crate::api::raising::{Thrown, arg_type_error, deprecated, throw, warning};
use crate::api::request::Request;
use crate::api::values::{ClassId, Key, PhpArray, PhpObject, PhpString, TypeTag, Value, ValueRef};

/// `zend_lookup_class_ex`: a leading `\` dropped, the table under the
/// lowered name, and the autoloaders where the table has none and
/// `autoload` asks for them.
pub(super) fn lookup_class(
    req: &mut Request,
    name: &[u8],
    autoload: bool,
) -> Result<Option<ClassId>, Thrown> {
    let bare = name.strip_prefix(b"\\").unwrap_or(name);
    if let Some(class) = req.class_named(bare) {
        return Ok(Some(class));
    }
    if !autoload {
        return Ok(None);
    }
    crate::api::call::autoload(req, bare)
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

/// `ce->name`: the declared name of a class, the interned string php
/// keeps on the class entry ([`Request::interned`]).
fn class_name(req: &mut Request, class: ClassId) -> Result<PhpString, Thrown> {
    let name = req.class_name_bytes(class);
    req.interned(&name)
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
fn get_called_class(req: &mut Request) -> Result<PhpString, Thrown> {
    match req.called_scope() {
        Some(class) => class_name(req, class),
        None => Err(throw(
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
) -> Result<OrFalse<PhpString>, Thrown> {
    let class = if let Passed::Given(given) = object_or_class {
        Some(object_or_class_name(req, given.value(), 1)?)
    } else {
        deprecated(
            req,
            "Calling get_parent_class() without arguments is deprecated",
        );
        if req.is_pending() {
            return Err(Thrown::of(None));
        }
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

/// `ce->properties_info` in its order: the class's own declarations,
/// instance and `static`, as written, then each one inherited from the
/// parent that the class does not declare again, in the parent's own
/// order, which is where `zend_do_inheritance` appends them. Each row
/// names the class that declared it, `property_info->ce`.
fn properties_info(req: &Request, class: ClassId) -> Vec<(ClassId, PropertyDecl)> {
    let mut chain = Vec::new();
    let mut at = Some(class);
    for _ in 0..64 {
        let Some(here) = at else { break };
        chain.push(here);
        at = req.class_parent(here);
    }
    let mut table: Vec<(ClassId, PropertyDecl)> = Vec::new();
    for here in chain {
        let Some(decl) = req.class_decl(here) else {
            continue;
        };
        for property in &decl.properties {
            if !table.iter().any(|(_, p)| p.name == property.name) {
                table.push((here, property.clone()));
            }
        }
    }
    table
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
#[php_function]
fn get_class_vars(req: &mut Request, class: PhpString) -> Result<PhpArray, Thrown> {
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
    let rows: Vec<(ClassId, PropertyDecl)> = properties_info(req, class)
        .into_iter()
        .filter(|(declaring, p)| visible(req, *declaring, p.visibility, scope))
        .filter(|(_, p)| !req.property_virtual(class, &p.name.0))
        .collect();
    let mut array = PhpArray::new(req)?;
    for statics in [false, true] {
        for (declaring, property) in rows.iter().filter(|(_, p)| p.is_instance != statics) {
            let value = match &property.default {
                Some(expr) => crate::api::call::eval_const_expr(req, expr, Some(*declaring))?,
                None => Value::null(),
            };
            let key = Key::Str(PhpString::new(req, &property.name.0)?);
            array.set(req, key, value)?;
        }
    }
    Ok(array)
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
#[php_function]
fn get_object_vars(req: &mut Request, object: PhpObject) -> Result<PhpArray, Thrown> {
    let class = object.class();
    let scope = req.scope();
    // `zend_get_properties_for(ZEND_PROP_PURPOSE_GET_OBJECT_VARS)`: a
    // class that answers the purpose itself hands back its table.
    let table = req.object_handlers(object.heap());
    match table.get_properties_for(req, object, PropertyPurpose::GetObjectVars)? {
        PropertiesFor::Rows(rows) => return answered_vars(req, class, rows),
        // No table: `RETURN_EMPTY_ARRAY` (`Z/Zend/zend_builtin_functions.c:849`).
        PropertiesFor::Null => return Ok(PhpArray::empty()),
        PropertiesFor::Standard => {}
    }
    // Over a class with a hooked property it is the table its hooks
    // answer, each row one owner of its own.
    if let Some(rows) = req.hooked_object_rows(object)? {
        let mut array = PhpArray::new(req)?;
        for (name, value, declared) in rows {
            let seen = declared
                .is_none_or(|(declaring, visibility)| visible(req, declaring, visibility, scope));
            if !seen {
                req.release(value);
                req.release(Value::string(name));
                continue;
            }
            let key = if declared.is_some() {
                Key::Str(name)
            } else {
                Key::from_string(name)
            };
            array.add_new(req, key, value)?;
        }
        return Ok(array);
    }
    let rows = req.property_rows(object)?;
    let mut array = PhpArray::new(req)?;
    for (name, value, declared) in rows {
        let seen = req.property_row_visible(
            class,
            name.as_bytes(),
            declared.map(|(declaring, _)| declaring),
        );
        if !seen {
            continue;
        }
        let value = if value.tag() == TypeTag::Reference && value.refcount() == 1 {
            value.deref()
        } else {
            value
        };
        req.addref(&value);
        req.addref(&Value::string(name));
        let key = if declared.is_some() {
            Key::Str(name)
        } else {
            Key::from_string(name)
        };
        array.add_new(req, key, value)?;
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
/// whose seven fields php adds to the table, and `DateInterval`'s,
/// which answers its fields for every reader.
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
    class: ClassId,
    rows: Vec<(Key, Value)>,
) -> Result<PhpArray, Thrown> {
    let mut array = PhpArray::new(req)?;
    for (key, value) in rows {
        // An integer key is an element the class shows among its
        // properties, which no access rule holds (`zend_hash_index_add`).
        let name = match key {
            Key::Int(n) => {
                req.addref(&value);
                array.set(req, Key::Int(n), value)?;
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
            Key::from_string(name)
        };
        req.addref(&Value::string(name));
        req.addref(&value);
        array.add_new(req, key, value)?;
    }
    Ok(array)
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
fn get_mangled_object_vars(req: &mut Request, object: PhpObject) -> Result<PhpArray, Thrown> {
    // `zend_get_properties_no_lazy_init`: a lazy object's table as it
    // stands, nothing initialised for it.
    let rows = req.properties_no_lazy_init(object)?;
    crate::ops::mangled_array(req, object, crate::ops::named_rows(rows))
}

/// `ce->function_table` in its order: the class's own methods as
/// declared, then each one inherited that it does not declare again,
/// the parent's table in its own order, and last the interfaces'
/// methods nothing in the chain declared. Each row is the method's name
/// as declared, its visibility and the class that declared it.
fn function_table(req: &Request, class: ClassId) -> Vec<(Box<[u8]>, Visibility, ClassId)> {
    let mut rows: Vec<(Box<[u8]>, Visibility, ClassId)> = Vec::new();
    let mut pending = vec![class];
    let mut seen = Vec::new();
    let mut interfaces = Vec::new();
    while let Some(here) = pending.pop() {
        if seen.contains(&here) || seen.len() > 64 {
            continue;
        }
        seen.push(here);
        let Some(decl) = req.class_decl(here) else {
            continue;
        };
        for method in &decl.methods {
            let name = &method.function.name.0;
            if !rows.iter().any(|(n, _, _)| n.eq_ignore_ascii_case(name)) {
                rows.push((name.clone(), method.visibility, here));
            }
        }
        interfaces.extend(decl.interfaces.iter().filter_map(|i| req.class_named(&i.0)));
        match req.class_parent(here) {
            Some(parent) => pending.push(parent),
            None => pending.extend(interfaces.drain(..).rev()),
        }
    }
    rows
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
#[php_function]
fn get_class_methods(
    req: &mut Request,
    object_or_class: Raw<Union<PhpObject, PhpString>>,
) -> Result<PhpArray, Thrown> {
    let class = object_or_class_name(req, object_or_class.value(), 1)?;
    let scope = req.scope();
    let names: Vec<Box<[u8]>> = function_table(req, class)
        .into_iter()
        .filter(|(_, visibility, declaring)| visible(req, *declaring, *visibility, scope))
        .map(|(name, _, _)| name)
        .collect();
    let mut array = PhpArray::new(req)?;
    for name in names {
        // `ZVAL_STR_COPY(&method_name, mptr->common.function_name)`.
        let name = Value::string(req.interned(&name)?);
        array.append(req, name)?;
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
    let object = object_or_class.0.deref().as_object().copied();
    let (class, is_object) = class_of_argument(req, object_or_class.0)?;
    let Some(class) = class else {
        return Ok(false);
    };
    let lowered = method.as_bytes().to_ascii_lowercase();
    if let Some(function) = req.method_named(class, &lowered) {
        let private = req
            .method_decl(function)
            .is_some_and(|decl| decl.visibility == Visibility::Private);
        return Ok(is_object || !private || req.method_class(function) == Some(class));
    }
    // `Z_OBJ_HT_P(klass)->get_method`, and the object it hands an
    // unknown name to; a trampoline answers `false`.
    let mut on = object;
    while let Some(object) = on {
        let table = req.object_handlers(object.heap());
        if table.get_method(req, object, &lowered)?.is_some() {
            return Ok(true);
        }
        if req
            .call_trampoline(object.class(), method.as_bytes(), false)
            .is_some()
        {
            break;
        }
        on = table.method_receiver(req, object);
    }
    let closure = req.class_named(b"Closure") == Some(class);
    Ok(closure && lowered == b"__invoke")
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
#[php_function]
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
    if properties_info(req, class).iter().any(|(declaring, p)| {
        &*p.name.0 == name && (p.visibility != Visibility::Private || *declaring == class)
    }) {
        return Ok(true);
    }
    if is_object && let Some(object) = given.as_object() {
        let object = *object;
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
    let bytes = name.as_bytes().to_vec();
    let found = lookup_class(req, &bytes, autoload)?;
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

/// `zend_get_object_type`: the word php names a class's kind by.
fn kind_word(kind: ClassKind) -> &'static str {
    match kind {
        ClassKind::Class => "class",
        ClassKind::Interface => "interface",
        ClassKind::Trait => "trait",
        ClassKind::Enum => "enum",
    }
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
    let lowered = alias_bytes
        .strip_prefix(b"\\")
        .unwrap_or(&alias_bytes)
        .to_ascii_lowercase();
    if reserved_class_name(&lowered) {
        let message = format!(
            "Cannot use \"{}\" as a class alias as it is reserved",
            String::from_utf8_lossy(&lowered)
        );
        req.note_error(php_unit::Level::CompileError, &message);
        req.bail_out(php_unit::Level::CompileError, &message);
        return Err(Thrown::of(None));
    }
    if lowered == b"_" {
        deprecated(req, "Using \"_\" as a class alias is deprecated since 8.4");
    }
    if req.class_alias(class, &lowered) {
        return Ok(true);
    }
    let shown = String::from_utf8_lossy(&alias_bytes).into_owned();
    let message = match req.class_decl(class) {
        Some(decl) if !decl.file.0.is_empty() => format!(
            "Cannot redeclare {} {shown} (previously declared in {}:{})",
            kind_word(decl.kind),
            String::from_utf8_lossy(&decl.file.0),
            decl.line
        ),
        Some(decl) => format!("Cannot redeclare {} {shown}", kind_word(decl.kind)),
        None => format!("Cannot redeclare class {shown}"),
    };
    warning(req, &message);
    Ok(false)
}

/// `get_declared_class_impl` (`Z/Zend/zend_builtin_functions.c:1430`):
/// `EG(class_table)` in its order, each row whose kind `kinds` takes, by
/// the class's declared name, and an alias's row by the lowered name it
/// is keyed by.
fn declared(req: &mut Request, kinds: &[ClassKind]) -> Result<PhpArray, Thrown> {
    let names: Vec<Vec<u8>> = req
        .class_table()
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
        let name = Value::string(req.interned(&name)?);
        array.append(req, name)?;
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
fn get_declared_classes(req: &mut Request) -> Result<PhpArray, Thrown> {
    declared(req, &[ClassKind::Class, ClassKind::Enum])
}

/// `get_declared_traits(): array`, php's
/// `ZEND_FUNCTION(get_declared_traits)`.
///
/// ```php
/// trait T {} class C { use T; } trait U {}
/// var_dump(get_declared_traits());
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
fn get_declared_traits(req: &mut Request) -> Result<PhpArray, Thrown> {
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
fn get_declared_interfaces(req: &mut Request) -> Result<PhpArray, Thrown> {
    declared(req, &[ClassKind::Interface])
}
