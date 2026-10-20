//! The response of a request: what leaves it through the `php_output`
//! layer (`main/output.c`), the sink the host lent the request, the
//! header block `sapi_send_headers` commits, the
//! response code, `connection_status()` and `fastcgi_finish_request()`.
//! The sink, the `php_output` state, the header callback, the
//! connection's status and whether the host closed the response are
//! [`Response`]'s, whose fields no file but this one reads.

use php_heap::Heap;

use crate::api::abi::Callable;
use crate::api::raising::Thrown;
use crate::api::values::{Key, Owner, PhpArray, PhpString, Value, adopted, adopted_some};
use crate::engine::{Aborted, OutputSink, ResponseHead};
use crate::output::{HttpStatus, Output};

use super::{Phase, Request};

/// One `ob_*` operation on the request's buffer stack.
#[derive(Debug, Clone, PartialEq)]
pub enum OutputOp {
    Start {
        handler: StartHandler,
        chunk_size: i64,
        flags: i64,
    },
    Flush,
    Clean,
    EndFlush,
    EndClean,
    GetFlush,
    GetClean,
    GetContents,
    GetLength,
    GetLevel,
    GetStatus {
        full: bool,
    },
    ListHandlers,
    ImplicitFlush(bool),
}

/// What `ob_start()` starts, `php_output_handler_create_user`'s switch
/// over its `$callback` (`Z/main/output.c:466`).
#[derive(Debug, Clone, PartialEq)]
pub enum StartHandler {
    /// `null`: the default handler, which passes its buffer on.
    Default,
    /// A name an extension registered its handler under, found by
    /// [`Request::buffer_handler`] before the name is read as a callable.
    Registered(BufferHandler),
    /// Anything else, as the callable it resolved to.
    User(Callable),
    /// A callable `zend_fcall_info_init` refused, its warning raised:
    /// no handler, which `php_output_handler_start` refuses after its
    /// check for a running handler, so inside one it is still the fatal.
    Refused,
}

/// A handler of the buffer stack an extension hands to
/// [`ExtensionBuilder::register`] under a name `ob_start()` takes,
/// `php_output_handler_alias_register` from its `MINIT`, with the
/// handlers it cannot stand beside: while the stack is not empty and one
/// of `conflicts` is on it, starting this one is php's warning and a
/// refusal, the check every `php_output_handler_conflict_register` in
/// php-src makes through `php_output_handler_conflict`.
///
/// [`ExtensionBuilder::register`]: crate::api::abi::ExtensionBuilder::register
#[derive(Debug, Clone, Copy)]
pub struct BufferHandler {
    pub name: &'static str,
    pub conflicts: &'static [&'static str],
    /// `php_output_handler_context_func_t`.
    pub handler: fn(&mut Request, &mut HandlerCall<'_>) -> Result<HandlerStatus, Thrown>,
    /// The alias's `php_output_handler_alias_ctor_t`, served as
    /// `ob_start()` makes the handler, before the conflict check.
    pub init: Option<BufferInit>,
}

/// [`BufferHandler::init`]: the chunk size `ob_start()` was handed, and
/// `false` where the alias's ctor returns NULL, which `ob_start()`
/// answers with its notice "Failed to create buffer".
pub type BufferInit = fn(&mut Request, usize) -> Result<bool, Thrown>;

impl PartialEq for BufferHandler {
    /// One registration a name, as php's alias table keys them.
    fn eq(&self, other: &BufferHandler) -> bool {
        self.name == other.name
    }
}

/// The operation a handler is served for, `php_output_context.op`'s
/// bits read once; none set is a plain write.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)] // php's op bits, one field per bit, each read alone
pub struct HandlerOp {
    /// `PHP_OUTPUT_HANDLER_START`: the handler's first call.
    pub start: bool,
    /// `PHP_OUTPUT_HANDLER_CLEAN`.
    pub clean: bool,
    /// `PHP_OUTPUT_HANDLER_FLUSH`.
    pub flush: bool,
    /// `PHP_OUTPUT_HANDLER_FINAL`: its last.
    pub finish: bool,
}

