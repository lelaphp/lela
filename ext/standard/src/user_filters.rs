//! `Z/ext/standard/user_filters.c`, a stream filter written in PHP: the
//! `php_user_filter` class a program extends, `StreamBucket`, the
//! `stream_bucket_*` natives its `filter()` moves buckets with,
//! `stream_filter_register` and `stream_get_filters`; beside them the
//! three filter natives of `streamsfuncs.c`, `stream_filter_append`,
//! `stream_filter_prepend` and `stream_filter_remove`, and the
//! `PSFS_*` and `STREAM_FILTER_*` constants. The chains are the engine's
//! (`main/streams/filter.c`); a user filter is a [`StreamFilter`] of this
//! crate's over the object its factory made, and a brigade is handed to
//! `filter()` as a resource of php's `userfilter.bucket brigade` type
//! holding the engine's buckets for the call, a bucket as a
//! `userfilter.bucket` resource inside a `StreamBucket`.
//!
//! The filters registered by name and the two resource types are the
//! request's (`BG(user_filter_map)`, ADR-0005).
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use php_ext::call::{call_method, lookup_class, new_object, write_prop};
use php_ext::coerce::{is_callable, to_long, value_name};
use php_ext::decl::{
    ClassDecl, ClassFlags, ClassKind, ConstDecl, ConstExpr, EnumBacking, FunctionDecl, MethodDecl,
    Name, Parameter, ParameterDefault, PropertyDecl, Type, Visibility,
};
use php_ext::raising::{Level, Thrown, arg_type_error, arg_value_error, docref, throw};
use php_ext::request::{Brigade, FilterFlags, FilterStatus, Request, Stream, StreamFilter};
use php_ext::values::{
    PhpArray, PhpObject, PhpRef, PhpString, Resource, ResourceType, Value, ValueRef,
};

use crate::file::stream_argument;
use crate::output::Entry;
use crate::stub::stub;
use crate::zpp::optional_long;
use crate::{State, argument_count_between, constant, declared, parameter, string_argument};

/// `PSFS_*` (`Z/main/streams/php_stream_filter_api.h`).
const PSFS_ERR_FATAL: i64 = 0;
const PSFS_FEED_ME: i64 = 1;
const PSFS_PASS_ON: i64 = 2;

/// What the request keeps for user filters: each name
/// `stream_filter_register` took with its class's name, and the two
/// resource types, registered on first use.
#[derive(Default)]
pub(super) struct FilterState {
    map: Vec<(Vec<u8>, Vec<u8>)>,
    brigade_type: Option<ResourceType>,
    bucket_type: Option<ResourceType>,
}

/// A brigade as `filter()` holds it, the engine's buckets for the call.
struct BrigadeCell(Brigade);

/// A bucket out of its brigade, its bytes.
struct BucketCell(Vec<u8>);

/// The constants `user_filters.stub.php` and `file.stub.php` declare
/// for filters.
///
/// ```php
/// var_dump(PSFS_PASS_ON, PSFS_FEED_ME, PSFS_ERR_FATAL, PSFS_FLAG_NORMAL, PSFS_FLAG_FLUSH_INC, PSFS_FLAG_FLUSH_CLOSE, STREAM_FILTER_READ, STREAM_FILTER_WRITE, STREAM_FILTER_ALL);
/// ```
/// ```expect
/// int(2)
/// int(1)
/// int(0)
/// int(0)
/// int(1)
/// int(2)
/// int(1)
/// int(2)
/// int(3)
/// ```
pub(crate) fn constants() -> Vec<ConstDecl> {
    vec![
        constant(b"PSFS_PASS_ON", PSFS_PASS_ON),
        constant(b"PSFS_FEED_ME", PSFS_FEED_ME),
        constant(b"PSFS_ERR_FATAL", PSFS_ERR_FATAL),
        constant(b"PSFS_FLAG_NORMAL", 0),
        constant(b"PSFS_FLAG_FLUSH_INC", 1),
        constant(b"PSFS_FLAG_FLUSH_CLOSE", 2),
        constant(b"STREAM_FILTER_READ", 1),
        constant(b"STREAM_FILTER_WRITE", 2),
        constant(b"STREAM_FILTER_ALL", 3),
    ]
}

/// A public property of the two classes, with its type and default.
fn property(name: &[u8], ty: Option<Type>, default: Option<ConstExpr>) -> PropertyDecl {
    PropertyDecl {
        name: Name(name.into()),
        visibility: Visibility::Public,
        set_visibility: None,
        is_instance: true,
        is_readonly: false,
        is_final: false,
        is_promoted: false,
        ty,
        default,
        attributes: Vec::new(),
        doc_comment: None,
    }
}

