//! `tidy.c`'s repair of what a request prints: `ob_tidyhandler`, the
//! handler `ob_start()` takes by name and `tidy.clean_output` starts at
//! request start or on `ini_set()`. libtidy parses no part of a document,
//! so the handler repairs a buffer handed to it whole, on a first call
//! that is also its last, and passes anything else through.

use php_ext::abi::IniStage;
use php_ext::raising::{Level, Thrown, docref};
use php_ext::request::{
    BufferHandler, HandlerCall, HandlerStatus, Ini, OutputOp, Request, StartHandler,
};

use crate::{CLEAN_OUTPUT, DEFAULT_CONFIG, State, default_config, ffi};

/// The name `php_output_handler_alias_register` registers.
const NAME: &str = "ob_tidyhandler";

/// `PHP_OUTPUT_HANDLER_STDFLAGS`.
const STDFLAGS: i64 = 0x70;

/// `PHP_OUTPUT_WRITTEN` and `PHP_OUTPUT_SENT`.
const WRITTEN: i64 = 0x04;
const SENT: i64 = 0x08;

/// `php_output_handler_alias_register("ob_tidyhandler", ...)`.
pub(crate) const OB_TIDYHANDLER: BufferHandler = BufferHandler {
    name: NAME,
    conflicts: &[],
    handler,
    init: Some(init),
};

/// `php_tidy_output_handler_init`: a chunk size refused with php's
/// warning, which `ob_start()` answers with its notice and `false`;
/// else `TG(clean_output)` set.
///
/// ```php
/// var_dump(ob_start("ob_tidyhandler", 10)); echo "x\n";
/// ```
/// ```expect
/// Warning: ob_start(): Cannot use a chunk size for ob_tidyhandler in %s on line 2
///
/// Notice: ob_start(): Failed to create buffer in %s on line 2
/// bool(false)
/// x
/// ```
#[allow(clippy::unnecessary_wraps)] // `BufferInit`'s signature
fn init(req: &mut Request, chunk_size: usize) -> Result<bool, Thrown> {
    if chunk_size != 0 {
        docref(
            req,
            Level::Warning,
            "Cannot use a chunk size for ob_tidyhandler",
        );
        return Ok(false);
    }
    req.state::<State>().clean_output = true;
    Ok(true)
}

/// `php_tidy_output_handler`: on a call that is both the first and the
/// last, with `TG(clean_output)` set, the buffer parsed, repaired and
/// saved over a document of its own, the last byte of what libtidy
/// wrote dropped as `FIX_BUFFER` drops it.
///
/// ```php
/// ob_start('ob_tidyhandler'); echo '<p>a<b>b</p>'; ob_end_flush(); echo "\n";
/// ob_start('ob_tidyhandler'); echo 'x'; ob_flush(); echo 'y'; ob_end_flush(); echo "\n";
/// ob_start('ob_tidyhandler'); var_dump(ob_end_clean());
/// ```
/// ```expect
/// <!DOCTYPE html>
/// <html>
/// <head>
/// <title></title>
/// </head>
/// <body>
/// <p>a<b>b</b></p>
/// </body>
/// </html>
/// xy
/// bool(true)
/// ```
fn handler(req: &mut Request, call: &mut HandlerCall<'_>) -> Result<HandlerStatus, Thrown> {
    if !(req.state::<State>().clean_output && call.op.start && call.op.finish) {
        return Ok(HandlerStatus::Failure);
    }
    if u32::try_from(call.input.len()).is_err() {
        docref(req, Level::Warning, "Input string is too long");
        return Ok(HandlerStatus::Failure);
    }
    let Some(mut doc) = ffi::Doc::new(req)? else {
        return Ok(HandlerStatus::Failure);
    };
    let repaired = repair(req, &mut doc, call.input);
    doc.release(req);
    match repaired? {
        Some(mut out) => {
            out.pop();
            call.out = out;
            Ok(HandlerStatus::Success)
        }
        None => Ok(HandlerStatus::Failure),
    }
}

