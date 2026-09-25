//! The `libxml` extension: `php-src/ext/libxml` as one crate under `ext/`
//! (ADR-0004). One dependency beside the facade, the platform's libxml2,
//! bound in [`ffi`] alone; its state on the value (ADR-0005). The phpt
//! subset is `php-src/ext/libxml/tests`, through `xtask phpt --subset`.
//!
//! Eight natives, the `LIBXML_*` constants and `LibXMLError`, over the
//! per-request error state every XML extension reads: `LIBXML(error_list)`,
//! the list `libxml_use_internal_errors(true)` keeps, and the buffer the
//! handlers join a message in. Beside `extension()` the crate exports
//! what `php_libxml.h` declares `PHPAPI` to the extensions that include
//! it (design §4.10): [`ffi`]'s node objects, frees and serialisers, and
//! [`report`], [`issue_error`] and [`uses_internal_errors`] over the
//! errors [`ffi::capture`] copied out of a call.
#![deny(unsafe_code)]
// seams.toml rule lints.no-panic: Internal invariant failures are InternalError fatals with a message, not panics (design §10).
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::todo,
    clippy::unimplemented
)]
// The `expect` fence is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

pub mod ffi;

use php_ext::abi::{Callable, Dependency, DependencyKind, Extension, ExtensionBuilder};
use php_ext::coerce::{argument, callable_argument, value_name};
use php_ext::decl::{
    Attribute, AttributeArgument, ClassDecl, ClassFlags, ClassKind, ConstDecl, ConstExpr,
    EnumBacking, FunctionDecl, Name, Parameter, ParameterDefault, PropertyDecl, Type, Visibility,
};
use php_ext::raising::{Level, Thrown, arg_count_error, arg_type_error, docref, throw};
use php_ext::request::Request;
use php_ext::values::{PhpArray, PhpString, Value};

use ffi::{Raised, Route, sys};

/// Request-scoped: built at request start, dropped after `rshutdown`,
/// private to this crate (ADR-0005); nothing in it is shared or
/// synchronised. php's `zend_libxml_globals`.
#[derive(Default)]
struct State {
    /// `LIBXML(error_list)`: the list, while internal errors are on.
    errors: Option<Vec<Raised>>,
    /// `LIBXML(error_buffer)`: a message the handlers joined until its
    /// newline came.
    buffer: Vec<u8>,
    /// `LIBXML(entity_loader_callback)`, and the value it was read off,
    /// held with one owner.
    entity_loader: Option<(Callable, Value)>,
    /// `LIBXML(entity_loader_disabled)`.
    entity_loader_disabled: bool,
    /// `LIBXML(stream_context)`, held with one owner.
    stream_context: Option<Value>,
}

/// A native's entry, the shape `ExtensionBuilder::functions` takes.
type Entry = fn(&mut Request) -> Result<Value, Thrown>;

