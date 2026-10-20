//! The `php_output` layer, `Z/main/output.c` at the pin's tag: the handler
//! stack a request's bytes pass through on their way to the host, laid
//! down on the request heap where php lays it, so what `ob_start()`
//! holds and what the first byte written holds are what
//! `memory_get_usage()` says under php (design §5.13). One `Output` per
//! request, php's `OG` and the header state `sapi_send_headers` keeps
//! beside it; `write` is `php_output_write` and `buffer_op` the `ob_*`
//! bodies. The functions that may serve PHP code, a user handler or the
//! header callback, take `&mut Request`; the stack's own moves take the
//! heap alone.
//!
//! What the first non-empty write holds, and #244, which named it: `php_output_op` (`Z/main/output.c:1098`) reaches
//! `php_output_header` (`:102`) once bytes are about to reach the host
//! (`:1131`), which is `php_header()` (`Z/ext/standard/head.c:70`) and
//! `sapi_send_headers()` (`Z/main/SAPI.c:861`). That lays down the
//! default `Content-type`: `get_default_content_type` (`:299`)
//! `emalloc`s `"text/html; charset=UTF-8"` and its NUL, twenty-five
//! bytes in the thirty-two-byte bin, and `SG(sapi_headers).mimetype`
//! (`:880`) holds them until `sapi_deactivate_destroy` (`:542`). The
//! `Content-type: ...` header line built beside it (`:884`) is freed at
//! once under the CLI, whose `header_handler` answers zero
//! (`Z/sapi/cli/php_cli.c:376`, `sapi_header_add_op` at
//! `Z/main/SAPI.c:656`). So the thirty-two bytes are the mimetype and
//! nothing of the handler stack: an empty `echo` holds nothing, since
//! `context.out.used` gates the header, and `default_charset=` holds
//! sixteen.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use php_heap::{Block, Fatal, Heap, Str};

use crate::api::abi::Callable;
use crate::api::call;
use crate::api::raising::{Level, Thrown, docref};
use crate::api::request::{
    BufferHandler, ContentType, HandlerCall, HandlerOp, HandlerStatus, OutputOp, Request,
    StartHandler,
};
use crate::api::values::{Owner, PhpString, Value, ValueRef};

/// The handler operations, `Z/main/php_output.h:23-29`.
const HANDLER_WRITE: u32 = 0x00;
const HANDLER_START: u32 = 0x01;
const HANDLER_CLEAN: u32 = 0x02;
const HANDLER_FLUSH: u32 = 0x04;
const HANDLER_FINAL: u32 = 0x08;

/// The handler flags, `Z/main/php_output.h:32-47`.
const HANDLER_USER: u32 = 0x0001;
const HANDLER_CLEANABLE: u32 = 0x0010;
const HANDLER_FLUSHABLE: u32 = 0x0020;
const HANDLER_REMOVABLE: u32 = 0x0040;
pub(crate) const HANDLER_STDFLAGS: u32 = 0x0070;
const HANDLER_STARTED: u32 = 0x1000;
const HANDLER_DISABLED: u32 = 0x2000;
const HANDLER_PROCESSED: u32 = 0x4000;
const HANDLER_PRODUCED_OUTPUT: u32 = 0x8000;

/// A size as php's `zend_long`.
fn long(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

/// `PHP_OUTPUT_HANDLER_ABILITY_FLAGS`.
fn ability_flags(flags: u32) -> u32 {
    flags & !0xf00f
}

/// `php_output_stack_pop`'s flags, `Z/main/php_output.h:57-60`.
const POP_TRY: u32 = 0x000;
const POP_FORCE: u32 = 0x001;
const POP_DISCARD: u32 = 0x010;
const POP_SILENT: u32 = 0x100;

/// `OG(flags)`, `Z/main/php_output.h:63-70`.
const IMPLICITFLUSH: u32 = 0x01;
const DISABLED: u32 = 0x02;
const WRITTEN: u32 = 0x04;
const SENT: u32 = 0x08;
const ACTIVATED: u32 = 0x10_0000;

/// `PHP_OUTPUT_HANDLER_INITBUF_SIZE` (`Z/main/php_output.h:84-90`): a
/// chunk size rounded up to 4 KiB, or 16 KiB for none.
fn initbuf_size(chunk_size: usize) -> usize {
    if chunk_size > 0 {
        (chunk_size + 0xfff) & !0xfff
    } else {
        0x4000
    }
}

/// `sizeof(php_output_handler)` (`Z/main/php_output.h:123-137`): the
/// name, two ints, the chunk size, the buffer's four words, `opaq`, the
/// dtor and the function, eighty bytes `ecalloc`'d by
/// `php_output_handler_init` (`Z/main/output.c:870`).
const HANDLER_RECORD: usize = 80;
/// `sizeof(php_output_handler_user_func_t)` (`Z/main/php_output.h:117`):
/// the `fci`, the `fcc` and the `zoh` zval, 120 bytes `ecalloc`'d by
/// `php_output_handler_create_user` (`Z/main/output.c:483`).
const USER_RECORD: usize = 120;
/// `STACK_BLOCK_SIZE` (`Z/Zend/zend_stack.h:31`) pointers: what
/// `zend_stack_push` grows `OG(handlers).elements` by (`Z/Zend/zend_stack.c:37`).
const STACK_BLOCK: usize = 16 * 8;

/// `php_output_default_handler_name`.
const DEFAULT_NAME: &[u8] = b"default output handler";

/// What the handler's function is, `php_output_handler.func`.
enum Func {
    /// `php_output_handler_default_func`: the buffer passed along.
    Default,
    /// `php_output_handler_user_func_t`: the callable, its `zoh` copy
    /// held, and the record `ecalloc`'d beside it.
    User { callable: Callable, record: Block },
    /// `php_output_handler_context_func_t`: an extension's, under the
    /// name it registered, `php_output_handler_create_internal`.
    Registered(BufferHandler),
}

/// One `php_output_handler` (`Z/main/php_output.h:123`): its fields
/// here, and `record` the eighty bytes php holds them in.
struct Handler {
    record: Block,
    /// `name`, a string of the request's.
    name: Str,
    flags: u32,
    level: usize,
    /// `size`, the chunk size.
    size: usize,
    /// `buffer.data` and `buffer.size`; `None` once a failed handler
    /// handed its buffer on as what came out.
    buffer: Option<Block>,
    /// `buffer.used`.
    used: usize,
    func: Func,
}

/// `php_output_handler_status_t`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Status {
    Failure,
    Success,
    NoData,
}

/// `php_output_context` (`Z/main/php_output.h:100`): the operation and
/// the bytes going into a handler and coming out of it. php holds them
/// as borrowed or `estrndup`'d pointers freed before the operation
/// returns; these are the loop's own and hold nothing of the heap.
struct Context {
    op: u32,
    input: Vec<u8>,
    out: Vec<u8>,
}

impl Context {
    fn new(op: u32) -> Context {
        Context {
            op,
            input: Vec::new(),
            out: Vec::new(),
        }
    }

    /// `php_output_context_pass`.
    fn pass(&mut self) {
        self.out = std::mem::take(&mut self.input);
    }

    /// `php_output_context_swap`.
    fn swap(&mut self) {
        self.input = std::mem::take(&mut self.out);
    }

    /// `php_output_context_reset`.
    fn reset(&mut self) {
        self.input.clear();
        self.out.clear();
    }
}

/// `SG(sapi_headers).http_status_line` and `http_response_code` as they
/// stood, the line taken off the response with the request heap it
/// holds: what `soap_real_error_handler` keeps in `_old_http_status_line`
/// and `_old_http_response_code` (`Z/ext/soap/soap.c:1881-1884`). Dropped
/// without being put back, the line is gone from the response and its
/// bytes stay the request heap's until it ends, as php's pointer, set to
/// `NULL`, is.
#[derive(Debug)]
pub struct HttpStatus {
    line: Option<(Vec<u8>, Block)>,
    code: i64,
}

/// `sizeof(zend_llist_element) + sizeof(sapi_header_struct) - 1`, the
/// cell `zend_llist_add_element` copies a header into: a 24-byte element
/// whose one byte of `data` the 16-byte `sapi_header_struct` overlays.
const HEADER_CELL: usize = 24 + 16 - 1;

/// One line of `SG(sapi_headers).headers` and the request heap it holds
/// there (`Z/main/SAPI.c:656-673`): `sapi_header_struct.header`, the
/// bytes in the block `sapi_header_op` `estrndup`'d for them before it
/// cut the trailing whitespace (or the `Content-type: ` line it
/// `emalloc`s in its place, `:806`), and the cell the list copies the
/// struct into. Both are freed as the line leaves the list, by a
/// replace, a `header_remove()` or `sapi_deactivate`. A web host's
/// request holds `X-Powered-By` from its start, which the first line's
/// `memory_get_usage()` counts, and the default `Content-type` once the
/// first byte has sent the block:
///
/// ```php
/// $m = memory_get_usage(); header('X-Long: ' . str_repeat('v', 30) . "  "); $added = memory_get_usage() - $m;
/// header('X-Long: w', true); $replaced = memory_get_usage() - $m;
/// header_remove('X-Long'); $removed = memory_get_usage() - $m;
/// $m = memory_get_usage(); echo $added, ' ', $replaced, ' ', $removed, "\n"; echo memory_get_usage() - $m, "\n";
/// ```
/// ```request fpm
/// --GET--
/// q=1
/// ```
/// ```expect
/// X-Powered-By: PHP/8.5.9
/// Content-type: text/html; charset=UTF-8
///
/// 88 56 0
/// 112
/// ```
#[derive(Debug)]
struct HeaderLine {
    bytes: Block,
    len: usize,
    cell: Block,
}