/// The default configuration, `tidyParseBuffer` and
/// `tidyCleanAndRepair`, and what `tidySaveBuffer` wrote where both
/// took.
fn repair(req: &mut Request, doc: &mut ffi::Doc, input: &[u8]) -> Result<Option<Vec<u8>>, Thrown> {
    default_config(req, doc)?;
    if doc.parse(req, input)? < 0 || doc.clean_repair(req)? < 0 {
        return Ok(None);
    }
    doc.save(req).map(Some)
}

/// `php_tidy_clean_output_start`: the handler started where
/// `TG(clean_output)` asks for it.
fn clean_output_start(req: &mut Request) -> Result<(), Thrown> {
    if req.state::<State>().clean_output {
        req.response_mut()
            .buffer_op(&OutputOp::Start {
                handler: StartHandler::Registered(OB_TIDYHANDLER),
                chunk_size: 0,
                flags: STDFLAGS,
            })?
            .give_up(req);
    }
    Ok(())
}

/// `PHP_RINIT_FUNCTION(tidy)`: `TG(clean_output)` as the ini holds it,
/// php's `RSHUTDOWN` having put it back there, and the handler started
/// where it asks for one.
pub(crate) fn rinit(req: &mut Request) -> Result<(), Thrown> {
    let on = Ini::parse_bool(req.ini().get(CLEAN_OUTPUT.as_bytes()).unwrap_or(b"0"));
    req.state::<State>().clean_output = on;
    clean_output_start(req)
}

/// `php_tidy_set_clean_output`: at runtime refused once bytes were
/// written, for an `on`, or sent, for any value; otherwise taken, and an
/// `on` starts the handler where it is not on the stack.
///
/// ```php
/// var_dump(ini_set('tidy.clean_output', '1'));
/// echo '<i>x';
/// ```
/// ```expect
/// <!DOCTYPE html>
/// <html>
/// <head>
/// <title></title>
/// </head>
/// <body>
/// string(1) "0" <i>x</i>
/// </body>
/// </html>
/// ```
pub(crate) fn on_clean_output(req: &mut Request, value: Option<&[u8]>) -> Result<bool, Thrown> {
    let on = Ini::parse_bool(value.unwrap_or_default());
    let runtime = req.ini().stage() == IniStage::Runtime;
    if runtime {
        let status = req.response().buffer_status();
        if on && status & WRITTEN != 0 {
            let text = "Cannot enable tidy.clean_output - there has already been output";
            docref(req, Level::Warning, text);
            return Ok(false);
        }
        if status & SENT != 0 {
            let text = "Cannot change tidy.clean_output - headers already sent";
            docref(req, Level::Warning, text);
            return Ok(false);
        }
    }
    if req.ini().stage() != IniStage::Startup {
        req.state::<State>().clean_output = on;
    }
    if runtime && on && !req.response().buffer_handler_started(NAME.as_bytes()) {
        clean_output_start(req)?;
    }
    Ok(true)
}

/// `DISPLAY_INI_ENTRIES()` for the two, `tidy.clean_output` through
/// `php_ini_boolean_displayer_cb`, `On` or `Off`.
pub(crate) fn info_ini(req: &mut Request) {
    let entries = req.ini().entries();
    let rows: Vec<[String; 3]> = [DEFAULT_CONFIG, CLEAN_OUTPUT]
        .iter()
        .map(|name| {
            let entry = entries.iter().find(|e| e.decl.name == name.as_bytes());
            let local = entry.and_then(|e| e.value.as_deref()).unwrap_or_default();
            let master = entry.and_then(|e| e.master.as_deref()).unwrap_or_default();
            let shown = |value: &[u8]| {
                if *name == CLEAN_OUTPUT {
                    let on = Ini::parse_bool(value);
                    String::from(if on { "On" } else { "Off" })
                } else {
                    String::from_utf8_lossy(value).into_owned()
                }
            };
            [(*name).to_string(), shown(local), shown(master)]
        })
        .collect();
    let cells: Vec<[&str; 3]> = rows
        .iter()
        .map(|[name, local, master]| [name.as_str(), local.as_str(), master.as_str()])
        .collect();
    req.ini_mut().info_ini_table(&cells);
}
