//! A file compiled and served on the loop from a native, `zend_compile_file` and
//! then `zend_execute`: what `spl_autoload`, SPL's default autoloader,
//! does with the class file it finds (`Z/ext/spl/php_spl.c:220`).

use std::sync::Arc;

use super::Request;
use crate::api::raising::Thrown;
use crate::api::values::Value;
use crate::exec;

impl Request {
    /// [`crate::api::call::load_file`]: the path resolved along the
    /// `include_path` as `php_stream_open_for_zend_ex(USE_PATH |
    /// STREAM_OPEN_FOR_INCLUDE)` opens it, with no warning where nothing
    /// opens it, and a stream wrapper's URL opened by its wrapper, as
    /// `Phar::webPhar` has an entry of its archive served; a path `EG(included_files)` has already is left alone
    /// (`zend_hash_add` failing), and any other is added before the
    /// compile; the unit compiled through the unit cache as a `require`
    /// is and served on a top-level code frame, `ZEND_CALL_TOP_CODE`,
    /// that shares the innermost user frame's variables
    /// (`zend_rebuild_symbol_table`) and has no `$this` and no scope, as
    /// `zend_execute` gives the frame of a native none.
    pub(crate) fn load_file(&mut self, written: &[u8]) -> Result<bool, Thrown> {
        if written.contains(&0) {
            return Ok(false);
        }
        let path = match self.resolve_path(written) {
            Ok(path) => path,
            Err(super::Unresolved::Threw(thrown)) => return Err(thrown),
            // A path a stream wrapper serves is opened as it is written,
            // `zend_stream_open`'s `php_stream_open_for_zend_ex`.
            Err(super::Unresolved::Missing(_)) if super::is_wrapper_url(written) => {
                written.to_vec()
            }
            Err(super::Unresolved::Missing(_)) => return Ok(false),
        };
        if self.included_files.contains(&path) {
            return Ok(false);
        }
        self.note_included(&path)?;
        if super::is_wrapper_url(&path) {
            let opened = self.include_stream(exec::IncludeKind::Require, written, &path)?;
            let super::Opened::Unit(unit) = opened else {
                return Ok(false);
            };
            if !crate::engine::note_compiled(self, &unit, php_compile::SourceKind::File) {
                return Err(Thrown::of(self.take_pending_view()));
            }
            self.serve_top_code(unit)?;
            return Ok(true);
        }
        let compiled = self.compile_file(&path, Some(written), super::HandlePath::Opened)?;
        let unit = match compiled {
            Some(Ok(unit)) => unit,
            Some(Err(refused)) => {
                return Err(self.refused_include(&refused, &path, php_compile::SourceKind::File));
            }
            None => return Ok(false),
        };
        if !crate::engine::note_compiled(self, &unit, php_compile::SourceKind::File) {
            return Err(Thrown::of(self.take_pending_view()));
        }
        self.serve_top_code(unit)?;
        Ok(true)
    }

    /// `zend_execute` over a unit's main op array from a native: a frame
    /// pushed with no receiver and no scope, attached to the innermost
    /// user frame's symbol table, entered on the engine's loop once more
    /// and popped with everything above where the stack stood; what the
    /// file left pending is the `Err`, and what it returned is given up.
    fn serve_top_code(&mut self, unit: Arc<php_unit::Unit>) -> Result<(), Thrown> {
        let standing = self.vm.depth();
        let code = exec::code_of(&unit);
        let slots = usize::try_from(code.num_cvs.saturating_add(code.num_tmps)).unwrap_or(0);
        let caller = self.innermost_user_frame();
        let mut header = exec::Header::of(None, code);
        // `zend_execute`'s `ZEND_CALL_TOP_CODE`.
        header.info.set_top_code(true);
        let record = Box::new(exec::code_record(&unit));
        header.callee = record.at();
        let pushed = self.vm.push(&mut self.heap, slots, 0, header.clone());
        let frame = self.fatal_of(pushed)?;
        let table = caller.map(|caller| self.rebuild_symbol_table(caller));
        exec::init_runtime_cache(&mut self.map, code, exec::Body::Main);
        self.forget_main_statics(code.runtime_cache);
        header.set_caller(caller.map_or(exec::Caller::Outside, exec::Caller::Opline));
        header.annex = Some(self.annexes_mut().insert(exec::Annex {
            unit: Some(unit),
            record: Some(record),
            symbol_table: table,
            ..exec::Annex::default()
        }));
        if let Some(laid) = self.vm.at_mut(frame) {
            *laid = header;
        }
        self.attach_symbol_table(frame);
        crate::observer::call(self, frame);
        let reenter = self.shared.loops.reenter;
        let served = reenter(self, frame);
        let served = self.fatal_of(served);
        while self.vm.depth() > standing + 1 {
            self.pop_frame();
        }
        // `zend_detach_symbol_table` on the way out, so what the file
        // wrote to a variable of the caller's is in the caller's CV.
        if self.vm.depth() > standing {
            self.leave_frame(frame);
        }
        served?;
        let returned = std::mem::replace(&mut self.returned, Value::null());
        self.release(returned);
        match self.take_pending_view() {
            Some(pending) => Err(Thrown::of(Some(pending))),
            None if self.is_pending() => Err(Thrown::of(None)),
            None => Ok(()),
        }
    }
}