impl HeaderLine {
    /// The line, its NUL and what the cut left past it off.
    fn line(&self) -> &[u8] {
        &self.bytes.bytes()[..self.len]
    }

    /// `sapi_free_header` and the cell's `efree`, as
    /// `zend_llist_del_element` and `zend_llist_destroy` free them.
    fn free(self, heap: &mut Heap) {
        heap.free_block(self.bytes);
        heap.free_block(self.cell);
    }
}

/// `zend_output_globals` (`Z/main/php_output.h:139-146`), and the
/// `sapi_globals` the header ops and the first write move:
/// `SG(sapi_headers)` and `SG(headers_sent)`.
#[allow(clippy::struct_excessive_bools)] // php's `OG` and `SG` flags, each its own global, read alone
pub(crate) struct Output {
    /// `OG(handlers)`, bottom first; the top is `OG(active)`.
    handlers: Vec<Handler>,
    /// `OG(handlers).elements`: the `zend_stack`'s own storage, sixteen
    /// pointers a block, grown by a push past `max` and freed by
    /// `zend_stack_destroy` at deactivation and never before, so a
    /// popped handler leaves its slot's 128 bytes out for the request.
    elements: Option<Block>,
    /// `OG(handlers).max`, in elements.
    max: usize,
    /// `OG(running)`, by position.
    running: Option<usize>,
    flags: u32,
    /// `OG(output_start_filename)` and `OG(output_start_lineno)`.
    started: Option<(Vec<u8>, u32)>,
    /// `SG(headers_sent)`.
    headers_sent: bool,
    /// `SG(request_info).no_headers`: the host writes no head, and a
    /// header op after the head counts sent is taken without a warning.
    no_headers: bool,
    /// `SG(sapi_headers).mimetype`, the thirty-two bytes this file's doc
    /// names, out from the first byte to the host until the request ends.
    mimetype: Option<Block>,
    /// `SG(sapi_headers).headers`: the lines the host will be handed, in
    /// the order `sapi_header_add_op` took them.
    headers: Vec<HeaderLine>,
    /// `SG(sapi_headers).send_default_content_type`: on until a
    /// `Content-Type` header is taken or the block is sent.
    send_default_content_type: bool,
    /// `SG(sapi_headers).http_response_code`: 200 under a web host, which
    /// sets it at request init (`Z/sapi/cgi/cgi_main.c:1181`), and zero
    /// under the CLI, which does not; `sapi_activate` leaves it.
    response_code: i64,
    /// `SG(sapi_headers).http_status_line`, an `HTTP/...` line `header()`
    /// was given, and the request heap it holds until
    /// `sapi_send_headers_free`.
    status_line: Option<(Vec<u8>, Block)>,
    /// Whether the host's `header_handler` takes a line: every host but
    /// the CLI, whose handler answers zero
    /// (`Z/sapi/cli/php_cli.c:376`), so `sapi_header_add_op` frees the
    /// line and the list stays empty. Whether the description carries a
    /// request of the web.
    takes_headers: bool,
    /// `SG(request_info).headers_only`: a `HEAD` request, whose body
    /// `php_header` refuses once the head is sent.
    headers_only: bool,
}

impl Output {
    /// `php_output_activate` (`Z/main/output.c:159`): `OG` zeroed,
    /// the stack initialised, which `zend_stack_init` does without an
    /// allocation, and the layer marked active; and `sapi_activate`
    /// (`Z/main/SAPI.c:429`) for the header state, with the response
    /// code the host leaves, zero under the CLI, and `headers_sent` and
    /// `no_headers` off whatever the host set before, as `sapi_activate`
    /// leaves them; [`Output::quiet`] sets them once the request started.
    pub(crate) fn activate(web: Option<&crate::engine::WebRequest>) -> Output {
        Output {
            handlers: Vec::new(),
            elements: None,
            max: 0,
            running: None,
            flags: ACTIVATED,
            started: None,
            headers_sent: false,
            no_headers: false,
            mimetype: None,
            headers: Vec::new(),
            send_default_content_type: true,
            response_code: web.map_or(0, |web| web.response_code),
            status_line: None,
            takes_headers: web.is_some(),
            headers_only: web.is_some_and(|web| web.method == Some(crate::engine::Method::Head)),
        }
    }

    /// `OG(running)->flags`, while a handler is being served.
    pub(crate) fn running_flags(&self) -> Option<u32> {
        self.running
            .and_then(|at| self.handlers.get(at))
            .map(|handler| handler.flags)
    }

    /// `SG(sapi_headers).headers`, in order.
    pub(crate) fn headers(&self) -> impl Iterator<Item = &[u8]> {
        self.headers.iter().map(HeaderLine::line)
    }

    /// `zend_llist_clean(&SG(sapi_headers).headers)`: every line freed.
    fn clean_headers(&mut self, heap: &mut Heap) {
        for line in self.headers.drain(..) {
            line.free(heap);
        }
    }

    /// `SG(sapi_headers).send_default_content_type = 0`: no default
    /// `Content-type` at the commit.
    pub(crate) fn no_default_content_type(&mut self) {
        self.send_default_content_type = false;
    }

    /// `SG(sapi_headers).http_response_code`.
    pub(crate) fn response_code(&self) -> i64 {
        self.response_code
    }

    /// `http_response_code(int)`'s write: the code alone, the status
    /// line left standing (`Z/ext/standard/head.c:399`).
    pub(crate) fn set_response_code(&mut self, code: i64) {
        self.response_code = code;
    }

    /// `SG(sapi_headers).http_status_line`, while one is held.
    pub(crate) fn status_line(&self) -> Option<&[u8]> {
        self.status_line.as_ref().map(|(line, _)| line.as_slice())
    }

    /// The status line and the response code as they stand, the line
    /// taken off, `SG(sapi_headers).http_status_line = NULL`, and the code
    /// left standing.
    pub(crate) fn take_status(&mut self) -> HttpStatus {
        HttpStatus {
            line: self.status_line.take(),
            code: self.response_code,
        }
    }

    pub(crate) fn started_at(&self) -> Option<(Vec<u8>, u32)> {
        self.started.clone()
    }

    /// `php_output_get_status` (`Z/main/output.c:218`): `OG(flags)` with
    /// `PHP_OUTPUT_ACTIVE` while a handler is on the stack and
    /// `PHP_OUTPUT_LOCKED` while one runs, the low byte.
    pub(crate) fn status(&self) -> u32 {
        let active = if self.handlers.is_empty() { 0 } else { 0x10 };
        let locked = if self.running.is_some() { 0x20 } else { 0 };
        (self.flags | active | locked) & 0xff
    }

    /// `SG(sapi_headers).mimetype`, its NUL off, and
    /// `send_default_content_type`.
    pub(crate) fn content_type(&self) -> ContentType {
        match &self.mimetype {
            Some(block) => {
                let bytes = block.bytes();
                ContentType::Held(bytes[..bytes.len().saturating_sub(1)].to_vec())
            }
            None if self.send_default_content_type => ContentType::Default,
            None => ContentType::Unset,
        }
    }

    pub(crate) fn headers_sent(&self) -> bool {
        self.headers_sent
    }

    /// What php-cgi does once `php_request_startup` returned, where the
    /// line asked for no head (`Z/sapi/cgi/cgi_main.c:2468`): the head
    /// counted sent and `no_headers` set, so `X-Powered-By` and what the
    /// startup took are in the list and no later op warns.
    pub(crate) fn quiet(&mut self) {
        self.headers_sent = true;
        self.no_headers = true;
    }

    /// `php_output_get_level`.
    fn level(&self) -> usize {
        self.handlers.len()
    }

    /// `php_output_handler_started` (`Z/main/output.c:569`): whether a
    /// handler of `name` is on the stack.
    pub(crate) fn handler_started(&self, name: &[u8]) -> bool {
        self.handlers.iter().any(|h| h.name.bytes() == name)
    }

    /// `php_output_set_implicit_flush`.
    fn set_implicit_flush(&mut self, on: bool) {
        if on {
            self.flags |= IMPLICITFLUSH;
        } else {
            self.flags &= !IMPLICITFLUSH;
        }
    }

    /// `zend_stack_push` (`Z/Zend/zend_stack.c:33`): another block of
    /// sixteen slots when the top has reached `max`, then the handler
    /// on top; its return is the level.
    fn push(&mut self, heap: &mut Heap, handler: Handler) -> Result<usize, Fatal> {
        if self.handlers.len() >= self.max {
            self.max += 16;
            self.elements = Some(match self.elements.take() {
                Some(elements) => heap.block_resize(elements, self.max * 8)?,
                None => heap.block(STACK_BLOCK)?,
            });
        }
        self.handlers.push(handler);
        Ok(self.handlers.len() - 1)
    }

    /// `php_output_handler_init` (`Z/main/output.c:865`): the record,
    /// the name held, the buffer of its initial size. What the heap
    /// refuses is the fatal, with what was laid down given back.
    fn handler_init(
        heap: &mut Heap,
        name: Str,
        chunk_size: usize,
        flags: u32,
        func: Func,
    ) -> Result<Handler, Fatal> {
        let record = heap.block(HANDLER_RECORD)?;
        let buffer = match heap.block(initbuf_size(chunk_size)) {
            Ok(buffer) => buffer,
            Err(fatal) => {
                heap.free_block(record);
                return Err(fatal);
            }
        };
        Ok(Handler {
            record,
            name,
            flags,
            level: 0,
            size: chunk_size,
            buffer: Some(buffer),
            used: 0,
            func,
        })
    }

