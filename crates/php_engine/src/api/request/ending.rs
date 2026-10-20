//! How a request ends: where it stands in `php_request_startup`,
//! `php_execute_script` and `php_request_shutdown`, `zend_bailout` and
//! the `zend_try` that catches it, the fatal, core and uncaught endings,
//! the compile-time refusal, `exit` and its unwind, and the status the
//! process ends with. Where the request stands is [`Progress`]'s, whose
//! field no file but this one reads.

use std::sync::Arc;

use php_heap::Fatal;

use crate::api::abi::AfterLog;
use crate::api::raising::Level;
use crate::api::values::Value;
use crate::supplied::Bail;
use crate::supplied::chain::ErrorArgs;
use crate::supplied::tell;

use diagnostics::{Display, Message};

use super::{Raised, Request, diagnostics};

/// Where a request stands, as `php_verror` reads it to name the origin
/// of a diagnostic raised while no function is active
/// (`Z/main/main.c:1086-1129`): `PG(during_request_startup)`, set by
/// `php_request_startup` and cleared by the CLI before it compiles the
/// file (`Z/sapi/cli/php_cli.c:943`), so the extensions' `RINIT` are
/// served inside it; the main op array; `EG_FLAGS_IN_SHUTDOWN`, which
/// `php_request_shutdown` sets first (`Z/main/main.c:1960`); and
/// `EG_FLAGS_IN_RESOURCE_SHUTDOWN`, which `shutdown_executor` sets beside
/// it as it closes the resources (`Z/Zend/zend_execute_API.c:276`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Phase {
    Startup,
    Running,
    Shutdown,
    ResourceShutdown,
    /// The shutdown request the engine end is served on, which
    /// `php_during_module_shutdown()` names.
    EngineEnd,
}

/// The fatal the request is ending in, when it is one: `E_ERROR` and
/// its kin, or a source that did not compile. An `exit` is not one:
/// php unwinds it as a throwable ([`Request::throw_unwind_exit`]) and
/// records its status alone, in `EG(exit_status)`, which a fatal writes
/// too and the last write wins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Ending {
    /// `E_ERROR`: the fatal as `php_error_cb` gave it to `php_log_err`,
    /// one entry, which is the text `serve` reports and not a second
    /// rendering; the streams already have theirs.
    Fatal(String),
    /// `E_PARSE`: the text is already on the stream `php_error_cb` put it
    /// on, and what is left is php's status, 255.
    Parse,
}

/// Where the request stands, whose field no file but this one reads.
pub(super) struct Progress {
    /// Where the request stands in `php_request_startup`,
    /// `php_execute_script` and `php_request_shutdown`, what `php_verror`
    /// names a diagnostic's origin by when no function is active.
    phase: Phase,
    /// `CG(unclean_shutdown)`: a fatal's `zend_bailout` was served, so
    /// the request ends without the frames it never unwound, and a debug
    /// build's shutdown collection and request leak sensor stay silent, as
    /// php's `zend_mm_shutdown` is told to be (`Z/main/main.c:2050`).
    /// Nothing reads it in a release build, which keeps php's fast
    /// teardown, so only a debug build keeps it.
    #[cfg(debug_assertions)]
    unclean: bool,
}

impl Default for Progress {
    fn default() -> Progress {
        Progress {
            phase: Phase::Startup,
            #[cfg(debug_assertions)]
            unclean: false,
        }
    }
}

impl Request {
    pub(crate) fn ending(&self) -> Option<&Ending> {
        self.ending.as_ref()
    }

    /// Whether a `zend_bailout` is under way, which leaves every step
    /// it passes through at once.
    pub fn bailing_out(&self) -> bool {
        self.bailout
    }

