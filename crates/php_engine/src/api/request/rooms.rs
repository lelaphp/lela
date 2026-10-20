//! What php's request heap holds for state this engine keeps typed and
//! elsewhere (design §5.13, #2863): the bytes alone, at php's sizes and
//! for as long as php holds them, which is all `memory_get_usage()`
//! observes of that state. Each is a structure php allocates whatever
//! the request compiles: what the request's start lays down, the
//! working directory opcache keys a relative path with, and the primary
//! file's `zend_file_handle` while the program is served. The engine's
//! own working directory, its fibers and its unit cache are where they
//! always were.

use crate::api::raising::Thrown;
use crate::api::values::{Emalloc, PhpString, Value};

use super::Request;

/// `ZEND_MMAP_AHEAD` (`Z/Zend/zend_language_scanner.h`): what
/// `zend_stream_fixup` reads a file into past its bytes.
const MMAP_AHEAD: usize = 32;

/// `sizeof(zend_llist_element) + sizeof(zend_file_handle) - 1`, the
/// element `zend_llist_add_element` lays a `zend_file_handle` into on
/// `CG(open_files)`: a 24-byte element whose one byte of `data` the
/// 80-byte `zend_file_handle` overlays.
const OPEN_FILES_ELEMENT: usize = 24 + 80 - 1;

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
    /// The host's copies of the program's path
    /// ([`WebRequest::path_copies`](crate::engine::WebRequest::path_copies)),
    /// from the request's start until the program has been served.
    path_copies: Vec<Emalloc>,
    /// `ZCG(cwd)`: `accel_getcwd`'s string of the working directory,
    /// laid down the first time opcache keys a relative path
    /// (`Z/ext/opcache/ZendAccelerator.c:235`) and kept until the
    /// request ends.
    accel_cwd: Option<PhpString>,
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
    /// `file_handle->buf`: the file's bytes and `ZEND_MMAP_AHEAD` past
    /// them, which `zend_stream_fixup` reads them into as the file is
    /// compiled.
    buffer: Option<Emalloc>,
    /// Its element on `CG(open_files)`, which `open_file_for_scanning`
    /// adds as the file is compiled.
    listed: Option<Emalloc>,
}

impl Request {
    /// What `zend_activate`, the process's startup and the host leave on
    /// a request's heap of state the engine keeps elsewhere, laid down as
    /// the request starts: `EG(main_fiber_context)`; the copy of the
    /// working directory `virtual_cwd_activate` made once
    /// `php_module_startup` was done, which a non-ZTS php frees at the
    /// process's first request's end and never makes again, so a web
    /// host's first request holds it and its later ones stand without it,
    /// where the CLI's and phpdbg's request is its process's one; and the
    /// host's copies of the program's path. A refusal is the memory
    /// limit's fatal, which the first allocation after it meets again.
    /// `sapi/fpm`'s line `a-program-s-first-line-reads-what-opcache-laid-for-it`
    /// holds a child's first request to the copy and its later ones to
    /// none.
    pub(crate) fn lay_start_rooms(&mut self) {
        if self.description.web.is_none() || self.shared.take_startup_cwd() {
            let len = self.cwd.as_os_str().len() + 1;
            self.rooms.cwd_state = Emalloc::new(self, len).ok();
        }
        self.rooms.main_fiber = Emalloc::new(self, FIBER_CONTEXT).ok();
        let len = self.description.script_name.as_ref().map_or(0, Vec::len) + 1;
        for _ in 0..self
            .description
            .web
            .as_ref()
            .map_or(0, |web| web.path_copies)
        {
            let copy = Emalloc::new(self, len).ok();
            self.rooms.path_copies.extend(copy);
        }
    }

    /// The host's copies of the program's path freed, once the program
    /// has been served and before the request shuts down.
    pub(crate) fn free_path_copies(&mut self) {
        for copy in std::mem::take(&mut self.rooms.path_copies) {
            copy.free(self);
        }
    }

    /// `accel_getcwd` as `accel_make_persistent_key` asks it for a
    /// relative path: `ZCG(cwd)` laid down once, the first time. The
    /// `Err` is the memory limit's fatal.
    pub(crate) fn accel_cwd(&mut self) -> Result<(), Thrown> {
        if self.rooms.accel_cwd.is_none() {
            let cwd = self.cwd.as_os_str().as_encoded_bytes().to_vec();
            self.rooms.accel_cwd = Some(self.string_init(&cwd)?);
        }
        Ok(())
    }

    /// The primary file's `zend_file_handle` opened as the host opens it, under the
    /// name it was `written` by, before the program is served.
    pub(crate) fn open_primary_handle(&mut self, written: &[u8]) -> Result<(), Thrown> {
        self.rooms.primary.filename = Some(self.string_init(written)?);
        Ok(())
    }

    /// What the primary file's compile adds to its `zend_file_handle`, a file of
    /// `len` bytes read whole: `zend_stream_fixup`'s buffer and its
    /// element on `CG(open_files)`. Nothing for a request whose
    /// `zend_file_handle` is not open, and nothing where the cache served
    /// the file, since opcache's hit never opens it for scanning. The `Err`
    /// is the memory limit's fatal.
    pub(crate) fn fill_primary_handle(&mut self, len: usize) -> Result<(), Thrown> {
        if self.rooms.primary.filename.is_none() || self.rooms.primary.buffer.is_some() {
            return Ok(());
        }
        self.rooms.primary.buffer = Some(Emalloc::new(self, len + MMAP_AHEAD)?);
        self.rooms.primary.listed = Some(Emalloc::new(self, OPEN_FILES_ELEMENT)?);
        Ok(())
    }

    /// `zend_destroy_file_handle` over the primary file's `zend_file_handle` once the
    /// program has been served, its element off `CG(open_files)` first.
    pub(crate) fn close_primary_handle(&mut self) {
        let primary = std::mem::take(&mut self.rooms.primary);
        if let Some(listed) = primary.listed {
            listed.free(self);
        }
        if let Some(buffer) = primary.buffer {
            buffer.free(self);
        }
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

    /// A debug build's teardown of what [`Request::lay_start_rooms`] and
    /// [`Request::accel_cwd`] laid down, before the request leak sensor:
    /// `accel_deactivate`'s release of `ZCG(cwd)`, `zend_fiber_shutdown`'s
    /// free of `EG(main_fiber_context)` and `virtual_cwd_deactivate`'s of
    /// the working directory's copy.
    #[cfg(debug_assertions)]
    pub(super) fn free_rooms(&mut self) {
        self.close_primary_handle();
        self.free_path_copies();
        let rooms = std::mem::take(&mut self.rooms);
        if let Some(cwd) = rooms.accel_cwd {
            self.release(Value::string(cwd));
        }
        for room in [rooms.main_fiber, rooms.cwd_state].into_iter().flatten() {
            room.free(self);
        }
    }
}
