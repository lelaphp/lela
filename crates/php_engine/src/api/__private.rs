//! What only a macro expansion names (design §4.4, revision 34): the
//! pieces `#[php_function]`, `#[php_class]` and `#[php_const]` build a
//! declaration and an entry from. Listed in `api.lock` like every other
//! item and held to a cap of its own; an extension's source never names
//! `__private` (`ext.no-private`).

use php_unit::decl::{
    Attribute, AttributeArgument, ClassDecl, ClassKind, ConstExpr, FunctionDecl, MethodDecl, Name,
    Parameter, ParameterDefault, PropertyDecl, Type, Visibility,
};

use crate::api::abi::{Cases, Class, ClassRef, ExtensionBuilder, ObjectHandlers};
use crate::api::raising::{Thrown, arg_count_error, fatal};
use crate::api::request::Request;
use crate::api::values::{ClassId, Value};

/// A native's entry: the concrete request, off which it reads its
/// arguments as `ZEND_FUNCTION` reads them off `execute_data`, and the
/// value or the pending throwable (ADR-0002). The call frame holding the
/// arguments stays on the VM stack while the entry runs, which is what
/// lets a backtrace taken inside it print them.
pub type Entry = fn(&mut Request) -> Result<Value, Thrown>;

/// A function declaration named `name`, the rest for the expansion to
/// fill.
#[must_use]
pub fn native_function(name: &[u8]) -> FunctionDecl {
    FunctionDecl {
        name: Name(name.into()),
        parameters: Vec::new(),
        return_type: None,
        returns_by_ref: false,
        is_generator: false,
        attributes: Vec::new(),
        doc_comment: None,
        file: Name(Box::new([])),
        line: 0,
        end_line: 0,
        frameless: Vec::new(),
    }
}

/// A parameter named `name` of type `ty`, required and by value.
#[must_use]
pub fn parameter(name: &[u8], ty: Option<Type>) -> Parameter {
    Parameter {
        name: Name(name.into()),
        ty,
        by_ref: false,
        prefer_ref: false,
        promoted: false,
        variadic: false,
        default: ParameterDefault::None,
        attributes: Vec::new(),
    }
}

/// A public instance property named `name` of type `ty`, no default.
#[must_use]
pub fn property(name: &[u8], ty: Option<Type>) -> PropertyDecl {
    PropertyDecl {
        name: Name(name.into()),
        visibility: Visibility::Public,
        set_visibility: None,
        is_instance: true,
        is_readonly: false,
        is_final: false,
        is_promoted: false,
        is_virtual: false,
        ty,
        default: None,
        attributes: Vec::new(),
        doc_comment: None,
        hooks: None,
    }
}

/// A class declaration named `name` of `kind`, the rest for the
/// expansion to fill.
#[must_use]
pub fn class(name: &[u8], kind: ClassKind) -> ClassDecl {
    ClassDecl {
        name: Name(name.into()),
        kind,
        flags: php_unit::decl::ClassFlags::default(),
        parent: None,
        interfaces: Vec::new(),
        traits: Vec::new(),
        adaptations: Vec::new(),
        backing: php_unit::decl::EnumBacking::default(),
        constants: Vec::new(),
        properties: Vec::new(),
        methods: Vec::new(),
        attributes: Vec::new(),
        doc_comment: None,
        file: Name(Box::new([])),
        line: 0,
        end_line: 0,
    }
}

/// A php attribute, `#[\Deprecated(since: "8.1")]`, its arguments as
/// written.
#[must_use]
pub fn attribute(name: &[u8], arguments: Vec<(Option<&[u8]>, ConstExpr)>) -> Attribute {
    Attribute {
        name: Name(name.into()),
        arguments: arguments
            .into_iter()
            .map(|(name, value)| AttributeArgument {
                name: name.map(|name| Name(name.into())),
                value,
            })
            .collect(),
    }
}