    /// `php_output_handler_append` (`Z/main/output.c:882`): the bytes
    /// into the buffer, grown by the larger of the chunk's initial size
    /// and the shortfall's, rounded; whether the handler need not be served
    /// yet, which a chunked handler answers no once it holds a chunk,
    /// unless a handler is already running.
    ///
    /// A chunk of 100 starts a 4 KiB buffer and 5000 bytes grow it by
    /// 4 KiB more; the outer buffer takes what the chunked one passes
    /// on, so nothing reaches the host and the mimetype is not laid
    /// down.
    ///
    /// ```php
    /// ob_start(); $b = memory_get_usage(); ob_start(null, 100); echo str_repeat("x", 5000); $grown = memory_get_usage() - $b; ob_end_clean();
    /// ob_start(null, 100); echo str_repeat("x", 250); $chunk = memory_get_usage() - $b; ob_end_clean(); ob_end_clean(); var_dump($grown, $chunk);
    /// ```
    /// ```expect
    /// int(8320)
    /// int(4224)
    /// ```
    fn append(&mut self, heap: &mut Heap, at: usize, input: &[u8]) -> Result<bool, Fatal> {
        let running = self.running.is_some();
        let Some(handler) = self.handlers.get_mut(at) else {
            return Ok(true);
        };
        if input.is_empty() {
            return Ok(true);
        }
        self.flags |= WRITTEN;
        let Some(mut buffer) = handler.buffer.take() else {
            return Ok(true);
        };
        if buffer.len() - handler.used <= input.len() {
            let grow_int = initbuf_size(handler.size);
            let grow_buf = initbuf_size(input.len() - (buffer.len() - handler.used));
            let grown = buffer.len() + grow_int.max(grow_buf);
            buffer = heap.block_resize(buffer, grown)?;
        }
        buffer.bytes_mut()[handler.used..handler.used + input.len()].copy_from_slice(input);
        handler.used += input.len();
        handler.buffer = Some(buffer);
        if handler.size != 0 && handler.used >= handler.size {
            return Ok(running);
        }
        Ok(true)
    }

    /// The handler's buffered bytes, `buffer.data` to `buffer.used`.
    fn buffered(&self, at: usize) -> Vec<u8> {
        self.handlers
            .get(at)
            .and_then(|handler| Some(handler.buffer.as_ref()?.bytes()[..handler.used].to_vec()))
            .unwrap_or_default()
    }

    /// `php_output_handler_dtor` and `php_output_handler_free`
    /// (`Z/main/output.c:706-736`): the name released, the buffer, the
    /// user record and the record itself given back.
    fn handler_free(heap: &mut Heap, handler: Handler) -> Option<Value> {
        heap.release_string(handler.name);
        if let Some(buffer) = handler.buffer {
            heap.free_block(buffer);
        }
        heap.free_block(handler.record);
        match handler.func {
            Func::Default | Func::Registered(_) => None,
            Func::User { callable, record } => {
                heap.free_block(record);
                Some(callable.into_value())
            }
        }
    }

    /// `php_output_handler_status`'s fields (`Z/main/output.c:1235`),
    /// read before the array they fill is built on the heap.
    fn status_fields(&self, at: usize) -> Option<StatusFields> {
        let handler = self.handlers.get(at)?;
        Some(StatusFields {
            name: handler.name.bytes().to_vec(),
            flags: handler.flags,
            level: handler.level,
            chunk_size: handler.size,
            buffer_size: handler.buffer.as_ref().map_or(0, Block::len),
            buffer_used: handler.used,
        })
    }
}

/// The seven keys of one `ob_get_status()` row.
struct StatusFields {
    name: Vec<u8>,
    flags: u32,
    level: usize,
    chunk_size: usize,
    buffer_size: usize,
    buffer_used: usize,
}

impl StatusFields {
    /// `php_output_handler_status`: the row as its array.
    fn into_array(self, req: &mut Request) -> Result<Value, Thrown> {
        let mut entry = req.array()?;
        let name = Value::string(req.string(&self.name)?);
        let rows: [(&[u8], Value); 7] = [
            (b"name", name),
            (b"type", Value::int(i64::from(self.flags & 0xf))),
            (b"flags", Value::int(i64::from(self.flags))),
            (b"level", Value::int(long(self.level))),
            (b"chunk_size", Value::int(long(self.chunk_size))),
            (b"buffer_size", Value::int(long(self.buffer_size))),
            (b"buffer_used", Value::int(long(self.buffer_used))),
        ];
        for (key, value) in rows {
            let key = PhpString::new(req, key)?.into_key();
            entry.set(req, key, Owner::adopt(value))?;
        }
        Ok(Value::array(entry))
    }
}

/// What a pop's notice calls its act, `discard` or `send`.
fn pop_verb(flags: u32) -> &'static str {
    if flags & POP_DISCARD != 0 {
        "discard"
    } else {
        "send"
    }
}

/// `php_output_write` (`Z/main/output.c:241`): through the stack while
/// the layer is active, else `php_output_direct`, unless the layer is
/// disabled.
///
/// The first non-empty write holds thirty-two bytes, the mimetype this
/// file's doc names, and an empty one holds nothing; a second write holds no
/// more (#211's witness reads its deltas before it prints for this
/// reason, and this is the reason).
///
/// ```php
/// $b = memory_get_usage(); echo ""; var_dump(memory_get_usage() - $b);
/// ```
/// ```expect
/// int(0)
/// ```
/// ```php
/// $b = memory_get_usage(); echo "x"; var_dump(memory_get_usage() - $b); var_dump(memory_get_usage() - $b);
/// ```
/// ```expect
/// xint(32)
/// int(32)
/// ```
pub(crate) fn write(req: &mut Request, bytes: &[u8]) {
    let flags = req.output().flags;
    if flags & ACTIVATED != 0 {
        op(req, HANDLER_WRITE, bytes);
    } else if flags & DISABLED == 0 {
        req.ub_write(bytes);
    }
}

/// `php_output_lock_error` (`Z/main/output.c:773`): an `ob_*` act
/// from inside a running handler deactivates the layer and is php's
/// fatal.
fn lock_error(req: &mut Request, operation: u32) -> bool {
    let output = req.output();
    if operation != 0 && output.level() > 0 && output.running.is_some() {
        deactivate(req);
        let message = format!(
            "{}(): Cannot use output buffering in output buffering display handlers",
            req.active_native()
        );
        req.end_in_fatal(&message);
        return true;
    }
    false
}

/// `php_output_op` (`Z/main/output.c:1098`): the operation through the
/// one active handler or the whole stack, and what comes out to the
/// host, with the header block committed at the first byte that does.
fn op(req: &mut Request, operation: u32, bytes: &[u8]) {
    if lock_error(req, operation) {
        return;
    }
    let mut context = Context::new(operation);
    let count = req.output().level();
    if count > 0 {
        context.input = bytes.to_vec();
        if count > 1 {
            apply_op_topdown(req, &mut context);
        } else if req.output().handlers[0].flags & HANDLER_DISABLED == 0 {
            handler_op(req, 0, &mut context);
        } else {
            context.pass();
        }
    } else {
        context.out = bytes.to_vec();
    }
    if !context.out.is_empty() {
        header(req);
        if req.output().flags & DISABLED == 0 {
            req.ub_write(&context.out);
            req.output_mut().flags |= SENT;
        }
    }
}

/// `zend_stack_apply_with_argument` over `php_output_stack_apply_op`,
/// `ZEND_STACK_APPLY_TOPDOWN`: from the top down until a handler keeps
/// everything.
fn apply_op_topdown(req: &mut Request, context: &mut Context) {
    let mut at = req.output().level();
    while at > 0 {
        at -= 1;
        if stack_apply_op(req, at, context) {
            break;
        }
    }
}

/// `php_output_stack_apply_op` (`Z/main/output.c:1160`): whether the
/// walk stops here.
fn stack_apply_op(req: &mut Request, at: usize, context: &mut Context) -> bool {
    let Some(handler) = req.output().handlers.get(at) else {
        return true;
    };
    let was_disabled = handler.flags & HANDLER_DISABLED != 0;
    let level = handler.level;
    let status = if was_disabled {
        Status::Failure
    } else {
        handler_op(req, at, context)
    };
    match status {
        Status::NoData => true,
        Status::Success => {
            if level != 0 {
                context.swap();
            }
            false
        }
        Status::Failure => {
            if was_disabled {
                if level == 0 {
                    context.pass();
                }
            } else if level != 0 {
                context.swap();
            }
            false
        }
    }
}

