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
        self.record.on = recording == Recording::Joined;
    }

    /// `zend_emit_recorded_errors` then `zend_free_recorded_errors`,
    /// where `recording` began the record: the recording stopped and
    /// each held diagnostic raised in the order it was raised, through
    /// the user error handler as any other.
    pub(crate) fn emit_recorded(&mut self, recording: Recording) {
        if recording == Recording::Joined {
            return;
        }
        self.record.on = false;
        for held in mem::take(&mut self.record.held) {
            self.note_error_in(held.level, &held.message, &held.file, held.line);
        }
    }

    /// `zend_error_zstr_at`'s two steps before the user handler: what is
    /// held emitted ahead of a fatal with the handler masked off, and a
    /// diagnostic below the fatal levels held where a record stands.
    /// `true` says the diagnostic was held and goes no further.
    pub(super) fn recorded(&mut self, level: Level, message: &str, file: &[u8], line: u32) -> bool {
        let fatal = super::fatal_errors(level);
        if fatal && !self.record.held.is_empty() {
            let held = mem::take(&mut self.record.held);
            let on = mem::replace(&mut self.record.on, false);
            let levels = self.replace_error_handler_levels(0);
            for held in held {
                self.note_error_in(held.level, &held.message, &held.file, held.line);
            }
            self.replace_error_handler_levels(levels);
            self.record.on = on;
        }
        // php records a fatal too and serves it on; its bailout then
        // frees the record, so a fatal is never emitted twice.
        if !self.record.on || fatal {
            return false;
        }
        self.record.held.push(Held {
            level,
            message: message.to_owned(),
            file: file.to_vec(),
            line,
        });
        true
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
