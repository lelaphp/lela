//! `ReflectionAttribute` over the declaration model's
//! [`Attribute`](php_ext::decl::Attribute), and `reflect_attributes`,
//! the one walk behind every `getAttributes` (`php_reflection.c`'s
//! `reflect_attributes` and `read_attributes`). php keeps a
//! `zend_attribute*` into the subject's attribute table; the table is
//! part of an immutable unit here, so the reflector keeps a copy of the
//! attribute as written beside the three facts php reads off the
//! reference: the scope its constant expressions are evaluated in, the
//! target bit of the subject it sits on, and whether the subject
//! carries another of the same name.
//!
//! The `validation_error` an internal attribute's own validator leaves
//! where `#[\DelayedTargetValidation]` deferred it is the refusal
//! [`class_attribute_refusal`] answers for the class the attribute
//! stands on, and [`hook_attribute_refusal`] for a property hook, the
//! rules the compiler raises by.

use std::fmt::Write as _;

use php_ext::abi::This;
use php_ext::attr::php_class;
use php_ext::call::{attribute_object, autoload, eval_const_expr, eval_declared};
use php_ext::coerce::value_name;
use php_ext::decl::{Attribute, AttributeTarget, class_attribute_refusal, hook_attribute_refusal};
use php_ext::raising::{Thrown, arg_value_error, throw, throw_error, throw_error_chained};
use php_ext::request::Request;
use php_ext::values::{ClassId, FreshString, Owner, PhpArray, PhpObject, PhpString};

use super::{
    class_name_of, class_named, default_value_string, is_internal, keep_attribute, new_object,
    set_name, this_attribute,
};

/// `ZEND_ATTRIBUTE_TARGET_*` (`Z/Zend/zend_attributes.h:26-33`), the
/// bits the `Attribute::TARGET_*` constants name, in the order
/// `target_names` prints them.
pub(super) const TARGET_CLASS: i64 = 1;
pub(super) const TARGET_FUNCTION: i64 = 2;
pub(super) const TARGET_METHOD: i64 = 4;
pub(super) const TARGET_PROPERTY: i64 = 8;
pub(super) const TARGET_CLASS_CONST: i64 = 16;
pub(super) const TARGET_PARAMETER: i64 = 32;
pub(super) const TARGET_CONST: i64 = 64;
const TARGET_ALL: i64 = 127;

/// `ZEND_ATTRIBUTE_IS_REPEATABLE`.
const IS_REPEATABLE: i64 = 128;

/// `REFLECTION_ATTRIBUTE_IS_INSTANCEOF`, the one bit
/// `getAttributes` takes in its `$flags` argument.
const IS_INSTANCEOF: i64 = 2;

/// `target_names` (`Z/Zend/zend_attributes.c:423`), what
/// `zend_get_attribute_target_names` joins with `, `.
const TARGET_NAMES: [&str; 7] = [
    "class",
    "function",
    "method",
    "property",
    "class constant",
    "parameter",
    "constant",
];

/// php's `attribute_reference` (`php_reflection.c:196-201`), kept by
/// value: the attribute as written, the class its constant expressions
/// are evaluated in, the target bit of the subject it sits on, whether
/// that subject is a property hook, and `zend_is_attribute_repeated`
/// answered where the sibling list was still at hand.
#[derive(Clone)]
pub(super) struct AttributeRef {
    pub(super) attribute: Attribute,
    pub(super) scope: Option<ClassId>,
    pub(super) target: i64,
    /// The subject is a property hook, which `validate_nodiscard`
    /// refuses ([`hook_attribute_refusal`]).
    pub(super) hook: bool,
    pub(super) repeated: bool,
    /// The file the subject was declared in, php's `attr->filename`:
    /// none for a native's.
    pub(super) file: Option<Vec<u8>>,
}

/// `zend_get_attribute_target_names`: the bits set, in bit order,
/// joined with `, `.
fn target_names(flags: i64) -> String {
    let mut out = String::new();
    for (bit, name) in TARGET_NAMES.iter().enumerate() {
        if flags & (1 << bit) == 0 {
            continue;
        }
        if !out.is_empty() {
            out.push_str(", ");
        }
        out.push_str(name);
    }
    out
}