/// `php_output_handler_op` (`Z/main/output.c:917`): the input into the
/// handler's buffer, and the handler served when it must, a user handler
/// with the buffer as a string of the request's and the operation, the
/// default one passing its buffer along. A user handler that wrote to
/// the layer is disabled for it, with php's deprecation; one that
/// returned `false`, or threw, fails and its buffer goes on unchanged.
fn handler_op(req: &mut Request, at: usize, context: &mut Context) -> Status {
    let Some(handler) = req.output().handlers.get(at) else {
        return Status::Failure;
    };
    if handler.flags & HANDLER_DISABLED != 0 {
        return Status::Failure;
    }
    if lock_error(req, context.op) {
        return Status::Failure;
    }
    let output = req.output_mut();
    if context.op == HANDLER_WRITE && output.level() > 0 && output.running.is_some() {
        output.handlers[at].flags |= HANDLER_PRODUCED_OUTPUT;
    }
    let original_op = context.op;
    let (output, heap) = req.output_and_heap();
    let storable = match output.append(heap, at, &context.input) {
        Ok(storable) => storable,
        Err(fatal) => {
            req.end_in(&fatal);
            return Status::Failure;
        }
    };
    let mut still_have_handler = true;
    let status;
    if storable && context.op == HANDLER_WRITE {
        return Status::NoData;
    }
    let (started, serve) = {
        let Some(handler) = req.output().handlers.get(at) else {
            return Status::Failure;
        };
        let serve = match &handler.func {
            Func::User { callable, .. } => Serve::User(callable.clone()),
            Func::Registered(registered) => Serve::Registered(*registered),
            Func::Default => Serve::Default,
        };
        (handler.flags & HANDLER_STARTED != 0, serve)
    };
    if !started {
        context.op |= HANDLER_START;
    }
    req.output_mut().running = Some(at);
    if let Serve::User(callable) = serve {
        let buffered = req.output().buffered(at);
        status = match user_handler(req, at, &callable, &buffered, context) {
            Ok(status) => status,
            Err(thrown) => {
                req.leave_pending(&thrown);
                // A handler that bailed out, a fatal or a throwable no
                // frame took at shutdown, leaves the operation there:
                // nothing settles and nothing it held is passed on.
                if req.bailing_out() {
                    req.output_mut().running = None;
                    return Status::Failure;
                }
                Status::Failure
            }
        };
        still_have_handler = req.output().handlers.len() > at;
    } else if let Serve::Registered(registered) = serve {
        status = registered_handler(req, at, &registered, context);
        still_have_handler = req.output().handlers.len() > at;
    } else {
        context.input = req.output().buffered(at);
        context.pass();
        status = if context.out.is_empty() {
            Status::NoData
        } else {
            Status::Success
        };
    }
    let output = req.output_mut();
    if still_have_handler && at < output.handlers.len() {
        output.handlers[at].flags |= HANDLER_STARTED;
    }
    output.running = None;
    if !still_have_handler {
        return status;
    }
    settle(req, at, status, context);
    context.op = original_op;
    status
}

/// Whom `php_output_handler_op` serves the buffer to.
enum Serve {
    Default,
    Registered(BufferHandler),
    User(Callable),
}

/// An extension's handler served (`Z/main/output.c:1038-1050`): the
/// buffer fed in as `context->in`, what it wrote the operation's, and
/// `PHP_OUTPUT_HANDLER_NO_DATA` for nothing written; a handler that
/// asked to be immutable loses `CLEANABLE` and `REMOVABLE`.
fn registered_handler(
    req: &mut Request,
    at: usize,
    registered: &BufferHandler,
    context: &mut Context,
) -> Status {
    let buffered = req.output().buffered(at);
    let op = context.op;
    let mut call = HandlerCall {
        op: HandlerOp {
            start: op & HANDLER_START != 0,
            clean: op & HANDLER_CLEAN != 0,
            flush: op & HANDLER_FLUSH != 0,
            finish: op & HANDLER_FINAL != 0,
        },
        input: &buffered,
        out: Vec::new(),
        immutable: false,
    };
    let answer = (registered.handler)(req, &mut call);
    if call.immutable
        && let Some(handler) = req.output_mut().handlers.get_mut(at)
    {
        handler.flags &= !(HANDLER_REMOVABLE | HANDLER_CLEANABLE);
    }
    match answer {
        Ok(HandlerStatus::Success) if call.out.is_empty() => Status::NoData,
        Ok(HandlerStatus::Success) => {
            context.out = call.out;
            Status::Success
        }
        Ok(HandlerStatus::Failure) => Status::Failure,
        Err(thrown) => {
            req.leave_pending(&thrown);
            Status::Failure
        }
    }
}

/// The end of `php_output_handler_op` (`Z/main/output.c:1063-1090`):
/// a failed handler disabled, its buffer handed on as what comes out
/// and given back; one that kept everything or gave something out is
/// emptied and marked processed.
fn settle(req: &mut Request, at: usize, status: Status, context: &mut Context) {
    let (output, heap) = req.output_and_heap();
    let Some(handler) = output.handlers.get_mut(at) else {
        return;
    };
    match status {
        Status::Failure => {
            handler.flags |= HANDLER_DISABLED;
            context.out.clear();
            if let Some(buffer) = handler.buffer.take() {
                context
                    .out
                    .extend_from_slice(&buffer.bytes()[..handler.used]);
                heap.free_block(buffer);
            }
            handler.used = 0;
        }
        Status::NoData => {
            context.reset();
            handler.used = 0;
            handler.flags |= HANDLER_PROCESSED;
        }
        Status::Success => {
            handler.used = 0;
            handler.flags |= HANDLER_PROCESSED;
        }
    }
}

/// The user handler's call (`Z/main/output.c:944-1035`): `ob_data` as
/// a string of the request's, `ob_mode` the operation, and what it
/// returned read: `false` fails, a bool keeps everything, anything
/// else is its string form and is what comes out.
///
/// A string is what the buffer becomes.
///
/// ```php
/// function wraps($buffer, $phase) { return "[$buffer]"; }
/// ob_start('wraps'); echo "one"; ob_end_flush(); echo "\n";
/// ```
/// ```expect
/// [one]
/// ```
///
/// `false` fails the handler, which disables it and hands its buffer on
/// unchanged.
///
/// ```php
/// function refuses($buffer, $phase) { return false; }
/// ob_start('refuses'); echo "two"; ob_end_flush(); echo "\n"; var_dump(ob_get_level());
/// ```
/// ```expect
/// two
/// int(0)
/// ```
///
/// `true` is the handler keeping everything: nothing comes out and the
/// buffer is emptied with it.
///
/// ```php
/// function keeps($buffer, $phase) { return true; }
/// ob_start('keeps'); echo "three"; ob_end_flush(); echo "|\n"; var_dump(ob_get_level());
/// ```
/// ```expect
/// |
/// int(0)
/// ```
///
/// A handler that writes through the layer it serves has that write
/// discarded and raises php's deprecation, named for the native that
/// ran the operation; the raise itself is written past the handler,
/// which is why it stands ahead of what the handler returned.
///
/// ```php
/// function talks($buffer, $phase) { echo "swallowed"; return "[$buffer]"; }
/// ob_start('talks'); echo "four"; ob_end_flush(); echo "\n";
/// ```
/// ```expect
/// Deprecated: ob_end_flush(): Producing output from user output handler talks is deprecated in %s on line 3
/// [four]
/// ```
///
/// A handler that threw failed, whatever took the throwable: at the
/// request's end the handler `set_exception_handler` set takes it and
/// echoes into the buffer, which is passed along as it stands.
///
/// ```php
/// set_exception_handler(function (Throwable $e) { echo "Caught: ", $e->getMessage(), "\n"; });
/// ob_start(function () { throw new Exception("ob_start"); });
/// echo "x";
/// ```
/// ```expect
/// xCaught: ob_start
/// ```
fn user_handler(
    req: &mut Request,
    at: usize,
    callable: &Callable,
    buffered: &[u8],
    context: &mut Context,
) -> Result<Status, Thrown> {
    let data = req.string(buffered)?;
    let arguments = [Value::string(data), Value::int(i64::from(context.op))];
    let returned = call::call(req, callable, &arguments).map(Owner::hand_on);
    req.release(Value::string(data));
    let returned = returned?;
    // `Z_TYPE(retval) != IS_UNDEF`: a handler that threw, even where
    // the exception handler took it at the request's end, failed, and
    // its buffer is passed along as it stands.
    if returned.tag() == crate::api::values::TypeTag::Undef {
        return Ok(Status::Failure);
    }
    produced_output(req, at);
    let status = match returned.as_ref() {
        ValueRef::Bool(false) => Status::Failure,
        ValueRef::Bool(true) => Status::NoData,
        _ => {
            let text = crate::api::coerce::to_string(req, &returned)?;
            let status = if text.is_empty() {
                Status::NoData
            } else {
                context.out = text.as_bytes().to_vec();
                Status::Success
            };
            text.give_up(req);
            status
        }
    };
    req.release(returned);
    Ok(status)
}

/// What a handler wrote through the layer it serves
/// (`Z/main/output.c:978-1015`): the write itself was appended to the
/// handler's own buffer and is dropped with it, so php raises the
/// deprecation with the handler disabled, which is what carries the
/// raise past the buffer that would swallow it, and enables it again
/// where the raise left it standing.
fn produced_output(req: &mut Request, at: usize) {
    let Some(handler) = req.output_mut().handlers.get_mut(at) else {
        return;
    };
    if handler.flags & HANDLER_PRODUCED_OUTPUT == 0 {
        return;
    }
    handler.flags |= HANDLER_DISABLED;
    handler.flags &= !HANDLER_PRODUCED_OUTPUT;
    let name = String::from_utf8_lossy(handler.name.bytes()).into_owned();
    docref(
        req,
        Level::Deprecated,
        &format!("Producing output from user output handler {name} is deprecated"),
    );
    if let Some(handler) = req.output_mut().handlers.get_mut(at) {
        handler.flags &= !HANDLER_DISABLED;
    }
}

/// `php_output_set_status(PHP_OUTPUT_DISABLED)` (`Z/main/output.c:209`):
/// the low bits of `OG(flags)` replaced by the one, so nothing reaches
/// the host again.
pub(crate) fn disable(req: &mut Request) {
    let flags = &mut req.output_mut().flags;
    *flags = (*flags & !0xf) | DISABLED;
}

/// `php_output_header` (`Z/main/output.c:102`): where the body started,
/// taken once, and the header block committed through `php_header`;
/// the layer is disabled when that refuses it.
pub(crate) fn header(req: &mut Request) {
    if req.output().headers_sent {
        return;
    }
    if req.output().started.is_none() {
        let file = req.current_file().map(<[u8]>::to_vec).unwrap_or_default();
        let line = req.current_line();
        req.output_mut().started = Some((file, line));
    }
    // `php_header`: a `HEAD` request's body goes nowhere once its head
    // is sent.
    if !send_headers(req) || req.output().headers_only {
        req.output_mut().flags |= DISABLED;
    }
}