/// The extension value (design §7.1): declarations paired with entries,
/// and the state declared on it.
///
/// ```php
/// var_dump(extension_loaded('libxml'), LIBXML_VERSION, LIBXML_DOTTED_VERSION, LIBXML_NOEMPTYTAG);
/// print_r((new ReflectionExtension('libxml'))->getDependencies());
/// ```
/// ```expect
/// bool(true)
/// int(20913)
/// string(6) "2.9.13"
/// int(4)
/// Array
/// (
///     [standard] => Required
/// )
/// ```
pub fn extension() -> Extension {
    ffi::Library::init();
    ExtensionBuilder::default()
        .functions(vec![
            (
                function(
                    b"libxml_set_streams_context",
                    vec![parameter(b"context", None, None)],
                    Type::Void,
                ),
                set_streams_context as Entry,
            ),
            (
                function(
                    b"libxml_use_internal_errors",
                    vec![parameter(
                        b"use_errors",
                        Some(Type::Nullable(Box::new(Type::Bool))),
                        Some(ConstExpr::Null),
                    )],
                    Type::Bool,
                ),
                use_internal_errors as Entry,
            ),
            (
                function(
                    b"libxml_get_last_error",
                    Vec::new(),
                    Type::Union(vec![
                        Type::Named(Name(b"LibXMLError".as_slice().into())),
                        Type::False,
                    ]),
                ),
                get_last_error as Entry,
            ),
            (
                function(b"libxml_get_errors", Vec::new(), Type::Array),
                get_errors as Entry,
            ),
            (
                function(b"libxml_clear_errors", Vec::new(), Type::Void),
                clear_errors as Entry,
            ),
            (
                FunctionDecl {
                    attributes: vec![Attribute {
                        name: Name(b"Deprecated".as_slice().into()),
                        arguments: vec![
                            named(b"since", b"8.0"),
                            named(
                                b"message",
                                b"as external entity loading is disabled by default",
                            ),
                        ],
                    }],
                    ..function(
                        b"libxml_disable_entity_loader",
                        vec![parameter(
                            b"disable",
                            Some(Type::Bool),
                            Some(ConstExpr::Bool(true)),
                        )],
                        Type::Bool,
                    )
                },
                disable_entity_loader as Entry,
            ),
            (
                function(
                    b"libxml_set_external_entity_loader",
                    vec![parameter(
                        b"resolver_function",
                        Some(Type::Nullable(Box::new(Type::Callable))),
                        None,
                    )],
                    Type::True,
                ),
                set_external_entity_loader as Entry,
            ),
            (
                function(
                    b"libxml_get_external_entity_loader",
                    Vec::new(),
                    Type::Nullable(Box::new(Type::Callable)),
                ),
                get_external_entity_loader as Entry,
            ),
        ])
        .constants(constants())
        .classes(vec![(error_class(), Vec::new())])
        .dependencies(vec![Dependency {
            name: String::from("standard"),
            kind: DependencyKind::Required,
            rel: None,
            version: None,
        }])
        .request_state::<State>()
        .rshutdown(rshutdown)
        .info(libxml_info)
        .build("libxml")
}

/// `PHP_MINFO_FUNCTION(libxml)`.
///
/// ```php
/// (new ReflectionExtension('libxml'))->info();
/// ```
/// ```expect
/// libxml
///
/// libXML support => active
/// libXML Compiled Version => 2.9.13
/// libXML Loaded Version => 20913
/// libXML streams => enabled
/// ```
fn libxml_info(req: &mut Request) {
    let loaded = String::from_utf8_lossy(&ffi::Library::loaded_version()).into_owned();
    req.info_table(&[
        &["libXML support", "active"],
        &["libXML Compiled Version", DOTTED_VERSION],
        &["libXML Loaded Version", &loaded],
        &["libXML streams", "enabled"],
    ]);
}

/// `PHP_RSHUTDOWN_FUNCTION(libxml)` and `php_libxml_post_deactivate`:
/// the loader and the stream given up, the list dropped and the
/// library's last error reset, so a thread's next request starts clean.
fn rshutdown(req: &mut Request) {
    let state = req.state::<State>();
    let loader = state.entity_loader.take();
    let context = state.stream_context.take();
    state.errors = None;
    state.buffer.clear();
    if let Some((_, value)) = loader {
        req.release(value);
    }
    if let Some(value) = context {
        req.release(value);
    }
    ffi::Raised::reset_last();
}

/// `LIBXML_DOTTED_VERSION` of the headers this binding was written to,
/// which the platform's library is (`build.rs` holds it to 2.9).
const DOTTED_VERSION: &str = "2.9.13";