/// A class of this file as its stub declares it.
fn class(name: &[u8], is_final: bool, properties: Vec<PropertyDecl>) -> ClassDecl {
    ClassDecl {
        name: Name(name.into()),
        kind: ClassKind::Class,
        flags: ClassFlags {
            is_final,
            ..ClassFlags::default()
        },
        parent: None,
        interfaces: Vec::new(),
        traits: Vec::new(),
        adaptations: Vec::new(),
        backing: EnumBacking::Pure,
        constants: Vec::new(),
        properties,
        methods: Vec::new(),
        attributes: Vec::new(),
        doc_comment: None,
        file: Name(Box::new([])),
        line: 0,
        end_line: 0,
    }
}

/// A public instance method with a tentative return type.
fn method(function: FunctionDecl) -> MethodDecl {
    MethodDecl {
        function,
        visibility: Visibility::Public,
        is_instance: true,
        is_abstract: false,
        is_final: false,
        tentative_return: true,
    }
}

/// `php_user_filter` and `StreamBucket`, as `user_filters.stub.php`
/// declares them.
///
/// ```php
/// echo new ReflectionClass('php_user_filter'), new ReflectionClass('StreamBucket');
/// ```
/// ```expect
/// Class [ <internal:standard> class php_user_filter ] {
///
///   - Constants [0] {
///   }
///
///   - Static properties [0] {
///   }
///
///   - Static methods [0] {
///   }
///
///   - Properties [3] {
///     Property [ public string $filtername = '' ]
///     Property [ public mixed $params = '' ]
///     Property [ public $stream = NULL ]
///   }
///
///   - Methods [3] {
///     Method [ <internal:standard> public method filter ] {
///
///       - Parameters [4] {
///         Parameter #0 [ <required> $in ]
///         Parameter #1 [ <required> $out ]
///         Parameter #2 [ <required> &$consumed ]
///         Parameter #3 [ <required> bool $closing ]
///       }
///       - Tentative return [ int ]
///     }
///
///     Method [ <internal:standard> public method onCreate ] {
///
///       - Parameters [0] {
///       }
///       - Tentative return [ bool ]
///     }
///
///     Method [ <internal:standard> public method onClose ] {
///
///       - Parameters [0] {
///       }
///       - Tentative return [ void ]
///     }
///   }
/// }
/// Class [ <internal:standard> final class StreamBucket ] {
///
///   - Constants [0] {
///   }
///
///   - Static properties [0] {
///   }
///
///   - Static methods [0] {
///   }
///
///   - Properties [4] {
///     Property [ public $bucket = NULL ]
///     Property [ public string $data ]
///     Property [ public int $datalen ]
///     Property [ public int $dataLength ]
///   }
///
///   - Methods [0] {
///   }
/// }
/// ```
pub(crate) fn classes() -> Vec<(ClassDecl, Vec<(MethodDecl, Entry)>)> {
    let filter = class(
        b"php_user_filter",
        false,
        vec![
            property(
                b"filtername",
                Some(Type::String),
                Some(ConstExpr::Str(b"".as_slice().into())),
            ),
            property(
                b"params",
                Some(Type::Mixed),
                Some(ConstExpr::Str(b"".as_slice().into())),
            ),
            property(b"stream", None, Some(ConstExpr::Null)),
        ],
    );
    let consumed = Parameter {
        by_ref: true,
        ..parameter(b"consumed", Type::Mixed)
    };
    let methods = vec![
        (
            method(declared(
                b"filter",
                &[
                    untyped(b"in"),
                    untyped(b"out"),
                    Parameter {
                        ty: None,
                        ..consumed
                    },
                    parameter(b"closing", Type::Bool),
                ],
                Type::Int,
            )),
            user_filter_filter as Entry,
        ),
        (
            method(declared(b"onCreate", &[], Type::Bool)),
            user_filter_on_create,
        ),
        (
            method(declared(b"onClose", &[], Type::Void)),
            user_filter_on_close,
        ),
    ];
    let bucket = class(
        b"StreamBucket",
        true,
        vec![
            property(b"bucket", None, Some(ConstExpr::Null)),
            property(b"data", Some(Type::String), None),
            property(b"datalen", Some(Type::Int), None),
            property(b"dataLength", Some(Type::Int), None),
        ],
    );
    vec![(filter, methods), (bucket, Vec::new())]
}

/// A parameter with no type.
fn untyped(name: &[u8]) -> Parameter {
    Parameter {
        ty: None,
        ..parameter(name, Type::Mixed)
    }
}

