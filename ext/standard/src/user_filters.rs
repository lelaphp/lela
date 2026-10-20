//! `Z/ext/standard/user_filters.c`, a stream filter written in PHP: the
//! `php_user_filter` class a program extends, `StreamBucket`, the
//! `stream_bucket_*` natives its `filter()` moves buckets with,
//! `stream_filter_register` and `stream_get_filters`; beside them the
//! three filter natives of `streamsfuncs.c`, `stream_filter_append`,
//! `stream_filter_prepend` and `stream_filter_remove`, and the
//! `PSFS_*` constants, `file.stub.php`'s `STREAM_FILTER_*` being
//! `file::constants`. The chains are the engine's
//! (`main/streams/filter.c`); a user filter is a [`StreamFilter`] of this
//! crate's over the object its factory made, and a brigade is handed to
//! `filter()` as a resource of php's `userfilter.bucket brigade` type
//! holding the engine's buckets for the call, a bucket as a
//! `userfilter.bucket` resource inside a `StreamBucket`.
//!
//! The filters registered by name and the two resource types are the
//! request's (`BG(user_filter_map)`, ADR-0005).
//!
//! The natives, as `user_filters.stub.php` and
//! `basic_functions.stub.php` declare them.
//!
//! ```php
//! foreach (['stream_bucket_make_writeable', 'stream_bucket_prepend', 'stream_bucket_append', 'stream_bucket_new', 'stream_get_filters', 'stream_filter_register', 'stream_filter_prepend', 'stream_filter_append', 'stream_filter_remove'] as $f) echo new ReflectionFunction($f);
//! ```
//! ```expect
//! Function [ <internal:standard> function stream_bucket_make_writeable ] {
//!
//!   - Parameters [1] {
//!     Parameter #0 [ <required> $brigade ]
//!   }
//!   - Return [ ?StreamBucket ]
//! }
//! Function [ <internal:standard> function stream_bucket_prepend ] {
//!
//!   - Parameters [2] {
//!     Parameter #0 [ <required> $brigade ]
//!     Parameter #1 [ <required> StreamBucket $bucket ]
//!   }
//!   - Return [ void ]
//! }
//! Function [ <internal:standard> function stream_bucket_append ] {
//!
//!   - Parameters [2] {
//!     Parameter #0 [ <required> $brigade ]
//!     Parameter #1 [ <required> StreamBucket $bucket ]
//!   }
//!   - Return [ void ]
//! }
//! Function [ <internal:standard> function stream_bucket_new ] {
//!
//!   - Parameters [2] {
//!     Parameter #0 [ <required> $stream ]
//!     Parameter #1 [ <required> string $buffer ]
//!   }
//!   - Return [ StreamBucket ]
//! }
//! Function [ <internal:standard> function stream_get_filters ] {
//!
//!   - Parameters [0] {
//!   }
//!   - Return [ array ]
//! }
//! Function [ <internal:standard> function stream_filter_register ] {
//!
//!   - Parameters [2] {
//!     Parameter #0 [ <required> string $filter_name ]
//!     Parameter #1 [ <required> string $class ]
//!   }
//!   - Return [ bool ]
//! }
//! Function [ <internal:standard> function stream_filter_prepend ] {
//!
//!   - Parameters [4] {
//!     Parameter #0 [ <required> $stream ]
//!     Parameter #1 [ <required> string $filter_name ]
//!     Parameter #2 [ <optional> int $mode = 0 ]
//!     Parameter #3 [ <optional> mixed $params = <default> ]
//!   }
//! }
//! Function [ <internal:standard> function stream_filter_append ] {
//!
//!   - Parameters [4] {
//!     Parameter #0 [ <required> $stream ]
//!     Parameter #1 [ <required> string $filter_name ]
//!     Parameter #2 [ <optional> int $mode = 0 ]
//!     Parameter #3 [ <optional> mixed $params = <default> ]
//!   }
//! }
//! Function [ <internal:standard> function stream_filter_remove ] {
//!
//!   - Parameters [1] {
//!     Parameter #0 [ <required> $stream_filter ]
//!   }
//!   - Return [ bool ]
//! }
//! ```
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use php_ext::abi::{ConstValue, Instance, OrFalse, Passed, Ref, This, Untyped};
use php_ext::attr::{php_class, php_const, php_function};
use php_ext::call::{autoload, call_method, lookup_class, new_object, write_prop};
use php_ext::coerce::{is_callable, to_long};
use php_ext::raising::{Level, Thrown, arg_value_error, docref, throw_error};
use php_ext::request::{
    Brigade, FilterFlags, FilterStatus, Request, Stream, StreamFilter, Streams as _,
};
use php_ext::values::{
    Owner, PhpArray, PhpObject, PhpRef, PhpString, Resource, ResourceType, Value, ValueRef,
};