/// One call of an extension's buffer handler, `php_output_context`, and
/// the one `php_output_handler_hook` an extension in php-src asks of the
/// handler being served.
#[derive(Debug)]
pub struct HandlerCall<'a> {
    pub op: HandlerOp,
    /// `context->in`: the handler's buffered bytes.
    pub input: &'a [u8],
    /// `context->out`: what goes on, none being `PHP_OUTPUT_HANDLER_NO_DATA`.
    pub out: Vec<u8>,
    /// `php_output_handler_hook(PHP_OUTPUT_HANDLER_HOOK_IMMUTABLE)`: set,
    /// the handler can no longer be cleaned or removed.
    pub immutable: bool,
}

/// What an extension's buffer handler answers, its `zend_result`: on
/// `Failure` the handler is disabled and its buffer goes on unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandlerStatus {
    Success,
    Failure,
}

/// `SG(sapi_headers)`'s content type, `mimetype` and
/// `send_default_content_type` read as the one state they are.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContentType {
    /// The mimetype a `Content-Type` header, or the default one sent
    /// with the block, laid down.
    Held(Vec<u8>),
    /// None yet, and the default one still to be sent.
    Default,
    /// None, and none to be sent.
    Unset,
}

/// `PG(connection_status)`'s bits this engine sets: `PHP_CONNECTION_NORMAL`
/// and `PHP_CONNECTION_ABORTED` (`Z/main/php.h`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Connection {
    Normal = 0,
    Aborted = 1,
}

/// What a request wrote beside the body, one call the sink will see:
/// an entry for the log or bytes for the error stream. Kept in the
/// order they were written, since under php's CLI the two land on one
/// fd, and there the log form of a diagnostic stands before its
/// `display_errors=stderr` form (#216).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Aside {
    /// `OutputSink::log`: one entry, whole, ended by the host.
    Log(Vec<u8>),
    /// `OutputSink::write_stderr`: bytes as given.
    Stderr(Vec<u8>),
}

/// The sink of a request no host lent one ([`Request::new`]): the log's
/// entries and the error stream's bytes in the order written, and the
/// body, kept; the head dropped, since no host sends it. The engine's
/// startup reads its error stream back as what it hands its hosts in
/// [`Engine::startup_errors`](crate::engine::Engine::startup_errors), and
/// a unit test reads the rest.
#[derive(Default)]
pub(crate) struct Kept {
    aside: Vec<Aside>,
    body: Vec<u8>,
}

impl OutputSink for Kept {
    fn write(&mut self, bytes: &[u8]) -> Result<(), Aborted> {
        self.body.extend_from_slice(bytes);
        Ok(())
    }

    fn flush(&mut self) {}

    fn send_headers(&mut self, _: &ResponseHead) {}

    fn log(&mut self, entry: &[u8]) {
        self.aside.push(Aside::Log(entry.to_vec()));
    }

    fn write_stderr(&mut self, bytes: &[u8]) {
        self.aside.push(Aside::Stderr(bytes.to_vec()));
    }
}

/// The sink, the `php_output` layer, the header callback, the
/// connection's status and whether the host closed the response, whose
/// fields no file but this one reads.
pub(super) struct Response {
    /// Where what passed the `php_output` layer goes, php's
    /// `sapi_module.ub_write`, and the header block, the log and the
    /// error stream beside it, each call as the request makes it: the
    /// sink the host lent the request, which it hands back at the end
    /// ([`Request::release_sink`]), or the request's own [`Kept`] where no
    /// host serves it.
    sink: Box<dyn OutputSink + Send>,
    /// The `php_output` layer, `main/output.c`'s `OG` and the header state
    /// `sapi_send_headers` keeps beside it (`crate::output`).
    output: Output,
    header_callback: Option<Owner<Callable>>,
    /// php's `PG(connection_status)`: whether the host's sink refused a
    /// write, `PHP_CONNECTION_ABORTED`, which `connection_aborted()`
    /// answers ([`Request::handle_aborted_connection`]).
    connection: Connection,
    /// `fcgi_is_closed`: the host closed the response when
    /// `fastcgi_finish_request()` ended it.
    closed: bool,
}

impl Response {
    /// The response as the request starts: its calls to `sink`, the
    /// `php_output` layer `output` as `php_output_activate` laid it, no
    /// header callback, the connection normal and the response open.
    pub(super) fn new(sink: Box<dyn OutputSink + Send>, output: Output) -> Response {
        Response {
            sink,
            output,
            header_callback: None,
            connection: Connection::Normal,
            closed: false,
        }
    }
}

impl Request {
    /// An entry handed to the host's log, past the `php_output` layer,
    /// as `php_log_err` hands it to `sapi_module.log_message`.
    pub(super) fn log_to_host(&mut self, entry: &[u8]) {
        self.response.sink.log(entry);
    }

