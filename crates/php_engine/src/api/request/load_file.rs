//! A file compiled and served on the loop from a native, `zend_compile_file` and
//! then `zend_execute`: what `spl_autoload`, SPL's default autoloader,
//! does with the class file it finds (`Z/ext/spl/php_spl.c:220`).

use super::Request;
use crate::api::raising::Thrown;
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
        let path = match self.code_mut().resolve_path(written) {
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
        let unit = if super::is_wrapper_url(&path) {
            let opened = self.include_stream(exec::IncludeKind::Require, written, &path)?;
            let super::Opened::Unit(unit) = opened else {
                return Ok(false);
            };
            unit
        } else {
            match self.compile_file(&path, Some(written), super::HandlePath::Opened)? {
                Some(Ok(unit)) => unit,
                Some(Err(refused)) => {
                    let kind = php_compile::SourceKind::File;
                    return Err(self.refused_include(&refused, &path, kind));
                }
                None => return Ok(false),
            }
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
    fn serve_top_code(&mut self, unit: php_unit::PublishedUnit) -> Result<(), Thrown> {
        let standing = self.vm.depth();
        let from = super::CodeFrom::Top(self.innermost_user_frame());
        let pushed = self.push_code_frame(unit, from);
        let frame = self.fatal_of(pushed)?;
        self.keep_file_statics(frame);
        crate::observer::call(self, frame);
        let reenter = self.loops().reenter;
        let served = reenter(self, frame);
        let served = self.fatal_of(served);
        // `zend_detach_symbol_table` on the way out, so what the file
        // wrote to a variable of the caller's is in the caller's CV.
        self.leave_code(standing, frame);
        served?;
        let returned = std::mem::take(&mut self.returned);
        self.release(returned);
        match self.take_pending_view() {
            Some(pending) => Err(Thrown::of(Some(pending))),
            None if self.is_pending() => Err(Thrown::of(None)),
            None => Ok(()),
        }
    }
}