/// `php_user_filter::filter($in, $out, &$consumed, bool $closing): int`:
/// `"rrzb"` held, and `PSFS_ERR_FATAL`, what a filter that does not
/// override it answers.
fn user_filter_filter(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 4, 4)?;
    for position in [1, 2] {
        let given = req.argument(position).unwrap_or_default().deref();
        if given.as_resource().is_none() {
            let name = value_name(req, &given);
            return Err(arg_type_error(
                req,
                position,
                &format!("be of type resource, {name} given"),
            ));
        }
    }
    crate::bool_argument(req, 4)?;
    Ok(Value::int(PSFS_ERR_FATAL))
}

/// `php_user_filter::onCreate(): bool`: `true`.
fn user_filter_on_create(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 0, 0)?;
    Ok(Value::bool(true))
}

/// `php_user_filter::onClose(): void`.
fn user_filter_on_close(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 0, 0)?;
    Ok(Value::null())
}

/// The natives, as `user_filters.stub.php` and
/// `basic_functions.stub.php` declare them.
///
/// ```php
/// foreach (['stream_bucket_make_writeable', 'stream_bucket_prepend', 'stream_bucket_append', 'stream_bucket_new', 'stream_get_filters', 'stream_filter_register', 'stream_filter_prepend', 'stream_filter_append', 'stream_filter_remove'] as $f) echo new ReflectionFunction($f);
/// ```
/// ```expect
/// Function [ <internal:standard> function stream_bucket_make_writeable ] {
///
///   - Parameters [1] {
///     Parameter #0 [ <required> $brigade ]
///   }
///   - Return [ ?StreamBucket ]
/// }
/// Function [ <internal:standard> function stream_bucket_prepend ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <required> $brigade ]
///     Parameter #1 [ <required> StreamBucket $bucket ]
///   }
///   - Return [ void ]
/// }
/// Function [ <internal:standard> function stream_bucket_append ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <required> $brigade ]
///     Parameter #1 [ <required> StreamBucket $bucket ]
///   }
///   - Return [ void ]
/// }
/// Function [ <internal:standard> function stream_bucket_new ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <required> $stream ]
///     Parameter #1 [ <required> string $buffer ]
///   }
///   - Return [ StreamBucket ]
/// }
/// Function [ <internal:standard> function stream_get_filters ] {
///
///   - Parameters [0] {
///   }
///   - Return [ array ]
/// }
/// Function [ <internal:standard> function stream_filter_register ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <required> string $filter_name ]
///     Parameter #1 [ <required> string $class ]
///   }
///   - Return [ bool ]
/// }
/// Function [ <internal:standard> function stream_filter_prepend ] {
///
///   - Parameters [4] {
///     Parameter #0 [ <required> $stream ]
///     Parameter #1 [ <required> string $filter_name ]
///     Parameter #2 [ <optional> int $mode = 0 ]
///     Parameter #3 [ <optional> mixed $params = <default> ]
///   }
/// }
/// Function [ <internal:standard> function stream_filter_append ] {
///
///   - Parameters [4] {
///     Parameter #0 [ <required> $stream ]
///     Parameter #1 [ <required> string $filter_name ]
///     Parameter #2 [ <optional> int $mode = 0 ]
///     Parameter #3 [ <optional> mixed $params = <default> ]
///   }
/// }
/// Function [ <internal:standard> function stream_filter_remove ] {
///
///   - Parameters [1] {
///     Parameter #0 [ <required> $stream_filter ]
///   }
///   - Return [ bool ]
/// }
/// ```
pub(crate) fn natives() -> Vec<(FunctionDecl, Entry)> {
    let params = || Parameter {
        default: ParameterDefault::Unshown,
        ..parameter(b"params", Type::Mixed)
    };
    let apply = |name: &[u8]| {
        declared(
            name,
            &[
                untyped(b"stream"),
                parameter(b"filter_name", Type::String),
                Parameter {
                    default: ParameterDefault::Expr(ConstExpr::Long(0)),
                    ..parameter(b"mode", Type::Int)
                },
                params(),
            ],
            Type::Mixed,
        )
    };
    let mut prepend = apply(b"stream_filter_prepend");
    prepend.return_type = None;
    let mut append = apply(b"stream_filter_append");
    append.return_type = None;
    vec![
        (
            stub("stream_bucket_make_writeable($brigade): ?StreamBucket"),
            stream_bucket_make_writeable as Entry,
        ),
        (
            stub("stream_bucket_prepend($brigade, StreamBucket $bucket): void"),
            stream_bucket_prepend,
        ),
        (
            stub("stream_bucket_append($brigade, StreamBucket $bucket): void"),
            stream_bucket_append,
        ),
        (
            stub("stream_bucket_new($stream, string $buffer): StreamBucket"),
            stream_bucket_new,
        ),
        (stub("stream_get_filters(): array"), stream_get_filters),
        (
            stub("stream_filter_register(string $filter_name, string $class): bool"),
            stream_filter_register,
        ),
        (prepend, stream_filter_prepend),
        (append, stream_filter_append),
        (
            stub("stream_filter_remove($stream_filter): bool"),
            stream_filter_remove,
        ),
    ]
}

