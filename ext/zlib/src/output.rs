//! `zlib.c`'s compression of what a request prints: the `zlib output compression` handler
//! `zlib.output_compression` starts at request start or on `ini_set()`,
//! the `ob_gzhandler` name `ob_start()` takes for the same handler, the
//! legacy `ob_gzhandler()` function over a stream of its own, and
//! `zlib_get_coding_type`. The coding comes from the request's
//! `Accept-Encoding`, read once per request.

use php_ext::abi::{IniStage, OrFalse};
use php_ext::attr::php_function;
use php_ext::coerce::with_string;
use php_ext::raising::{Level, Thrown, docref};
use php_ext::request::{
    BufferHandler, HandlerCall, HandlerOp, HandlerStatus, OutputOp, Request, StartHandler,
};
use php_ext::values::{Owner, PhpString, ValueRef};

use crate::State;
use crate::encode::{DEFAULT_STRATEGY, MAX_MEM_LEVEL, size_guess};
use crate::ffi::{Deflate, Flush, Status};

/// The three ini entries.
pub(crate) const COMPRESSION: &str = "zlib.output_compression";
pub(crate) const LEVEL: &str = "zlib.output_compression_level";
pub(crate) const HANDLER: &str = "zlib.output_handler";

/// `PHP_ZLIB_OUTPUT_HANDLER_NAME`.
const HANDLER_NAME: &str = "zlib output compression";

/// `PHP_OUTPUT_HANDLER_DEFAULT_SIZE`.
const DEFAULT_SIZE: i64 = 0x4000;

/// `PHP_OUTPUT_HANDLER_STDFLAGS`.
const STDFLAGS: i64 = 0x70;

/// What `php_zlib_output_conflict_check` refuses to start beside, for
/// both names.
const CONFLICTS: &[&str] = &[
    HANDLER_NAME,
    "ob_gzhandler",
    "mb_output_handler",
    "URL-Rewriter",
];

/// `php_output_handler_alias_register("ob_gzhandler", ...)`.
pub(crate) const OB_GZHANDLER: BufferHandler = BufferHandler {
    name: "ob_gzhandler",
    conflicts: CONFLICTS,
    handler,
    init: Some(init),
};

/// The handler `php_zlib_output_compression_start` starts.
const COMPRESSION_HANDLER: BufferHandler = BufferHandler {
    name: HANDLER_NAME,
    conflicts: CONFLICTS,
    handler,
    init: Some(init),
};

/// `ZLIBG(compression_coding)`: none until the request's
/// `Accept-Encoding` named one.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
enum Coding {
    #[default]
    None,
    Gzip,
    Deflate,
}

impl Coding {
    /// The window bits `deflateInit2` is handed for the coding.
    fn bits(self) -> i64 {
        match self {
            Coding::Gzip => 0x1f,
            Coding::Deflate | Coding::None => 0x0f,
        }
    }

    /// The `Content-Encoding` header for it.
    fn header(self) -> Option<&'static [u8]> {
        match self {
            Coding::Gzip => Some(b"Content-Encoding: gzip"),
            Coding::Deflate => Some(b"Content-Encoding: deflate"),
            Coding::None => None,
        }
    }
}

/// `php_zlib_context` as the buffer handlers use it: the stream and the
/// input not yet deflated.
#[derive(Default)]
struct Context {
    stream: Option<Deflate>,
    buffer: Vec<u8>,
}

impl Context {
    /// `deflateEnd` over the stream there is, its state given back.
    fn end(&mut self, req: &mut Request) {
        if let Some(stream) = self.stream.take() {
            stream.end(req);
        }
    }
}

/// The buffer handlers' half of `zend_zlib_globals`, per request.
#[derive(Default)]
pub(crate) struct Globals {
    /// `ZLIBG(output_compression)`: the chunk size the handler runs at,
    /// zero for none.
    compression: i64,
    coding: Coding,
    /// The running handler's `php_zlib_context`.
    handler: Context,
    /// `ZLIBG(ob_gzhandler)`, the legacy function's.
    legacy: Option<Context>,
}