/// `sapi_send_headers` (`Z/main/SAPI.c:861`): while
/// `send_default_content_type` stands, the default `Content-type` laid
/// down as this file's doc says and its header line built beside it,
/// which `sapi_header_add_op` hands the host, so a web host's block
/// ends in it and the CLI's, whose `header_handler` answers zero, frees
/// it at once; the callback `header_register_callback` left served;
/// `headers_sent` set; and `sapi_send_headers_free`, the status line
/// given back. The block is handed to the sink here, php's
/// `sapi_module.send_headers`, whose answer is that it sent them.
/// Nothing here answers `headers_only`, the
/// `HEAD` request that refuses a body, since no host describes one yet.
fn send_headers(req: &mut Request) -> bool {
    if req.output().headers_sent {
        return true;
    }
    if req.output().send_default_content_type {
        let mimetype = default_content_type(req);
        let (output, heap) = req.output_and_heap();
        match heap.block(mimetype.len() + 1) {
            Ok(mut block) => {
                block.bytes_mut()[..mimetype.len()].copy_from_slice(&mimetype);
                output.mimetype = Some(block);
            }
            Err(fatal) => {
                req.end_in(&fatal);
                return false;
            }
        }
        // `default_header.header`, `Content-type: ` and the mimetype, on
        // the request heap: the list's where the host takes it.
        let mut line = b"Content-type: ".to_vec();
        line.extend_from_slice(&mimetype);
        let Some(bytes) = header_bytes(req, &line, line.len()) else {
            return false;
        };
        if !header_add_op(req, false, bytes, line.len()) {
            return false;
        }
        req.output_mut().send_default_content_type = false;
    }
    if let Some(callback) = req.take_header_callback() {
        match call::call(req, &callback, &[]) {
            Ok(returned) => returned.give_up(req),
            Err(thrown) => req.leave_pending(&thrown),
        }
        callback.give_up(req);
    }
    req.output_mut().headers_sent = true;
    req.send_header_block();
    send_headers_free(req);
    true
}

/// `sapi_send_headers_free` (`Z/main/SAPI.c:490`): the status line and
/// its bytes given back.
fn send_headers_free(req: &mut Request) {
    let (output, heap) = req.output_and_heap();
    if let Some((_, block)) = output.status_line.take() {
        heap.free_block(block);
    }
}

/// `status` put back as it was taken: the status line standing now
/// freed, `efree(SG(sapi_headers).http_status_line)`, then the line and
/// the code that stood, as `soap_real_error_handler`'s `zend_catch` puts
/// them back (`Z/ext/soap/soap.c:1950-1957`).
pub(crate) fn put_back_status(req: &mut Request, status: HttpStatus) {
    send_headers_free(req);
    let output = req.output_mut();
    output.status_line = status.line;
    output.response_code = status.code;
}

/// `sapi_header_op`'s refusal once the block is sent
/// (`Z/main/SAPI.c:685-694`): php's warning naming where the body
/// started, through `sapi_module.sapi_error`, which is `php_error`
/// under the CLI, so the text carries no native's name; none where
/// the host writes no head (`no_headers`).
fn header_modifiable(req: &mut Request) -> bool {
    if !req.output().headers_sent || req.output().no_headers {
        return true;
    }
    let message = match req.output().started_at() {
        Some((file, line)) => format!(
            "Cannot modify header information - headers already sent by (output started at {}:{line})",
            String::from_utf8_lossy(&file)
        ),
        None => String::from("Cannot modify header information - headers already sent"),
    };
    req.note_error(Level::Warning, &message);
    false
}

/// `sapi_update_response_code` (`Z/main/SAPI.c:586`): a code that
/// changed drops the status line; one that did not changes nothing.
fn update_response_code(req: &mut Request, code: i64) {
    if req.output().response_code == code {
        return;
    }
    send_headers_free(req);
    req.output_mut().response_code = code;
}

/// `sapi_extract_response_code` (`Z/main/SAPI.c:572`): the number after
/// the first space that a second space does not follow, 200 without one,
/// read by C's `atoi`, so a number past `int` is cut to its low 32 bits
/// and one past the long is `LONG_MAX`'s `-1`.
///
/// ```php
/// $r = []; foreach (["HTTP/1.1 4294967497 OK", "HTTP/1.1 99999999999999999999 X", "HTTP/1.0 \x0b302 Found"] as $h) { header($h); $r[] = http_response_code(); } echo implode(" ", $r);
/// ```
/// ```expect
/// 201 -1 302
/// ```
fn extract_response_code(line: &[u8]) -> i64 {
    let mut code = 200;
    for (at, byte) in line.iter().enumerate() {
        if *byte == b' ' && line.get(at + 1) != Some(&b' ') {
            code = i64::from(crate::c_number::atoi(&line[at + 1..]));
            break;
        }
    }
    code
}

/// `sapi_apply_default_charset` (`Z/main/SAPI.c:365`): a `text/` type
/// with no `charset=` of its own takes `;charset=` and
/// `default_charset` after it, with no space, where the default
/// `Content-type` writes `; charset=`; the type as given otherwise, and
/// whether it changed.
fn apply_default_charset(req: &Request, mimetype: &[u8]) -> (Vec<u8>, bool) {
    let charset = req
        .ini_get(b"default_charset")
        .map_or_else(|| b"UTF-8".to_vec(), <[u8]>::to_vec);
    let has_charset = mimetype
        .windows("charset=".len())
        .any(|window| window == b"charset=");
    if !charset.is_empty() && mimetype.starts_with(b"text/") && !has_charset {
        let mut applied = mimetype.to_vec();
        applied.extend_from_slice(b";charset=");
        applied.extend_from_slice(&charset);
        (applied, true)
    } else {
        (mimetype.to_vec(), false)
    }
}

/// The line less its trailing spaces, linefeeds and carriage returns,
/// as `sapi_header_op` cuts them (`Z/main/SAPI.c:736`).
fn trimmed_end(line: &[u8]) -> &[u8] {
    let mut end = line.len();
    while end > 0 && line[end - 1].is_ascii_whitespace() {
        end -= 1;
    }
    &line[..end]
}

/// `sapi_remove_header` (`Z/main/SAPI.c:600`): every line whose name is
/// `name`, case aside, with `:` after it; with `prefix_len` set, one
/// whose first `name.len()` bytes are `name` however it goes on; each
/// freed as `zend_llist_del_element` frees it.
fn remove_header(req: &mut Request, name: &[u8], prefix_len: usize) {
    let (output, heap) = req.output_and_heap();
    let mut kept = Vec::with_capacity(output.headers.len());
    for line in output.headers.drain(..) {
        let header = line.line();
        if header.len() > name.len()
            && (header[name.len()] == b':' || (prefix_len != 0 && name.len() > prefix_len))
            && header[..name.len()].eq_ignore_ascii_case(name)
        {
            line.free(heap);
        } else {
            kept.push(line);
        }
    }
    output.headers = kept;
}

/// `sapi_header_op`'s `estrndup(p->line, p->line_len)` over a line
/// written `written` bytes long, holding `line`, what the cut of its
/// trailing whitespace leaves of it; `None` where the heap refused, the
/// memory limit's fatal having ended the request.
fn header_bytes(req: &mut Request, line: &[u8], written: usize) -> Option<Block> {
    match req.heap_mut().block(written + 1) {
        Ok(mut bytes) => {
            bytes.bytes_mut()[..line.len()].copy_from_slice(line);
            Some(bytes)
        }
        Err(fatal) => {
            req.end_in(&fatal);
            None
        }
    }
}

/// `sapi_header_add_op` (`Z/main/SAPI.c:656`): the line, `len` bytes of
/// `bytes`, to the host's `header_handler`, which under the CLI answers
/// zero and frees it; where the host takes it, a replace drops the
/// lines of the same name first, and the line goes on the end in the
/// cell `zend_llist_add_element` lays down. Whether the heap took the
/// cell, which a refusal answers by ending the request.
fn header_add_op(req: &mut Request, replace: bool, bytes: Block, len: usize) -> bool {
    if !req.output().takes_headers {
        req.heap_mut().free_block(bytes);
        return true;
    }
    if replace && let Some(colon) = bytes.bytes()[..len].iter().position(|b| *b == b':') {
        let name = bytes.bytes()[..colon].to_vec();
        remove_header(req, &name, 0);
    }
    let cell = match req.heap_mut().block(HEADER_CELL) {
        Ok(cell) => cell,
        Err(fatal) => {
            req.heap_mut().free_block(bytes);
            req.end_in(&fatal);
            return false;
        }
    };
    req.output_mut()
        .headers
        .push(HeaderLine { bytes, len, cell });
    true
}