    /// The sink the request wrote to, flushed and handed back at its
    /// end; what the request writes after goes to a [`Kept`] of its own,
    /// which no host reads.
    pub(crate) fn release_sink(&mut self) -> Box<dyn OutputSink + Send> {
        let mut sink = self.take_sink();
        sink.flush();
        sink
    }

    /// The sink handed back unflushed, where php makes no `sapi_flush`:
    /// `php_error_cb` displaying what php's startup raised, and the
    /// `zend_post_startup_cb` chain, neither of which is a request, so
    /// what they wrote stays where the host holds it, as C's `stdout`
    /// holds it.
    pub(crate) fn take_sink(&mut self) -> Box<dyn OutputSink + Send> {
        std::mem::replace(&mut self.response.sink, Box::new(Kept::default()))
    }

    /// The sink the request writes to, where it is a `T`: the host's own
    /// type, or the request's [`Kept`].
    pub(crate) fn sink_as<T: OutputSink>(&mut self) -> Option<&mut T> {
        let sink: &mut dyn std::any::Any = self.response.sink.as_mut();
        sink.downcast_mut()
    }

    /// The request's own sink, where no host lent it one.
    fn kept(&mut self) -> Option<&mut Kept> {
        self.sink_as()
    }

    /// The body the request's own sink kept, for a test that serves no
    /// host.
    #[cfg(test)]
    pub(crate) fn take_body(&mut self) -> Vec<u8> {
        self.kept()
            .map(|kept| std::mem::take(&mut kept.body))
            .unwrap_or_default()
    }

    /// The log's entries and the error stream's bytes the request's own
    /// sink kept, in the order written, for a test that serves no host.
    #[cfg(test)]
    pub(crate) fn take_aside(&mut self) -> Vec<Aside> {
        self.kept()
            .map(|kept| std::mem::take(&mut kept.aside))
            .unwrap_or_default()
    }

    /// `fastcgi_finish_request()` (`Z/sapi/fpm/fpm/fpm_main.c:1494`):
    /// where the response is open, every buffer flushed, the head sent
    /// and the host told the response ends; whether it was open. What
    /// the program writes after goes nowhere.
    pub fn finish_response(&mut self) -> bool {
        if self.response.closed {
            return false;
        }
        crate::output::end_all(self);
        crate::output::header(self);
        self.response.closed = self.response.sink.finish();
        true
    }

    /// What the host says of itself, made an array of the request's, a
    /// map's keys strings and a list's the next index: what
    /// `fpm_get_status()` answers from the process manager's shared table
    /// (`Z/sapi/fpm/fpm/fpm_status.c` `fpm_status_export_to_zval`);
    /// `None` where the host says nothing.
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn host_status(&mut self) -> Result<Option<Owner<PhpArray>>, Thrown> {
        let made = self.host_status_made();
        adopted_some(made)
    }

    /// What [`host_status`](Self::host_status) makes, its one owner not yet named.
    pub(crate) fn host_status_made(&mut self) -> Result<Option<PhpArray>, Thrown> {
        match self.response.sink.status() {
            Some(status) => host_value(self, status).map(Some),
            None => Ok(None),
        }
    }

    /// `sapi_module.send_headers`: the response's head handed to the
    /// sink, once, at the commit `sapi_send_headers` makes, or when the
    /// request ends where nothing committed it; nothing where the host
    /// asked for none (`no_headers`) once the request started, since
    /// php-cgi sets it after `php_request_startup` returns
    /// (`Z/sapi/cgi/cgi_main.c:2468-2471`): what an extension's `rinit`
    /// displays commits the head under `-q` too, a failed start's fatal
    /// among it (#1879).
    pub(crate) fn send_header_block(&mut self) {
        if self.phase() != Phase::Startup
            && self
                .description
                .web
                .as_ref()
                .is_some_and(|web| web.no_headers)
        {
            return;
        }
        let names = self
            .description
            .web
            .as_ref()
            .map_or(&[][..], |web| web.head_ini);
        let ini = names
            .iter()
            .filter_map(|name| Some((*name, self.ini_get(name)?.to_vec())))
            .collect();
        let head = ResponseHead {
            code: self.response.output.response_code(),
            status_line: self.response.output.status_line().map(<[u8]>::to_vec),
            lines: self.response.output.headers().to_vec(),
            ini,
        };
        self.response.sink.send_headers(&head);
    }