fn globals(req: &mut Request) -> &mut Globals {
    &mut req.state::<State>().output
}

/// `zlib.output_compression` read as `OnUpdate_zlib_output_compression`
/// reads it: `off` and `on` in any case, else the quantity `quantity`
/// reads, cut to php's `int`.
fn parse(value: &[u8], quantity: impl FnOnce(&[u8]) -> i64) -> i64 {
    if value.eq_ignore_ascii_case(b"off") {
        0
    } else if value.eq_ignore_ascii_case(b"on") {
        1
    } else {
        #[allow(clippy::cast_possible_truncation)] // php's `(int)` cast
        let cut = quantity(value) as i32;
        i64::from(cut)
    }
}

/// `php_zlib_output_encoding`: the coding once chosen, or the one the
/// request's `HTTP_ACCEPT_ENCODING` names, `gzip` before `deflate`.
fn encoding(req: &mut Request) -> Result<Coding, Thrown> {
    if globals(req).coding != Coding::None {
        return Ok(globals(req).coding);
    }
    let found = req
        .superglobal(b"_SERVER")
        .and_then(|server| server.find(b"HTTP_ACCEPT_ENCODING"));
    if let Some(value) = found {
        let coding = with_string(req, &value, |text| {
            let has = |word: &[u8]| text.windows(word.len()).any(|w| w == word);
            if has(b"gzip") {
                Coding::Gzip
            } else if has(b"deflate") {
                Coding::Deflate
            } else {
                Coding::None
            }
        })?;
        globals(req).coding = coding;
    }
    Ok(globals(req).coding)
}

/// `zlib.output_compression_level` as `OnUpdateLong` stored it.
///
/// The engine serves the entry's `OnUpdateLong`, which warns where the
/// quantity rule interprets the value:
///
/// ```php
/// var_dump(ini_set('zlib.output_compression_level', '9x'), ini_get('zlib.output_compression_level'));
/// ```
/// ```expect
/// Warning: Invalid "zlib.output_compression_level" setting. Invalid quantity "9x": unknown multiplier "x", interpreting as "9" for backwards compatibility in %s on line 2
/// string(2) "-1"
/// string(2) "9x"
/// ```
fn level(req: &Request) -> i64 {
    req.ini_long(LEVEL.as_bytes())
}

/// What `php_zlib_output_handler_ex` handed on: nothing, where it only
/// cleaned, or the deflated bytes.
type Handed = Option<Vec<u8>>;

/// `php_zlib_output_handler_ex`: the stream made at the start and again
/// after a clean, the input appended to what the last call left and
/// deflated into a buffer of [`size_guess`] with a sync flush, a full
/// one on a flush and a finish at the end; `Err` is php's `FAILURE`.
fn handle(
    req: &mut Request,
    ctx: &mut Context,
    op: HandlerOp,
    input: &[u8],
    level: i64,
    coding: Coding,
) -> Result<Result<Handed, ()>, Thrown> {
    let init = |req: &mut Request| -> Result<Result<Deflate, ()>, Thrown> {
        Ok(Deflate::new(
            req,
            (level, coding.bits(), MAX_MEM_LEVEL, DEFAULT_STRATEGY),
            false,
        )?
        .map_err(|_| ()))
    };
    if op.start {
        ctx.end(req);
        match init(req)? {
            Ok(stream) => ctx.stream = Some(stream),
            Err(()) => return Ok(Err(())),
        }
    }
    if op.clean {
        ctx.end(req);
        if op.finish {
            return Ok(Ok(None));
        }
        match init(req)? {
            Ok(stream) => ctx.stream = Some(stream),
            Err(()) => return Ok(Err(())),
        }
        ctx.buffer.clear();
        return Ok(Ok(None));
    }
    ctx.buffer.extend_from_slice(input);
    let mut out = vec![0; size_guess(input.len())];
    let flush = if op.finish {
        Flush::Finish
    } else if op.flush {
        Flush::Full
    } else {
        Flush::Sync
    };
    let Some(stream) = ctx.stream.as_mut() else {
        return Ok(Err(()));
    };
    let step = stream.deflate(req, &ctx.buffer, &mut out, flush)?;
    match step.status {
        Status::Ok if flush == Flush::Finish => {
            ctx.end(req);
            return Ok(Err(()));
        }
        Status::Ok | Status::StreamEnd => {
            ctx.buffer.drain(..step.read);
            out.truncate(step.written);
        }
        _ => {
            ctx.end(req);
            return Ok(Err(()));
        }
    }
    if op.finish {
        ctx.end(req);
    }
    Ok(Ok(Some(out)))
}