/// `sapi_header_op` for `SAPI_HEADER_ADD` and `SAPI_HEADER_REPLACE`
/// (`Z/main/SAPI.c:675`), what `header()` and `sapi_add_header` reach:
/// refused with the warning once the block is sent, and silently for an
/// empty line; the trailing whitespace cut; a second line or a NUL is
/// the warning and nothing taken. An `HTTP/` line is the status line and
/// the response code read off it. A `Content-Type` takes the default
/// charset, holds the mimetype where none is held yet, is rewritten as
/// `Content-type: ` and the applied type when the charset was applied,
/// and stops the default one; a `Location` outside a redirect sets
/// `response_code` or 302 (the 303 a non-`GET` request past HTTP/1.0
/// takes reads `request_method` and `proto_num`, which no description
/// carries yet); a `WWW-Authenticate` sets 401; a `Content-Length`
/// switches `zlib.output_compression` off, as a user's `ini_set()` would
/// (`Z/main/SAPI.c:819-828`). Then `response_code`, when given, and
/// `sapi_header_add_op`.
///
/// ```php
/// ini_set('zlib.output_compression', '1');
/// header('Content-Length: 3');
/// var_dump(ini_get('zlib.output_compression'));
/// ```
/// ```expect
/// string(1) "0"
/// ```
/// Whether the line was taken is the answer.
pub(crate) fn header_add(
    req: &mut Request,
    line: &[u8],
    replace: bool,
    response_code: i64,
) -> bool {
    if !header_modifiable(req) || line.is_empty() {
        return false;
    }
    let written = line.len();
    let line = trimmed_end(line);
    // The safety check walks the line once, and the first byte it trips
    // on names the refusal (`Z/main/SAPI.c:740-755`).
    match line.iter().find(|b| matches!(b, b'\n' | b'\r' | 0)) {
        Some(0) => {
            req.note_error(Level::Warning, "Header may not contain NUL bytes");
            return false;
        }
        Some(_) => {
            req.note_error(
                Level::Warning,
                "Header may not contain more than a single header, new line detected",
            );
            return false;
        }
        None => {}
    }
    if line.len() >= 5 && line[..5].eq_ignore_ascii_case(b"HTTP/") {
        update_response_code(req, extract_response_code(line));
        send_headers_free(req);
        let (output, heap) = req.output_and_heap();
        if let Ok(block) = heap.block(line.len() + 1) {
            output.status_line = Some((line.to_vec(), block));
        }
        return true;
    }
    let Some(mut held) = header_bytes(req, line, written) else {
        return false;
    };
    let mut len = line.len();
    if let Some(colon) = line.iter().position(|b| *b == b':') {
        let name = &line[..colon];
        if name.eq_ignore_ascii_case(b"Content-Type") {
            let mut value = &line[colon + 1..];
            while value.first() == Some(&b' ') {
                value = &value[1..];
            }
            let (mimetype, applied) = apply_default_charset(req, value);
            let (output, heap) = req.output_and_heap();
            if output.mimetype.is_none()
                && let Ok(mut block) = heap.block(mimetype.len() + 1)
            {
                block.bytes_mut()[..mimetype.len()].copy_from_slice(&mimetype);
                output.mimetype = Some(block);
            }
            if applied {
                // `newheader = emalloc(newlen)`, then `efree(header_line)`.
                let mut rewritten = b"Content-type: ".to_vec();
                rewritten.extend_from_slice(&mimetype);
                let Some(bytes) = header_bytes(req, &rewritten, rewritten.len()) else {
                    req.heap_mut().free_block(held);
                    return false;
                };
                req.heap_mut()
                    .free_block(std::mem::replace(&mut held, bytes));
                len = rewritten.len();
            }
            req.output_mut().send_default_content_type = false;
        } else if name.eq_ignore_ascii_case(b"Location") {
            let code = req.output().response_code;
            if !(300..=399).contains(&code) && code != 201 {
                update_response_code(
                    req,
                    if response_code != 0 {
                        response_code
                    } else {
                        302
                    },
                );
            }
        } else if name.eq_ignore_ascii_case(b"WWW-Authenticate") {
            update_response_code(req, 401);
        } else if name.eq_ignore_ascii_case(b"Content-Length") {
            // Req #44164: a script that sets the length cannot know the
            // size after compression, so compression is switched off.
            req.ini_set(b"zlib.output_compression", b"0").ok();
        }
    }
    if response_code != 0 {
        update_response_code(req, response_code);
    }
    header_add_op(req, replace, held, len)
}

/// `sapi_header_op` for `SAPI_HEADER_DELETE` and `SAPI_HEADER_DELETE_ALL`
/// (`Z/main/SAPI.c:675`), what `header_remove()` reaches: refused with
/// the warning once the block is sent; with no name, the list cleared;
/// with one, silently nothing for an empty name, the warning for a name
/// with a colon in it, and every line of that name dropped otherwise.
/// Whether the act was taken is the answer.
pub(crate) fn header_remove(req: &mut Request, name: Option<&[u8]>) -> bool {
    if !header_modifiable(req) {
        return false;
    }
    let Some(name) = name else {
        let (output, heap) = req.output_and_heap();
        output.clean_headers(heap);
        return true;
    };
    if name.is_empty() {
        return false;
    }
    let name = trimmed_end(name);
    if name.contains(&b':') {
        req.note_error(Level::Warning, "Header to delete may not contain colon.");
        return false;
    }
    remove_header(req, name, 0);
    true
}

/// `sapi_header_op` for `SAPI_HEADER_DELETE_PREFIX` (`Z/main/SAPI.c:705`):
/// refused with the warning once the block is sent, nothing for an empty
/// prefix, and otherwise, the trailing whitespace cut, every line that
/// opens with the prefix, whatever its case, dropped, where the prefix
/// runs past the header's name of `name_len` bytes; a colon in it is no
/// refusal, since the prefix names a value too (`Set-Cookie: PHPSESSID=`).
pub(crate) fn header_remove_prefix(req: &mut Request, prefix: &[u8], name_len: usize) -> bool {
    if !header_modifiable(req) || prefix.is_empty() {
        return false;
    }
    let prefix = trimmed_end(prefix);
    remove_header(req, prefix, name_len);
    true
}

/// `get_default_content_type` (`Z/main/SAPI.c:299`): `default_mimetype`
/// with `; charset=` and `default_charset` after it when the charset
/// is set and the type is `text/`.
fn default_content_type(req: &Request) -> Vec<u8> {
    let mimetype = req
        .ini_get(b"default_mimetype")
        .map_or_else(|| b"text/html".to_vec(), <[u8]>::to_vec);
    let charset = req
        .ini_get(b"default_charset")
        .map_or_else(|| b"UTF-8".to_vec(), <[u8]>::to_vec);
    if !charset.is_empty() && mimetype.len() >= 5 && mimetype[..5].eq_ignore_ascii_case(b"text/") {
        let mut content_type = mimetype;
        content_type.extend_from_slice(b"; charset=");
        content_type.extend_from_slice(&charset);
        content_type
    } else {
        mimetype
    }
}

/// `php_output_stack_pop` (`Z/main/output.c:1258`): the top handler
/// served a last time, `FINAL` with `CLEAN` when discarding, popped, what
/// came out written on through the rest, and the handler freed after
/// the write. A pop of nothing, or of a handler that is not removable
/// without `FORCE`, is the notice and nothing else.
///
/// ```php
/// ob_start(); echo "a"; ob_start(); echo "b"; ob_end_flush(); $outer = ob_get_contents(); ob_end_clean(); var_dump($outer);
/// ```
/// ```expect
/// string(2) "ab"
/// ```
fn stack_pop(req: &mut Request, flags: u32) -> bool {
    let level = req.output().level();
    if level == 0 {
        if flags & POP_SILENT == 0 {
            let verb = pop_verb(flags);
            docref(
                req,
                Level::Notice,
                &format!("Failed to {verb} buffer. No buffer to {verb}"),
            );
        }
        return false;
    }
    let top = level - 1;
    let (top_flags, name, top_level) = {
        let handler = &req.output().handlers[top];
        (
            handler.flags,
            String::from_utf8_lossy(handler.name.bytes()).into_owned(),
            handler.level,
        )
    };
    if flags & POP_FORCE == 0 && top_flags & HANDLER_REMOVABLE == 0 {
        if flags & POP_SILENT == 0 {
            let verb = pop_verb(flags);
            docref(
                req,
                Level::Notice,
                &format!("Failed to {verb} buffer of {name} ({top_level})"),
            );
        }
        return false;
    }
    let mut context = Context::new(HANDLER_FINAL);
    if top_flags & HANDLER_DISABLED == 0 {
        if top_flags & HANDLER_STARTED == 0 {
            context.op |= HANDLER_START;
        }
        if flags & POP_DISCARD != 0 {
            context.op |= HANDLER_CLEAN;
        }
        handler_op(req, top, &mut context);
    }
    // `zend_stack_del_top`: a handler a fatal inside the run already
    // freed is not popped twice.
    let orphan = if req.output().handlers.len() > top {
        req.output_mut().handlers.pop()
    } else {
        None
    };
    if !context.out.is_empty() && flags & POP_DISCARD == 0 {
        write(req, &context.out);
    }
    if let Some(orphan) = orphan {
        free_handler(req, orphan);
    }
    true
}

/// `php_output_handler_free` with the user handler's `zoh` released
/// through the request, which is where a value's last owner is served.
fn free_handler(req: &mut Request, handler: Handler) {
    if let Some(value) = Output::handler_free(req.heap_mut(), handler) {
        req.release(value);
    }
}

/// `php_output_flush` (`Z/main/output.c:255`): the top handler served
/// with `FLUSH`, and what came out written through the handlers under
/// it, the top set aside for the write and put back.
fn flush(req: &mut Request) -> bool {
    let level = req.output().level();
    if level == 0 || req.output().handlers[level - 1].flags & HANDLER_FLUSHABLE == 0 {
        return false;
    }
    let mut context = Context::new(HANDLER_FLUSH);
    handler_op(req, level - 1, &mut context);
    if !context.out.is_empty() {
        let top = req.output_mut().handlers.pop();
        write(req, &context.out);
        if let Some(top) = top {
            let (output, heap) = req.output_and_heap();
            if let Err(fatal) = output.push(heap, top) {
                req.end_in(&fatal);
            }
        }
    }
    true
}

/// `php_output_clean` (`Z/main/output.c:290`): the top handler served
/// with `CLEAN`, what it holds dropped.
fn clean(req: &mut Request) -> bool {
    let level = req.output().level();
    if level == 0 || req.output().handlers[level - 1].flags & HANDLER_CLEANABLE == 0 {
        return false;
    }
    let mut context = Context::new(HANDLER_CLEAN);
    handler_op(req, level - 1, &mut context);
    true
}