/// `register_libxml_symbols`: the constants in `libxml.stub.php`'s order,
/// `LIBXML_NO_XXE` absent as it is below 2.13.
fn constants() -> Vec<ConstDecl> {
    let long = |name: &[u8], value: i32| ConstDecl {
        name: Name(name.into()),
        value: ConstExpr::Long(i64::from(value)),
        attributes: Vec::new(),
    };
    let string = |name: &[u8], value: &[u8]| ConstDecl {
        name: Name(name.into()),
        value: ConstExpr::Str(value.into()),
        attributes: Vec::new(),
    };
    vec![
        long(b"LIBXML_VERSION", 20913),
        string(b"LIBXML_DOTTED_VERSION", DOTTED_VERSION.as_bytes()),
        string(b"LIBXML_LOADED_VERSION", &ffi::Library::loaded_version()),
        long(b"LIBXML_RECOVER", sys::XML_PARSE_RECOVER),
        long(b"LIBXML_NOENT", sys::XML_PARSE_NOENT),
        long(b"LIBXML_DTDLOAD", sys::XML_PARSE_DTDLOAD),
        long(b"LIBXML_DTDATTR", sys::XML_PARSE_DTDATTR),
        long(b"LIBXML_DTDVALID", sys::XML_PARSE_DTDVALID),
        long(b"LIBXML_NOERROR", sys::XML_PARSE_NOERROR),
        long(b"LIBXML_NOWARNING", sys::XML_PARSE_NOWARNING),
        long(b"LIBXML_NOBLANKS", sys::XML_PARSE_NOBLANKS),
        long(b"LIBXML_XINCLUDE", sys::XML_PARSE_XINCLUDE),
        long(b"LIBXML_NSCLEAN", sys::XML_PARSE_NSCLEAN),
        long(b"LIBXML_NOCDATA", sys::XML_PARSE_NOCDATA),
        long(b"LIBXML_NONET", sys::XML_PARSE_NONET),
        long(b"LIBXML_PEDANTIC", sys::XML_PARSE_PEDANTIC),
        long(b"LIBXML_COMPACT", sys::XML_PARSE_COMPACT),
        long(b"LIBXML_NOXMLDECL", sys::XML_SAVE_NO_DECL),
        long(b"LIBXML_PARSEHUGE", sys::XML_PARSE_HUGE),
        long(b"LIBXML_BIGLINES", sys::XML_PARSE_BIG_LINES),
        long(b"LIBXML_NOEMPTYTAG", NOEMPTYTAG),
        long(b"LIBXML_SCHEMA_CREATE", 1),
        long(b"LIBXML_HTML_NOIMPLIED", sys::HTML_PARSE_NOIMPLIED),
        long(b"LIBXML_HTML_NODEFDTD", sys::HTML_PARSE_NODEFDTD),
        long(b"LIBXML_ERR_NONE", sys::XML_ERR_NONE),
        long(b"LIBXML_ERR_WARNING", sys::XML_ERR_WARNING),
        long(b"LIBXML_ERR_ERROR", sys::XML_ERR_ERROR),
        long(b"LIBXML_ERR_FATAL", sys::XML_ERR_FATAL),
    ]
}

/// `LIBXML_SAVE_NOEMPTYTAG`, php's own bit beside libxml's save options.
pub const NOEMPTYTAG: i32 = 1 << 2;

/// `class LibXMLError` as `libxml.stub.php` declares it.
fn error_class() -> ClassDecl {
    let property = |name: &[u8], ty: Type| PropertyDecl {
        name: Name(name.into()),
        visibility: Visibility::Public,
        set_visibility: None,
        is_instance: true,
        is_readonly: false,
        is_final: false,
        is_promoted: false,
        ty: Some(ty),
        default: None,
        attributes: Vec::new(),
        doc_comment: None,
    };
    ClassDecl {
        name: Name(b"LibXMLError".as_slice().into()),
        kind: ClassKind::Class,
        flags: ClassFlags::default(),
        parent: None,
        interfaces: Vec::new(),
        traits: Vec::new(),
        adaptations: Vec::new(),
        backing: EnumBacking::default(),
        constants: Vec::new(),
        properties: vec![
            property(b"level", Type::Int),
            property(b"code", Type::Int),
            property(b"column", Type::Int),
            property(b"message", Type::String),
            property(b"file", Type::String),
            property(b"line", Type::Int),
        ],
        methods: Vec::new(),
        attributes: Vec::new(),
        doc_comment: None,
        file: Name(Box::new([])),
        line: 0,
        end_line: 0,
    }
}

fn parameter(name: &[u8], ty: Option<Type>, default: Option<ConstExpr>) -> Parameter {
    Parameter {
        name: Name(name.into()),
        ty,
        by_ref: false,
        prefer_ref: false,
        promoted: false,
        variadic: false,
        default: default.map_or(ParameterDefault::None, ParameterDefault::Expr),
        attributes: Vec::new(),
    }
}