    /// `zend_error_noreturn(E_ERROR, ...)`: `php_error_cb` renders the
    /// fatal where the request stands, on the streams the two ini entries
    /// say, and the request ends in it. It is rendered once, here, and
    /// `serve` writes nothing of its own. `php_error_cb` marks the store
    /// destructed before the request bails out
    /// (`zend_objects_store_mark_destructed`), so no
    /// object standing then is destructed, and a delegation of
    /// generators a frame lets go of on the way out is not given up
    /// link by link through their destructors:
    ///
    /// ```php
    /// class D { function __destruct() { echo "d\n"; } }
    /// $d = new D;
    /// register_shutdown_function(function () { $e = new D; echo "sd\n"; });
    /// function a() { yield from a(); }
    /// ini_set("memory_limit", "8M");
    /// foreach (a() as $v);
    /// ```
    /// ```expect exit=255
    /// Fatal error: Allowed memory size of 8388608 bytes exhausted %s
    /// sd
    /// d
    /// ```
    pub(crate) fn end_in_fatal(&mut self, message: &(impl AsRef<[u8]> + ?Sized)) {
        // One raised while a bailout is under way is not rendered: php
        // left what raised it at the first. One raised after the bailout
        // was caught, in a later step of `php_request_shutdown`, is.
        if self.bailout {
            return;
        }
        let (file, line) = self.executed_site();
        self.end_in_fatal_at(message, &file, line);
    }

    /// [`Request::end_in_fatal`] rendered at a site of its own, what a
    /// debug sensor's fatal names: the line of the handler that tripped
    /// it, which the frame has moved past by the time the loop asks.
    pub(super) fn end_in_fatal_at(
        &mut self,
        message: &(impl AsRef<[u8]> + ?Sized),
        file: &[u8],
        line: u32,
    ) {
        if self.bailout {
            return;
        }
        self.note_message_in(Level::Error, Message::Text(message.as_ref()), file, line);
        self.bail_out();
    }

    /// `zend_error_noreturn(E_CORE_ERROR, ...)`: the one core fatal
    /// this engine raises, `zend_objects_destroy_object`'s. php gives
    /// `E_CORE_ERROR` no site at all (`get_filename_lineno`,
    /// `Z/Zend/zend.c:1642`), so it is rendered at `Unknown` on line
    /// zero wherever it was raised, and the request ends in it.
    pub(super) fn end_in_core_error(&mut self, message: &str) {
        if self.bailout {
            return;
        }
        self.note_error_in(Level::CoreError, message, b"Unknown", 0);
        self.bail_out();
    }

    /// `zend_exception_error` (`Z/Zend/zend_exceptions.c:936-990`): an
    /// uncaught throwable rendered at the site it recorded, through
    /// `zend_error_cb` alone and never `zend_error_impl`, with
    /// `EG(last_fatal_error_backtrace)` cleared first, so the diagnostic
    /// carries the throwable's own trace in its message and never a
    /// fatal's after the site, whatever `fatal_error_backtraces` says. A
    /// `ParseError` or a `CompileError` is rendered at its own level and
    /// the rest as `E_ERROR`, under php's `E_DONT_BAIL`, so the store is
    /// not marked destructed, and the request ends. No `_zend_bailout` is
    /// served: the request leaves what it was serving, and the collector
    /// and `CG(unclean_shutdown)` stand as they were, so a debug build's
    /// shutdown collects and its leak sensor reads the request as a debug
    /// php does (#2058). Where no frame stands, php bails out after the
    /// rendering ([`Request::thrown_without_frame`]).
    pub(crate) fn end_in_uncaught(&mut self, level: Level, message: &[u8], file: &[u8], line: u32) {
        if self.bailout {
            return;
        }
        self.clear_fatal_error_backtrace();
        self.serve_uncaught(level, message, file, line);
        self.leave();
    }

    /// `zend_exception_error`'s `zend_error_va(severity | E_DONT_BAIL, ...)`
    /// (`Z/Zend/zend_exceptions.c:921-930`, `:999`): the observers told
    /// of an uncaught throwable with `E_DONT_BAIL`
    /// ([`crate::observer::uncaught`]) and the diagnostic served by
    /// `zend_error_cb` alone, never asking the user's handler, at
    /// `level`: `E_ERROR` as the request ends, or `E_WARNING` where a
    /// fatal reports the throwable that stood pending as it was raised
    /// (`Z/Zend/zend.c:1514`).
    pub(crate) fn serve_uncaught(&mut self, level: Level, message: &[u8], file: &[u8], line: u32) {
        // `zend_strpprintf`'s message, handed to `zend_error_cb` and
        // released after: the throwable's bytes as they stand, which
        // every display form writes.
        if let Ok(string) = self.string(message) {
            crate::observer::uncaught(self, level, file, line, string.as_bytes());
            let text = String::from_utf8_lossy(message);
            self.zend_error_cb(level, Bail::DontBail, string, &text, file, line);
            self.release(Value::string(string));
        }
    }

