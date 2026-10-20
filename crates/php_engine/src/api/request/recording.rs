//! Diagnostics held back and raised later, php's `EG(record_errors)` and
//! `EG(errors)` (`Z/Zend/zend.c:1457-1497`, `:1777-1810`). `compile_file`
//! records what a file's compile raises and emits it once the compile
//! has declared what the file declares
//! (`Z/Zend/zend_language_scanner.l:655-666`), and `zend_do_link_class`
//! records what a link raises and emits it at the link's end, unless a
//! record already stands, whose owner emits it
//! (`Z/Zend/zend_inheritance.c:3611`, `:3818`). A fatal raised while
//! anything is held emits what is held first, with the user error
//! handler masked off, since running the program's code past a fatal is
//! not safe; `compile_string` records nothing, so an `eval` raises as it
//! compiles.

use std::mem;

use php_unit::Level;

use super::Request;

/// One diagnostic held back, `zend_error_info`.
struct Held {
    level: Level,
    message: String,
    file: Vec<u8>,
    line: u32,
}

/// `EG(record_errors)` beside `EG(errors)`: whether a diagnostic raised
/// now is held rather than served, and what is held. What is held can
/// outlive the recording, as `zend_do_link_class` stops recording before
/// its delayed autoloads and emits after them.
#[derive(Default)]
pub(crate) struct Record {
    on: bool,
    held: Vec<Held>,
}

/// Who emits what a record holds: the owner that began it, or the one
/// that stood when it was asked for, php's `orig_record_errors`.
#[must_use]
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Recording {
    /// This caller began the record and emits it.
    Began,
    /// A record stood already, and its owner emits what this caller
    /// adds.
    Joined,
}

impl Request {
    /// `zend_begin_record_errors` where no record stands: from here a
    /// diagnostic below the fatal levels is held, not served.
    pub(crate) fn begin_recording(&mut self) -> Recording {
        if self.record.on {
            return Recording::Joined;
        }
        self.record.on = true;
        Recording::Began
    }

    /// `EG(record_errors) = orig_record_errors`: the recording as it
    /// stood before `recording` was asked for, what is held kept for the
    /// emit.
    pub(crate) fn pause_recording(&mut self, recording: Recording) {
        if self.bailing_out() {
            return;
        }
        self.record.on = recording == Recording::Joined;
    }

    /// `zend_emit_recorded_errors` then `zend_free_recorded_errors`,
    /// where `recording` began the record: the recording stopped and
    /// each held diagnostic raised in the order it was raised, through
    /// the user error handler as any other. A fatal's `zend_bailout` jumps past
    /// both, as it jumps past `compile_file`'s and `zend_do_link_class`'s
    /// calls of them, so the record stands on, the fatal held last, for
    /// the rest of the request: what a shutdown function raises below
    /// the fatal levels after an `include` the compiler refused is held
    /// and never shown, and `error_get_last()` still answers the fatal.
    ///
    /// ```php
    /// register_shutdown_function(function () {
    ///     echo $undefined, "after\n";
    ///     echo error_get_last()["message"], "\n";
    /// });
    /// $f = tempnam(sys_get_temp_dir(), 'rc');
    /// file_put_contents($f, "<?php\nbreak 2;\n");
    /// register_shutdown_function('unlink', $f);
    /// include $f;
    /// ```
    /// ```expect exit=255
    /// Fatal error: 'break' not in the 'loop' or 'switch' context in %s on line 2
    /// after
    /// 'break' not in the 'loop' or 'switch' context
    /// ```
    pub(crate) fn emit_recorded(&mut self, recording: Recording) {
        if recording == Recording::Joined || self.bailing_out() {
            return;
        }
        self.record.on = false;
        for held in mem::take(&mut self.record.held) {
            self.note_error_in(held.level, &held.message, &held.file, held.line);
        }
    }

    /// `zend_error_zstr_at`'s two steps before the user handler: what is
    /// held emitted ahead of a fatal with the handler masked off, and a
    /// diagnostic held where a record stands, a fatal too, which goes on
    /// to be served. `true` says the diagnostic goes no further: it was
    /// held, or a fatal among what was held bailed out ahead of it, as
    /// php's jump leaves `zend_error_impl` before it puts the record back,
    /// so the record is left off and empty, and the handler masked.
    ///
    /// A second `include` of a file the compiler refused, from a shutdown
    /// function: the first left its record standing, the second's fatal
    /// emits it, and the first's fatal bails out ahead of its own, with
    /// the trace of where the second stands.
    ///
    /// ```php
    /// $f = tempnam(sys_get_temp_dir(), 'rc');
    /// file_put_contents($f, "<?php\nfunction f(int \$a = null) {}\nbreak 2;\n");
    /// register_shutdown_function(function () use ($f) {
    ///     include $f;
    /// });
    /// register_shutdown_function('unlink', $f);
    /// include $f;
    /// ```
    /// ```expect exit=255
    /// Deprecated: f(): Implicitly marking parameter $a as nullable is deprecated, the explicit nullable type must be used instead in %s on line 2
    ///
    /// Fatal error: 'break' not in the 'loop' or 'switch' context in %s on line 3
    ///
    /// Deprecated: f(): Implicitly marking parameter $a as nullable is deprecated, the explicit nullable type must be used instead in %s on line 2
    ///
    /// Fatal error: 'break' not in the 'loop' or 'switch' context in %s on line 3
    /// ```
    pub(super) fn recorded(&mut self, level: Level, message: &str, file: &[u8], line: u32) -> bool {
        let fatal = super::fatal_errors(level);
        if fatal && !self.record.held.is_empty() {
            let held = mem::take(&mut self.record.held);
            let on = mem::replace(&mut self.record.on, false);
            let levels = self.replace_error_handler_levels(0);
            for each in &held {
                self.note_error_in(each.level, &each.message, &each.file, each.line);
                if super::fatal_errors(each.level) && !self.bailing_out() {
                    self.bail_out();
                }
                if self.bailing_out() {
                    return true;
                }
            }
            self.replace_error_handler_levels(levels);
            self.record = Record { on, held };
        }
        if !self.record.on {
            return false;
        }
        self.record.held.push(Held {
            level,
            message: message.to_owned(),
            file: file.to_vec(),
            line,
        });
        !fatal
    }

    /// The record while the user error handler runs: none, as
    /// `zend_error_impl` clears `EG(record_errors)` and `EG(errors)`
    /// around the call and puts them back after it.
    pub(super) fn record_set_aside(&mut self) -> Record {
        mem::take(&mut self.record)
    }

    /// The record [`Request::record_set_aside`] took, back.
    pub(super) fn record_restored(&mut self, record: Record) {
        self.record = record;
    }
}