fn named(name: &[u8], value: &[u8]) -> AttributeArgument {
    AttributeArgument {
        name: Some(Name(name.into())),
        value: ConstExpr::Str(value.into()),
    }
}

fn function(name: &[u8], parameters: Vec<Parameter>, returns: Type) -> FunctionDecl {
    FunctionDecl {
        name: Name(name.into()),
        parameters,
        return_type: Some(returns),
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

/// `ZEND_PARSE_PARAMETERS_START(min, max)`'s count check.
fn arity(req: &mut Request, min: usize, max: usize) -> Result<(), Thrown> {
    let given = req.argument_count();
    if (min..=max).contains(&given) {
        return Ok(());
    }
    let (how, n) = if min == max {
        ("exactly", min)
    } else if given < min {
        ("at least", min)
    } else {
        ("at most", max)
    };
    let word = if n == 1 { "argument" } else { "arguments" };
    Err(arg_count_error(
        req,
        &format!("{how} {n} {word}, {given} given"),
    ))
}

// ---- the face php_libxml.h declares: the error state

/// `LIBXML(error_list)` and `LIBXML(error_buffer)`: the request's libxml
/// errors, as the extensions that include `php_libxml.h` raise and route
/// them.
pub struct Errors;

impl Errors {
    /// `php_libxml_uses_internal_errors`: whether `libxml_use_internal_errors`
    /// is on, so an error goes to the list and not to the program's page.
    pub fn uses_internal(req: &mut Request) -> bool {
        req.state::<State>().errors.is_some()
    }

    /// `php_libxml_issue_error`: a message php raises itself, onto the list
    /// with internal errors on and as a diagnostic of `level` otherwise.
    pub fn issue(req: &mut Request, level: Level, message: &str) {
        let state = req.state::<State>();
        if let Some(list) = state.errors.as_mut() {
            list.push(Raised::internal(message.as_bytes()));
            return;
        }
        docref(req, level, message);
    }

    /// The errors [`ffi::capture`] copied out of one call, routed as php's
    /// handlers would have met them: onto the list with internal errors on,
    /// `php_list_set_error_structure`; otherwise joined in the buffer until a
    /// message ends with its newline and raised then, as
    /// `php_libxml_internal_error_handler_ex` does, with the text and level
    /// of the handler the error reached.
    ///
    pub fn route(req: &mut Request, raised: Vec<Raised>) {
        for error in raised {
            let state = req.state::<State>();
            if let Some(list) = state.errors.as_mut() {
                list.push(error);
                continue;
            }
            // libxml's own handler writes its report in pieces, one call
            // of the generic handler per line, and php raises each.
            let pieces: Vec<Vec<u8>> = match error.route {
                Route::Silent => continue,
                Route::Default => error
                    .printed
                    .split_inclusive(|b| *b == b'\n')
                    .map(<[u8]>::to_vec)
                    .collect(),
                Route::Parser | Route::Generic => vec![error.message.clone()],
            };
            for piece in pieces {
                Self::piece(req, &error, &piece);
            }
        }
    }

    /// One call of php's handler: `text` joined to the buffer, and raised
    /// once the buffer ends with a newline.
    fn piece(req: &mut Request, error: &Raised, text: &[u8]) {
        let state = req.state::<State>();
        state.buffer.extend_from_slice(text);
        let mut end = state.buffer.len();
        while end > 0 && state.buffer[end - 1] == b'\n' {
            end -= 1;
        }
        if end == state.buffer.len() {
            return;
        }
        state.buffer.truncate(end);
        let message = String::from_utf8_lossy(&std::mem::take(&mut state.buffer)).into_owned();
        match error.route {
            Route::Parser => {
                let level = if error.level == ffi::Level::Warning {
                    Level::Notice
                } else {
                    Level::Warning
                };
                let file = error.file.as_deref().map_or_else(
                    || String::from("Entity"),
                    |f| String::from_utf8_lossy(f).into_owned(),
                );
                docref(
                    req,
                    level,
                    &format!("{message} in {file}, line: {}", error.line),
                );
            }
            Route::Generic | Route::Default | Route::Silent => {
                docref(req, Level::Warning, &message);
            }
        }
    }
}

// ---- the natives

/// `libxml_set_streams_context($context): void`: the stream kept for the
/// next load or save, a `stream-context` resource and nothing else.
///
/// ```php
/// libxml_set_streams_context(stream_context_create());
/// try { libxml_set_streams_context(1); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { libxml_set_streams_context(new stdClass); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { libxml_set_streams_context(fopen('php://memory', 'r')); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// libxml_set_streams_context(): Argument #1 ($context) must be of type resource, int given
/// libxml_set_streams_context(): Argument #1 ($context) must be of type resource, stdClass given
/// libxml_set_streams_context(): supplied resource is not a valid Stream-Context resource
/// ```
fn set_streams_context(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 1, 1)?;
    let given = argument(req, 1)?.unwrap_or_default();
    let Some(resource) = given.as_resource() else {
        let name = value_name(req, &given);
        return Err(arg_type_error(
            req,
            1,
            &format!("be of type resource, {name} given"),
        ));
    };
    // `zend_fetch_resource_ex(arg, "Stream-Context", le_stream_context)`.
    let is_context = resource.type_name(req) == Some(b"stream-context".as_slice());
    if !is_context {
        return Err(throw(
            req,
            "TypeError",
            "libxml_set_streams_context(): supplied resource is not a valid Stream-Context resource",
        ));
    }
    req.addref(&given);
    if let Some(old) = req.state::<State>().stream_context.replace(given) {
        req.release(old);
    }
    Ok(Value::null())
}

/// `libxml_use_internal_errors(?bool $use_errors = null): bool`: the
/// setting before the call, and the list laid down or dropped.
///
/// ```php
/// var_dump(libxml_use_internal_errors(), libxml_use_internal_errors(true), libxml_use_internal_errors(null), libxml_use_internal_errors(false), libxml_use_internal_errors());
/// ```
/// ```expect
/// bool(false)
/// bool(false)
/// bool(true)
/// bool(true)
/// bool(false)
/// ```
fn use_internal_errors(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 0, 1)?;
    let wanted = argument(req, 1)?.and_then(|v| v.as_bool());
    let state = req.state::<State>();
    let before = state.errors.is_some();
    match wanted {
        None => {}
        Some(false) => state.errors = None,
        Some(true) => {
            state.errors.get_or_insert_with(Vec::new);
        }
    }
    Ok(Value::bool(before))
}

