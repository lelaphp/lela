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
    /// `Phar::webPhar` has an entry of its archive served
    /// ([`Request::load_stream`]); a path `EG(included_files)` has already
    /// is left alone (`zend_hash_add` failing), and any other is added
    /// before the compile; the unit compiled through the unit cache as a `require`
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
        if super::is_wrapper_url(&path) {
            return self.load_stream(written, &path);
        }
        if self.included_files.contains(&path) {
            return Ok(false);
        }
        self.note_included(&path)?;
        let unit = match self.compile_file(&path, Some(written), super::HandlePath::Opened)? {
            Some(Ok(unit)) => unit,
            Some(Err(refused)) => {
                let kind = php_compile::SourceKind::File;
                return Err(self.refused_include(&refused, &path, kind));
            }
            None => return Ok(false),
        };
        if !crate::engine::note_compiled(self, &unit, php_compile::SourceKind::File) {
            return Err(Thrown::of(self.take_pending_view()));
        }
        self.serve_top_code(unit)?;
        Ok(true)
    }

    /// [`Request::load_file`] over a stream wrapper's URL, in
    /// `spl_autoload`'s order: the stream opened first, and
    /// `EG(included_files)` asked by the name the open gave it, the
    /// `*opened_path` its opener set or the path, a name it holds closed
    /// unread (`zend_hash_add` failing); then the file compiled as a
    /// `require`, its diagnostics raised and its classes declared
    /// ([`Request::compile_stream`]), and served by `zend_execute` before
    /// `zend_destroy_file_handle` closes the stream. So the wrapper's
    /// `stream_close` and destructor come after what the file printed, what
    /// the file threw refuses `stream_close` and is what the destructor
    /// chains onto, and a close that throws after the file was served leaves
    /// its class declared.
    ///
    /// ```php
    /// class W { public $context; private $d = false; static $src; static $throw = false; public $p;
    ///     function stream_open($p, $m, $o, &$op) { $this->p = $p; echo "open $p\n"; return true; }
    ///     function stream_read($n) { if ($this->d) return ''; $this->d = true; return self::$src; }
    ///     function stream_eof() { return $this->d; }
    ///     function stream_stat() { return []; }
    ///     function stream_close() { echo "close {$this->p}\n"; if (self::$throw) throw new Exception('close'); }
    ///     function stream_set_option($o, $a, $b) { return false; }
    ///     function __destruct() { echo "dtor {$this->p}\n"; } }
    /// stream_wrapper_register('ww', 'W');
    /// W::$src = "<?php class Foo {} echo 'ran foo', PHP_EOL;";
    /// spl_autoload('ww://Foo', '.inc');
    /// spl_autoload('ww://Foo', '.inc');
    /// W::$src = "<?php class Bar {} echo 'ran bar', PHP_EOL; throw new Exception('run');";
    /// try { spl_autoload('ww://Bar', '.inc'); } catch (Exception $e) { echo "caught ", $e->getMessage(), "\n"; }
    /// W::$throw = true;
    /// W::$src = "<?php class Baz {} echo 'ran baz', PHP_EOL;";
    /// try { spl_autoload('ww://Baz', '.inc'); } catch (Exception $e) { echo "caught ", $e->getMessage(), "\n"; }
    /// var_dump(class_exists('Bar', false), class_exists('Baz', false), count(get_included_files()));
    /// ```
    /// ```expect
    /// open ww://foo.inc
    /// ran foo
    /// close ww://foo.inc
    /// dtor ww://foo.inc
    /// open ww://foo.inc
    /// close ww://foo.inc
    /// dtor ww://foo.inc
    /// open ww://bar.inc
    /// ran bar
    /// dtor ww://bar.inc
    /// caught run
    /// open ww://baz.inc
    /// ran baz
    /// close ww://baz.inc
    /// dtor ww://baz.inc
    /// caught close
    /// bool(true)
    /// bool(true)
    /// int(4)
    /// ```
    fn load_stream(&mut self, written: &[u8], path: &[u8]) -> Result<bool, Thrown> {
        let Some(hooks) = self.stream_hooks() else {
            return Ok(false);
        };
        let named = (hooks.open)(self, path)?;
        let served = self.serve_stream(written, named, hooks.read_include);
        match self.close_stream(hooks.close_include, served.as_ref().err()) {
            Some(thrown) => Err(thrown),
            None => served,
        }
    }

    /// [`Request::load_stream`] over the file the open `named`, before its
    /// `zend_file_handle` is destroyed: `false` for a file that did not
    /// open, or that the table holds, and `true` once the file was compiled
    /// and served.
    fn serve_stream(
        &mut self,
        written: &[u8],
        named: Option<Vec<u8>>,
        read: fn(&mut Request) -> Result<Option<Vec<u8>>, Thrown>,
    ) -> Result<bool, Thrown> {
        let Some(named) = named else {
            return self.stream_thrown.take().map_or(Ok(false), Err);
        };
        if self.included_files.contains(&named) {
            return Ok(false);
        }
        self.note_included(&named)?;
        let kind = exec::IncludeKind::Require;
        match self.compile_stream(kind, written, Some(named), read)? {
            super::Opened::Declared(unit) => self.serve_top_code(unit).map(|()| true),
            _ => Ok(false),
        }
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
