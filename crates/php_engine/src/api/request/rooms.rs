//! What php's request heap holds for state this engine keeps typed and
//! elsewhere (design §5.13, #2863): the bytes alone, at php's sizes and
//! for as long as php holds them, which is all `memory_get_usage()`
//! observes of that state. Each is a structure php allocates whatever
//! the request compiles: what the request's start lays down, and the
//! primary file's `zend_file_handle` while the program is served. The
//! engine's own working directory and its fibers are where they always
//! were.

use crate::api::raising::Thrown;
use crate::api::values::{Emalloc, PhpString, Value};

use super::Request;

/// `sizeof(zend_fiber_context)`, as `zend_fiber_init` `ecalloc`s
/// `EG(main_fiber_context)`, in the 112-byte bin.
const FIBER_CONTEXT: usize = 112;

/// `OLD_CWD_SIZE` (`Z/main/main.c:2581`), the buffer
/// `php_execute_script_ex` keeps the working directory it entered in.
#[cfg(target_vendor = "apple")]
const OLD_CWD: usize = 4096;

/// The rooms a request holds ([`Request::rooms`]).
#[derive(Debug, Default)]
pub(crate) struct Rooms {
    /// `CWDG(cwd)`: `virtual_cwd_activate`'s `CWD_STATE_COPY` of the
    /// working directory the process started in, its bytes and a NUL
    /// (`Z/Zend/zend_virtual_cwd.c:233`).
    cwd_state: Option<Emalloc>,
    /// `EG(main_fiber_context)`, `zend_fiber_init`'s
    /// (`Z/Zend/zend_fibers.c:1126`).
    main_fiber: Option<Emalloc>,
    /// The primary file's `zend_file_handle`, from the host's
    /// `zend_stream_init_fp` until `zend_destroy_file_handle` after
    /// `php_execute_script`.
    primary: PrimaryHandle,
}

/// What the primary file's `zend_file_handle` holds of the request's
/// heap.
#[derive(Debug, Default)]
struct PrimaryHandle {
    /// `handle->filename`, the name the host wrote.
    filename: Option<PhpString>,
}

impl Request {
    /// What `zend_activate` and the process's startup leave on a
    /// request's heap of state the engine keeps elsewhere, laid down as
    /// the request starts: `EG(main_fiber_context)`, and, for a request
    /// that is its process's one, as the CLI's and phpdbg's is, the copy
    /// of the working directory `virtual_cwd_activate` made once
    /// `php_module_startup` was done. A non-ZTS php frees that copy at the first
    /// request's end and never makes it again, so a web host's later
    /// requests stand without it, which a request of the engine's
    /// threads is taken to be. A refusal is the memory limit's fatal,
    /// which the first allocation after it meets again.
    pub(crate) fn lay_start_rooms(&mut self) {
        if self.description.web.is_none() {
            let len = self.cwd.as_os_str().len() + 1;
            self.rooms.cwd_state = Emalloc::new(self, len).ok();
        }
        self.rooms.main_fiber = Emalloc::new(self, FIBER_CONTEXT).ok();
    }

    /// The primary file's `zend_file_handle` opened as the host opens it, under the
    /// name it was `written` by, before the program is served.
    pub(crate) fn open_primary_handle(&mut self, written: &[u8]) -> Result<(), Thrown> {
        self.rooms.primary.filename = Some(self.string_init(written)?);
        Ok(())
    }

    /// `zend_destroy_file_handle` over the primary file's `zend_file_handle` once the
    /// program has been served.
    pub(crate) fn close_primary_handle(&mut self) {
        let primary = std::mem::take(&mut self.rooms.primary);
        if let Some(filename) = primary.filename {
            self.release(Value::string(filename));
        }
    }

    /// `php_execute_script_ex`'s `do_alloca(OLD_CWD_SIZE, use_heap)`,
    /// which is `emalloc` on Apple's platforms (`Z/Zend/zend_portability.h`,
    /// `!defined(__APPLE__)` keeps `alloca` off them) and the stack's
    /// elsewhere: held while the program is served and given back with
    /// [`Request::give_back_old_cwd`].
    pub(crate) fn old_cwd(&mut self) -> Option<Emalloc> {
        #[cfg(target_vendor = "apple")]
        {
            Emalloc::new(self, OLD_CWD).ok()
        }
        #[cfg(not(target_vendor = "apple"))]
        {
            None
        }
    }

    /// `free_alloca` of [`Request::old_cwd`]'s buffer.
    pub(crate) fn give_back_old_cwd(&mut self, buffer: Option<Emalloc>) {
        if let Some(buffer) = buffer {
            buffer.free(self);
        }
    }

    /// A debug build's teardown of what [`Request::lay_start_rooms`] laid
    /// down, before the request leak sensor: `zend_fiber_shutdown`'s free
    /// of `EG(main_fiber_context)` and `virtual_cwd_deactivate`'s of the
    /// working directory's copy.
    #[cfg(debug_assertions)]
    pub(super) fn free_rooms(&mut self) {
        self.close_primary_handle();
        let rooms = std::mem::take(&mut self.rooms);
        for room in [rooms.main_fiber, rooms.cwd_state].into_iter().flatten() {
            room.free(self);
        }
    }
}