/// `php_libxml_create_error_object`: a `LibXMLError` of one error.
fn error_object(req: &mut Request, error: &Raised) -> Result<Value, Thrown> {
    let Some(class) = php_ext::call::lookup_class(req, b"LibXMLError")? else {
        return Err(php_ext::raising::fatal(
            req,
            "InternalError: LibXMLError is not declared",
        ));
    };
    let object = php_ext::call::new_object(req, class)?;
    let rows: [(&[u8], Value); 6] = [
        (b"level", Value::int(error.level.number())),
        (b"code", Value::int(i64::from(error.code))),
        (b"column", Value::int(i64::from(error.column))),
        (
            b"message",
            Value::string(PhpString::new(req, &error.message)?),
        ),
        (
            b"file",
            Value::string(PhpString::new(
                req,
                error.file.as_deref().unwrap_or_default(),
            )?),
        ),
        (b"line", Value::int(i64::from(error.line))),
    ];
    for (name, value) in rows {
        let name = PhpString::new(req, name)?;
        object.set_prop(req, name, value)?;
    }
    Ok(object.as_value())
}

/// `libxml_get_last_error(): LibXMLError|false`: the list's last with
/// internal errors on, the library's own last otherwise.
///
/// ```php
/// var_dump(libxml_get_last_error());
/// libxml_use_internal_errors(true);
/// var_dump(libxml_get_last_error());
/// ```
/// ```expect
/// bool(false)
/// bool(false)
/// ```
fn get_last_error(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 0, 0)?;
    let last = match &req.state::<State>().errors {
        Some(list) => list.last().cloned(),
        None => ffi::Raised::last(),
    };
    match last {
        Some(error) => error_object(req, &error),
        None => Ok(Value::bool(false)),
    }
}