/// `php_output_end_all` (`Z/main/output.c:325`): every handler popped,
/// removable or not, top first, at shutdown's third step.
///
/// ```php
/// ob_start(); echo "a"; ob_start(); echo "b"; ob_start(null, 0, 0); echo "c";
/// ```
/// ```expect
/// abc
/// ```
///
/// A handler that throws here has no frame to take it: the uncaught
/// fatal is rendered into the buffer being handled and `zend_bailout`
/// leaves the walk, so neither is written.
///
/// ```php
/// echo "start\n";
/// ob_start(function ($b) { throw new Exception("x"); });
/// echo "hmm\n";
/// ```
/// ```expect exit=255
/// start
/// ```
pub(crate) fn end_all(req: &mut Request) {
    while req.output().level() > 0 && !req.bailing_out() && stack_pop(req, POP_FORCE) {}
}

/// `php_output_discard_all` (`Z/main/output.c:345`): every handler
/// popped with its buffer thrown away, each handler still served its
/// final call, which is what `php_error_cb` does before it displays the
/// memory limit's fatal while the heap's limit is lifted
/// (`Z/main/main.c:1405-1407`), so the fatal is not written into a
/// buffer the request would then flush.
///
/// ```php
/// ini_set('memory_limit', '2M');
/// ob_start(function ($b) { fwrite(STDOUT, "handled\n"); return $b; });
/// echo "discarded\n";
/// $a = str_repeat('x', 4 << 20);
/// ```
/// ```expect exit=255
/// handled
///
/// Fatal error: Allowed memory size of 2097152 bytes exhausted (tried to allocate 4194336 bytes) in %s on line 5
/// ```
pub(crate) fn discard_all(req: &mut Request) {
    while req.output().level() > 0 && stack_pop(req, POP_DISCARD | POP_FORCE) {}
}

/// `php_output_deactivate` (`Z/main/output.c:176`): the header block
/// committed if nothing sent it, the layer inactive, every handler
/// left freed and the stack's storage with them; and
/// `sapi_deactivate_destroy` (`Z/main/SAPI.c:537`) after it, the
/// mimetype and the status line given back. Nothing of the request
/// runs between the two, so they are one act here.
pub(crate) fn deactivate(req: &mut Request) {
    if req.output().flags & ACTIVATED != 0 {
        header(req);
        let output = req.output_mut();
        output.flags ^= ACTIVATED;
        output.running = None;
        while let Some(handler) = req.output_mut().handlers.pop() {
            free_handler(req, handler);
        }
        let (output, heap) = req.output_and_heap();
        if let Some(elements) = output.elements.take() {
            heap.free_block(elements);
        }
        output.max = 0;
    }
    let (output, heap) = req.output_and_heap();
    output.started = None;
    if let Some(mimetype) = output.mimetype.take() {
        heap.free_block(mimetype);
    }
    output.clean_headers(heap);
    send_headers_free(req);
}

/// `php_output_handler_start` (`Z/main/output.c:545`): the handler
/// pushed and made active, or refused while the layer is inactive, a
/// handler is running, or a registered handler meets one it conflicts
/// with.
fn handler_start(req: &mut Request, handler: Handler) -> bool {
    if req.output().flags & ACTIVATED == 0 || lock_error(req, HANDLER_START) {
        free_handler(req, handler);
        return false;
    }
    if let Func::Registered(registered) = &handler.func
        && conflicts(req, registered)
    {
        free_handler(req, handler);
        return false;
    }
    let (output, heap) = req.output_and_heap();
    match output.push(heap, handler) {
        Ok(level) => {
            output.handlers[level].level = level;
            true
        }
        Err(fatal) => {
            req.end_in(&fatal);
            false
        }
    }
}

/// The check a `php_output_handler_conflict_register` function makes,
/// `php_output_handler_conflict` over each name while the stack is not
/// empty: a name on the stack is php's warning, and the refusal.
fn conflicts(req: &mut Request, registered: &BufferHandler) -> bool {
    if req.output().level() == 0 {
        return false;
    }
    for &set in registered.conflicts {
        if !req.output().handler_started(set.as_bytes()) {
            continue;
        }
        let text = if set == registered.name {
            format!("Output handler '{set}' cannot be used twice")
        } else {
            format!(
                "Output handler '{}' conflicts with '{set}'",
                registered.name
            )
        };
        docref(req, Level::Warning, &text);
        return true;
    }
    false
}

/// `php_output_start_user` (`Z/main/output.c:430`): the default
/// handler for no callback, `php_output_handler_create_internal` with
/// the name a string of the request's; else
/// `php_output_handler_create_user`, the user record `ecalloc`'d, the
/// name what `zend_fcall_info_init` names the callable, the string
/// itself for a string, and the callable's value held as `zoh`.
fn start_user(
    req: &mut Request,
    start: &StartHandler,
    chunk_size: usize,
    flags: u32,
) -> Result<bool, Thrown> {
    let handler = match start {
        // `php_output_handler_start(NULL)`: the lock check, then the
        // refusal.
        StartHandler::Refused => {
            lock_error(req, HANDLER_START);
            return Ok(false);
        }
        StartHandler::Default => {
            let name = req.string(DEFAULT_NAME)?.heap();
            let (_, heap) = req.output_and_heap();
            Output::handler_init(heap, name, chunk_size, ability_flags(flags), Func::Default)
        }
        StartHandler::Registered(registered) => {
            // `php_output_handler_create_user`'s alias ctor; a NULL is
            // `php_output_handler_start(NULL)`.
            if let Some(init) = registered.init
                && !init(req, chunk_size)?
            {
                lock_error(req, HANDLER_START);
                return Ok(false);
            }
            let name = req.string(registered.name.as_bytes())?.heap();
            let (_, heap) = req.output_and_heap();
            let func = Func::Registered(*registered);
            Output::handler_init(heap, name, chunk_size, ability_flags(flags), func)
        }
        StartHandler::User(callable) => {
            let value = callable.clone().into_value();
            let name = match value.as_php_string() {
                Some(s) => {
                    req.addref(&value);
                    s.heap()
                }
                None => req.string(callable.name().as_bytes())?.heap(),
            };
            req.addref(&value);
            let (_, heap) = req.output_and_heap();
            let record = match heap.block(USER_RECORD) {
                Ok(record) => record,
                Err(fatal) => {
                    heap.release_string(name);
                    req.release(value);
                    req.end_in(&fatal);
                    return Err(Thrown::of(None));
                }
            };
            Output::handler_init(
                heap,
                name,
                chunk_size,
                ability_flags(flags) | HANDLER_USER,
                Func::User {
                    callable: callable.clone(),
                    record,
                },
            )
        }
    };
    match handler {
        Ok(handler) => Ok(handler_start(req, handler)),
        Err(fatal) => {
            req.end_in(&fatal);
            Err(Thrown::of(None))
        }
    }
}

/// `php_output_get_contents`: the active handler's buffer as a string
/// of the request's, `None` for no handler.
fn contents(req: &mut Request) -> Result<Option<Value>, Thrown> {
    let level = req.output().level();
    if level == 0 {
        return Ok(None);
    }
    let buffered = req.output().buffered(level - 1);
    Ok(Some(Value::string(req.string(&buffered)?)))
}

/// `php_request_startup`'s buffer (`Z/main/main.c:1931-1941`): the
/// handler `output_handler` names, started as `php_output_start_user`
/// starts a string, an extension's registered one before a callable of
/// that name; else the default handler where `output_buffering` asks
/// for one, a size above one its chunk size. A name that is no callable
/// is `php_output_handler_create_user`'s warning and no buffer.
pub(crate) fn start_at_request(req: &mut Request) -> Result<(), Thrown> {
    let name = req.ini_get(b"output_handler").unwrap_or_default().to_vec();
    if name.is_empty() {
        let size = req.ini_long(b"output_buffering");
        if size != 0 {
            let chunk = if size > 1 {
                usize::try_from(size).unwrap_or(0)
            } else {
                0
            };
            start_user(req, &StartHandler::Default, chunk, HANDLER_STDFLAGS)?;
        }
        return Ok(());
    }
    if let Some(registered) = req.buffer_handler(&name) {
        start_user(
            req,
            &StartHandler::Registered(registered),
            0,
            HANDLER_STDFLAGS,
        )?;
        return Ok(());
    }
    let value = Value::string(req.string(&name)?);
    let started = match crate::api::coerce::callable_checked(req, &value) {
        Ok(callable) => {
            start_user(req, &StartHandler::User(callable), 0, HANDLER_STDFLAGS).map(|_| ())
        }
        Err(error) => {
            docref(req, Level::Warning, &error);
            Ok(())
        }
    };
    req.release(value);
    started
}

/// `php_output_start_default`, what `body` writes, then
/// `php_output_get_contents` and `php_output_discard`: how
/// `highlight_file`, `highlight_string` and `php_strip_whitespace` hand
/// back what they would have printed. The default handler starts as any
/// does, so inside a running handler it is `php_output_lock_error`'s
/// fatal; `None` where it did not start.
pub(crate) fn captured(
    req: &mut Request,
    body: impl FnOnce(&mut Request),
) -> Result<Option<PhpString>, Thrown> {
    if !start_user(req, &StartHandler::Default, 0, HANDLER_STDFLAGS)? {
        return Ok(None);
    }
    body(req);
    let contents = contents(req)?;
    stack_pop(req, POP_DISCARD | POP_TRY);
    Ok(contents.and_then(|value| value.as_php_string().copied()))
}

/// The active handler's name and level, for a notice.
fn active_named(req: &Request) -> String {
    let output = req.output();
    match output.handlers.last() {
        Some(handler) => format!(
            "{} ({})",
            String::from_utf8_lossy(handler.name.bytes()),
            handler.level
        ),
        None => String::new(),
    }
}