    /// `sapi_module.send_headers` made one that sends nothing, for the
    /// rest of the request.
    pub(crate) fn discard_headers(&mut self) {
        if let Some(web) = &mut self.description.web {
            web.no_headers = true;
        }
    }

    /// The header block as the sink is handed it: `SG(sapi_headers).headers`
    /// rendered one line each, and nothing else, since the status line
    /// and its shape are the host's (`sapi_cgi_send_headers` writes
    /// `Status:`, the dev server the HTTP status line).
    #[cfg(test)]
    pub(crate) fn header_block(&self) -> Vec<u8> {
        let mut block = Vec::new();
        for header in self.response.output.headers() {
            block.extend_from_slice(header);
            block.extend_from_slice(b"\r\n");
        }
        block
    }

    /// What the startup's handlers wrote to the error stream, php's
    /// `fprintf(stderr)` before the first request, in order.
    pub(crate) fn startup_stderr(&mut self) -> Vec<u8> {
        let Some(kept) = self.kept() else {
            return Vec::new();
        };
        let mut written = Vec::new();
        for aside in kept.aside.drain(..) {
            if let Aside::Stderr(bytes) = aside {
                written.extend_from_slice(&bytes);
            }
        }
        written
    }

    /// Write body bytes, `php_output_write`: through the handler stack
    /// while one is active, and what passes it reaches the host, with
    /// the header block committed at the first byte that does
    /// (`crate::output`).
    pub fn write(&mut self, bytes: &[u8]) {
        crate::output::write(self, bytes);
    }

    /// `sapi_module.ub_write`: bytes past the `php_output` layer, the
    /// host's, handed to the sink as they come; one the host could not
    /// write is [`Request::handle_aborted_connection`]. Where the host
    /// carries php's shell callbacks, the shell's `write` sees each
    /// write first, as `sapi_cli_ub_write` calls `cli_shell_ub_write`
    /// and `cli_shell_write` for every write that is not empty.
    pub(crate) fn ub_write(&mut self, bytes: &[u8]) {
        if let Some((by, shell)) = self.shared.supplied.shell.entry()
            && !bytes.is_empty()
        {
            self.serve_for(by, |req| (shell.write)(req, bytes));
        }
        if let Err(aborted) = self.response.sink.write(bytes) {
            self.handle_aborted_connection(aborted);
        }
    }

    /// `php_handle_aborted_connection` (`Z/main/main.c:2722`), after what
    /// the host set `EG(exit_status)` to, where it set it, as php's CLI
    /// sets 255 (`sapi_cli_ub_write`): the connection marked aborted, the
    /// `php_output` layer disabled, and, unless `ignore_user_abort` is on,
    /// `zend_bailout`, which leaves the program with no diagnostic for the
    /// shutdown to go on from.
    ///
    /// ```php
    /// $path = sys_get_temp_dir() . '/' . 'a' . bin2hex(random_bytes(8)) . '.php';
    /// file_put_contents($path, '<?php ignore_user_abort((bool) $argv[1]); register_shutdown_function(function () { file_put_contents("php://stderr", "shutdown " . connection_aborted() . connection_status() . "\n"); }); for ($i = 0; $i < 3; $i++) { echo str_repeat("x", 100000); file_put_contents("php://stderr", "wrote $i\n"); }');
    /// foreach ([0, 1] as $ignore) {
    ///     $p = proc_open([PHP_BINARY, '-n', $path, $ignore], [1 => ['pipe', 'w'], 2 => ['pipe', 'w']], $pipes);
    ///     fclose($pipes[1]);
    ///     echo stream_get_contents($pipes[2]), 'exit ', proc_close($p), "\n";
    /// }
    /// unlink($path);
    /// ```
    /// ```expect process
    /// shutdown 11
    /// exit 255
    /// wrote 0
    /// wrote 1
    /// wrote 2
    /// shutdown 11
    /// exit 255
    /// ```
    pub(crate) fn handle_aborted_connection(&mut self, aborted: Aborted) {
        if let Some(status) = aborted.exit_status {
            self.exit_status = status;
        }
        self.response.connection = Connection::Aborted;
        crate::output::disable(self);
        if !self.ini_flag(b"ignore_user_abort") {
            self.bail_out();
        }
    }

