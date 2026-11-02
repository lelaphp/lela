//! What only a macro expansion names (design §4.4, revision 34): the
//! pieces `#[php_function]`, `#[php_class]` and `#[php_const]` build a
//! declaration and an entry from. Listed in `api.lock` like every other
//! item and held to a cap of its own; an extension's source never names
//! `__private` (`ext.no-private`).

use php_unit::decl::{
    Attribute, AttributeArgument, Bare, ClassDecl, ClassKind, ConstExpr, FunctionDecl, MethodDecl,
    Name, Parameter, ParameterDefault, PropertyDecl, Type, Visibility,
};

use crate::api::abi::{Cases, Class, ClassRef, ExtensionBuilder, ObjectHandlers};
use crate::api::raising::{Thrown, fatal};
use crate::api::request::Request;
use crate::api::values::{ClassId, Owner, Value, ValueRef};

/// A native's entry: the concrete request, off which it reads its
/// arguments as `ZEND_FUNCTION` reads them off `execute_data`, and the
/// value's owner, the engine's, or the pending throwable (ADR-0002). The call frame holding the
/// arguments stays on the VM stack while the entry runs, which is what
/// lets a backtrace taken inside it print them.
pub type Entry = fn(&mut Request) -> Result<Owner<Value>, Thrown>;

/// A native function as `#[php_function]` lays it down: a `const` of
/// static data, php's `zend_function_entry` with its
/// `zend_internal_arg_info` rows, which the engine reads once as it
/// registers the function and builds the declaration model of
/// ([`NativeFunction::declaration`]), as
/// `zend_register_functions` walks an extension's entry array. What is
/// a tree in the model, a declared type, a default expression, a php
/// attribute, is the function that builds it: a type's is its Rust
/// type's `FromArgument::declared` or `IntoReturn::declared`, one per
/// type and shared by every native that names it, and a default's or an
/// attribute's is the expansion's own.
#[derive(Debug, Clone, Copy)]
pub struct NativeFunction {
    /// The PHP name, namespace included.
    pub name: &'static [u8],
    pub parameters: &'static [NativeParameter],
    pub return_type: fn() -> Option<Type>,
    /// `zend_frameless_function_info`'s arities ([`FunctionDecl::frameless`]).
    pub frameless: &'static [u32],
    pub attributes: &'static [fn() -> Attribute],
    pub entry: Entry,
}

/// One parameter of a [`NativeFunction`], php's `zend_internal_arg_info`:
/// the facts of its Rust type (`FromArgument`'s `BY_REF`, `VARIADIC` and
/// `PASSED`) and of its `#[php(..)]`.
#[derive(Debug, Clone, Copy)]
#[allow(clippy::struct_excessive_bools)] // the ZEND_SEND_* bits of a slot and its type's `Passed`, each one fact
pub struct NativeParameter {
    pub name: &'static [u8],
    pub ty: fn() -> Option<Type>,
    pub by_ref: bool,
    pub prefer_ref: bool,
    pub variadic: bool,
    /// The type is `Passed`: optional, its absence the body's to see,
    /// which the declaration shows as `Unshown` where no default is
    /// written.
    pub passed: bool,
    /// The `#[php(default = ..)]` expression.
    pub default: Option<fn() -> ConstExpr>,
    pub attributes: &'static [fn() -> Attribute],
}

/// A native method as `#[php_class]` lays it down on its member impl:
/// a [`NativeFunction`] with the `ZEND_ACC_*` bits php's method entry
/// carries ([`MethodDecl`]).
#[derive(Debug, Clone, Copy)]
#[allow(clippy::struct_excessive_bools)] // the ZEND_ACC_* bits of a method, each one fact
pub struct NativeMethod {
    pub function: NativeFunction,
    pub visibility: Visibility,
    pub is_instance: bool,
    pub is_abstract: bool,
    pub is_final: bool,
    pub tentative_return: bool,
}

impl NativeFunction {
    /// The declaration model of the function, built as
    /// `zend_register_functions` builds a `zend_internal_function` from
    /// its entry: no file, line zero, by value.
    pub(crate) fn declaration(&self) -> FunctionDecl {
        FunctionDecl {
            parameters: (self.parameters.iter())
                .map(NativeParameter::declaration)
                .collect(),
            return_type: (self.return_type)(),
            attributes: self
                .attributes
                .iter()
                .map(|attribute| attribute())
                .collect(),
            frameless: self.frameless.to_vec(),
            ..php_unit::decl::native_function(self.name)
        }
    }
}

impl NativeParameter {
    fn declaration(&self) -> Parameter {
        let default = match self.default {
            Some(expr) => ParameterDefault::Expr(expr()),
            None if self.passed => ParameterDefault::Unshown,
            None => ParameterDefault::None,
        };
        Parameter {
            by_ref: self.by_ref,
            prefer_ref: self.prefer_ref,
            variadic: self.variadic,
            default,
            attributes: self
                .attributes
                .iter()
                .map(|attribute| attribute())
                .collect(),
            ..php_unit::decl::parameter(self.name, (self.ty)())
        }
    }
}