/// A notice of the `ob_*` natives, and the `false` it stands before.
fn refused(req: &mut Request, message: &str) -> Value {
    docref(req, Level::Notice, message);
    Value::bool(false)
}

/// The `PHP_FUNCTION` bodies of `Z/main/output.c:1373-1591`, each over
/// the operation it wraps, with php's notices: the ones that move the
/// stack here, the ones that read it in [`read_op`].
///
/// `ob_start()` inside a running handler is `php_output_lock_error`'s
/// fatal, which leaves by `zend_bailout` with no notice after it:
///
/// ```php
/// function f($s) { ob_start(); return $s; }
/// ob_start('f');
/// echo "done";
/// ```
/// ```expect exit=255
/// Fatal error: ob_start(): Cannot use output buffering in output buffering display handlers in %s on line 2
/// ```
pub(crate) fn buffer_op(req: &mut Request, op: &OutputOp) -> Result<Value, Thrown> {
    let level = req.output().level();
    Ok(match op {
        OutputOp::Start {
            handler,
            chunk_size,
            flags,
        } => {
            let chunk_size = usize::try_from(*chunk_size).unwrap_or(0);
            let flags = u32::try_from(*flags).unwrap_or(HANDLER_STDFLAGS);
            if start_user(req, handler, chunk_size, flags)? {
                Value::bool(true)
            } else if req.bailing_out() {
                // `php_output_lock_error`'s fatal leaves by
                // `zend_bailout`, before `ob_start()` could say more.
                return Err(Thrown::of(None));
            } else {
                refused(req, "Failed to create buffer")
            }
        }
        OutputOp::Flush => {
            if level == 0 {
                refused(req, "Failed to flush buffer. No buffer to flush")
            } else if flush(req) {
                Value::bool(true)
            } else {
                let named = active_named(req);
                refused(req, &format!("Failed to flush buffer of {named}"))
            }
        }
        OutputOp::Clean => {
            if level == 0 {
                refused(req, "Failed to delete buffer. No buffer to delete")
            } else if clean(req) {
                Value::bool(true)
            } else {
                let named = active_named(req);
                refused(req, &format!("Failed to delete buffer of {named}"))
            }
        }
        OutputOp::EndFlush => {
            if level == 0 {
                refused(
                    req,
                    "Failed to delete and flush buffer. No buffer to delete or flush",
                )
            } else {
                Value::bool(stack_pop(req, POP_TRY))
            }
        }
        OutputOp::EndClean => {
            if level == 0 {
                refused(req, "Failed to delete buffer. No buffer to delete")
            } else {
                Value::bool(stack_pop(req, POP_DISCARD | POP_TRY))
            }
        }
        OutputOp::GetFlush => match contents(req)? {
            None => refused(
                req,
                "Failed to delete and flush buffer. No buffer to delete or flush",
            ),
            Some(contents) => {
                if !stack_pop(req, POP_TRY) {
                    let named = active_named(req);
                    refused(req, &format!("Failed to delete buffer of {named}"));
                }
                contents
            }
        },
        OutputOp::GetClean => {
            if level == 0 {
                return Ok(Value::bool(false));
            }
            match contents(req)? {
                None => refused(req, "Failed to delete buffer. No buffer to delete"),
                Some(contents) => {
                    if !stack_pop(req, POP_DISCARD | POP_TRY) {
                        let named = active_named(req);
                        refused(req, &format!("Failed to delete buffer of {named}"));
                    }
                    contents
                }
            }
        }
        OutputOp::ImplicitFlush(on) => {
            req.output_mut().set_implicit_flush(*on);
            Value::null()
        }
        OutputOp::GetContents
        | OutputOp::GetLevel
        | OutputOp::GetLength
        | OutputOp::ListHandlers
        | OutputOp::GetStatus { .. } => read_op(req, op)?,
    })
}

/// The `ob_*` natives that read the stack: `ob_get_contents`,
/// `ob_get_level`, `ob_get_length`, `ob_list_handlers` and
/// `ob_get_status`.
fn read_op(req: &mut Request, op: &OutputOp) -> Result<Value, Thrown> {
    let level = req.output().level();
    Ok(match op {
        OutputOp::GetContents => contents(req)?.unwrap_or_else(|| Value::bool(false)),
        OutputOp::GetLevel => Value::int(long(level)),
        OutputOp::GetLength => match req.output().handlers.last() {
            Some(handler) => Value::int(long(handler.used)),
            None => Value::bool(false),
        },
        OutputOp::ListHandlers => {
            let mut list = req.array()?;
            let names: Vec<Vec<u8>> = req
                .output()
                .handlers
                .iter()
                .map(|handler| handler.name.bytes().to_vec())
                .collect();
            for name in names {
                let name = PhpString::new(req, &name)?;
                list.append(req, name.into_value())?;
            }
            Value::array(list)
        }
        OutputOp::GetStatus { full } => {
            if level == 0 {
                Value::array(req.array()?)
            } else if *full {
                let mut rows = req.array()?;
                for at in 0..level {
                    if let Some(fields) = req.output().status_fields(at) {
                        let row = fields.into_array(req)?;
                        rows.append(req, Owner::adopt(row))?;
                    }
                }
                Value::array(rows)
            } else {
                match req.output().status_fields(level - 1) {
                    Some(fields) => fields.into_array(req)?,
                    None => Value::array(req.array()?),
                }
            }
        }
        _ => Value::bool(false),
    })
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use crate::api::request::Request;
    use crate::engine::{Engine, RequestDescription};

    /// A request under a host whose `header_handler` takes a line.
    fn web_request() -> Request {
        let engine = Engine::builder().build();
        Request::new(
            Arc::clone(&engine.shared),
            RequestDescription {
                web: Some(crate::engine::WebRequest {
                    response_code: 200,
                    ..crate::engine::WebRequest::default()
                }),
                ..RequestDescription::default()
            },
        )
    }

    /// `sapi_send_headers` under a web host: the default `Content-type`
    /// goes on the end of the list when the first byte reaches the host,
    /// after the lines `header()` left, and the block is sent (#289).
    #[test]
    fn a_web_host_s_block_ends_in_the_default_content_type_after_the_first_byte() {
        let mut req = web_request();
        assert!(req.header_add(b"X-A: 1", true, 0));
        assert_eq!(req.header_block(), b"X-A: 1\r\n");
        assert!(req.headers_sent().is_none());
        assert_eq!(req.response_code(), 200);
        req.write(b"x");
        assert_eq!(
            req.header_block(),
            b"X-A: 1\r\nContent-type: text/html; charset=UTF-8\r\n"
        );
        assert!(req.headers_sent().is_some());
        assert!(!req.header_add(b"X-B: 2", true, 0));
    }

    /// php-cgi's `-q` once the request started: the head counts sent,
    /// and a line `header()` takes after it is listed without a warning,
    /// since `no_headers` is set; the first byte sends nothing more.
    #[test]
    fn a_quiet_head_counts_sent_and_takes_a_line_without_a_warning() {
        let mut req = web_request();
        req.output_mut().quiet();
        assert!(req.headers_sent().is_some());
        assert!(req.header_add(b"X-A: 1", true, 0));
        req.write(b"x");
        assert_eq!(req.headers_list(), [b"X-A: 1".to_vec()]);
        assert!(req.header_add(b"X-B: 2", true, 0));
        assert_eq!(req.headers_list().len(), 2);
    }

    /// `sapi_header_op` on a `Content-Type`: the default charset applied
    /// with no space and the line rewritten as `Content-type: `, the
    /// default line stopped, and a replace dropping the lines of the
    /// name whatever their case; a type that is not `text/` stands as
    /// written.
    #[test]
    fn a_content_type_header_takes_the_default_charset_and_stops_the_default_line() {
        let mut req = web_request();
        assert!(req.header_add(b"Content-Type: text/plain", true, 0));
        assert_eq!(
            req.headers_list(),
            [b"Content-type: text/plain;charset=UTF-8".to_vec()]
        );
        assert!(req.header_add(b"content-type: application/json", true, 0));
        assert_eq!(
            req.headers_list(),
            [b"content-type: application/json".to_vec()]
        );
        assert!(req.header_add(b"X-B: 3", false, 0));
        assert!(req.header_add(b"X-B: 4", false, 0));
        assert!(req.header_remove(Some(b"x-b")));
        req.write(b"x");
        assert_eq!(req.header_block(), b"content-type: application/json\r\n");
    }

    /// The CLI's `header_handler` answers zero, so no line is listed, and
    /// what a `header()` moves under it is the status line and the code:
    /// an `HTTP/` line sets both, a `Location` outside a redirect sets
    /// 302, and the block sent gives the status line back.
    #[test]
    fn the_cli_s_handler_takes_no_line_and_the_status_line_moves_the_code() {
        let mut req = crate::test::request();
        assert_eq!(req.response_code(), 0);
        assert!(req.header_add(b"X-A: 1", true, 0));
        assert!(req.headers_list().is_empty());
        assert!(req.header_add(b"HTTP/1.1 404 Nope\r\n", true, 0));
        assert_eq!(req.status_line(), Some(&b"HTTP/1.1 404 Nope"[..]));
        assert_eq!(req.response_code(), 404);
        assert!(req.header_add(b"Location: /x", true, 0));
        assert_eq!(req.response_code(), 302);
        assert!(req.status_line().is_none());
        assert!(req.header_add(b"HTTP/1.1 302 Found", true, 0));
        assert!(req.status_line().is_some());
        assert!(!req.header_add(b"", true, 0));
        assert!(!req.header_add(b"X-C: a\nb", true, 0));
        req.write(b"x");
        assert!(req.status_line().is_none());
        assert!(req.headers_list().is_empty());
        assert_eq!(req.header_block(), b"");
    }
}