    /// `PG(connection_status)`: `CONNECTION_NORMAL`, 0, or
    /// `CONNECTION_ABORTED`, 1, once the host's sink refused a write.
    #[must_use]
    pub fn connection_status(&self) -> i64 {
        self.response.connection as i64
    }

    /// The `php_output` layer, and the heap it lays its handlers down in.
    pub(crate) fn output_and_heap(&mut self) -> (&mut Output, &mut Heap) {
        (&mut self.response.output, &mut self.heap)
    }

    pub(crate) fn output(&self) -> &Output {
        &self.response.output
    }

    pub(crate) fn output_mut(&mut self) -> &mut Output {
        &mut self.response.output
    }

    /// `header_register_callback`'s callable, taken to be served once by
    /// `sapi_send_headers`, which unsets `SG(callback_func)` before the
    /// call so a second send finds none.
    pub(crate) fn take_header_callback(&mut self) -> Option<Owner<Callable>> {
        self.response.header_callback.take()
    }

    /// Write to the host's error stream, bytes as given: php's
    /// `fputs(stderr)`, which the `display_errors=stderr` form takes and
    /// the `STDERR` stream constant will. The log is another channel,
    /// [`Request::error_log`]'s; whether the host puts the two on one fd
    /// is the host's, as it is php's CLI's (#216).
    pub fn write_stderr(&mut self, bytes: &[u8]) {
        self.response.sink.write_stderr(bytes);
    }

    /// `php://stdout`'s write: the host's own fd 1 where it has one apart
    /// from the response, else the body past the `php_output` layer.
    pub fn write_stdout(&mut self, bytes: &[u8]) {
        if !self.response.sink.write_stdout(bytes) {
            self.ub_write(bytes);
        }
    }

    /// `flush()`, `sapi_flush`: the host may push what it has, the head
    /// sent first where the host's flush sends it; the handler stack is
    /// `ob_flush`'s, not this.
    pub fn flush(&mut self) {
        if self.response.sink.flush_sends_head() {
            crate::output::header(self);
        }
        self.response.sink.flush();
    }

    /// One `ob_*` operation on the handler stack, the `PHP_FUNCTION`
    /// bodies of `Z/main/output.c:1373-1591` over the layer in
    /// `crate::output`; the notices are theirs, raised through the
    /// native being served.
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn output_buffer_op(&mut self, op: &OutputOp) -> Result<Owner<Value>, Thrown> {
        let made = crate::output::buffer_op(self, op);
        adopted(made)
    }

    /// `php_output_handler_started` (`Z/main/output.c:569`): whether a
    /// handler of `name` is on the stack, read off the stack itself, as
    /// `zlib.output_compression` and `tidy.clean_output` ask before they
    /// start theirs; nothing is made, where `ob_list_handlers()`'s array
    /// would be.
    #[must_use]
    pub fn buffer_handler_started(&self, name: &[u8]) -> bool {
        self.output().handler_started(name)
    }

    /// `php_output_start_default`, what `body` writes through the stack,
    /// then `php_output_get_contents` and `php_output_discard`: what the
    /// body printed, as a string, for a native that hands back what it
    /// would have printed. The default handler starts as `ob_start()`'s
    /// does, so inside a running handler it is `php_output_lock_error`'s
    /// fatal; `None` where it did not start, which such a native answers
    /// with its `Error`.
    ///
    /// ```php
    /// var_dump(highlight_string("x", true));
    /// ob_start(function ($s) { return strlen(highlight_string("x", true)) . "\n"; }); echo "y"; ob_end_flush();
    /// ```
    /// ```expect exit=255
    /// string(48) "<pre><code style="color: #000000">x</code></pre>"
    ///
    /// Fatal error: highlight_string(): Cannot use output buffering in output buffering display handlers in %s on line 3
    /// ```
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn captured(
        &mut self,
        body: impl FnOnce(&mut Request),
    ) -> Result<Option<Owner<PhpString>>, Thrown> {
        let made = crate::output::captured(self, body);
        adopted_some(made)
    }

    /// Where the first byte reached the host, `php_output_get_start_filename`
    /// and `_lineno`, once one has.
    #[must_use]
    pub fn output_started_at(&self) -> Option<(Vec<u8>, u32)> {
        self.response.output.started_at()
    }