/// The two resource types, registered on first use.
fn brigade_type(req: &mut Request) -> ResourceType {
    if let Some(kind) = req.state::<State>().user_filters.brigade_type {
        return kind;
    }
    let kind = req
        .resources()
        .register_type(b"userfilter.bucket brigade", None);
    req.state::<State>().user_filters.brigade_type = Some(kind);
    kind
}

fn bucket_type(req: &mut Request) -> ResourceType {
    if let Some(kind) = req.state::<State>().user_filters.bucket_type {
        return kind;
    }
    let kind = req.resources().register_type(b"userfilter.bucket", None);
    req.state::<State>().user_filters.bucket_type = Some(kind);
    kind
}

/// A filter written in PHP (`userfilter_ops`): the object its factory
/// made, one owner.
struct UserFilter {
    object: PhpObject,
}

/// `zend_call_method_if_exists` of a method with no arguments.
fn call_if_exists(
    req: &mut Request,
    object: PhpObject,
    method: &[u8],
) -> Result<Option<Value>, Thrown> {
    let mut callable = PhpArray::new(req)?;
    req.addref(&object.as_value());
    callable.append(req, object.as_value())?;
    let name = Value::string(PhpString::new(req, method)?);
    callable.append(req, name)?;
    let callable = Value::array(callable);
    let exists = is_callable(req, &callable);
    req.release(callable);
    if !exists {
        return Ok(None);
    }
    call_method(req, &object, method, &[]).map(Some)
}

impl StreamFilter for UserFilter {
    /// `userfilter_filter` (`Z/ext/standard/user_filters.c:124`): the
    /// stream put in `$this->stream` where the object has the property;
    /// the two brigades as resources, `$consumed` by reference, and
    /// `$closing`, to `filter()`; its answer as an int, the consumed
    /// count read back, php's warning where input buckets were left, and
    /// `$this->stream` put back to `null`.
    fn filter(
        &mut self,
        req: &mut Request,
        stream: Stream,
        input: &mut Brigade,
        output: &mut Brigade,
        consumed: Option<&mut usize>,
        flags: FilterFlags,
    ) -> Result<FilterStatus, Thrown> {
        let object = self.object;
        let has_stream = object.prop(req, b"stream").is_some();
        if has_stream {
            let value = stream.as_value();
            req.addref(&value);
            write_prop(req, &object, b"stream", value)?;
        }
        let kind = brigade_type(req);
        let in_res = Resource::new(req, kind, BrigadeCell(std::mem::take(input)))?;
        let out_res = Resource::new(req, kind, BrigadeCell(std::mem::take(output)))?;
        let count = match &consumed {
            Some(n) => Value::int(i64::try_from(**n).unwrap_or(i64::MAX)),
            None => Value::null(),
        };
        let count = PhpRef::new(req, count)?;
        let closing = Value::bool(flags == FilterFlags::FlushClose);
        let args = [
            in_res.as_value(),
            out_res.as_value(),
            count.as_value(),
            closing,
        ];
        let called = call_method(req, &object, b"filter", &args);
        if let Some(cell) = in_res.native_mut::<BrigadeCell>(req) {
            *input = std::mem::take(&mut cell.0);
        }
        if let Some(cell) = out_res.native_mut::<BrigadeCell>(req) {
            *output = std::mem::take(&mut cell.0);
        }
        let status = match &called {
            Ok(value) => match to_long(req, value)? {
                PSFS_PASS_ON => FilterStatus::PassOn,
                PSFS_ERR_FATAL => FilterStatus::ErrFatal,
                _ => FilterStatus::FeedMe,
            },
            Err(_) => FilterStatus::ErrFatal,
        };
        if let Some(n) = consumed {
            let read = count.get();
            *n = usize::try_from(to_long(req, &read.deref())?).unwrap_or(0);
        }
        if !input.is_empty() {
            docref(
                req,
                Level::Warning,
                "Unprocessed filter buckets remaining on input brigade",
            );
        }
        if has_stream {
            write_prop(req, &object, b"stream", Value::null())?;
        }
        req.release(count.as_value());
        req.release(in_res.as_value());
        req.release(out_res.as_value());
        let value = called?;
        req.release(value);
        Ok(status)
    }

    /// `userfilter_dtor` (`Z/ext/standard/user_filters.c:103`):
    /// `onClose()` where the object has it, then the object given up.
    fn close(&mut self, req: &mut Request) -> Result<(), Thrown> {
        let object = self.object;
        let closed = call_if_exists(req, object, b"onclose");
        req.release(object.as_value());
        if let Some(value) = closed? {
            req.release(value);
        }
        Ok(())
    }
}