    /// `zend_bailout` (`Z/Zend/zend.c:1256-1270`) out of a level
    /// `zend_error_cb` was handed and cannot recover from: the cycle
    /// collector takes no root from here on, as `_zend_bailout` protects
    /// it, and a collection serving destructors when the fatal came
    /// returns without freeing; and the request leaves for the `zend_try`
    /// that catches it. What `php_error_cb` does before it bails out is
    /// its own ([`Request::cannot_recover`]), so a fatal a link of
    /// `zend_error_cb` served without it leaves by this alone, as soap's
    /// `soap_real_error_handler` throws a client's fault and bails out.
    /// `CG(unclean_shutdown)` is set here, where php's `_zend_bailout`
    /// sets it, and only here. What `trigger_error()` reaches with `E_USER_ERROR`: every object
    /// standing then is marked destructed, and one a shutdown function
    /// makes is not.
    ///
    /// ```php
    /// class D { function __destruct() { echo "d\n"; } }
    /// $d = new D;
    /// register_shutdown_function(function () { $e = new D; echo "sd\n"; });
    /// trigger_error("boom", E_USER_ERROR);
    /// ```
    /// ```expect exit=255
    /// Deprecated: Passing E_USER_ERROR to trigger_error() is deprecated since 8.4, throw an exception or call exit with a string message instead in %s on line 5
    ///
    /// Fatal error: boom in %s on line 5
    /// sd
    /// d
    /// ```
    pub(crate) fn bail_out(&mut self) {
        self.heap.gc_protect(true);
        #[cfg(debug_assertions)]
        {
            self.progress.unclean = true;
        }
        self.leave();
    }

    /// `CG(unclean_shutdown)`: whether a fatal's `zend_bailout` was
    /// served in this request ([`Progress`]).
    #[cfg(debug_assertions)]
    pub(super) fn unclean_shutdown(&self) -> bool {
        self.progress.unclean
    }

    /// The request leaving what it serves for the `zend_try` that
    /// catches it, the fatal or the uncaught throwable already rendered:
    /// every frame still open to an observer left open and kept as it
    /// stands ([`crate::observer::bailed_out`]), since the serving that
    /// catches it pops them on its way out.
    fn leave(&mut self) {
        self.set_bailing_out(true);
        crate::observer::bailed_out(self);
    }

    /// `php_error_cb`'s last arm, "Bail out if we can't recover"
    /// (`Z/main/main.c:1509-1545`), for the level it was handed: the
    /// status 255, and with nothing displayed, a response not yet
    /// committed at 200 answers `HTTP/1.0 500 Internal Server Error`;
    /// unless php's `E_DONT_BAIL` rides on the level, every object
    /// standing is marked destructed (`zend_objects_store_mark_destructed`),
    /// so none of them is destructed after it. A host that bailed out from
    /// inside its log, or a memory limit met while the diagnostic was
    /// rendered, left before any of it. The ending is the host's: php's
    /// status alone for `E_PARSE`, and for every other level the log form,
    /// the trace under `Stack trace:` where `php_error_cb` gave
    /// `php_log_err` one; the first fatal's stands.
    ///
    /// An `include` that does not compile is a compile error past the
    /// object standing, which is not destructed:
    ///
    /// ```php
    /// class D { function __destruct() { echo "d\n"; } }
    /// $d = new D;
    /// register_shutdown_function(function () { echo "sd\n"; });
    /// $f = tempnam(sys_get_temp_dir(), 'ce');
    /// file_put_contents($f, "<?php\nfunction f() {}\nfunction f() {}\n");
    /// try { include $f; } finally { unlink($f); }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Cannot redeclare function f() (previously declared in %s:2) in %s on line 3
    /// sd
    /// ```
    pub(super) fn cannot_recover(&mut self, error: ErrorArgs<'_>) {
        let ErrorArgs {
            level,
            bail,
            message,
            file,
            line,
            ..
        } = error;
        if !diagnostics::fatal_errors(level) {
            return;
        }
        let left_in_log = self.take_left_in_log();
        if self.bailout && !left_in_log {
            return;
        }
        if !left_in_log {
            self.exit_status = 255;
            if self.display_errors() == Display::Off
                && !self.output().headers_sent()
                && self.output().response_code() == 200
            {
                let _ =
                    crate::output::header_add(self, b"HTTP/1.0 500 Internal Server Error", true, 0);
            }
            if bail == Bail::Out {
                self.heap.mark_destructed();
            }
        }
        if self.ending.is_some() {
            return;
        }
        self.ending = Some(match level {
            Level::Parse => Ending::Parse,
            _ => Ending::Fatal(format!(
                "PHP Fatal error:  {message} in {} on line {line}{}",
                String::from_utf8_lossy(file),
                self.stack_trace_after_the_site()
            )),
        });
    }