/// `php_zlib_output_handler`, both names' handler: nothing compressed
/// without a coding, and `Vary` sent unless the whole buffer is thrown
/// away; on the first call the headers, unless they went out already or
/// compression was switched off meanwhile, and the handler made
/// immutable.
fn handler(req: &mut Request, call: &mut HandlerCall<'_>) -> Result<HandlerStatus, Thrown> {
    let op = call.op;
    let coding = encoding(req)?;
    if coding == Coding::None {
        if op.start && !(op.clean && op.finish) {
            req.header_add(b"Vary: Accept-Encoding", false, 0);
        }
        return Ok(HandlerStatus::Failure);
    }
    let level = level(req);
    let mut ctx = std::mem::take(&mut globals(req).handler);
    let handed = handle(req, &mut ctx, op, call.input, level, coding);
    globals(req).handler = ctx;
    let Ok(handed) = handed? else {
        return Ok(HandlerStatus::Failure);
    };
    if (!op.clean || !op.finish) && op.start {
        if req.headers_sent().is_some() || globals(req).compression == 0 {
            let mut ctx = std::mem::take(&mut globals(req).handler);
            ctx.end(req);
            globals(req).handler = ctx;
            return Ok(HandlerStatus::Failure);
        }
        if let Some(header) = coding.header() {
            req.header_add(header, true, 0);
        }
        req.header_add(b"Vary: Accept-Encoding", false, 0);
        call.immutable = true;
    }
    call.out = handed.unwrap_or_default();
    Ok(HandlerStatus::Success)
}

/// `php_zlib_output_handler_init`, which both names are made through:
/// where no chunk size is running, the one `ob_start()` was handed, or
/// `PHP_OUTPUT_HANDLER_DEFAULT_SIZE` for none. The handler
/// `zlib.output_compression` starts has its chunk size already, so a
/// zero on its first call is the ini switched off since, and [`handler`]
/// passes what it is handed through unchanged.
///
/// ```php
/// var_dump(ob_start('ob_gzhandler', 10), ini_get('zlib.output_compression'), ob_get_status()['chunk_size']);
/// ```
/// ```expect
/// bool(true)
/// string(1) "0"
/// int(10)
/// ```
#[allow(clippy::unnecessary_wraps)] // `BufferInit`'s signature
fn init(req: &mut Request, chunk_size: usize) -> Result<bool, Thrown> {
    if globals(req).compression == 0 {
        globals(req).compression = match i64::try_from(chunk_size) {
            Ok(0) | Err(_) => DEFAULT_SIZE,
            Ok(size) => size,
        };
    }
    Ok(true)
}