/// `user_filter_factory_create` (`Z/ext/standard/user_filters.c:231`):
/// the class registered under the name or, for a name nobody registered
/// as it is, under the nearest wildcard over it; php's warning where
/// the class is not defined; then an object of it, no constructor
/// called, its `filtername` and `params` set, and `onCreate()`, whose
/// `false` refuses the filter.
fn user_filter_factory(
    req: &mut Request,
    name: &[u8],
    params: Option<&Value>,
) -> Result<Option<Box<dyn StreamFilter>>, Thrown> {
    let map = &req.state::<State>().user_filters.map;
    let find = |wanted: &[u8]| {
        map.iter()
            .find(|(n, _)| n.as_slice() == wanted)
            .map(|(_, class)| class.clone())
    };
    let mut class_name = find(name);
    if class_name.is_none() {
        let mut wild = name.to_vec();
        while let Some(period) = wild.iter().rposition(|b| *b == b'.') {
            wild.truncate(period + 1);
            wild.push(b'*');
            if let Some(found) = find(&wild) {
                class_name = Some(found);
                break;
            }
            wild.truncate(period);
        }
    }
    let Some(class_name) = class_name else {
        return Ok(None);
    };
    let Some(class) = lookup_class(req, &class_name)? else {
        let text = format!(
            "User-filter \"{}\" requires class \"{}\", but that class is not defined",
            String::from_utf8_lossy(name),
            String::from_utf8_lossy(&class_name)
        );
        docref(req, Level::Warning, &text);
        return Ok(None);
    };
    let object = new_object(req, class)?;
    let filtername = Value::string(PhpString::new(req, name)?);
    write_prop(req, &object, b"filtername", filtername)?;
    let params = match params {
        Some(value) => {
            let value = value.deref();
            req.addref(&value);
            value
        }
        None => Value::null(),
    };
    write_prop(req, &object, b"params", params)?;
    match call_if_exists(req, object, b"oncreate") {
        Ok(Some(value)) if matches!(value.as_ref(), ValueRef::Bool(false)) => {
            req.release(object.as_value());
            Ok(None)
        }
        Ok(value) => {
            if let Some(value) = value {
                req.release(value);
            }
            Ok(Some(Box::new(UserFilter { object })))
        }
        Err(thrown) => {
            req.release(object.as_value());
            Err(thrown)
        }
    }
}

/// `Z_PARAM_RESOURCE` then `zend_fetch_resource` of a brigade: the
/// argument as a live `userfilter.bucket brigade`, or php's `TypeError`.
fn brigade_argument(req: &mut Request, native: &str) -> Result<Resource, Thrown> {
    let given = req.argument(1).unwrap_or_default().deref();
    let Some(res) = given.as_resource().copied() else {
        let name = value_name(req, &given);
        return Err(arg_type_error(
            req,
            1,
            &format!("be of type resource, {name} given"),
        ));
    };
    let kind = brigade_type(req);
    if res.kind() != Some(kind) {
        let text = format!(
            "{native}(): supplied resource is not a valid userfilter.bucket brigade resource"
        );
        return Err(throw(req, "TypeError", &text));
    }
    Ok(res)
}

/// A `StreamBucket` over a new `userfilter.bucket` resource of `bytes`,
/// its `data`, `datalen` and `dataLength` the bytes and their count.
fn bucket_object(req: &mut Request, bytes: Vec<u8>) -> Result<Value, Thrown> {
    let Some(class) = lookup_class(req, b"StreamBucket")? else {
        return Ok(Value::null());
    };
    let kind = bucket_type(req);
    let len = i64::try_from(bytes.len()).unwrap_or(i64::MAX);
    let data = Value::string(PhpString::new(req, &bytes)?);
    let res = Resource::new(req, kind, BucketCell(bytes))?;
    let object = new_object(req, class)?;
    write_prop(req, &object, b"bucket", res.as_value())?;
    write_prop(req, &object, b"data", data)?;
    write_prop(req, &object, b"datalen", Value::int(len))?;
    write_prop(req, &object, b"dataLength", Value::int(len))?;
    Ok(object.as_value())
}