    /// `E_PARSE` or `E_COMPILE_ERROR` at a site of its own, which is what
    /// the compiler's refusal carries: rendered once through
    /// `php_error_cb` at its level, and the request ended as that level
    /// ends it, php's status alone for the parse error and the log form
    /// beside it for the compile error, which `php_error_cb` bails out
    /// of as it does from `E_ERROR`. php's parser refuses by throwing a
    /// `ParseError` (`zenderror`, `Z/Zend/zend_language_parser.y`), which
    /// `zend_exception_error` renders with no trace fetched, while the
    /// compiler's `zend_error_noreturn(E_COMPILE_ERROR, …)` goes through
    /// `zend_error_impl` and fetches one, `#0 {main}` with no frame
    /// standing.
    pub(crate) fn end_in_compile_time(
        &mut self,
        level: Level,
        message: &str,
        file: &[u8],
        line: u32,
    ) {
        if level == Level::Parse {
            self.end_in_uncaught(level, message.as_bytes(), file, line);
            return;
        }
        self.note_error_in(level, message, file, line);
        self.bail_out();
    }

    /// `EG(exit_status) = status`, `ZEND_FUNCTION(exit)`'s int arm: the
    /// last write wins, so an `exit` in a destructor or a shutdown
    /// function moves the status a fatal or an earlier `exit` set.
    pub(crate) fn end_in_exit(&mut self, code: i32) {
        self.exit_status = code;
    }

    /// php's `EG(exit_status)`, what `serve` reports the request ended
    /// with: what `exit` last wrote, or 255 after a fatal.
    #[must_use]
    pub fn exit_status(&self) -> i32 {
        self.exit_status
    }

    /// `zend_throw_unwind_exit` (`Z/Zend/zend_exceptions.c:1061`): the
    /// unwind exit is standing where a throwable would be, for the
    /// unwinder to read; php asserts nothing else is pending, and a
    /// raiser that found something pending returns it instead.
    pub(crate) fn throw_unwind_exit(&mut self) {
        if self.raised.is_none() {
            self.raised = Some(Raised::UnwindExit);
        }
    }

    /// `zend_throw_graceful_exit` (`Z/Zend/zend_exceptions.c:1069`): the
    /// graceful exit standing where a throwable would, which is what
    /// `zend_fiber_object_destroy` resumes a suspended fiber with.
    pub(crate) fn throw_graceful_exit(&mut self) {
        if self.raised.is_none() {
            self.raised = Some(Raised::GracefulExit);
        }
    }

    /// Whether the unwind exit is what is being unwound: php's
    /// `zend_is_unwind_exit(EG(exception))`, read by the unwinder where
    /// it would enter a `finally` block or chain a throwable, and by
    /// [`Request::serve_user`] where a native reads what its callback
    /// left.
    pub(crate) fn unwind_exit(&self) -> bool {
        self.raised == Some(Raised::UnwindExit)
    }

    /// What the unwinder leaves by that no `catch` takes and no `finally`
    /// is served for: the unwind exit, or a fatal leaving for its
    /// `zend_try`.
    pub(crate) fn unwinding(&self) -> bool {
        self.unwind_exit() || self.bailout
    }

