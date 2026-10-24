//! The memory limit's two ends of a request, as php 8.5.12 raises them
//! (CVE-2026-109618, #3260). An allocation past `memory_limit` is served,
//! and the heap records a delayed OOM and raises the request's interrupt
//! ([`php_heap::DelayedOom`]); this file names the site php records with
//! it, and raises it where php's `zend_handle_delayed_oom` does: at the
//! next interrupt check, at the next `zend_call_function`, and once the
//! program has been served. An allocation past the hard limit ends the process
//! where it stands ([`Request::terminate_at_hard_limit`]).

use php_heap::DelayedOom;

use super::Request;
use super::diagnostics::Message;
use crate::api::raising::Level;

impl Request {
    /// The site of a delayed OOM recorded and not yet named, where the
    /// request stands: what php reads in `zend_mm_delayed_verror` as it
    /// records one, `zend_get_executed_filename_ex()` and
    /// `zend_get_executed_lineno()`, `Unknown` on line zero with nothing
    /// executing. The heap cannot read a frame, so the engine names it
    /// before the frame it was recorded under moves on: at the interrupt
    /// check, before a back-edge stands its frame at the target; as a
    /// frame leaves; and after a compile.
    #[cold]
    pub(crate) fn locate_oom(&mut self) {
        if self.oom_unlocated() {
            let (file, line) = self.executed_site();
            self.heap.locate_oom(&file, line);
        }
    }

    /// `ZEND_HAS_OOM()`: the memory limit was met in this request and
    /// `php_request_shutdown` has not begun ([`php_heap::Heap::has_oom`]).
    #[inline]
    pub(crate) fn has_oom(&self) -> bool {
        self.heap.has_oom()
    }

    /// Whether a delayed OOM is recorded and not yet named, which a
    /// frame's leave reads before it moves the site: one load of the
    /// heap's state where nothing is pending.
    #[inline]
    pub(crate) fn oom_unlocated(&self) -> bool {
        matches!(self.heap.delayed_oom(), DelayedOom::Pending(pending) if pending.site.is_none())
    }

    /// `zend_handle_delayed_oom` (`Z/Zend/zend_alloc.c:3891` at 8.5.12):
    /// the limit's fatal raised where it was recorded. With a frame
    /// executing, whatever was pending is given up and the unwind exit
    /// thrown in its place, so no `catch` takes it and no `finally` is
    /// served; the fatal is then rendered at its site, the `ob_start()`
    /// buffers discarded first while the limit is reported
    /// (`php_output_discard_all`), its status 255 and the objects marked
    /// destructed as `php_error_cb` leaves them, and php's `zend_catch`
    /// lets the unwind go on. With no frame executing the fatal's
    /// `zend_bailout` is not caught, and the request ends as an immediate fatal does.
    /// Anything but a pending OOM is nothing to raise.
    ///
    /// The allocation is served, so what follows it on its line runs,
    /// up to the next safepoint, and the fatal's trace stops at its first
    /// frame, the native that allocated:
    ///
    /// ```php
    /// ini_set('memory_limit', '2M');
    /// ini_set('fatal_error_backtraces', '1');
    /// function g($n) { $s = str_repeat('x', $n); echo "g after\n"; return $s; }
    /// function f($n) { echo "f\n"; $r = g($n); echo "f after\n"; return $r; }
    /// try { f(4 << 20); } finally { echo "finally\n"; }
    /// ```
    /// ```expect exit=255
    /// f
    ///
    /// Fatal error: Allowed memory size of 2097152 bytes exhausted (tried to allocate 4210688 bytes) in %s on line 4
    /// Stack trace:
    /// #0 %s(4): str_repeat('x', 4194304)
    /// #1 {main}
    /// ```
    pub(crate) fn handle_delayed_oom(&mut self) {
        self.locate_oom();
        let Some(pending) = self.heap.take_pending_oom() else {
            return;
        };
        let executing = self.frames().executing();
        if executing {
            // `EG(exception) = NULL; zend_throw_unwind_exit();`
            if let Some(given_up) = self.raised.take() {
                self.release_raised(given_up);
            }
            self.throw_unwind_exit();
        }
        // Discarding frees every handler on the stack, which is not safe
        // while one runs (`PHP_OUTPUT_LOCKED`, `Z/main/main.c:1434`).
        if self.output().status() & crate::output::LOCKED == 0 {
            crate::output::discard_all(self);
        }
        let (file, line) = pending.site.map_or_else(
            || (b"Unknown".to_vec(), 0),
            |(file, line)| (file.into_vec(), line),
        );
        self.note_message_in(
            Level::Error,
            Message::Text(pending.message.as_bytes()),
            &file,
            line,
        );
        self.heap.oom_handled();
        if executing {
            // The fatal's `zend_bailout`, caught by `zend_catch`.
            self.bailout_marks();
        } else {
            self.bail_out();
        }
    }

    /// `zend_mm_hard_limit_error` (`Z/Zend/zend_alloc.c:162` at 8.5.12):
    /// php's line on the request's standard error, at the site the
    /// request stands on, and its status, 137, with no shutdown of any
    /// kind, as `_exit` ends php's process: no shutdown function, no
    /// destructor, no `ob_start()` buffer flushed ([`Ending::Terminated`]). What
    /// the request wrote before is the host's already. The engine ends
    /// the request and not the process, which other requests may share;
    /// a host that is one request's process ends on its status, as the
    /// CLI does.
    ///
    /// [`Ending::Terminated`]: super::Ending::Terminated
    ///
    /// ```php
    /// $child = '<?php ini_set("memory_limit", "2M"); register_shutdown_function(function () { echo "shutdown\n"; });'
    ///     . ' ob_start(); echo "buffered\n"; ob_end_flush(); ob_start(); echo "lost\n";'
    ///     . ' $a = []; for ($i = 0; $i < 1000000; $i++) { $a[] = $i; }';
    /// $f = sys_get_temp_dir() . '/hard-' . getmypid() . '.php';
    /// file_put_contents($f, $child);
    /// $p = proc_open([PHP_BINARY, '-n', $f], [1 => ['pipe', 'w'], 2 => ['pipe', 'w']], $pipes);
    /// echo stream_get_contents($pipes[1]), str_replace([realpath($f), $f], 'child', stream_get_contents($pipes[2])), proc_close($p), "\n";
    /// unlink($f);
    /// ```
    /// ```expect process
    /// buffered
    /// Hard memory limit of 4194304 bytes exhausted (tried to allocate 2113536 bytes) in child on line 1
    /// 137
    /// ```
    pub(crate) fn terminate_at_hard_limit(&mut self, limit: usize, asked: usize) {
        if self.terminated() {
            return;
        }
        let (file, line) = self.executed_site();
        let said = format!(
            "Hard memory limit of {limit} bytes exhausted (tried to allocate {asked} bytes) in {} on line {line}\n",
            String::from_utf8_lossy(&file)
        );
        self.response_mut().write_stderr(said.as_bytes());
        self.end_in_exit(137);
        self.end_as_terminated();
        // No destructor is served as the request's tables are freed.
        self.heap.mark_destructed();
        self.bail_out();
    }
}