/// `stream_bucket_make_writeable($brigade): ?StreamBucket`,
/// `PHP_FUNCTION(stream_bucket_make_writeable)`
/// (`Z/ext/standard/user_filters.c:345`): the brigade's first bucket
/// taken off it as a `StreamBucket`, and `null` for an empty brigade.
///
/// ```php
/// class upper extends php_user_filter {
///     public function filter($in, $out, &$consumed, bool $closing): int {
///         while ($bucket = stream_bucket_make_writeable($in)) {
///             echo get_class($bucket), ' ', $bucket->datalen, ' ', $bucket->dataLength, ' ', get_resource_type($bucket->bucket), "\n";
///             $bucket->data = strtoupper($bucket->data);
///             $consumed += $bucket->datalen;
///             stream_bucket_append($out, $bucket);
///         }
///         return PSFS_PASS_ON;
///     }
/// }
/// var_dump(stream_filter_register('upper', 'upper'));
/// $m = fopen('php://memory', 'w+'); fwrite($m, "abc\n"); rewind($m);
/// stream_filter_append($m, 'upper', STREAM_FILTER_READ);
/// var_dump(fgets($m));
/// try { stream_bucket_make_writeable($m); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// StreamBucket 4 4 userfilter.bucket
/// string(4) "ABC
/// "
/// stream_bucket_make_writeable(): supplied resource is not a valid userfilter.bucket brigade resource
/// ```
fn stream_bucket_make_writeable(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 1)?;
    let res = brigade_argument(req, "stream_bucket_make_writeable")?;
    let head = res
        .native_mut::<BrigadeCell>(req)
        .and_then(|cell| cell.0.pop_front());
    match head {
        Some(bytes) => bucket_object(req, bytes),
        None => Ok(Value::null()),
    }
}

/// `php_stream_bucket_attach` (`Z/ext/standard/user_filters.c:373`): the
/// bucket a `StreamBucket` holds, its bytes made the object's `data`,
/// put at the tail of the brigade or at its head.
fn bucket_attach(req: &mut Request, native: &str, append: bool) -> Result<Value, Thrown> {
    argument_count_between(req, 2, 2)?;
    let brigade = brigade_argument(req, native)?;
    let given = req.argument(2).unwrap_or_default().deref();
    let object = match given.as_object().copied() {
        Some(object)
            if lookup_class(req, b"StreamBucket")?.is_some_and(|c| object.instance_of(req, c)) =>
        {
            object
        }
        _ => {
            let name = value_name(req, &given);
            return Err(arg_type_error(
                req,
                2,
                &format!("be of type StreamBucket, {name} given"),
            ));
        }
    };
    let bucket = object.prop(req, b"bucket").map(|value| value.deref());
    let kind = bucket_type(req);
    let Some(res) = bucket
        .as_ref()
        .and_then(Value::as_resource)
        .copied()
        .filter(|res| res.kind() == Some(kind))
    else {
        let what = if bucket.as_ref().and_then(Value::as_resource).is_some() {
            "resource"
        } else {
            "argument"
        };
        let text = format!("{native}(): supplied {what} is not a valid userfilter.bucket resource");
        return Err(throw(req, "TypeError", &text));
    };
    if let Some(data) = object.prop(req, b"data").map(|value| value.deref())
        && let Some(bytes) = data.as_php_string().map(|s| s.as_bytes().to_vec())
        && let Some(cell) = res.native_mut::<BucketCell>(req)
    {
        cell.0 = bytes;
    }
    let bytes = res
        .native::<BucketCell>(req)
        .map(|cell| cell.0.clone())
        .unwrap_or_default();
    if let Some(cell) = brigade.native_mut::<BrigadeCell>(req) {
        if append {
            cell.0.push_back(bytes);
        } else {
            cell.0.push_front(bytes);
        }
    }
    Ok(Value::null())
}

/// `stream_bucket_prepend($brigade, StreamBucket $bucket): void`: the
/// bucket put at the brigade's head.
///
/// ```php
/// class twice extends php_user_filter {
///     public function filter($in, $out, &$consumed, bool $closing): int {
///         while ($bucket = stream_bucket_make_writeable($in)) {
///             $consumed += $bucket->datalen;
///             stream_bucket_append($out, $bucket);
///             stream_bucket_prepend($out, stream_bucket_new($this->stream, '<'));
///         }
///         return PSFS_PASS_ON;
///     }
/// }
/// stream_filter_register('twice', 'twice');
/// $m = fopen('php://memory', 'w+');
/// stream_filter_append($m, 'twice', STREAM_FILTER_WRITE);
/// var_dump(fwrite($m, 'ab'), fwrite($m, 'cd'));
/// rewind($m); var_dump(fread($m, 10));
/// try { stream_bucket_prepend(1, 1); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(2)
/// int(2)
/// string(6) "<ab<cd"
/// stream_bucket_prepend(): Argument #1 ($brigade) must be of type resource, int given
/// ```
fn stream_bucket_prepend(req: &mut Request) -> Result<Value, Thrown> {
    bucket_attach(req, "stream_bucket_prepend", false)
}