/// `zend_get_attribute_target_names(flags)` as php lays it down: the
/// names appended to a `smart_str`, which starts at its 256-byte bin,
/// and `smart_str_extract` trimming it to their length by
/// `zend_string_realloc`; none where no bit names a target, the empty
/// string php allocates nothing for. The string stands until the
/// caller frees it.
fn held_target_names(
    req: &mut Request,
    flags: i64,
) -> Result<(String, Option<FreshString>), Thrown> {
    let names = target_names(flags);
    if names.is_empty() {
        return Ok((names, None));
    }
    let laid = FreshString::new(req, 256 - 25)?;
    let mut held = laid.truncate(req, names.len())?;
    held.bytes_mut().copy_from_slice(names.as_bytes());
    Ok((names, Some(held)))
}

/// What `reflect_attributes` reads off the subject whose attributes it
/// reflects: the class its attributes' constant expressions are
/// evaluated in, its target bit, whether it is a property hook, and
/// the file it was declared in.
#[derive(Clone, Copy)]
pub(super) struct Subject<'a> {
    pub(super) scope: Option<ClassId>,
    pub(super) target: i64,
    pub(super) hook: bool,
    pub(super) file: Option<&'a [u8]>,
}

impl<'a> Subject<'a> {
    /// A subject that is no property hook.
    pub(super) fn new(scope: Option<ClassId>, target: i64, file: Option<&'a [u8]>) -> Subject<'a> {
        Subject {
            scope,
            target,
            hook: false,
            file,
        }
    }
}

/// php's `attr->filename` for a subject declared in `file`: the file,
/// and none for a native's, whose declaration names none.
pub(super) fn declared_in(file: &[u8]) -> Option<Vec<u8>> {
    (!file.is_empty()).then(|| file.to_vec())
}

/// `reflect_attributes` (`php_reflection.c`) with `read_attributes`
/// under it: the subject's attributes as `ReflectionAttribute`s, held
/// to the optional `$name` and the `IS_INSTANCEOF` bit. `$name` alone
/// matches the attribute's own name, case-insensitively as
/// `zend_string_equals_ci` does; with that bit set it is a class every
/// match must be an instance of, and a name no class has is php's
/// `Error`.
///
/// ```php
/// $m = new ReflectionMethod('ReflectionFunction', 'isDisabled');
/// var_dump(count($m->getAttributes('Deprecated', ReflectionAttribute::IS_INSTANCEOF)),
///     count($m->getAttributes('Override', ReflectionAttribute::IS_INSTANCEOF)));
/// try { $m->getAttributes('Nope', ReflectionAttribute::IS_INSTANCEOF); }
/// catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(1)
/// int(0)
/// Class "Nope" not found
/// ```
///
/// Both lookups are `zend_lookup_class`'s, so the autoloader is asked
/// for the class filtered by and for each attribute's own; one it does
/// not load is passed over, and one it throws through is thrown with
/// the array built so far given up:
///
/// ```php
/// spl_autoload_register(function ($c) {
///     echo "load $c\n";
///     if ($c === 'Marker') eval('#[Attribute] class Marker {}');
///     if ($c === 'Sub') eval('#[Attribute] class Sub extends Marker {}');
///     if ($c === 'Boom') throw new RuntimeException("boom");
/// });
/// #[Sub] #[Missing] #[Marker] function f() {}
/// #[Marker] #[Boom] function g() {}
/// var_dump(count((new ReflectionFunction('f'))->getAttributes('Marker', ReflectionAttribute::IS_INSTANCEOF)));
/// foreach (['Marker', 'Boom'] as $base) {
///     try { (new ReflectionFunction('g'))->getAttributes($base, ReflectionAttribute::IS_INSTANCEOF); }
///     catch (RuntimeException $e) { echo $e->getMessage(), "\n"; }
/// }
/// ```
/// ```expect
/// load Marker
/// load Sub
/// load Missing
/// int(2)
/// load Boom
/// boom
/// load Boom
/// boom
/// ```
pub(super) fn reflect_attributes(
    req: &mut Request,
    attributes: &[Attribute],
    subject: Subject<'_>,
    name: Option<PhpString>,
    flags: i64,
) -> Result<Owner<PhpArray>, Thrown> {
    let ce = crate::throwables::Throwables::of(req)?;
    let wanted = name.as_ref().map(|name| name.as_bytes().to_vec());
    if flags & !IS_INSTANCEOF != 0 {
        return Err(arg_value_error(req, 2, "be a valid attribute filter flag"));
    }
    let base = match (&wanted, flags & IS_INSTANCEOF != 0) {
        (Some(name), true) => match autoload(req, name)? {
            Some(class) => Some(class),
            None => {
                return Err(throw_error(
                    req,
                    ce.error,
                    &format!("Class \"{}\" not found", String::from_utf8_lossy(name)),
                ));
            }
        },
        _ => None,
    };
    if attributes.is_empty() {
        return Ok(Owner::take(req, PhpArray::empty()));
    }
    let repeated: Vec<bool> = attributes
        .iter()
        .map(|one| {
            attributes
                .iter()
                .filter(|other| other.name.0.eq_ignore_ascii_case(&one.name.0))
                .count()
                > 1
        })
        .collect();
    let mut out = PhpArray::new(req)?;
    let filled = (|| {
        for (at, attribute) in attributes.iter().enumerate() {
            if !matches(req, attribute, wanted.as_deref(), base)? {
                continue;
            }
            let kept = AttributeRef {
                attribute: attribute.clone(),
                scope: subject.scope,
                target: subject.target,
                hook: subject.hook,
                repeated: repeated.get(at).copied().unwrap_or(false),
                file: subject.file.map(<[u8]>::to_vec),
            };
            let object = attribute_factory(req, &kept)?;
            out.append(req, object.into_value())?;
        }
        Ok(())
    })();
    // `read_attributes`' `FAILURE`: the array built so far is given up
    // with the attributes in it, as the VM frees the return value
    // `RETURN_THROWS()` leaves.
    if let Err(thrown) = filled {
        out.give_up(req);
        return Err(thrown);
    }
    Ok(out)
}

/// `read_attributes`'s two filters: the name as written, or the class
/// every match must be an instance of. The attribute's own class is
/// autoloaded for the second (`zend_lookup_class_ex(attr->name,
/// attr->lcname, 0)`): one still not found is passed over, and what the
/// autoloader threw is handed on.
fn matches(
    req: &mut Request,
    attribute: &Attribute,
    wanted: Option<&[u8]>,
    base: Option<ClassId>,
) -> Result<bool, Thrown> {
    let written = bare_name(&attribute.name.0);
    if let Some(base) = base {
        return Ok(
            autoload(req, written)?.is_some_and(|class| req.symbols().instance_of(class, base))
        );
    }
    Ok(match wanted {
        Some(asked) => written.eq_ignore_ascii_case(bare_name(asked)),
        None => true,
    })
}

/// A class name with the leading `\` a written one may carry dropped,
/// as `zend_lookup_class` drops it.
fn bare_name(name: &[u8]) -> &[u8] {
    name.strip_prefix(b"\\").unwrap_or(name)
}

/// `reflection_attribute_factory`: one `ReflectionAttribute` with the
/// reference on its payload and `$name` written, as php writes the
/// property beside `intern->ptr`.
fn attribute_factory(req: &mut Request, kept: &AttributeRef) -> Result<Owner<PhpObject>, Thrown> {
    let class = class_named(req, b"ReflectionAttribute")?;
    let object = new_object(req, class)?;
    let name = kept.attribute.name.0.to_vec();
    set_name(req, *object, &name)?;
    keep_attribute(req, *object, kept.clone())?;
    Ok(object)
}

/// `zend_attribute_attribute_get_flags`: the first argument of the
/// class's own `#[Attribute]` marker, `TARGET_ALL` where the marker
/// takes none, and php's `Error` for a class carrying no marker.
fn marker_flags(req: &mut Request, class: ClassId, written: &str) -> Result<i64, Thrown> {
    let ce = crate::throwables::Throwables::of(req)?;
    let marker = req.symbols().class_decl(class).and_then(|decl| {
        decl.attributes
            .iter()
            .find(|one| one.name.0.eq_ignore_ascii_case(b"Attribute"))
            .cloned()
    });
    let Some(marker) = marker else {
        return Err(throw_error(
            req,
            ce.error,
            &format!("Attempting to use non-attribute class \"{written}\" as attribute"),
        ));
    };
    let Some(first) = marker.arguments.first() else {
        return Ok(TARGET_ALL);
    };
    let value = eval_const_expr(req, &first.value, Some(class))?;
    let Some(flags) = value.as_int() else {
        // `zend_zval_value_name(&flags)`
        // (`Z/Zend/zend_attributes.c:54`): the value's name, so `true`
        // stands where the type alone would read `bool`, and the value
        // given up after it, `zval_ptr_dtor(&flags)`.
        let given = value_name(req, &value);
        value.give_up(req);
        return Err(throw_error(
            req,
            ce.error,
            &format!(
                "Attribute::__construct(): Argument #1 ($flags) must be of type int, {given} given"
            ),
        ));
    };
    value.give_up(req);
    // `ZEND_ATTRIBUTE_FLAGS`, every target and `IS_REPEATABLE`.
    if flags & !(TARGET_ALL | IS_REPEATABLE) != 0 {
        return Err(throw_error(
            req,
            ce.error,
            "Invalid attribute flags specified",
        ));
    }
    Ok(flags)
}

/// `zend_get_attribute_object`: the instance, its constructor served
/// with the attribute's arguments, the named ones by their names, as
/// `object_init_with_constructor` serves them
/// ([`instantiate`]).
/// `ReflectionAttribute` as the stub declares it, with the
/// `attribute_reference` `reflection_attribute_factory` lays down beside
/// it, given back with it:
///
/// ```php
/// #[Attribute] class A {}
/// #[A] function f() {}
/// $rf = new ReflectionFunction('f'); $w = $rf->getAttributes()[0];
/// $m = memory_get_usage(); $r = $rf->getAttributes()[0]; $n = memory_get_usage(); unset($r);
/// var_dump($n - $m, memory_get_usage() - $n);
/// ```
/// ```expect
/// int(136)
/// int(-136)
/// ```
#[php_class(implements(crate::Reflector), not_serializable, size = 96, payload = crate::Reflected, handlers = crate::HANDLERS)]
pub(crate) struct ReflectionAttribute;

#[php_class]
impl ReflectionAttribute {
    const IS_INSTANCEOF: i64 = 2;
    #[php(property)]
    const name: PhpString;