/// `libxml_get_errors(): array`: the list in the order the errors came,
/// empty with internal errors off.
///
/// ```php
/// var_dump(libxml_get_errors());
/// libxml_use_internal_errors(true);
/// var_dump(libxml_get_errors());
/// ```
/// ```expect
/// array(0) {
/// }
/// array(0) {
/// }
/// ```
fn get_errors(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 0, 0)?;
    let list = req.state::<State>().errors.clone().unwrap_or_default();
    let mut out = PhpArray::new(req)?;
    for error in &list {
        let object = error_object(req, error)?;
        out.append(req, object)?;
    }
    Ok(Value::array(out))
}

/// `libxml_clear_errors(): void`: the library's last error reset and the
/// list emptied.
///
/// ```php
/// libxml_use_internal_errors(true);
/// libxml_clear_errors();
/// var_dump(libxml_get_last_error(), count(libxml_get_errors()));
/// ```
/// ```expect
/// bool(false)
/// int(0)
/// ```
fn clear_errors(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 0, 0)?;
    ffi::Raised::reset_last();
    if let Some(list) = req.state::<State>().errors.as_mut() {
        list.clear();
    }
    Ok(Value::null())
}

/// `libxml_disable_entity_loader(bool $disable = true): bool`,
/// deprecated since 8.0: the setting before the call.
///
/// ```php
/// var_dump(libxml_disable_entity_loader(), libxml_disable_entity_loader(false));
/// ```
/// ```expect
/// Deprecated: Function libxml_disable_entity_loader() is deprecated since 8.0, as external entity loading is disabled by default in %s on line %d
///
/// Deprecated: Function libxml_disable_entity_loader() is deprecated since 8.0, as external entity loading is disabled by default in %s on line %d
/// bool(false)
/// bool(true)
/// ```
fn disable_entity_loader(req: &mut Request) -> Result<Value, Thrown> {
    php_ext::raising::deprecated(
        req,
        "Function libxml_disable_entity_loader() is deprecated since 8.0, as external entity loading is disabled by default",
    );
    arity(req, 0, 1)?;
    let disable = argument(req, 1)?.and_then(|v| v.as_bool()).unwrap_or(true);
    let state = req.state::<State>();
    let before = std::mem::replace(&mut state.entity_loader_disabled, disable);
    Ok(Value::bool(before))
}

/// `libxml_set_external_entity_loader(?callable $resolver_function):
/// true`: the loader kept, or dropped for `null`.
///
/// ```php
/// var_dump(libxml_set_external_entity_loader('strlen'), libxml_get_external_entity_loader());
/// var_dump(libxml_set_external_entity_loader(null), libxml_get_external_entity_loader());
/// ```
/// ```expect
/// bool(true)
/// string(6) "strlen"
/// bool(true)
/// NULL
/// ```
fn set_external_entity_loader(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 1, 1)?;
    let given = req.argument(1).unwrap_or_default().deref();
    let callable = callable_argument(req, 1)?;
    if let Some((_, old)) = req.state::<State>().entity_loader.take() {
        req.release(old);
    }
    if let Some(callable) = callable {
        req.addref(&given);
        req.state::<State>().entity_loader = Some((callable, given));
    }
    Ok(Value::bool(true))
}

/// `libxml_get_external_entity_loader(): ?callable`: the value the loader
/// was set from, or `null`.
///
/// ```php
/// $f = fn($p, $s, $c) => null;
/// libxml_set_external_entity_loader($f);
/// var_dump(libxml_get_external_entity_loader() === $f);
/// ```
/// ```expect
/// bool(true)
/// ```
fn get_external_entity_loader(req: &mut Request) -> Result<Value, Thrown> {
    arity(req, 0, 0)?;
    match req
        .state::<State>()
        .entity_loader
        .as_ref()
        .map(|(_, value)| *value)
    {
        Some(value) => {
            req.addref(&value);
            Ok(value)
        }
        None => Ok(Value::null()),
    }
}