/// `stream_bucket_append($brigade, StreamBucket $bucket): void`: the
/// bucket put at the brigade's tail, what a filter hands on.
///
/// ```php
/// class same extends php_user_filter {
///     public function filter($in, $out, &$consumed, bool $closing): int {
///         while ($bucket = stream_bucket_make_writeable($in)) {
///             $bucket->data = "[$bucket->data]";
///             stream_bucket_append($out, $bucket);
///         }
///         return PSFS_PASS_ON;
///     }
/// }
/// stream_filter_register('same', 'same');
/// $m = fopen('php://memory', 'w+');
/// stream_filter_append($m, 'same', STREAM_FILTER_WRITE);
/// var_dump(fwrite($m, 'ab'));
/// rewind($m); var_dump(fread($m, 10));
/// ```
/// ```expect
/// int(0)
/// string(4) "[ab]"
/// ```
fn stream_bucket_append(req: &mut Request) -> Result<Value, Thrown> {
    bucket_attach(req, "stream_bucket_append", true)
}

/// `stream_bucket_new($stream, string $buffer): StreamBucket`,
/// `PHP_FUNCTION(stream_bucket_new)` (`Z/ext/standard/user_filters.c:432`):
/// a bucket of the bytes, on no brigade yet.
///
/// ```php
/// $bucket = stream_bucket_new(fopen('php://memory', 'r'), 'hello');
/// var_dump($bucket->data, $bucket->datalen, get_resource_type($bucket->bucket));
/// ```
/// ```expect
/// string(5) "hello"
/// int(5)
/// string(17) "userfilter.bucket"
/// ```
fn stream_bucket_new(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 2, 2)?;
    stream_argument(req, 1)?;
    let buffer = string_argument(req, 2)?.as_bytes().to_vec();
    bucket_object(req, buffer)
}

/// `stream_get_filters(): array`, `PHP_FUNCTION(stream_get_filters)`
/// (`Z/ext/standard/user_filters.c:460`): every filter name registered,
/// the standard ones first and the program's after. The oracle's list
/// has `zlib.*`, `bzip2.*` and `convert.iconv.*` of extensions this
/// engine does not have, and `convert.*`, `consumed` and `dechunk`,
/// which `filters.c` registers and this crate does not yet.
///
/// ```php
/// class noop extends php_user_filter {}
/// stream_filter_register('my.*', 'noop');
/// var_dump(array_values(array_intersect(stream_get_filters(), ['string.rot13', 'string.toupper', 'string.tolower', 'my.*'])));
/// ```
/// ```expect
/// array(4) {
///   [0]=>
///   string(12) "string.rot13"
///   [1]=>
///   string(14) "string.toupper"
///   [2]=>
///   string(14) "string.tolower"
///   [3]=>
///   string(4) "my.*"
/// }
/// ```
fn stream_get_filters(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 0, 0)?;
    let mut out = PhpArray::new(req)?;
    for name in req.stream_filters() {
        let value = Value::string(PhpString::new(req, &name)?);
        out.append(req, value)?;
    }
    Ok(Value::array(out))
}

/// `stream_filter_register(string $filter_name, string $class): bool`,
/// `PHP_FUNCTION(stream_filter_register)`
/// (`Z/ext/standard/user_filters.c:485`): neither name empty, php's
/// `ValueError` otherwise; the class's name kept under the filter's for
/// the request, and `false` where the filter's name is taken.
///
/// ```php
/// var_dump(stream_filter_register('mine', 'Missing'), stream_filter_register('mine', 'Other'), stream_filter_register('string.rot13', 'X'));
/// var_dump(stream_filter_append(fopen('php://memory', 'r'), 'mine'));
/// try { stream_filter_register('', 'X'); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { stream_filter_register('x', ''); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// bool(false)
///
/// Warning: stream_filter_append(): User-filter "mine" requires class "Missing", but that class is not defined in %s on line 3
///
/// Warning: stream_filter_append(): Unable to create or locate filter "mine" in %s on line 3
/// bool(false)
/// stream_filter_register(): Argument #1 ($filter_name) must be a non-empty string
/// stream_filter_register(): Argument #2 ($class) must be a non-empty string
/// ```
fn stream_filter_register(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 2, 2)?;
    let name = string_argument(req, 1)?.as_bytes().to_vec();
    let class = string_argument(req, 2)?.as_bytes().to_vec();
    if name.is_empty() {
        return Err(arg_value_error(req, 1, "be a non-empty string"));
    }
    if class.is_empty() {
        return Err(arg_value_error(req, 2, "be a non-empty string"));
    }
    let state = &mut req.state::<State>().user_filters;
    if state.map.iter().any(|(n, _)| *n == name) {
        return Ok(Value::bool(false));
    }
    state.map.push((name.clone(), class));
    if req.register_filter(&name, user_filter_factory) {
        return Ok(Value::bool(true));
    }
    req.state::<State>()
        .user_filters
        .map
        .retain(|(n, _)| *n != name);
    Ok(Value::bool(false))
}