/// `php_zlib_output_compression_start`: with a chunk size and a coding,
/// the handler started, and `zlib.output_handler`'s after it.
fn compression_start(req: &mut Request) -> Result<(), Thrown> {
    match globals(req).compression {
        0 => return Ok(()),
        1 => globals(req).compression = DEFAULT_SIZE,
        _ => {}
    }
    if encoding(req)? == Coding::None {
        return Ok(());
    }
    let chunk_size = globals(req).compression;
    let begun = req.output_buffer_op(&OutputOp::Start {
        handler: StartHandler::Registered(COMPRESSION_HANDLER),
        chunk_size,
        flags: STDFLAGS,
    })?;
    let user = req.ini_get(HANDLER.as_bytes()).unwrap_or_default().to_vec();
    let started = matches!(begun.as_ref(), ValueRef::Bool(true));
    begun.give_up(req);
    if started && !user.is_empty() {
        let mut name = None;
        let handler = if let Some(registered) = req.buffer_handler(&user) {
            StartHandler::Registered(registered)
        } else {
            let made = PhpString::new(req, &user)?;
            let value = php_ext::values::Value::string(*made);
            match php_ext::coerce::to_callable(req, &value) {
                Ok(callable) => {
                    name = Some(made);
                    StartHandler::User(callable)
                }
                Err(thrown) => {
                    made.give_up(req);
                    return Err(thrown);
                }
            }
        };
        let done = req.output_buffer_op(&OutputOp::Start {
            handler,
            chunk_size,
            flags: STDFLAGS,
        });
        if let Some(made) = name {
            made.give_up(req);
        }
        done?.give_up(req);
    }
    Ok(())
}

/// `PHP_RINIT_FUNCTION(zlib)`: no coding yet, the chunk size the ini
/// holds, and the handler started where it asks for one.
pub(crate) fn rinit(req: &mut Request) -> Result<(), Thrown> {
    let value = req.ini_get(COMPRESSION.as_bytes()).unwrap_or(b"0");
    let compression = parse(value, Request::ini_parse_bytes);
    let globals = globals(req);
    globals.coding = Coding::None;
    globals.compression = compression;
    compression_start(req)
}

/// `OnUpdate_zlib_output_compression`: the value read, refused beside a
/// `output_handler`, and once the headers went out; the chunk size reset
/// to it, and at runtime the handler started when it is not. At startup
/// and as the request ends, when the headers are out, the value is
/// taken and nothing started.
///
/// ```php
/// var_dump(ini_set('zlib.output_compression', 'Off'), ini_get('zlib.output_compression'));
/// var_dump(ini_set('zlib.output_compression', '5 apples'));
/// ```
/// ```expect
/// string(1) "0"
/// string(3) "Off"
///
/// Warning: Invalid "zlib.output_compression" setting. Invalid quantity "5 apples": unknown multiplier "s", interpreting as "5 " for backwards compatibility in %s on line 3
///
/// Warning: ini_set(): Cannot change zlib.output_compression - headers already sent in %s on line 3
/// bool(false)
/// ```
pub(crate) fn on_compression(req: &mut Request, value: Option<&[u8]>) -> Result<bool, Thrown> {
    let value = value.unwrap_or_default();
    let compression = parse(value, |value| {
        req.ini_parse_quantity_warn(COMPRESSION.as_bytes(), value)
    });
    let handler = req.ini_get(b"output_handler").unwrap_or_default();
    if !handler.is_empty() && compression != 0 {
        docref(
            req,
            Level::CoreError,
            "Cannot use both zlib.output_compression and output_handler together!!",
        );
        return Ok(false);
    }
    if matches!(req.ini_stage(), IniStage::Startup | IniStage::Deactivate) {
        return Ok(true);
    }
    if req.headers_sent().is_some() {
        docref(
            req,
            Level::Warning,
            "Cannot change zlib.output_compression - headers already sent",
        );
        return Ok(false);
    }
    globals(req).compression = compression;
    if compression != 0 && !req.buffer_handler_started(HANDLER_NAME.as_bytes()) {
        compression_start(req)?;
    }
    Ok(true)
}

