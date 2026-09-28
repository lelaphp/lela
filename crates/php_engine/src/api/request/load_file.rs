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
    /// opens it; a path `EG(included_files)` has already is left alone
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
        let Ok(path) = self.resolve_path(written) else {
            return Ok(false);
        };
        if self.included_files.contains(&path) {
            return Ok(false);
        }
        self.included_files.push(path.clone());
        let replaced = match self.compile_file_source(&path)? {
            Some(source) => {
                let shared = Arc::clone(&self.shared);
                Some(shared.compile_for(self, &source, &path, php_compile::SourceKind::File))
            }
            None => self.cached_file(&path),
        };
        let unit = match replaced {
            Some(Ok(unit)) => unit,
            Some(Err(refused)) => return Err(self.refused_include(&refused, &path)),
            None => return Ok(false),
        };
        crate::engine::note_compiled(self, &unit);
        if !self.declare_classes(&unit) || !self.declare_functions(&unit) {
            return Err(Thrown::of(self.pending.take()));
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
        let standing = self.vm.frames().len();
        let code = exec::code_of(&unit);
        let slots = usize::try_from(code.num_cvs.saturating_add(code.num_tmps)).unwrap_or(0);
        let pushed = self.stack.push_frame(&mut self.heap, slots, 0);
        let frame = self.fatal_of(pushed)?;
        let caller = standing
            .checked_sub(1)
            .and_then(|at| u32::try_from(at).ok())
            .map(php_heap::FrameRef);
        let table = caller.map(|caller| self.rebuild_symbol_table(caller));
        exec::init_runtime_cache(&mut self.map, code, exec::Body::Main);
        self.forget_main_statics(code.runtime_cache);
        self.vm
            .push_body(unit, exec::Body::Main, None, None, None, 0, None, caller);
        if let Some(state) = self.vm.at_mut(frame) {
            state.scope = None;
            state.symbol_table = table;
        }
        self.attach_symbol_table(frame);
        let reenter = self.shared.reenter;
        let served = reenter(self, frame);
        let served = self.fatal_of(served);
        while self.vm.frames().len() > standing + 1 {
            self.pop_frame();
        }
        // `zend_detach_symbol_table` on the way out, so what the file
        // wrote to a variable of the caller's is in the caller's CV.
        if self.vm.frames().len() > standing {
            self.leave_frame(frame);
        }
        served?;
        let returned = std::mem::replace(&mut self.returned, Value::null());
        self.release(returned);
        match self.pending.take() {
            Some(pending) => Err(Thrown::of(Some(pending))),
            None if self.unwinding() => Err(Thrown::of(None)),
            None => Ok(()),
        }
    }
}