    /// `php_output_get_status()`: `PHP_OUTPUT_WRITTEN` (`0x04`) once a
    /// byte reached a handler's buffer, `PHP_OUTPUT_SENT` (`0x08`) once
    /// one reached the host, `PHP_OUTPUT_ACTIVE` (`0x10`) while a handler
    /// is on the stack and `PHP_OUTPUT_LOCKED` (`0x20`) while one runs,
    /// beside `PHP_OUTPUT_IMPLICITFLUSH` and `PHP_OUTPUT_DISABLED`, which
    /// `tidy.clean_output`'s handler reads before it starts tidy's.
    ///
    /// ```php
    /// ob_start(); echo 'x'; ob_clean();
    /// var_dump(ini_set('tidy.clean_output', '1'), ini_get('tidy.clean_output'));
    /// ```
    /// ```expect
    /// Warning: ini_set(): Cannot enable tidy.clean_output - there has already been output in %s on line 3
    /// bool(false)
    /// string(1) "0"
    /// ```
    #[must_use]
    pub fn buffer_status(&self) -> i64 {
        i64::from(self.response.output.status())
    }

    /// `php_output_handler_alias`: the handler an extension registered
    /// under `name`, which `ob_start()` takes before it reads the name as
    /// a callable.
    #[must_use]
    pub fn buffer_handler(&self, name: &[u8]) -> Option<BufferHandler> {
        self.registered::<BufferHandler>()
            .find(|handler| handler.name.as_bytes() == name)
            .copied()
    }

    /// `SG(sapi_headers).mimetype` and `send_default_content_type`.
    #[must_use]
    pub fn content_type(&self) -> ContentType {
        self.response.output.content_type()
    }

    /// `php_output_handler_hook(PHP_OUTPUT_HANDLER_HOOK_GET_FLAGS)`: the
    /// flags of the buffer handler the stack is serving, `OG(running)`'s
    /// `PHP_OUTPUT_HANDLER_*` bits as `ob_get_status()` reports them, and
    /// `None` while it serves none. What tells a native served as a
    /// handler from the same native called outside one, as
    /// `mb_output_handler` tells them apart before it adds a header.
    #[must_use]
    pub fn running_handler_flags(&self) -> Option<i64> {
        self.response.output.running_flags().map(i64::from)
    }

    /// `header()`, `sapi_header_op` under `SAPI_HEADER_ADD` or
    /// `SAPI_HEADER_REPLACE` (`crate::output`): the line taken, replacing
    /// the lines of its name when `replace` is set, with `response_code`
    /// set when it is not zero; once the block is sent, php's warning
    /// naming where the body started, and `false`. Under the CLI the
    /// host's `header_handler` takes no line, so the list stays empty
    /// and the status line and the response code are all a `header()`
    /// moves, as under php's.
    pub fn header_add(&mut self, line: &[u8], replace: bool, response_code: i64) -> bool {
        crate::output::header_add(self, line, replace, response_code)
    }

    /// `header_remove()`, `sapi_header_op` under `SAPI_HEADER_DELETE` or,
    /// with `None`, `SAPI_HEADER_DELETE_ALL`: the lines of that name
    /// dropped, or all of them; the warnings are `sapi_header_op`'s.
    pub fn header_remove(&mut self, name: Option<&[u8]>) -> bool {
        crate::output::header_remove(self, name)
    }

    /// `sapi_header_op` under `SAPI_HEADER_DELETE_PREFIX`: the lines that
    /// open with `prefix`, a header's name of `name_len` bytes and more,
    /// dropped, as `php_session_remove_cookie` drops the session's
    /// earlier `Set-Cookie`.
    pub fn header_remove_prefix(&mut self, prefix: &[u8], name_len: usize) -> bool {
        crate::output::header_remove_prefix(self, prefix, name_len)
    }

    /// `headers_list()`: `SG(sapi_headers).headers`, in order.
    #[must_use]
    pub fn headers_list(&self) -> Vec<Vec<u8>> {
        self.response.output.headers().to_vec()
    }

    /// `SG(sapi_headers).http_status_line`: the `HTTP/...` line a
    /// `header()` set, held until the block is sent.
    #[must_use]
    pub fn status_line(&self) -> Option<&[u8]> {
        self.response.output.status_line()
    }

    /// `headers_sent()`: `SG(headers_sent)`, and where the body started
    /// when it was a body byte that sent them.
    #[must_use]
    pub fn headers_sent(&self) -> Option<(Vec<u8>, u32)> {
        if self.response.output.headers_sent() {
            Some(self.response.output.started_at().unwrap_or_default())
        } else {
            None
        }
    }