/// `OnUpdate_zlib_output_handler`: refused at runtime once the headers
/// went out; the request's end hands it back its value past them.
#[allow(clippy::unnecessary_wraps)] // `OnModify`'s signature
pub(crate) fn on_handler(req: &mut Request, _value: Option<&[u8]>) -> Result<bool, Thrown> {
    if matches!(req.ini_stage(), IniStage::Activate | IniStage::Runtime)
        && req.headers_sent().is_some()
    {
        docref(
            req,
            Level::Warning,
            "Cannot change zlib.output_handler - headers already sent",
        );
        return Ok(false);
    }
    Ok(true)
}

/// `ob_gzhandler(string $data, int $flags): string|false`, the legacy
/// hack: the coding asked for, the headers on a start, and the bytes
/// through a stream of its own, given back on a failure and at the
/// request's end.
///
/// ```php
/// var_dump(ob_gzhandler('abc', PHP_OUTPUT_HANDLER_START));
/// $_SERVER['HTTP_ACCEPT_ENCODING'] = 'gzip, deflate';
/// var_dump(ob_gzhandler('abc', PHP_OUTPUT_HANDLER_START));
/// ```
/// ```expect
/// bool(false)
/// bool(false)
/// ```
#[php_function]
pub(crate) fn ob_gzhandler(
    req: &mut Request,
    data: PhpString,
    flags: i64,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let coding = encoding(req)?;
    if coding == Coding::None {
        return Ok(OrFalse::False);
    }
    let op = HandlerOp {
        start: flags & 0x01 != 0,
        clean: flags & 0x02 != 0,
        flush: flags & 0x04 != 0,
        finish: flags & 0x08 != 0,
    };
    if op.start {
        if let Some(header) = coding.header() {
            req.header_add(header, true, 0);
        }
        req.header_add(b"Vary: Accept-Encoding", false, 0);
    }
    let level = level(req);
    let mut ctx = globals(req).legacy.take().unwrap_or_default();
    let Ok(handed) = handle(req, &mut ctx, op, data.as_bytes(), level, coding)? else {
        ctx.end(req);
        return Ok(OrFalse::False);
    };
    globals(req).legacy = Some(ctx);
    let out = PhpString::new(req, &handed.unwrap_or_default())?;
    Ok(OrFalse::Value(out))
}

/// `zlib_get_coding_type(): string|false`: the coding chosen, which only
/// starting the compression chooses.
///
/// ```php
/// var_dump(zlib_get_coding_type());
/// ```
/// ```expect
/// bool(false)
/// ```
#[php_function]
pub(crate) fn zlib_get_coding_type(req: &mut Request) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let name: &[u8] = match globals(req).coding {
        Coding::Gzip => b"gzip",
        Coding::Deflate => b"deflate",
        Coding::None => return Ok(OrFalse::False),
    };
    Ok(OrFalse::Value(PhpString::new(req, name)?))
}

/// `DISPLAY_INI_ENTRIES()` for the three, `zlib.output_compression`
/// through `php_ini_boolean_displayer_cb`, `On` or `Off`.
pub(crate) fn info_ini(req: &mut Request) {
    let entries = req.ini_entries();
    let shown = |name: &str, value: &[u8]| -> String {
        if name == COMPRESSION {
            return if Request::ini_parse_bool(value) {
                "On"
            } else {
                "Off"
            }
            .into();
        }
        String::from_utf8_lossy(value).into_owned()
    };
    let rows: Vec<[String; 3]> = [COMPRESSION, LEVEL, HANDLER]
        .iter()
        .map(|name| {
            let entry = entries
                .iter()
                .find(|e| e.decl.name.0.as_ref() == name.as_bytes());
            let local = entry.and_then(|e| e.value.as_deref()).unwrap_or_default();
            let master = entry.and_then(|e| e.master.as_deref()).unwrap_or_default();
            [(*name).to_string(), shown(name, local), shown(name, master)]
        })
        .collect();
    let cells: Vec<[&str; 3]> = rows
        .iter()
        .map(|[name, local, master]| [name.as_str(), local.as_str(), master.as_str()])
        .collect();
    req.info_ini_table(&cells);
}