    /// `ReflectionAttribute::getName()`: `attr->data->name`, the attribute
    /// as it was written.
    ///
    /// A native's attribute, the one the stub's `deprecated` puts on
    /// `ReflectionFunction::isDisabled`, is read as a program's is:
    ///
    /// ```php
    /// $a = (new ReflectionMethod('ReflectionFunction', 'isDisabled'))->getAttributes();
    /// var_dump($a[0]->getName());
    /// ```
    /// ```expect
    /// string(10) "Deprecated"
    /// ```
    fn getName(req: &mut Request, this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
        let kept = this_attribute(req, this.object())?;
        let name = kept.attribute.name.0.to_vec();
        PhpString::declared(req, &name)
    }

    /// `ReflectionAttribute::getTarget()`: `attr->target`, the one
    /// `Attribute::TARGET_*` bit of the subject the attribute sits on.
    ///
    /// ```php
    /// $a = (new ReflectionMethod('ReflectionFunction', 'isDisabled'))->getAttributes();
    /// var_dump($a[0]->getTarget());
    /// ```
    /// ```expect
    /// int(4)
    /// ```
    fn getTarget(req: &mut Request, this: This<Self>) -> Result<i64, Thrown> {
        let kept = this_attribute(req, this.object())?;
        Ok(kept.target)
    }