use crate::State;

use constants::{PSFS_ERR_FATAL, PSFS_PASS_ON};

/// What the request keeps for user filters: each name
/// `stream_filter_register` took with its class's name, and the two
/// resource types, registered on first use.
#[derive(Default)]
pub(super) struct FilterState {
    map: Vec<(Vec<u8>, Vec<u8>)>,
    brigade_type: Option<ResourceType>,
    bucket_type: Option<ResourceType>,
}

/// A brigade as `filter()` holds it: the engine's buckets for the call,
/// and beside each the bucket resource it is where a filter put it
/// there, so a bucket put on a brigade again is moved as php's linked
/// buckets are.
struct BrigadeCell(Brigade, std::collections::VecDeque<Option<Resource>>);

impl BrigadeCell {
    /// The engine's buckets, none of them a program's yet.
    fn of(buckets: Brigade) -> BrigadeCell {
        let ids = buckets.iter().map(|_| None).collect();
        BrigadeCell(buckets, ids)
    }
}

/// A bucket: its bytes, and the brigade it is on, `bucket->brigade`.
struct BucketCell(Vec<u8>, Option<Resource>);

/// The constants `user_filters.stub.php` declares for filters, beside
/// `file.stub.php`'s `STREAM_FILTER_*` the filter natives take.
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
#[php_const]
pub(crate) mod constants {
    /// `PSFS_*` (`Z/main/streams/php_stream_filter_api.h`).
    pub(crate) const PSFS_PASS_ON: i64 = 2;
    pub(crate) const PSFS_FEED_ME: i64 = 1;
    pub(crate) const PSFS_ERR_FATAL: i64 = 0;
    pub(crate) const PSFS_FLAG_NORMAL: i64 = 0;
    pub(crate) const PSFS_FLAG_FLUSH_INC: i64 = 1;
    pub(crate) const PSFS_FLAG_FLUSH_CLOSE: i64 = 2;
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
///
/// The three methods a subclass may leave as they are: `filter` holds
/// `"rrzb"` and answers `PSFS_ERR_FATAL`, `onCreate` answers `true` and
/// `onClose` nothing.
///
/// ```php
/// $f = new php_user_filter;
/// $c = 0;
/// var_dump($f->onCreate(), $f->onClose(), $f->filter(STDIN, STDOUT, $c, false), $c);
/// try { $f->filter('in', STDOUT, $c, false); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// NULL
/// int(0)
/// int(0)
/// php_user_filter::filter(): Argument #1 ($in) must be of type resource, string given
/// ```
#[php_class]
pub(crate) struct php_user_filter;

#[php_class]
impl php_user_filter {
    #[php(property)]
    const filtername: PhpString = "";
    #[php(property)]
    const params: Value = "";
    #[php(property)]
    const stream: Untyped<Value> = ConstValue::Null;