/// `zend_wrong_parameters_count_error` (`Z/Zend/zend_API.c:231`): the
/// `ArgumentCountError` of a call given fewer than `min` or more than
/// `max` arguments, in php's three sentences.
///
/// ```php
/// foreach ([fn () => strlen(), fn () => str_repeat('a'), fn () => array_key_first([], 1), fn () => min(), fn () => mt_srand(1, 2, 3)] as $f) {
///     try { $f(); } catch (ArgumentCountError $e) { echo $e->getMessage(), "\n"; }
/// }
/// ```
/// ```expect
/// strlen() expects exactly 1 argument, 0 given
/// str_repeat() expects exactly 2 arguments, 1 given
/// array_key_first() expects exactly 1 argument, 2 given
/// min() expects at least 1 argument, 0 given
/// mt_srand() expects at most 2 arguments, 3 given
/// ```
pub fn arity(req: &mut Request, min: usize, max: Option<usize>) -> Result<(), Thrown> {
    let given = req.argument_count();
    if given >= min && max.is_none_or(|max| given <= max) {
        return Ok(());
    }
    let (word, bound) = match max {
        Some(max) if max == min => ("exactly", min),
        _ if given < min => ("at least", min),
        Some(max) => ("at most", max),
        None => ("at least", min),
    };
    let plural = if bound == 1 { "" } else { "s" };
    Err(arg_count_error(
        req,
        &format!("{word} {bound} argument{plural}, {given} given"),
    ))
}

/// The entry of an abstract or interface method, which the engine never
/// serves: a call reaches the class that implements it.
pub fn abstract_entry(req: &mut Request) -> Result<Value, Thrown> {
    Err(fatal(req, "InternalError: an abstract method served"))
}

fn linked<E: Class>(req: &mut Request) -> Result<ClassId, Thrown> {
    match req.class_named(E::NAME) {
        Some(class) => Ok(class),
        None => Err(fatal(req, "InternalError: a declared enum is not linked")),
    }
}

/// The case object of a Rust variant, `zend_enum_get_case`, with an owner
/// for the caller: the one instance the case has, by identity.
///
/// ```php
/// var_dump(RoundingMode::cases()[2] === RoundingMode::HalfEven, round(2.5, 0, RoundingMode::HalfEven), round(2.5, 0, RoundingMode::HalfAwayFromZero));
/// ```
/// ```expect
/// bool(true)
/// float(2)
/// float(3)
/// ```
pub fn case_value<E: Cases>(req: &mut Request, variant: E) -> Result<Value, Thrown> {
    let class = linked::<E>(req)?;
    let Some((name, _)) = E::CASES.iter().find(|(_, case)| *case == variant) else {
        return Err(fatal(req, "InternalError: a variant that is no case"));
    };
    let value = req.class_constant(class, name)?;
    req.addref(&value);
    Ok(value)
}

/// The Rust variant a case object is, by identity.
pub fn case_of<E: Cases>(req: &mut Request, value: &Value) -> Result<E, Thrown> {
    let class = linked::<E>(req)?;
    for (name, variant) in E::CASES {
        let case = req.class_constant(class, name)?;
        if crate::api::coerce::identical(&case, value) {
            return Ok(*variant);
        }
    }
    Err(fatal(
        req,
        "InternalError: an object that is no case of its enum",
    ))
}

/// A class whose objects carry `C::Payload`, laid down by
/// [`Class::new_payload`] for the class `new` names: php's
/// `create_object` beside the handler table.
#[must_use]
pub fn class_new<C: Class>(
    builder: ExtensionBuilder,
    class: ClassDecl,
    methods: Vec<(MethodDecl, Entry)>,
    handlers: &'static dyn ObjectHandlers,
) -> ExtensionBuilder {
    builder.class_created(class, methods, handlers, new_by::<C>)
}

fn new_by<C: Class>(
    heap: &mut php_heap::Heap,
    class: u32,
    slots: u32,
) -> Result<php_heap::Obj, php_heap::Fatal> {
    heap.object_with(class, slots, C::new_payload(ClassRef(ClassId(class))))
}

/// The entry of a native on its way in, named as the expansion names it:
/// the crate, the `#[php_class]` type or `None`, the Rust identifier.
/// Recorded where the request was served traced, for `xtask witness`'s
/// coverage (harness §4.5 condition 2, revision 58); in a build without
/// `php_engine`'s `witness-trace` feature it does nothing and compiles to
/// nothing, so the host's binary pays no branch for it.
#[inline]
pub fn entered(
    req: &mut Request,
    krate: &'static str,
    owner: Option<&'static str>,
    name: &'static str,
) {
    #[cfg(feature = "witness-trace")]
    req.entered(crate::test::Entered { krate, owner, name });
    #[cfg(not(feature = "witness-trace"))]
    let _ = (req, krate, owner, name);
}