    /// `ReflectionAttribute::isRepeated()`:
    /// `zend_is_attribute_repeated`, whether the subject carries another
    /// attribute of the same name at the same offset.
    ///
    /// ```php
    /// $a = (new ReflectionMethod('ReflectionFunction', 'isDisabled'))->getAttributes();
    /// var_dump($a[0]->isRepeated());
    /// ```
    /// ```expect
    /// bool(false)
    /// ```
    fn isRepeated(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        let kept = this_attribute(req, this.object())?;
        Ok(kept.repeated)
    }

    /// `ReflectionAttribute::getArguments()`: each argument's constant
    /// expression evaluated in the attribute's scope, the positional ones
    /// appended and the named ones at their name, as
    /// `zend_get_attribute_value` and `zend_hash_add_new` place them.
    ///
    /// ```php
    /// $a = (new ReflectionMethod('ReflectionFunction', 'isDisabled'))->getAttributes();
    /// var_dump($a[0]->getArguments());
    /// ```
    /// ```expect
    /// array(2) {
    ///   ["since"]=>
    ///   string(3) "8.0"
    ///   ["message"]=>
    ///   string(73) "as ReflectionFunction can no longer be constructed for disabled functions"
    /// }
    /// ```
    ///
    /// An argument over a class constant is evaluated as it is read, its
    /// operators and a conditional among them:
    ///
    /// ```php
    /// class K { const A = 1|2; }
    /// #[Attribute]
    /// class At { function __construct(public $v, public $w = null) {} }
    /// #[At(K::A | 4, K::A ?: 0)]
    /// class T {}
    /// var_dump((new ReflectionClass('T'))->getAttributes()[0]->getArguments());
    /// ```
    /// ```expect
    /// array(2) {
    ///   [0]=>
    ///   int(7)
    ///   [1]=>
    ///   int(3)
    /// }
    /// ```
    ///
    /// A string the attribute's declaration wrote is the unit's interned
    /// string, as opcache persists `attr->args[i].value`:
    ///
    /// ```php
    /// #[Attribute] class At { function __construct(public $v) {} }
    /// #[At('attr')] class B {} debug_zval_dump((new ReflectionClass('B'))->getAttributes()[0]->getArguments());
    /// ```
    /// ```expect regime=immutable-literals
    /// array(1) packed refcount(1){
    ///   [0]=>
    ///   string(4) "attr" interned
    /// }
    /// ```
    fn getArguments(req: &mut Request, this: This<Self>) -> Result<Owner<PhpArray>, Thrown> {
        let kept = this_attribute(req, this.object())?;
        let mut out = PhpArray::new(req)?;
        let filled = (|| {
            for argument in &kept.attribute.arguments {
                let value = eval_declared(req, &argument.value, kept.scope)?;
                match &argument.name {
                    Some(name) => {
                        out.set_str(req, &name.0, value)?;
                    }
                    None => out.append(req, value)?,
                }
            }
            Ok(())
        })();
        // The array built so far is given up where an argument fails to
        // evaluate, as the VM frees the return value php's `RETURN_THROWS()`
        // leaves.
        if let Err(thrown) = filled {
            out.give_up(req);
            return Err(thrown);
        }
        Ok(out)
    }