    /// `php_user_filter::filter($in, $out, &$consumed, bool $closing): int`:
    /// `"rrzb"` held, and `PSFS_ERR_FATAL`, what a filter that does not
    /// override it answers.
    #[php(tentative)]
    fn filter(
        this: This<Self>,
        r#in: Resource,
        out: Resource,
        consumed: Ref<Untyped<Value>>,
        closing: bool,
    ) -> i64 {
        let _ = (this, r#in, out, consumed, closing);
        PSFS_ERR_FATAL
    }

    /// `php_user_filter::onCreate(): bool`: `true`.
    #[php(tentative)]
    fn onCreate(this: This<Self>) -> bool {
        let _ = this;
        true
    }

    /// `php_user_filter::onClose(): void`.
    #[php(tentative)]
    fn onClose(this: This<Self>) {
        let _ = this;
    }
}

/// `final class StreamBucket`, what `stream_bucket_make_writeable` and
/// `stream_bucket_new` hand a filter: its `userfilter.bucket` resource
/// and the bytes, whose `data` a filter may rewrite before the bucket is
/// appended.
#[php_class(final)]
pub(crate) struct StreamBucket;

#[php_class]
impl StreamBucket {
    #[php(property)]
    const bucket: Untyped<Value> = ConstValue::Null;
    #[php(property)]
    const data: PhpString;
    #[php(property)]
    const datalen: i64;
    #[php(property)]
    const dataLength: i64;
}

/// The two resource types, registered on first use.
fn brigade_type(req: &mut Request) -> ResourceType {
    if let Some(kind) = req.state::<State>().user_filters.brigade_type {
        return kind;
    }
    let kind = req
        .memory_mut()
        .resources()
        .register_type(b"userfilter.bucket brigade", None);
    req.state::<State>().user_filters.brigade_type = Some(kind);
    kind
}

fn bucket_type(req: &mut Request) -> ResourceType {
    if let Some(kind) = req.state::<State>().user_filters.bucket_type {
        return kind;
    }
    let kind = req
        .memory_mut()
        .resources()
        .register_type(b"userfilter.bucket", None);
    req.state::<State>().user_filters.bucket_type = Some(kind);
    kind
}

/// A filter written in PHP (`userfilter_ops`): the object its factory
/// made, one owner, until `close` gives it up.
struct UserFilter {
    object: Option<Owner<PhpObject>>,
}

/// php's warning for a call that left buckets on its input brigade.
fn unprocessed(req: &mut Request, input: &Brigade) {
    if !input.is_empty() {
        docref(
            req,
            Level::Warning,
            "Unprocessed filter buckets remaining on input brigade",
        );
    }
}

/// `zend_call_method_if_exists` of a method with no arguments.
fn call_if_exists(
    req: &mut Request,
    object: PhpObject,
    method: &[u8],
) -> Result<Option<Owner<Value>>, Thrown> {
    let mut callable = PhpArray::new(req)?;
    callable.append(req, Owner::take(req, object.as_value()))?;
    let name = PhpString::new(req, method)?.into_value();
    callable.append(req, name)?;
    let exists = is_callable(req, &Value::array(*callable));
    callable.give_up(req);
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
    /// `$this->stream` put back to `null`. A `stream` property whose type
    /// refuses the resource throws before `filter()` is called: php's
    /// warning where input buckets are left, and `PSFS_ERR_FATAL`.
    ///
    /// ```php
    /// class F {
    ///     public $filtername, $params;
    ///     public int $stream = 1;
    ///     function filter($in, $out, &$consumed, $closing): int { echo "called\n"; return PSFS_PASS_ON; }
    /// }
    /// stream_filter_register('f', 'F');
    /// $m = fopen('php://memory', 'w');
    /// stream_filter_append($m, 'f', STREAM_FILTER_WRITE);
    /// try { var_dump(fwrite($m, 'data')); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// try { fclose($m); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// Warning: fwrite(): Unprocessed filter buckets remaining on input brigade in %s on line 10
    /// Cannot assign resource to property F::$stream of type int
    /// Cannot assign resource to property F::$stream of type int
    /// ```
    fn filter(
        &mut self,
        req: &mut Request,
        stream: Stream,
        input: &mut Brigade,
        output: &mut Brigade,
        consumed: Option<&mut usize>,
        flags: FilterFlags,
    ) -> Result<FilterStatus, Thrown> {
        let Some(object) = self.object.as_deref().copied() else {
            return Ok(FilterStatus::ErrFatal);
        };
        let has_stream = object.prop(req, b"stream").is_some();
        if has_stream {
            let value = Owner::take(req, stream.as_value());
            // A property update that threw skips the call: php's warning
            // where input buckets remain, and `PSFS_ERR_FATAL` with the
            // throwable pending (`Z/ext/standard/user_filters.c:161`).
            if let Err(thrown) = write_prop(req, &object, b"stream", value) {
                unprocessed(req, input);
                return Err(thrown);
            }
        }
        let kind = brigade_type(req);
        let in_res = Resource::new(req, kind, BrigadeCell::of(std::mem::take(input)))?;
        let out_res = Resource::new(req, kind, BrigadeCell::of(std::mem::take(output)))?;
        let count = match &consumed {
            Some(n) => Value::int(i64::try_from(**n).unwrap_or(i64::MAX)),
            None => Value::null(),
        };
        let count = Owner::take(req, count);
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
        let finished = (|| {
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
            unprocessed(req, input);
            if has_stream {
                write_prop(req, &object, b"stream", Owner::null())?;
            }
            Ok(status)
        })();
        count.give_up(req);
        in_res.give_up(req);
        out_res.give_up(req);
        let status = match finished {
            Ok(status) => status,
            Err(thrown) => {
                if let Ok(value) = called {
                    value.give_up(req);
                }
                return Err(thrown);
            }
        };
        called?.give_up(req);
        Ok(status)
    }

    /// `userfilter_dtor` (`Z/ext/standard/user_filters.c:103`):
    /// `onClose()` where the object has it and no throwable is pending,
    /// as `zend_call_method_if_exists` calls nothing under
    /// `EG(exception)`, then the object given up either way.
    fn close(&mut self, req: &mut Request) -> Result<(), Thrown> {
        let Some(object) = self.object.take() else {
            return Ok(());
        };
        let closed = if req.diagnostics_mut().stream_thrown().is_some() {
            Ok(None)
        } else {
            call_if_exists(req, *object, b"onclose")
        };
        object.give_up(req);
        if let Some(value) = closed? {
            value.give_up(req);
        }
        Ok(())
    }
}

/// `user_filter_factory_create` (`Z/ext/standard/user_filters.c:231`):
/// php's warning and no filter for a persistent stream; else the class registered under the name or, for a name nobody registered
/// as it is, under the nearest wildcard over it; php's warning where
/// the class is not defined; then an object of it, no constructor
/// called, its `filtername` and `params` set, and `onCreate()`, whose
/// `false` refuses the filter.
///
/// The class named is `zend_lookup_class`, which asks the autoloaders.
///
/// ```php
/// spl_autoload_register(function ($c) { echo "load $c\n"; if ($c === 'Lazy') { class Lazy extends php_user_filter {} } });
/// stream_filter_register('lazy.*', 'Lazy');
/// var_dump(stream_filter_append(fopen("php://memory", "w+"), "lazy.one") !== false);
/// ```
/// ```expect
/// load Lazy
/// bool(true)
/// ```
///
/// ```php
/// class Upper extends php_user_filter {}
/// stream_filter_register('upper', 'Upper');
/// $s = stream_socket_server('tcp://127.0.0.1:0');
/// $a = pfsockopen(...explode(':', stream_socket_get_name($s, false)));
/// var_dump(stream_filter_append($a, 'upper'));
/// ```
/// ```expect
///
/// Warning: stream_filter_append(): Cannot use a user-space filter with a persistent stream in %s on line 6
///
/// Warning: stream_filter_append(): Unable to create or locate filter "upper" in %s on line 6
/// bool(false)
/// ```
fn user_filter_factory(
    req: &mut Request,
    name: &[u8],
    params: Option<&Value>,
    persistent: bool,
) -> Result<Option<Box<dyn StreamFilter>>, Thrown> {
    if persistent {
        docref(
            req,
            Level::Warning,
            "Cannot use a user-space filter with a persistent stream",
        );
        return Ok(None);
    }
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
    let Some(class) = autoload(req, &class_name)? else {
        let text = format!(
            "User-filter \"{}\" requires class \"{}\", but that class is not defined",
            String::from_utf8_lossy(name),
            String::from_utf8_lossy(&class_name)
        );
        docref(req, Level::Warning, &text);
        return Ok(None);
    };
    let object = new_object(req, class)?;
    match created(req, *object, name, params) {
        Ok(Some(value)) if matches!(value.as_ref(), ValueRef::Bool(false)) => {
            value.give_up(req);
            object.give_up(req);
            Ok(None)
        }
        Ok(value) => {
            if let Some(value) = value {
                value.give_up(req);
            }
            Ok(Some(Box::new(UserFilter {
                object: Some(object),
            })))
        }
        Err(thrown) => {
            object.give_up(req);
            Err(thrown)
        }
    }
}

/// The new filter object's `filtername` and `params` set, and what
/// `onCreate()` answered, where the object has it.
fn created(
    req: &mut Request,
    object: PhpObject,
    name: &[u8],
    params: Option<&Value>,
) -> Result<Option<Owner<Value>>, Thrown> {
    let filtername = PhpString::new(req, name)?.into_value();
    write_prop(req, &object, b"filtername", filtername)?;
    let params = params.map_or_else(Value::null, Value::deref);
    let params = Owner::take(req, params);
    write_prop(req, &object, b"params", params)?;
    call_if_exists(req, object, b"oncreate")
}

/// `Z_PARAM_RESOURCE` then `zend_fetch_resource` of a brigade: the
/// argument as a live `userfilter.bucket brigade`, or php's `TypeError`.
fn brigade_of(req: &mut Request, res: Resource, native: &str) -> Result<Resource, Thrown> {
    let kind = brigade_type(req);
    if res.kind() != Some(kind) {
        let text = format!(
            "{native}(): supplied resource is not a valid userfilter.bucket brigade resource"
        );
        return Err(throw_error(req, "TypeError", &text));
    }
    Ok(res)
}

/// A `StreamBucket` over a new `userfilter.bucket` resource of `bytes`,
/// its `data`, `datalen` and `dataLength` the bytes and their count.
fn bucket_object(
    req: &mut Request,
    bytes: Vec<u8>,
) -> Result<Owner<Instance<StreamBucket>>, Thrown> {
    let Some(class) = lookup_class(req, b"StreamBucket")? else {
        return Err(php_ext::raising::fatal(
            req,
            "InternalError: a bucket without its StreamBucket class",
        ));
    };
    let kind = bucket_type(req);
    let len = i64::try_from(bytes.len()).unwrap_or(i64::MAX);
    let data = PhpString::new(req, &bytes)?.into_value();
    let res = Resource::new(req, kind, BucketCell(bytes, None))?;
    let object = new_object(req, class)?;
    write_prop(req, &object, b"bucket", res.into_value())?;
    write_prop(req, &object, b"data", data)?;
    for name in [&b"datalen"[..], b"dataLength"] {
        write_prop(req, &object, name, Owner::int(len))?;
    }
    Ok(object.into_instance())
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
#[php_function]
fn stream_bucket_make_writeable(
    req: &mut Request,
    brigade: Resource,
) -> Result<Option<Owner<Instance<StreamBucket>>>, Thrown> {
    let res = brigade_of(req, brigade, "stream_bucket_make_writeable")?;
    let head = res.native_mut::<BrigadeCell>(req).and_then(|cell| {
        cell.1.pop_front();
        cell.0.pop_front()
    });
    match head {
        Some(bytes) => bucket_object(req, bytes).map(Some),
        None => Ok(None),
    }
}

/// `php_stream_bucket_attach` (`Z/ext/standard/user_filters.c:373`): the
/// bucket a `StreamBucket` holds, its bytes made the object's `data`,
/// put at the tail of the brigade or at its head.
fn bucket_attach(
    req: &mut Request,
    native: &str,
    (brigade, bucket): (Resource, Instance<StreamBucket>),
    append: bool,
) -> Result<(), Thrown> {
    let brigade = brigade_of(req, brigade, native)?;
    let object = bucket.object();
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
        return Err(throw_error(req, "TypeError", &text));
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
    // "If the bucket is already on a brigade we have to unlink it first":
    // the bucket moves, and one already at the tail stays where it is.
    let on = res.native::<BucketCell>(req).and_then(|cell| cell.1);
    if let Some(from) = on
        && let Some(cell) = from.native_mut::<BrigadeCell>(req)
        && let Some(at) = cell.1.iter().position(|id| *id == Some(res))
    {
        cell.0.remove(at);
        cell.1.remove(at);
    }
    if let Some(cell) = brigade.native_mut::<BrigadeCell>(req) {
        if append {
            cell.0.push_back(bytes);
            cell.1.push_back(Some(res));
        } else {
            cell.0.push_front(bytes);
            cell.1.push_front(Some(res));
        }
    }
    if let Some(cell) = res.native_mut::<BucketCell>(req) {
        cell.1 = Some(brigade);
    }
    Ok(())
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
#[php_function]
fn stream_bucket_prepend(
    req: &mut Request,
    brigade: Resource,
    bucket: Instance<StreamBucket>,
) -> Result<(), Thrown> {
    bucket_attach(req, "stream_bucket_prepend", (brigade, bucket), false)
}

/// `stream_bucket_append($brigade, StreamBucket $bucket): void`: the
/// bucket put at the brigade's tail, what a filter hands on. A bucket
/// already on a brigade is taken off it first, as
/// `php_stream_bucket_attach` unlinks it, so putting one bucket on twice
/// (or a clone of its object, which holds the same bucket) moves it.
///
/// ```php
/// class same extends php_user_filter {
///     public function filter($in, $out, &$consumed, bool $closing): int {
///         while ($bucket = stream_bucket_make_writeable($in)) {
///             $bucket->data = "[$bucket->data]";
///             stream_bucket_append($out, $bucket);
///             stream_bucket_append($out, $bucket);
///             stream_bucket_prepend($out, clone $bucket);
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
#[php_function]
fn stream_bucket_append(
    req: &mut Request,
    brigade: Resource,
    bucket: Instance<StreamBucket>,
) -> Result<(), Thrown> {
    bucket_attach(req, "stream_bucket_append", (brigade, bucket), true)
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
#[php_function]
fn stream_bucket_new(
    req: &mut Request,
    stream: Stream,
    buffer: PhpString,
) -> Result<Owner<Instance<StreamBucket>>, Thrown> {
    let _ = stream;
    bucket_object(req, buffer.as_bytes().to_vec())
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
#[php_function]
fn stream_get_filters(req: &mut Request) -> Result<Owner<PhpArray>, Thrown> {
    let mut out = PhpArray::new(req)?;
    for name in req.stream_filters() {
        let value = PhpString::new(req, &name)?.into_value();
        out.append(req, value)?;
    }
    Ok(out)
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
#[php_function]
fn stream_filter_register(
    req: &mut Request,
    filter_name: PhpString,
    class: PhpString,
) -> Result<bool, Thrown> {
    let name = filter_name.as_bytes().to_vec();
    let class = class.as_bytes().to_vec();
    if name.is_empty() {
        return Err(arg_value_error(req, 1, "be a non-empty string"));
    }
    if class.is_empty() {
        return Err(arg_value_error(req, 2, "be a non-empty string"));
    }
    let state = &mut req.state::<State>().user_filters;
    if state.map.iter().any(|(n, _)| *n == name) {
        return Ok(false);
    }
    state.map.push((name.clone(), class));
    if req.register_filter(&name, user_filter_factory) {
        return Ok(true);
    }
    req.state::<State>()
        .user_filters
        .map
        .retain(|(n, _)| *n != name);
    Ok(false)
}

/// `apply_filter_to_stream` (`Z/ext/standard/streamsfuncs.c:1208`): the
/// parameters, then the engine's attach.
fn apply_filter(
    req: &mut Request,
    (stream, name, read_write, params): (Stream, PhpString, i64, Passed<Value>),
    append: bool,
) -> Result<Untyped<OrFalse<Owner<Resource>>>, Thrown> {
    let name = name.as_bytes().to_vec();
    let params = match params {
        Passed::Given(params) => Some(params),
        Passed::Omitted => None,
    };
    Ok(Untyped(
        match stream.add_filter(req, &name, params.as_ref(), read_write, append)? {
            Some(res) => OrFalse::Value(res),
            None => OrFalse::False,
        },
    ))
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
#[php_function]
fn stream_filter_prepend(
    req: &mut Request,
    stream: Stream,
    filter_name: PhpString,
    #[php(default = 0)] mode: i64,
    params: Passed<Value>,
) -> Result<Untyped<OrFalse<Owner<Resource>>>, Thrown> {
    apply_filter(req, (stream, filter_name, mode, params), false)
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
#[php_function]
fn stream_filter_append(
    req: &mut Request,
    stream: Stream,
    filter_name: PhpString,
    #[php(default = 0)] mode: i64,
    params: Passed<Value>,
) -> Result<Untyped<OrFalse<Owner<Resource>>>, Thrown> {
    apply_filter(req, (stream, filter_name, mode, params), true)
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
#[php_function]
fn stream_filter_remove(req: &mut Request, stream_filter: Resource) -> Result<bool, Thrown> {
    match req.remove_filter(stream_filter)? {
        Some(removed) => Ok(removed),
        None => Err(throw_error(
            req,
            "TypeError",
            "stream_filter_remove(): supplied resource is not a valid stream filter resource",
        )),
    }
}