    /// `header_register_callback()` (`Z/main/SAPI.c:98`): the callable
    /// held, with the owners `zend_fcc_addref` takes, for
    /// `sapi_send_headers` to serve once; one held before is given up.
    pub fn set_header_callback(&mut self, callback: Callable) {
        let held = Owner::take(self, callback);
        if let Some(previous) = self.response.header_callback.replace(held) {
            previous.give_up(self);
        }
    }

    /// `SG(sapi_headers).http_response_code` as it stands: 200 under a
    /// web host and zero under the CLI, which `http_response_code()`
    /// answers `false` for.
    #[must_use]
    pub fn response_code(&self) -> i64 {
        self.response.output.response_code()
    }

    /// `http_response_code(int)`'s write, the code alone
    /// (`Z/ext/standard/head.c:399`); the checks before it are the
    /// native's.
    pub fn set_response_code(&mut self, code: i64) {
        self.response.output.set_response_code(code);
    }

    /// `SG(sapi_headers).http_status_line` and `http_response_code` kept
    /// as they stand, the status line taken off the response
    /// (`SG(sapi_headers).http_status_line = NULL`) and the code left,
    /// for [`put_back_http_status`](Self::put_back_http_status) to put
    /// back after a step that may move them, as soap's error handler keeps
    /// them around the handler beneath it.
    #[must_use]
    pub fn take_http_status(&mut self) -> HttpStatus {
        self.response.output.take_status()
    }

    /// The status line and the response code put back as
    /// [`take_http_status`](Self::take_http_status) kept them, the status
    /// line standing now given up.
    pub fn put_back_http_status(&mut self, status: HttpStatus) {
        crate::output::put_back_status(self, status);
    }

    /// The header list as name and value, what a host's
    /// `apache_response_headers()` answers (`Z/sapi/cgi/cgi_main.c:1689`,
    /// `Z/sapi/cli/php_cli_server.c:425`): each line with a name before
    /// its colon, the name with its trailing blanks off and the value with
    /// its leading blanks off; a line with no colon or an empty name is
    /// passed over.
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn response_headers(&mut self) -> Result<Owner<PhpArray>, Thrown> {
        let made = self.response_headers_made();
        adopted(made)
    }

    /// What [`response_headers`](Self::response_headers) makes, its one owner not yet named.
    pub(crate) fn response_headers_made(&mut self) -> Result<PhpArray, Thrown> {
        let lines = self.response.output.headers().to_vec();
        let mut array = self.array()?;
        for line in lines {
            let Some(colon) = line.iter().position(|&b| b == b':') else {
                continue;
            };
            let mut end = colon;
            while end > 0 && matches!(line[end - 1], b' ' | b'\t') {
                end -= 1;
            }
            if end == 0 {
                continue;
            }
            let mut start = colon + 1;
            while matches!(line.get(start), Some(b' ' | b'\t')) {
                start += 1;
            }
            let key = PhpString::new(self, &line[..end])?;
            let key = Key::from_string(self, key);
            let value = PhpString::new(self, &line[start..])?;
            array.set(self, key, value.into_value())?;
        }
        Ok(array)
    }
}

/// A host's value made an array of `req`'s: a map's keys strings, a
/// list's the next index ([`Request::host_status`]).
fn host_value(req: &mut Request, value: crate::engine::HostValue) -> Result<PhpArray, Thrown> {
    use crate::engine::HostValue;
    let mut array = req.array()?;
    let rows: Vec<(Option<Vec<u8>>, HostValue)> = match value {
        HostValue::Map(rows) => rows.into_iter().map(|(k, v)| (Some(k), v)).collect(),
        HostValue::List(rows) => rows.into_iter().map(|v| (None, v)).collect(),
        other => vec![(None, other)],
    };
    for (key, row) in rows {
        let value = match row {
            HostValue::Int(n) => Value::int(n),
            HostValue::Float(f) => Value::float(f),
            HostValue::Str(s) => Value::string(req.string(&s)?),
            nested => Value::array(host_value(req, nested)?),
        };
        let key = match key {
            Some(key) => {
                let key = PhpString::new(req, &key)?;
                Some(Key::from_string(req, key))
            }
            None => None,
        };
        let value = Owner::adopt(value);
        match key {
            Some(key) => array.set(req, key, value)?,
            None => array.append(req, value)?,
        }
    }
    Ok(array)
}