    /// `ReflectionAttribute::newInstance()`: the attribute's class
    /// instantiated with the arguments, php's four refusals before it —
    /// the class no table has, a class carrying no `#[Attribute]` marker,
    /// a marker whose flags do not name this target, and a user attribute
    /// repeated where its marker did not allow it.
    ///
    /// An internal attribute is made from the class
    /// `zend_register_attribute_ce` registered, its marker's targets read
    /// as a user attribute's are:
    ///
    /// ```php
    /// $a = (new ReflectionMethod('ReflectionFunction', 'isDisabled'))->getAttributes();
    /// var_dump($a[0]->newInstance());
    /// ```
    /// ```expect
    /// object(Deprecated)#1 (2) {
    ///   ["message"]=>
    ///   string(73) "as ReflectionFunction can no longer be constructed for disabled functions"
    ///   ["since"]=>
    ///   string(3) "8.0"
    /// }
    /// ```
    ///
    /// A user attribute's own `#[Attribute(Attribute::TARGET_*)]` is held
    /// to the target it stands on, and to the repetition it allows:
    ///
    /// ```php
    /// #[Attribute(Attribute::TARGET_METHOD)] class B {}
    /// class C { #[B] function m() {} #[B] public $p; }
    /// echo get_class((new ReflectionMethod('C', 'm'))->getAttributes()[0]->newInstance()), "\n";
    /// try { (new ReflectionProperty('C', 'p'))->getAttributes()[0]->newInstance(); }
    /// catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// #[Attribute] class D {}
    /// #[D] #[D] function f() {}
    /// try { (new ReflectionFunction('f'))->getAttributes()[0]->newInstance(); }
    /// catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// #[Attribute(Attribute::TARGET_ALL | Attribute::IS_REPEATABLE)] class E {}
    /// #[E] #[E] function g() {}
    /// echo get_class((new ReflectionFunction('g'))->getAttributes()[1]->newInstance()), "\n";
    /// #[Attribute(1024)] class G {}
    /// #[G] function i() {}
    /// try { (new ReflectionFunction('i'))->getAttributes()[0]->newInstance(); }
    /// catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// B
    /// Attribute "B" cannot target property (allowed targets: method)
    /// Attribute "D" must not be repeated
    /// E
    /// Invalid attribute flags specified
    /// ```
    ///
    /// A validator's refusal `#[\DelayedTargetValidation]` deferred is
    /// thrown here, php's `validation_error`:
    ///
    /// ```php
    /// #[DelayedTargetValidation]
    /// #[Attribute]
    /// trait T {}
    /// #[DelayedTargetValidation]
    /// #[AllowDynamicProperties]
    /// readonly class R {}
    /// foreach (['T', 'R'] as $c) {
    ///     try { (new ReflectionClass($c))->getAttributes()[1]->newInstance(); }
    ///     catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// }
    /// ```
    /// ```expect
    /// Cannot apply #[\Attribute] to trait T
    /// Cannot apply #[\AllowDynamicProperties] to readonly class R
    /// ```
    ///
    /// and so is `validate_nodiscard`'s refusal of a property hook:
    ///
    /// ```php
    /// class H { public $p { #[DelayedTargetValidation] #[NoDiscard] get => 1; } }
    /// try { (new ReflectionProperty("H", "p"))->getHook(PropertyHookType::Get)->getAttributes()[1]->newInstance(); }
    /// catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// #[\NoDiscard] is not supported for property hooks
    /// ```
    ///
    /// The arguments are evaluated and the constructor called under a
    /// site at the attribute's own file and line, which no trace row
    /// shows (`zend_get_attribute_object`'s dummy frame,
    /// [`attribute_object`]): the constructor's row names the
    /// attribute as its caller, and a throwable an argument raises has no
    /// `[constant expression]` row over it.
    ///
    /// ```php
    /// #[Attribute]
    /// class A { public function __construct($x = null) { throw new Exception("ctor"); } }
    /// #[A]
    /// class C {}
    /// #[A(1 % 0)]
    /// class D {}
    /// try { (new ReflectionClass('C'))->getAttributes()[0]->newInstance(); } catch (Exception $e) { echo $e->getTraceAsString(), "\n"; }
    /// try { (new ReflectionClass('D'))->getAttributes()[0]->newInstance(); } catch (Error $e) { echo $e->getLine(), "\n", $e->getTraceAsString(), "\n"; }
    /// ```
    /// ```expect
    /// #0 %s(4): A->__construct()
    /// #1 %s(8): ReflectionAttribute->newInstance()
    /// #2 {main}
    /// 6
    /// #0 %s(9): ReflectionAttribute->newInstance()
    /// #1 {main}
    /// ```
    ///
    /// The target refusal holds `zend_get_attribute_target_names`' two
    /// strings on the request's heap across its throw.
    ///
    /// ```php
    /// function pk($f) { $f(); $s = memory_get_usage(); memory_reset_peak_usage(); $f(); echo memory_get_peak_usage() - $s, " ", memory_get_usage() - $s, "\n"; }
    /// #[Attribute(Attribute::TARGET_CLASS)] class A {}
    /// #[A] function g() {}
    /// $a = (new ReflectionFunction('g'))->getAttributes()[0];
    /// pk(function () use ($a) { try { $a->newInstance(); } catch (Error $e) {} });
    /// ```
    /// ```expect
    /// 2368 32
    /// ```
    ///
    /// The class is `zend_lookup_class`'s, so the autoloader is asked
    /// for it, and what it throws is the refusal's `previous`:
    ///
    /// ```php
    /// spl_autoload_register(function ($c) {
    ///     echo "load $c\n";
    ///     if ($c === 'Marker') eval('#[Attribute] class Marker {}');
    ///     if ($c === 'Boom') throw new RuntimeException("boom");
    /// });
    /// #[Marker] #[Boom] function f() {}
    /// [$marker, $boom] = (new ReflectionFunction('f'))->getAttributes();
    /// var_dump(get_class($marker->newInstance()));
    /// try { $boom->newInstance(); }
    /// catch (Error $e) { echo $e->getMessage(), " / ", $e->getPrevious()->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// load Marker
    /// string(6) "Marker"
    /// load Boom
    /// Attribute class "Boom" not found / boom
    /// ```
    ///
    /// That refusal is `zend_throw_error` over what the autoloader threw,
    /// so `zend_vspprintf`'s buffer stands across it, which php's peak
    /// shows.
    ///
    /// ```php
    /// spl_autoload_register(function ($c) { throw new RuntimeException('boom'); });
    /// #[Boom] function f() {}
    /// $a = (new ReflectionFunction('f'))->getAttributes()[0];
    /// function pk($f) { $f(); $s = memory_get_usage(); memory_reset_peak_usage(); $f(); echo memory_get_peak_usage() - $s, " ", memory_get_usage() - $s, "\n"; }
    /// pk(function () use ($a) { try { $a->newInstance(); } catch (Error $e) {} });
    /// ```
    /// ```expect
    /// 4616 32
    /// ```
    fn newInstance(req: &mut Request, this: This<Self>) -> Result<Owner<PhpObject>, Thrown> {
        let ce = crate::throwables::Throwables::of(req)?;
        let kept = this_attribute(req, this.object())?;
        let written = String::from_utf8_lossy(&kept.attribute.name.0).into_owned();
        let looked_up = bare_name(&kept.attribute.name.0).to_vec();
        // `zend_lookup_class`, whose autoloader may throw: php's
        // `zend_throw_error` over it chains what was thrown as the
        // refusal's `previous`.
        let class = match autoload(req, &looked_up) {
            Ok(Some(class)) => class,
            Ok(None) => {
                let message = format!("Attribute class \"{written}\" not found");
                return Err(throw_error(req, ce.error, &message));
            }
            Err(thrown) => {
                let message = format!("Attribute class \"{written}\" not found");
                return Err(throw_error_chained(req, thrown, ce.error, &message));
            }
        };
        let flags = marker_flags(req, class, &written)?;
        if kept.target & flags == 0 {
            let location = held_target_names(req, kept.target)?;
            let allowed = held_target_names(req, flags & TARGET_ALL)?;
            let thrown = throw_error(
                req,
                ce.error,
                &format!(
                    "Attribute \"{written}\" cannot target {} (allowed targets: {})",
                    location.0, allowed.0
                ),
            );
            for held in [location.1, allowed.1].into_iter().flatten() {
                held.free(req);
            }
            return Err(thrown);
        }
        // `zend_attribute.validation_error`: the refusal of an internal
        // attribute's validator `#[\DelayedTargetValidation]` deferred,
        // thrown where the object would be made.
        if kept.target == TARGET_CLASS
            && let Some(subject) = kept.scope
            && let Some(decl) = req.symbols().class_decl(subject)
            && let Some(refused) = class_attribute_refusal(
                &looked_up,
                decl.kind,
                &decl.flags,
                &String::from_utf8_lossy(&class_name_of(req, subject)),
            )
        {
            return Err(throw(req, ce.error, &refused));
        }
        if kept.hook
            && let Some(refused) = hook_attribute_refusal(&looked_up)
        {
            return Err(throw(req, ce.error, refused));
        }
        // An extension's validator over a program's subject, which the
        // compiler ran where the attribute stands unless
        // `#[\DelayedTargetValidation]` deferred its refusal to here; a
        // native's subject was never compiled, so never validated.
        if kept.file.is_some()
            && let Some(validator) = req
                .symbols()
                .class_decl(class)
                .and_then(|decl| decl.attribute_validator)
            && let Some(target) = u32::try_from(kept.target)
                .ok()
                .and_then(|bit| AttributeTarget::try_from(bit).ok())
            && let Some(refused) = (validator.0)(
                &kept.attribute,
                target,
                kept.scope.map(|scope| class_name_of(req, scope)).as_deref(),
            )
        {
            return Err(throw(req, ce.error, &refused));
        }
        if !is_internal(req, class) && flags & IS_REPEATABLE == 0 && kept.repeated {
            return Err(throw_error(
                req,
                ce.error,
                &format!("Attribute \"{written}\" must not be repeated"),
            ));
        }
        attribute_object(
            req,
            class,
            &kept.attribute,
            kept.scope,
            kept.file.as_deref(),
        )
    }