    /// `zend_end_try()` over a fatal's `zend_bailout`: the `zend_try` it left
    /// for has caught it, and what follows is served again.
    ///
    /// A fatal a native raises inside a callback leaves the callback,
    /// the native that called it and the program, as `zend_bailout`
    /// leaves them all for the `zend_try` in `php_execute_script`:
    ///
    /// ```php
    /// ini_set('memory_limit', '2M');
    /// array_map(function ($x) { echo "cb $x\n"; str_repeat('x', 4 << 20); echo "after $x\n"; }, [1, 2]);
    /// echo "main after\n";
    /// ```
    /// ```expect exit=255
    /// cb 1
    ///
    /// Fatal error: Allowed memory size of 2097152 bytes exhausted (tried to allocate 4194336 bytes) in %s on line 3
    /// ```
    ///
    /// No `finally` is served on the way out, and each step of
    /// `php_request_shutdown` is a `zend_try` of its own: the shutdown
    /// functions are served after the program's fatal, one of them
    /// raising a second, which is rendered and ends the walk.
    ///
    /// ```php
    /// register_shutdown_function(function () { echo "sd1\n"; str_repeat('x', 4 << 20); echo "sd1 after\n"; });
    /// register_shutdown_function(function () { echo "sd2\n"; });
    /// ini_set('memory_limit', '2M');
    /// try { str_repeat('y', 4 << 20); } finally { echo "finally\n"; }
    /// ```
    /// ```expect exit=255
    /// Fatal error: Allowed memory size of 2097152 bytes exhausted (tried to allocate 4194336 bytes) in %s on line 5
    /// sd1
    ///
    /// Fatal error: Allowed memory size of 2097152 bytes exhausted (tried to allocate 4194336 bytes) in %s on line 2
    /// ```
    pub(crate) fn catch_bailout(&mut self) {
        self.set_bailing_out(false);
    }

    /// The one write of whether a `zend_bailout` is under way, which a
    /// debug build's heap is told of: an owner dropped while a fatal
    /// leaves is not a defect, as php's jump leaves a native's locals
    /// for the heap to discard ([`php_heap::Heap::fatal_leaving`]).
    pub(super) fn set_bailing_out(&mut self, leaving: bool) {
        self.bailout = leaving;
        #[cfg(debug_assertions)]
        self.heap.fatal_leaving(leaving);
    }

    /// The request's [`Phase`].
    pub(crate) fn phase(&self) -> Phase {
        self.progress.phase
    }

    /// The request moved to `phase`, as `serve` crosses php's startup
    /// and shutdown.
    pub(crate) fn enter_phase(&mut self, phase: Phase) {
        self.progress.phase = phase;
    }

    pub(crate) fn end_in(&mut self, fatal: &Fatal) {
        // `php_output_discard_all()` where the heap refused with its
        // limit lifted, before the fatal is displayed
        // (`zend_alloc_in_memory_limit_error_reporting`).
        if self.heap.in_memory_limit_error() {
            crate::output::discard_all(self);
        }
        match fatal.reason() {
            php_heap::FatalReason::Error(message) => self.end_in_fatal(message),
            php_heap::FatalReason::OutOfMemory(bytes) => self.end_in_fatal(&format!(
                "Allowed memory size of {} bytes exhausted (tried to allocate {bytes} bytes)",
                self.ini_parse_bytes_of("memory_limit")
            )),
        }
        // A fatal the heap raised lifted its limit for the rendering, as
        // `zend_mm_safe_error` does; it holds again from here.
        self.heap.overflow_rendered();
    }

    /// Each extension's hook once the primary program has ended and before
    /// the request shuts down, in bundle order
    /// ([`ExtensionBuilder::program_ended`]).
    ///
    /// [`ExtensionBuilder::program_ended`]: crate::api::abi::ExtensionBuilder::program_ended
    pub(crate) fn program_ended(&mut self) {
        tell!(self, program_ended, |req, hook| hook(req));
    }

    /// The host's log for a fatal it took, `sapi_module.log_message`,
    /// the last extension's that supplied one
    /// ([`ExtensionBuilder::fatal_logged`]).
    ///
    /// [`ExtensionBuilder::fatal_logged`]: crate::api::abi::ExtensionBuilder::fatal_logged
    pub(super) fn fatal_logged(&mut self) -> AfterLog {
        let shared = Arc::clone(&self.shared);
        shared
            .supplied
            .fatal_logged
            .entry()
            .map_or(AfterLog::Returned, |(by, hook)| self.serve_for(by, hook))
    }
}