impl NativeMethod {
    /// The method's declaration model and its entry.
    pub(crate) fn declaration(&self) -> (MethodDecl, Entry) {
        let method = MethodDecl {
            function: self.function.declaration(),
            visibility: self.visibility,
            is_instance: self.is_instance,
            is_abstract: self.is_abstract,
            is_final: self.is_final,
            tentative_return: self.tentative_return,
        };
        (method, self.function.entry)
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
        implementors: Vec::new(),
        aliases: Vec::new(),
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
        object_size: 0,
        object_beside: Box::default(),
        attribute_validator: None,
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
        line: 0,
        strict: false,
        own_name: true,
    }
}

/// What a declared native's entry reads before its body,
/// `ZEND_PARSE_PARAMETERS_START(min, max)`'s: the call being served read
/// once, its count held to `min..=max` by [`raising::arity`]'s sentence,
/// the one place it is written, which an `arity = manual` native calls
/// from its body after its own first step (its entry asks `0..`, which
/// nothing refuses); and the first `N` arguments as the call passed
/// them, a reference not followed and `UNDEF` past what it was given, as
/// `Z_PARAM_*` steps a pointer over `ZEND_CALL_ARG(execute_data, 1)`.
/// Each is a view the call keeps one owner of until it is popped.
///
/// [`raising::arity`]: crate::api::raising::arity
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
#[inline(always)] // every native's entry reads its call here, once
pub fn arguments<const N: usize>(
    req: &mut Request,
    min: usize,
    max: Option<usize>,
) -> Result<[Value; N], Thrown> {
    let (given, arguments) = req.arguments_given::<N>();
    if given < min || max.is_some_and(|max| given > max) {
        crate::api::raising::arity_of(req, given, min, max)?;
    }
    Ok(arguments)
}

/// The entry of an abstract or interface method, which the engine never
/// serves: a call reaches the class that implements it.
pub fn abstract_entry(req: &mut Request) -> Result<Owner<Value>, Thrown> {
    Err(fatal(req, "InternalError: an abstract method served"))
}

fn linked<E: Class>(req: &mut Request) -> Result<ClassId, Thrown> {
    match req.class_named(Bare::resolved(E::NAME)) {
        Some(class) => Ok(class),
        None => Err(fatal(req, "InternalError: a declared enum is not linked")),
    }
}

/// The case object of a Rust variant, `zend_enum_get_case`, with an owner
/// for the caller, the one `RETURN_OBJ_COPY` takes: the one instance the
/// case has, by identity.
///
/// ```php
/// var_dump(RoundingMode::cases()[2] === RoundingMode::HalfEven, round(2.5, 0, RoundingMode::HalfEven), round(2.5, 0, RoundingMode::HalfAwayFromZero));
/// ```
/// ```expect
/// bool(true)
/// float(2)
/// float(3)
/// ```
pub fn case_value<E: Cases>(req: &mut Request, variant: E) -> Result<Owner<Value>, Thrown> {
    let class = linked::<E>(req)?;
    let Some((name, _)) = E::CASES.iter().find(|(_, case)| *case == variant) else {
        return Err(fatal(req, "InternalError: a variant that is no case"));
    };
    let case = req.enum_case(class, name)?;
    Ok(Owner::adopt(case.as_value()))
}

/// The Rust variant a case object of `E` is, read off its `name` as
/// `zend_enum_fetch_case_name` reads it: a view, no owner taken and no
/// other case made, so the object a program makes next is numbered as
/// php numbers it:
///
/// ```php
/// $mode = RoundingMode::HalfOdd;
/// var_dump(round(2.5, 0, $mode), new stdClass);
/// ```
/// ```expect
/// float(3)
/// object(stdClass)#2 (0) {
/// }
/// ```
pub fn case_of<E: Cases>(req: &mut Request, value: &Value) -> Result<E, Thrown> {
    let class = linked::<E>(req)?;
    if let ValueRef::Object(case) = value.deref().as_ref()
        && case.class() == class
        && let Some(name) = case.prop(req, b"name")
        && let ValueRef::Str(name) = name.as_ref()
        && let Some((_, variant)) = E::CASES.iter().find(|(held, _)| *held == name.as_bytes())
    {
        return Ok(*variant);
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
    custom: usize,
) -> Result<php_heap::Obj, php_heap::Fatal> {
    heap.object_with(
        class,
        slots,
        custom,
        C::new_payload(ClassRef(ClassId(class))),
    )
}

/// The entry of a native on its way in, named as the expansion names it:
/// the crate, the `#[php_class]` type or `None`, the Rust identifier.
/// Recorded where the request was served traced, for `xtask witness`'s
/// coverage (harness §4.5 condition 2, revision 58); elsewhere it is one
/// branch on a field the request holds as `None` (#2168).
#[inline]
pub fn entered(
    req: &mut Request,
    krate: &'static str,
    owner: Option<&'static str>,
    name: &'static str,
) {
    if req.tracing_natives() {
        req.entered(crate::trace::Entered { krate, owner, name });
    }
}