/// `apply_filter_to_stream` (`Z/ext/standard/streamsfuncs.c:1208`): the
/// parameters, then the engine's attach.
fn apply_filter(req: &mut Request, append: bool) -> Result<Value, Thrown> {
    argument_count_between(req, 2, 4)?;
    let stream = stream_argument(req, 1)?;
    let name = string_argument(req, 2)?.as_bytes().to_vec();
    let read_write = optional_long(req, 3, 0)?;
    let params = crate::zpp::given(req, 4);
    match stream.add_filter(req, &name, params.as_ref(), read_write, append)? {
        Some(res) => Ok(res.as_value()),
        None => Ok(Value::bool(false)),
    }
}

/// `stream_filter_prepend($stream, string $filter_name, int $mode = 0,
/// mixed $params = UNKNOWN)`, `PHP_FUNCTION(stream_filter_prepend)`: the
/// filter at the head of the chains `$mode` names, or of those the
/// stream's `mode` reads and writes.
///
/// ```php
/// $m = fopen('php://memory', 'w+');
/// stream_filter_append($m, 'string.toupper', STREAM_FILTER_WRITE);
/// stream_filter_prepend($m, 'string.rot13', STREAM_FILTER_WRITE);
/// fwrite($m, 'Hello'); rewind($m);
/// var_dump(fread($m, 10));
/// ```
/// ```expect
/// string(5) "URYYB"
/// ```
fn stream_filter_prepend(req: &mut Request) -> Result<Value, Thrown> {
    apply_filter(req, false)
}

/// `stream_filter_append($stream, string $filter_name, int $mode = 0,
/// mixed $params = UNKNOWN)`, `PHP_FUNCTION(stream_filter_append)`: the
/// filter at the tail of the chains `$mode` names, or of those the
/// stream's `mode` reads and writes, a new filter for each; a filter
/// appended to the read chain winds what the read buffer holds through
/// it at once. The answer is a `stream filter` resource naming the last
/// filter attached, and `false` after php's warning for a name no
/// factory matched.
///
/// ```php
/// $m = fopen('php://memory', 'w+'); fwrite($m, "one\ntwo\n"); rewind($m);
/// var_dump(fgets($m));
/// $f = stream_filter_append($m, 'string.rot13');
/// var_dump($f, fgets($m));
/// var_dump(stream_filter_append($m, 'nope.filter'));
/// $w = fopen(__DIR__ . '/filter.txt', 'w'); stream_filter_append($w, 'string.toupper'); fwrite($w, "shout\n"); fclose($w);
/// var_dump(file_get_contents(__DIR__ . '/filter.txt'));
/// ```
/// ```expect
/// string(4) "one
/// "
/// resource(%d) of type (stream filter)
/// string(4) "gjb
/// "
///
/// Warning: stream_filter_append(): Unable to locate filter "nope.filter" in %s on line 6
/// bool(false)
/// string(6) "SHOUT
/// "
/// ```
fn stream_filter_append(req: &mut Request) -> Result<Value, Thrown> {
    apply_filter(req, true)
}

/// `stream_filter_remove($stream_filter): bool`,
/// `PHP_FUNCTION(stream_filter_remove)`
/// (`Z/ext/standard/streamsfuncs.c:1298`): the filter flushed and taken
/// off its chain, its resource closed; a resource of another type, or
/// one its stream's close already closed, is php's `TypeError`.
///
/// ```php
/// $m = fopen('php://memory', 'w+');
/// $f = stream_filter_append($m, 'string.toupper', STREAM_FILTER_WRITE);
/// fwrite($m, 'ab'); var_dump(stream_filter_remove($f), $f); fwrite($m, 'cd');
/// rewind($m); var_dump(fread($m, 10));
/// try { stream_filter_remove($f); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { stream_filter_remove($m); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// resource(%d) of type (Unknown)
/// string(4) "ABcd"
/// stream_filter_remove(): supplied resource is not a valid stream filter resource
/// stream_filter_remove(): supplied resource is not a valid stream filter resource
/// ```
fn stream_filter_remove(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 1)?;
    let given = req.argument(1).unwrap_or_default().deref();
    let Some(res) = given.as_resource().copied() else {
        let name = value_name(req, &given);
        return Err(arg_type_error(
            req,
            1,
            &format!("be of type resource, {name} given"),
        ));
    };
    match req.remove_filter(res)? {
        Some(removed) => Ok(Value::bool(removed)),
        None => Err(throw(
            req,
            "TypeError",
            "stream_filter_remove(): supplied resource is not a valid stream filter resource",
        )),
    }
}