    /// `ReflectionAttribute::__toString()`: `Attribute [ name ]` alone for
    /// one with no argument, and the arguments block for one with some,
    /// each argument as `format_default_value` writes it, which is the
    /// expression as written and not its value.
    ///
    /// ```php
    /// $a = (new ReflectionMethod('ReflectionFunction', 'isDisabled'))->getAttributes();
    /// echo $a[0];
    /// ```
    /// ```expect
    /// Attribute [ Deprecated ] {
    ///   - Arguments [2] {
    ///     Argument #0 [ since = '8.0' ]
    ///     Argument #1 [ message = 'as ReflectionFunction can no longer be constructed for disabled functions' ]
    ///   }
    /// }
    /// ```
    fn __toString(req: &mut Request, this: This<Self>) -> Result<Owner<PhpString>, Thrown> {
        let kept = this_attribute(req, this.object())?;
        let mut out = format!(
            "Attribute [ {} ]",
            String::from_utf8_lossy(&kept.attribute.name.0)
        );
        if kept.attribute.arguments.is_empty() {
            out.push('\n');
            return PhpString::new(req, out.as_bytes());
        }
        out.push_str(" {\n");
        writeln!(out, "  - Arguments [{}] {{", kept.attribute.arguments.len()).ok();
        for (at, argument) in kept.attribute.arguments.iter().enumerate() {
            write!(out, "    Argument #{at} [ ").ok();
            if let Some(name) = &argument.name {
                out.push_str(&String::from_utf8_lossy(&name.0));
                out.push_str(" = ");
            }
            let written = default_value_string(req, &argument.value);
            out.push_str(&written);
            out.push_str(" ]\n");
        }
        out.push_str("  }\n}\n");
        PhpString::new(req, out.as_bytes())
    }

    #[php(private, implementation = crate::func::ReflectionFunctionAbstract::__clone)]
    fn __clone(this: This<Self>);

    /// `ReflectionAttribute::__construct()`: private in the stub and
    /// reachable through reflection, where php refuses it.
    ///
    /// ```php
    /// #[Attribute] class A {}
    /// #[A] class C {}
    /// $a = (new ReflectionClass('C'))->getAttributes()[0];
    /// try { (new ReflectionMethod('ReflectionAttribute', '__construct'))->invoke($a); } catch (ReflectionException $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// Cannot directly instantiate ReflectionAttribute
    /// ```
    #[php(private, arity = manual)]
    fn __construct(req: &mut Request, _: This<Self>) -> Result<(), Thrown> {
        let ce = crate::throwables::Throwables::of(req)?;
        Err(throw(
            req,
            ce.reflection_exception,
            "Cannot directly instantiate ReflectionAttribute",
        ))
    }
}
