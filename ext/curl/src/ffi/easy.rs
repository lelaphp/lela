//! The easy transfer: `CURL *` with what the library points into, the
//! trampolines `interface.c` installs, and the MIME tree
//! `build_mime_structure_from_hash` builds.

use std::ffi::{c_char, c_double, c_int, c_void};
use std::marker::PhantomData;
use std::os::fd::RawFd;
use std::ptr::{self, NonNull};

use php_ext::raising::Thrown;
use php_ext::request::{Foreign, ForeignId, Request, Stream};
use php_ext::values::{Owner, PhpObject, Value};

use super::multi::ShareHandle;
use super::sys::{self, curl_mime, curl_mimepart};
use super::{
    CFile, Cx, ERROR_SIZE, Lent, Session, ShareRef, Slist, arg, bytes_of, c_long, c_string, code,
    holding_all, lines_of, slice,
};
use crate::callbacks;
use crate::constants as c;
use crate::file::Source;
use crate::throwables::Pending;

/// An option number as the library's `CURLoption`.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
const fn opt(option: i64) -> sys::CURLoption {
    option as sys::CURLoption
}

/// `CURLE_BAD_FUNCTION_ARGUMENT`, what the library answers a `CURL *` it
/// was not given, and what a `CURL *` its owner gave up answers here.
const GONE: i32 = 43;

/// What a call into the library that may call back answers: the
/// library's code, and what a callback threw during it, which php leaves
/// pending in `EG(exception)` as `zend_call_known_fcc` returns and goes
/// on past.
pub(crate) type Called = (i32, Option<Thrown>);

/// What the library keeps a pointer to from one `CURL *`, freed after
/// the `CURL *` is: its object, the error buffer, the `CURLOPT_STDERR`
/// file, the lists, the MIME trees and the share it points at.
struct Tag {
    /// The `CurlHandle` a callback is served for, `None` once it is being
    /// freed.
    object: Option<PhpObject>,
    /// `CURLOPT_ERRORBUFFER`, held as the bytes the library writes into
    /// it: `char` is `i8` on some targets and `u8` on others, and the
    /// text is bytes on all of them.
    error: [u8; ERROR_SIZE + 1],
    stderr: Option<CFile>,
    slists: Vec<(i64, Slist)>,
    mimes: Vec<Mime>,
    /// The share `CURLOPT_SHARE` points at: a request's, held across a
    /// transfer with the `CURL *`, or a persistent one, counted so the
    /// pool never lends it on while this points at it.
    share: Option<ShareRef>,
    /// Whether `curl_easy_duphandle` copied this `CURL *`, whose copies
    /// point at its lists and its trees' file parts from then on, as
    /// php's `clone` count above one keeps `to_free` whole.
    copied: bool,
    /// Dropped last, after what it keeps.
    session: Session,
}

impl Tag {
    /// The lists and trees let go of: kept by the request's link where a
    /// copy may still point at them, and a tree where no call in progress
    /// lends the request its file parts close through; freed here
    /// otherwise.
    fn let_go(&self, lists: Vec<Slist>, trees: Vec<Mime>) {
        if self.copied {
            self.session.keep(lists, trees);
        } else if !trees.is_empty() && !self.session.entered() {
            self.session.keep(Vec::new(), trees);
        }
    }
}

impl Drop for Tag {
    fn drop(&mut self) {
        let lists = std::mem::take(&mut self.slists)
            .into_iter()
            .map(|(_, list)| list);
        let trees = std::mem::take(&mut self.mimes);
        self.let_go(lists.collect(), trees);
    }
}

/// The `CURLOPT_STDERR` a copy of a `CURL *` that has a file points at:
/// the file opened again as the copy's own, or the process's `stderr`,
/// `None`, where it would not open.
struct Reopened(Option<CFile>);

/// One `CURL *` the request keeps, with its tag.
pub(super) struct Handle {
    raw: NonNull<sys::CURL>,
    tag: NonNull<Tag>,
    /// False for a `CURL *` the library frees itself, a pushed one it was
    /// told to deny.
    owned: bool,
}

impl Drop for Handle {
    fn drop(&mut self) {
        if self.owned {
            // SAFETY: the live handle this value owns, cleaned up once.
            unsafe { sys::curl_easy_cleanup(self.raw.as_ptr()) };
        } else {
            // The library frees it after this, its data pointing nowhere.
            self.owned().unpoint();
        }
        // SAFETY: the tag `Handle::make` leaked, freed once the handle
        // that pointed into it is gone.
        drop(unsafe { Box::from_raw(self.tag.as_ptr()) });
    }
}

// SAFETY: a `CURL *` and its tag are the request's, kept in its table of
// foreign objects (ADR-0013) and reached by the thread serving it alone;
// the request moves between threads only whole (ADR-0005).
unsafe impl Send for Handle {}

/// `CURL *`'s one owner, what a `CurlHandle`'s payload stores: the request
/// keeps the `CURL *`, and [`Easy::close`] gives it up.
pub(crate) struct Easy(Foreign<Handle>);

/// The name of an [`Easy`], copied freely: it reaches the `CURL *` while its
/// owner keeps it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EasyId(ForeignId<Handle>);

/// Which `CURL *` a message or a push names, by the library's pointer,
/// compared and nothing more.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Address(usize);

impl Address {
    pub(super) fn of(raw: *mut sys::CURL) -> Address {
        Address(raw as usize)
    }
}

/// The callbacks whose `*FUNCTION` the `CURL *` points at a trampoline for
/// the length of its life once set, `HANDLE_CURL_OPTION_CALLABLE`'s.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Hook {
    Progress,
    Xferinfo,
    Fnmatch,
    Debug,
    Prereq,
    SshHostkey,
}

/// What one `curl_easy_getinfo` answers, by the type its number masks.
pub(crate) enum Info {
    Str(Option<Vec<u8>>),
    Long(i64),
    Double(f64),
    List(Vec<Vec<u8>>),
    OffT(i64),
    Certs(Vec<Vec<Vec<u8>>>),
}

/// A `CURL *` the library may call back from inside: held for one call
/// with the request lent to the callbacks ([`EasyId::call`]), or owned
/// by value while it is made or dropped ([`Handle::owned`]). It is the
/// only value `curl_easy_setopt` is called through. libcurl logs from
/// inside that call, `infof`'s and `failf`'s lines (`lib/setopt.c`,
/// `lib/easy.c`) reaching the debug callable and the `CURLOPT_STDERR`
/// file, so a set made inside [`EasyId::with`] would serve PHP code a
/// second `&mut Request` while the `&mut Handle` borrowed from the first
/// is live, over a `CURL *` that code could free. `'h` is the span it is
/// reached for.
#[derive(Clone, Copy)]
struct Live<'h> {
    raw: NonNull<sys::CURL>,
    tag: NonNull<Tag>,
    span: PhantomData<&'h mut Handle>,
}

impl Handle {
    fn make(raw: NonNull<sys::CURL>, session: Session, object: PhpObject, owned: bool) -> Handle {
        let tag = Box::into_raw(Box::new(Tag {
            object: Some(object),
            error: [0; ERROR_SIZE + 1],
            stderr: None,
            slists: Vec::new(),
            mimes: Vec::new(),
            share: None,
            copied: false,
            session,
        }));
        // SAFETY: `into_raw` never answers null.
        let tag = unsafe { NonNull::new_unchecked(tag) };
        let mut handle = Handle { raw, tag, owned };
        handle.owned().point();
        handle
    }

    fn raw(&self) -> *mut sys::CURL {
        self.raw.as_ptr()
    }

    fn tag(&self) -> &Tag {
        // SAFETY: a live tag, written only through `tag_mut`, which no
        // borrow of this one outlives.
        unsafe { self.tag.as_ref() }
    }

    fn tag_mut(&mut self) -> &mut Tag {
        // SAFETY: a live tag; the library reads only the error buffer
        // through it, and only inside a call this borrow is not across.
        unsafe { self.tag.as_mut() }
    }

    /// This `CURL *` as [`Live`] while it is owned by value: being made,
    /// before the request keeps it, or being dropped, after. No borrow
    /// of the request's table reaches it and no PHP code can free it, and
    /// only the pointers at its tag are set on it, which libcurl takes
    /// without a line (`setopt_pointers`).
    fn owned(&mut self) -> Live<'_> {
        Live {
            raw: self.raw,
            tag: self.tag,
            span: PhantomData,
        }
    }

    /// For a copy of this `CURL *`, which points at this one's file and
    /// does not keep it: the file opened again as its own, or `None`, the
    /// process's `stderr`, where it would not open; nothing where there
    /// is no file.
    fn stderr_again(&self) -> Option<Reopened> {
        let tag = self.tag();
        tag.stderr
            .as_ref()
            .map(|file| Reopened(file.reopen(&tag.session)))
    }
}

impl Live<'_> {
    fn raw(self) -> *mut sys::CURL {
        self.raw.as_ptr()
    }

    /// The error buffer, the three data pointers and the hooks' data
    /// pointed at this `CURL *`'s tag: what `_php_curl_set_default_options`
    /// and `_php_setup_easy_copy_handlers` point, and what a copy's options
    /// must stop pointing at the original's.
    fn point(self) {
        let tag = arg(self.tag);
        // SAFETY: a live tag, whose array field is borrowed for the address.
        let error = unsafe { (*self.tag.as_ptr()).error.as_mut_ptr() };
        self.set_ptr(sys::CURLOPT_ERRORBUFFER.into(), error.cast());
        self.point_data(tag);
    }

    /// Every data pointer at `data`.
    fn point_data(self, data: *mut c_void) {
        for option in [
            c::CURLOPT_FILE,
            c::CURLOPT_INFILE,
            c::CURLOPT_WRITEHEADER,
            sys::CURLOPT_PROGRESSDATA.into(),
            sys::CURLOPT_FNMATCH_DATA.into(),
            sys::CURLOPT_DEBUGDATA.into(),
            sys::CURLOPT_PREREQDATA.into(),
            sys::CURLOPT_SSH_HOSTKEYDATA.into(),
        ] {
            self.set_ptr(option, data);
        }
    }

    /// The error buffer, the `CURLOPT_STDERR` file and every data pointer
    /// off the tag, for a pushed `CURL *` the library frees once the tag
    /// is gone.
    fn unpoint(self) {
        self.set_ptr(sys::CURLOPT_ERRORBUFFER.into(), ptr::null_mut());
        self.set_ptr(c::CURLOPT_STDERR, ptr::null_mut());
        self.point_data(ptr::null_mut());
    }

    fn set_ptr(self, option: i64, value: *mut c_void) -> i32 {
        // SAFETY: a live handle and an option the library reads as a
        // pointer, which outlives the handle or is null.
        code(unsafe { sys::curl_easy_setopt(self.raw(), opt(option), value) })
    }

    fn set_long(self, option: i64, value: i64) -> i32 {
        // SAFETY: a live handle and an option the library reads as a
        // `long`.
        code(unsafe { sys::curl_easy_setopt(self.raw(), opt(option), c_long(value)) })
    }

    fn set_off(self, option: i64, value: i64) -> i32 {
        // SAFETY: a live handle and an option the library reads as a
        // `curl_off_t`.
        code(unsafe { sys::curl_easy_setopt(self.raw(), opt(option), value) })
    }

    fn set_str(self, option: i64, value: Option<&[u8]>) -> i32 {
        let value = value.map(c_string);
        let raw = value.as_ref().map_or(ptr::null(), |value| value.as_ptr());
        // SAFETY: a live handle, an option the library copies a string of,
        // and a NUL-terminated string that outlives the call, or null.
        code(unsafe { sys::curl_easy_setopt(self.raw(), opt(option), raw) })
    }

    /// A blob option under `CURL_BLOB_COPY`.
    fn set_blob(self, option: i64, value: &[u8]) -> i32 {
        let mut blob = sys::curl_blob {
            data: value.as_ptr().cast_mut().cast(),
            len: value.len(),
            flags: 1,
        };
        // SAFETY: a live handle and a blob the library copies before the
        // call returns.
        code(unsafe { sys::curl_easy_setopt(self.raw(), opt(option), &raw mut blob) })
    }

    /// `CURLOPT_POSTFIELDSIZE` then `CURLOPT_COPYPOSTFIELDS`.
    fn set_post_fields(self, body: &[u8]) -> i32 {
        let size = i64::try_from(body.len()).unwrap_or(i64::MAX);
        self.set_long(sys::CURLOPT_POSTFIELDSIZE.into(), size);
        // SAFETY: a live handle and `len` bytes the library copies; the
        // size set first says how many.
        code(unsafe {
            sys::curl_easy_setopt(
                self.raw(),
                sys::CURLOPT_COPYPOSTFIELDS,
                body.as_ptr().cast::<c_char>(),
            )
        })
    }

    /// `CURLOPT_DEBUGFUNCTION` at `curl_debug`, or at nothing.
    fn set_debug(self, on: bool) {
        let f: Option<sys::curl_debug_callback> = if on { Some(debug) } else { None };
        // SAFETY: a live handle and the trampoline, or null.
        unsafe { sys::curl_easy_setopt(self.raw(), opt(c::CURLOPT_DEBUGFUNCTION), f) };
        let data = if on { arg(self.tag) } else { ptr::null_mut() };
        self.set_ptr(sys::CURLOPT_DEBUGDATA.into(), data);
    }

    /// A hook's `*FUNCTION` at its trampoline and its data at the tag.
    fn set_hook(self, hook: Hook) {
        let raw = self.raw();
        match hook {
            Hook::Progress => {
                let f: sys::curl_progress_callback = progress;
                // SAFETY: a live handle and the trampoline for the option.
                unsafe { sys::curl_easy_setopt(raw, opt(c::CURLOPT_PROGRESSFUNCTION), f) };
            }
            Hook::Xferinfo => {
                let f: sys::curl_xferinfo_callback = xferinfo;
                // SAFETY: as above.
                unsafe { sys::curl_easy_setopt(raw, opt(c::CURLOPT_XFERINFOFUNCTION), f) };
            }
            Hook::Fnmatch => {
                let f: sys::curl_fnmatch_callback = fnmatch;
                // SAFETY: as above.
                unsafe { sys::curl_easy_setopt(raw, opt(c::CURLOPT_FNMATCH_FUNCTION), f) };
            }
            Hook::Debug => self.set_debug(true),
            Hook::Prereq => {
                let f: sys::curl_prereq_callback = prereq;
                // SAFETY: as above.
                unsafe { sys::curl_easy_setopt(raw, opt(c::CURLOPT_PREREQFUNCTION), f) };
            }
            Hook::SshHostkey => {
                let f: sys::curl_sshhostkeycallback = ssh_hostkey;
                // SAFETY: as above.
                unsafe { sys::curl_easy_setopt(raw, opt(c::CURLOPT_SSH_HOSTKEYFUNCTION), f) };
            }
        }
        self.point();
    }

    /// `_php_curl_set_default_options`' sets.
    fn defaults(self, cainfo: Option<&[u8]>) {
        self.set_long(c::CURLOPT_NOPROGRESS, 1);
        self.set_long(c::CURLOPT_VERBOSE, 0);
        self.point();
        let raw = self.raw();
        let write: sys::curl_write_callback = write_body;
        let read: sys::curl_read_callback = read_body;
        let header: sys::curl_write_callback = write_header;
        // SAFETY: a live handle and the trampolines, which take the tag
        // `point` set as their data.
        unsafe { sys::curl_easy_setopt(raw, opt(c::CURLOPT_WRITEFUNCTION), write) };
        // SAFETY: as above.
        unsafe { sys::curl_easy_setopt(raw, opt(c::CURLOPT_READFUNCTION), read) };
        // SAFETY: as above.
        unsafe { sys::curl_easy_setopt(raw, opt(c::CURLOPT_HEADERFUNCTION), header) };
        self.set_long(c::CURLOPT_DNS_CACHE_TIMEOUT, 120);
        self.set_long(c::CURLOPT_MAXREDIRS, 20);
        if let Some(cainfo) = cainfo {
            self.set_str(c::CURLOPT_CAINFO, Some(cainfo));
        }
        self.set_long(c::CURLOPT_NOSIGNAL, 1);
    }
}

impl Easy {
    /// `curl_easy_init()` for `object`, kept by the request; `None` where
    /// the library made none.
    pub(crate) fn init(req: &mut Request, session: Session, object: PhpObject) -> Option<Easy> {
        // SAFETY: no argument.
        let raw = NonNull::new(unsafe { sys::curl_easy_init() })?;
        Some(Easy(
            req.memory_mut()
                .adopt(Handle::make(raw, session, object, true)),
        ))
    }

    /// A pushed `CURL *` the library made, served for `object`, kept by
    /// the request; not owned until the push is accepted
    /// ([`EasyId::accept`]).
    pub(super) fn adopt(
        req: &mut Request,
        raw: *mut sys::CURL,
        session: Session,
        object: PhpObject,
    ) -> Option<Easy> {
        let raw = NonNull::new(raw)?;
        Some(Easy(
            req.memory_mut()
                .adopt(Handle::make(raw, session, object, false)),
        ))
    }

    /// Its name.
    pub(crate) fn id(&self) -> EasyId {
        EasyId(self.0.id())
    }

    /// The `CURL *` given up, its object no longer served: cleaned up now
    /// with the request reachable, so what the library frees with it
    /// calls back where php's would, or as the call that holds it ends.
    pub(crate) fn close(self, req: &mut Request) -> Option<Thrown> {
        let id = self.id();
        let session = id.with(req, None, |handle| {
            let tag = handle.tag_mut();
            tag.object = None;
            Some(tag.session.clone())
        })?;
        let handle = req.memory_mut().disown(self.0)?;
        let ((), thrown) = session.enter(req, || drop(handle));
        thrown
    }
}

impl EasyId {
    /// `f` over the `CURL *` while its owner keeps it, `gone` otherwise.
    fn with<R>(self, req: &mut Request, gone: R, f: impl FnOnce(&mut Handle) -> R) -> R {
        req.memory_mut().foreign(self.0).map_or(gone, f)
    }

    /// Which `CURL *` this is.
    pub(crate) fn address(self, req: &mut Request) -> Option<Address> {
        self.with(req, None, |handle| Some(Address::of(handle.raw())))
    }

    /// The library's pointer, for a call that holds it: resolved inside
    /// the hold (`MultiId::call`), so it stays live to
    /// the call's end whatever PHP code the call serves.
    pub(super) fn raw(self, req: &mut Request) -> Option<NonNull<sys::CURL>> {
        self.with(req, None, |handle| Some(handle.raw))
    }

    /// What a transfer over `easies` holds: each `CURL *`, and each
    /// request's share one points at.
    pub(super) fn held_by(
        req: &mut Request,
        easies: &[EasyId],
    ) -> (Vec<ForeignId<Handle>>, Vec<ForeignId<ShareHandle>>) {
        let mut shares = Vec::new();
        for easy in easies {
            if let Some(Some(ShareRef::Request(id))) = req
                .memory_mut()
                .foreign(easy.0)
                .map(|handle| handle.tag().share.clone())
            {
                shares.push(id.0);
            }
        }
        (easies.iter().map(|easy| easy.0).collect(), shares)
    }

    /// `curl_easy_duphandle(ch->cp)` for `object`, kept by the request.
    /// The copy points at this `CURL *`'s lists and its trees' file parts,
    /// which this one keeps from here to the request's end
    /// ([`Tag::let_go`]); its `CURLOPT_STDERR` file is its own.
    pub(crate) fn duplicate(self, req: &mut Request, object: PhpObject) -> Option<Easy> {
        let (raw, session, stderr) = self.with(req, None, |handle| {
            // SAFETY: a live handle.
            let raw = NonNull::new(unsafe { sys::curl_easy_duphandle(handle.raw()) })?;
            handle.tag_mut().copied = true;
            Some((raw, handle.tag().session.clone(), handle.stderr_again()))
        })?;
        let copy = Easy(
            req.memory_mut()
                .adopt(Handle::make(raw, session, object, true)),
        );
        if let Some(Reopened(stderr)) = stderr {
            copy.id().point_stderr(req, stderr);
        }
        Some(copy)
    }

    /// A pushed `CURL *`, which the library made a copy of `parent`'s,
    /// given a `CURLOPT_STDERR` file of its own, as a copy is.
    pub(crate) fn stderr_from(self, req: &mut Request, parent: EasyId) {
        if let Some(Reopened(stderr)) = parent.with(req, None, |handle| handle.stderr_again()) {
            self.point_stderr(req, stderr);
        }
    }

    /// The pushed `CURL *` accepted: the multi owns it from here, as any.
    pub(crate) fn accept(self, req: &mut Request) {
        self.with(req, (), |handle| handle.owned = true);
    }

    /// `curl_easy_duphandle`'s copy made by the library for a push: the
    /// pushed `CURL *` points at this one's lists and file parts, as a copy
    /// does.
    pub(crate) fn copied(self, req: &mut Request) {
        self.with(req, (), |handle| handle.tag_mut().copied = true);
    }

    /// `_php_curl_set_default_options`: the trampolines installed, the
    /// defaults php sets, `cainfo` where one is configured, and
    /// `CURLOPT_NOSIGNAL` as php's ZTS build sets it, since requests here
    /// are served on threads.
    pub(crate) fn defaults(self, req: &mut Request, cainfo: Option<&[u8]>) -> Option<Thrown> {
        self.call(req, (), |live| live.defaults(cainfo)).1
    }

    /// `curl_easy_setopt(ch->cp, option, (long) value)`.
    pub(crate) fn set_long(self, req: &mut Request, option: i64, value: i64) -> Called {
        self.call(req, GONE, |live| live.set_long(option, value))
    }

    /// `curl_easy_setopt(ch->cp, option, (curl_off_t) value)`.
    pub(crate) fn set_off(self, req: &mut Request, option: i64, value: i64) -> Called {
        self.call(req, GONE, |live| live.set_off(option, value))
    }

    /// `curl_easy_setopt(ch->cp, option, str)`, `NULL` for `None`; the
    /// library copies the string.
    pub(crate) fn set_str(self, req: &mut Request, option: i64, value: Option<&[u8]>) -> Called {
        self.call(req, GONE, |live| live.set_str(option, value))
    }

    /// A blob option under `CURL_BLOB_COPY`.
    pub(crate) fn set_blob(self, req: &mut Request, option: i64, value: &[u8]) -> Called {
        self.call(req, GONE, |live| live.set_blob(option, value))
    }

    /// `CURLOPT_POSTFIELDSIZE` then `CURLOPT_COPYPOSTFIELDS`.
    pub(crate) fn set_post_fields(self, req: &mut Request, body: &[u8]) -> Called {
        self.call(req, GONE, |live| live.set_post_fields(body))
    }

    /// A list option: the list the library is pointed at, held by the
    /// `CURL *` in place of the one the option held, which is let go of
    /// once the call is over ([`Tag::let_go`]).
    pub(crate) fn set_slist(self, req: &mut Request, option: i64, list: Option<Slist>) -> Called {
        let Some(session) = self.with(req, None, |handle| Some(handle.tag().session.clone()))
        else {
            return (GONE, None);
        };
        let raw = list
            .as_ref()
            .map_or(ptr::null_mut(), |list| list.0.as_ptr());
        let (code, thrown) = self.call(req, GONE, |live| live.set_ptr(option, raw.cast()));
        let Some(handle) = req.memory_mut().foreign(self.0) else {
            // Cleaned up as the call's hold ended; a copy made meanwhile
            // may still point at the list.
            session.keep(list.into_iter().collect(), Vec::new());
            return (code, thrown);
        };
        let tag = handle.tag_mut();
        let (old, kept): (Vec<_>, Vec<_>) = std::mem::take(&mut tag.slists)
            .into_iter()
            .partition(|(held, _)| *held == option);
        tag.slists = kept;
        if let Some(list) = list {
            tag.slists.push((option, list));
        }
        let old = old.into_iter().map(|(_, list)| list).collect();
        tag.let_go(old, Vec::new());
        (code, thrown)
    }

    /// `CURLOPT_STDERR`: the file the library writes its verbose lines
    /// to, what php's cast makes of the stream and the fd
    /// [`Cast::Stdio`](php_ext::request::Cast::Stdio) answered for it
    /// ([`CFile::of`]), the process's `stderr` for `None`.
    pub(crate) fn set_stderr(
        self,
        req: &mut Request,
        to: Option<(Stream, Option<RawFd>)>,
    ) -> Called {
        let Some(session) = self.with(req, None, |handle| Some(handle.tag().session.clone()))
        else {
            return (GONE, None);
        };
        let file = to.and_then(|(stream, fd)| CFile::of(&session, stream, fd));
        self.point_stderr(req, file)
    }

    /// `CURLOPT_STDERR` at `file`, held in place of the one it held,
    /// which is let go of once the call is over, as
    /// [`Session::let_file_go`] says.
    fn point_stderr(self, req: &mut Request, file: Option<CFile>) -> Called {
        let raw = file.as_ref().map_or(ptr::null_mut(), CFile::as_ptr);
        let (code, thrown) = self.call(req, GONE, |live| {
            live.set_ptr(c::CURLOPT_STDERR, raw.cast())
        });
        if let Some(handle) = req.memory_mut().foreign(self.0) {
            let tag = handle.tag_mut();
            if let Some(old) = std::mem::replace(&mut tag.stderr, file) {
                tag.session.let_file_go(old);
            }
        }
        (code, thrown)
    }

    /// The `CURLOPT_STDERR` stream's value let go of as
    /// [`Session::let_stream_go`] says, or now where the `CURL *` is gone.
    pub(crate) fn let_stream_go(self, req: &mut Request, value: Owner<Value>) {
        match self.with(req, None, |handle| Some(handle.tag().session.clone())) {
            Some(session) => session.let_stream_go(req, value),
            None => value.give_up(req),
        }
    }

    /// The `CURLOPT_STDERR` file, flushed, as `php_stream_flush` flushes
    /// the stream `curl_exec` names.
    pub(crate) fn flush_stderr(self, req: &mut Request) {
        self.with(req, (), |handle| {
            if let Some(file) = &handle.tag().stderr {
                file.flush();
            }
        });
    }

    /// `CURLOPT_SHARE` at `share`, which the `CURL *` holds across a
    /// transfer, or counts, while it points at it.
    pub(crate) fn set_share(self, req: &mut Request, share: ShareRef) -> Called {
        let raw = match &share {
            ShareRef::Request(id) => id.raw(req),
            ShareRef::Persistent(persistent) => Some(persistent.raw()),
        };
        let Some(raw) = raw else {
            return (GONE, None);
        };
        let called = self.call(req, GONE, |live| live.set_ptr(c::CURLOPT_SHARE, raw.cast()));
        if let Some(handle) = req.memory_mut().foreign(self.0) {
            handle.tag_mut().share = Some(share);
        }
        called
    }

    /// A hook's `*FUNCTION` pointed at its trampoline and its data at the
    /// tag, as `HANDLE_CURL_OPTION_CALLABLE` does whatever the callable.
    pub(crate) fn set_hook(self, req: &mut Request, hook: Hook) -> Option<Thrown> {
        self.call(req, (), |live| live.set_hook(hook)).1
    }

    /// `CURLOPT_DEBUGFUNCTION` at `curl_debug`, or at nothing.
    pub(crate) fn set_debug(self, req: &mut Request, on: bool) -> Option<Thrown> {
        self.call(req, (), |live| live.set_debug(on)).1
    }

    /// `f`, a call into the library that may call back, over the `CURL *`
    /// as [`Live`]: held with the share it points at ([`Memory::holding`]),
    /// so a callback that frees either's object leaves the cleanup to the
    /// call's end, with no request lent ([`Session::holding`]), and the
    /// request lent to the callbacks meanwhile ([`Session::enter`]), what
    /// one threw handed back beside `f`'s answer, `gone` where the owner
    /// gave the `CURL *` up. The one way the binding calls the library
    /// with a `CURL *` that can call back.
    fn call<R>(
        self,
        req: &mut Request,
        gone: R,
        f: impl for<'h> FnOnce(Live<'h>) -> R,
    ) -> (R, Option<Thrown>) {
        let Some((session, share)) = self.with(req, None, |handle| {
            let share = match handle.tag().share {
                Some(ShareRef::Request(id)) => Some(id.0),
                _ => None,
            };
            Some((handle.tag().session.clone(), share))
        }) else {
            return (gone, None);
        };
        let held = session.holding(req, self.0, |req, held| {
            let handle = held.get(req)?;
            let live = Live {
                raw: handle.raw,
                tag: handle.tag,
                span: PhantomData,
            };
            let mut body = Some(|req: &mut Request| session.enter(req, || f(live)));
            let ids: &[ForeignId<ShareHandle>] = share.as_slice();
            holding_all(req, ids, &mut body)
        });
        match held.flatten() {
            Some(called) => called,
            None => (gone, None),
        }
    }

    /// `curl_easy_perform`, the request reachable by the callbacks. The
    /// transfer's waits are libcurl's and lend nothing (#2595, `ffi`'s
    /// *Waits*).
    pub(crate) fn perform(self, req: &mut Request) -> Called {
        self.call(req, GONE, |live| {
            // SAFETY: a handle held for the call; its callbacks find the
            // request `enter` leaves.
            code(unsafe { sys::curl_easy_perform(live.raw()) })
        })
    }

    /// `curl_easy_pause`, which may deliver what was held back.
    pub(crate) fn pause(self, req: &mut Request, bitmask: i64) -> Called {
        #[allow(clippy::cast_possible_truncation)]
        let bitmask = bitmask as c_int;
        self.call(req, GONE, |live| {
            // SAFETY: as in `perform`.
            code(unsafe { sys::curl_easy_pause(live.raw(), bitmask) })
        })
    }

    /// `curl_easy_upkeep`.
    pub(crate) fn upkeep(self, req: &mut Request) -> Called {
        self.call(req, GONE, |live| {
            // SAFETY: as in `perform`.
            code(unsafe { sys::curl_easy_upkeep(live.raw()) })
        })
    }

    /// `curl_easy_reset`, the defaults to be laid again by the caller.
    /// The library frees its copy of a MIME tree there, whose file parts'
    /// `free_cb` closes what they opened.
    pub(crate) fn reset(self, req: &mut Request) -> Option<Thrown> {
        // SAFETY: a handle held for the call.
        let ((), thrown) = self.call(req, (), |live| unsafe { sys::curl_easy_reset(live.raw()) });
        // The library writes to `stderr` again, and a copy made from here
        // on must not open the file again.
        if let Some(handle) = req.memory_mut().foreign(self.0) {
            let tag = handle.tag_mut();
            if let Some(old) = tag.stderr.take() {
                tag.session.let_file_go(old);
            }
        }
        thrown
    }

    /// The error buffer's text up to its NUL.
    pub(crate) fn error_text(self, req: &mut Request) -> Vec<u8> {
        self.with(req, Vec::new(), |handle| {
            let error = &handle.tag().error;
            let end = error.iter().position(|b| *b == 0).unwrap_or(ERROR_SIZE);
            error[..end].to_vec()
        })
    }

    /// `memset(ch->err.str, 0, CURL_ERROR_SIZE + 1)`.
    pub(crate) fn clear_error(self, req: &mut Request) {
        self.with(req, (), |handle| {
            handle.tag_mut().error = [0; ERROR_SIZE + 1];
        });
    }

    /// `curl_easy_escape`.
    pub(crate) fn escape(self, req: &mut Request, text: &[u8]) -> Option<Vec<u8>> {
        let len = c_int::try_from(text.len()).ok()?;
        self.with(req, None, |handle| {
            // SAFETY: a live handle and `len` bytes at `text`.
            let out = unsafe { sys::curl_easy_escape(handle.raw(), text.as_ptr().cast(), len) };
            let escaped = bytes_of(out)?;
            // SAFETY: the string the library allocated for this call.
            unsafe { sys::curl_free(out.cast()) };
            Some(escaped)
        })
    }

    /// `curl_easy_unescape`.
    pub(crate) fn unescape(self, req: &mut Request, text: &[u8]) -> Option<Vec<u8>> {
        let len = c_int::try_from(text.len()).ok()?;
        self.with(req, None, |handle| {
            let mut out_len: c_int = 0;
            // SAFETY: a live handle, `len` bytes at `text` and a live out-length.
            let out = unsafe {
                sys::curl_easy_unescape(handle.raw(), text.as_ptr().cast(), len, &raw mut out_len)
            };
            if out.is_null() {
                return None;
            }
            let bytes = slice(out, usize::try_from(out_len).unwrap_or(0)).to_vec();
            // SAFETY: the string the library allocated for this call.
            unsafe { sys::curl_free(out.cast()) };
            Some(bytes)
        })
    }

    /// `curl_easy_getinfo(ch->cp, info, &out)` read as the type the
    /// number masks, `None` where the library refused.
    pub(crate) fn info(self, req: &mut Request, info: i64) -> Option<Info> {
        self.with(req, None, |handle| info_of(handle.raw(), info))
    }

    /// `curl_mime_init(ch->cp)` filled by `fill`, then, where `fill`
    /// answers that the walk reached its end with no error, set in place
    /// of the trees the `CURL *` held ([`EasyId::set_mime`]), and freed
    /// otherwise (`out_mime`). Each free runs under what stands in
    /// `pending` ([`Mime::let_go`]), and what a free or the set threw is
    /// left there. The tree never leaves this call, so no way out of
    /// `fill` leaves it standing. `None` where the library made no tree.
    pub(crate) fn post_form(
        self,
        req: &mut Request,
        pending: &mut Pending,
        fill: impl FnOnce(&mut Request, &mut MimeBuilder, &mut Pending) -> bool,
    ) -> Option<Posted> {
        let mut built = self.mime(req)?;
        if fill(req, &mut built, pending) {
            return Some(self.set_mime(req, built.mime, pending));
        }
        built.mime.let_go(req, pending);
        Some(Posted::Freed)
    }

    /// The trees the `CURL *` held let go of, then `CURLOPT_MIMEPOST`
    /// pointed at `mime`, which the `CURL *` holds in their place: php's
    /// `zend_llist_clean(&ch->to_free->post)` before the set, skipped
    /// where a copy may still point at them (`(*ch->clone) == 1`), whose
    /// trees are kept to the link's end. The frees and the set are made under
    /// what stands in `pending`, an `exit` among it as php's unwind exit
    /// stands; where a free leaves a fatal there, the set is not made and
    /// `mime` is freed, as `zend_bailout` leaves before it.
    fn set_mime(self, req: &mut Request, mime: Mime, pending: &mut Pending) -> Posted {
        let Some(handle) = req.memory_mut().foreign(self.0) else {
            mime.let_go(req, pending);
            return Posted::Set(GONE);
        };
        let tag = handle.tag_mut();
        let mut old = std::mem::take(&mut tag.mimes);
        if tag.copied {
            tag.session.keep(Vec::new(), std::mem::take(&mut old));
        }
        for tree in old {
            tree.let_go(req, pending);
        }
        let tree = mime.raw.as_ptr();
        // Replacing the tree a copy of a `CURL *` points at frees the
        // library's copy of it, whose parts call back.
        let set = pending.step(req, |req, _| {
            self.call(req, GONE, |live| {
                live.set_ptr(sys::CURLOPT_MIMEPOST.into(), tree.cast())
            })
        });
        let Some((code, thrown)) = set else {
            mime.let_go(req, pending);
            return Posted::Freed;
        };
        if let Some(thrown) = thrown {
            pending.park(req, thrown);
        }
        match req.memory_mut().foreign(self.0) {
            Some(handle) => handle.tag_mut().mimes.push(mime),
            None => mime.let_go(req, pending),
        }
        Posted::Set(code)
    }

    /// `curl_mime_init(ch->cp)`, `None` where the library made none.
    fn mime(self, req: &mut Request) -> Option<MimeBuilder> {
        self.with(req, None, |handle| {
            // SAFETY: a live handle.
            let raw = NonNull::new(unsafe { sys::curl_mime_init(handle.raw()) })?;
            Some(MimeBuilder {
                mime: Mime {
                    raw,
                    files: Vec::new(),
                    link: handle.tag().session.lent(),
                },
                session: handle.tag().session.clone(),
            })
        })
    }
}

/// `curl_easy_getinfo(raw, info, &out)` read as the type the number
/// masks.
fn info_of(raw: *mut sys::CURL, info: i64) -> Option<Info> {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let code = info as sys::CURLINFO;
    if info == c::CURLINFO_CERTINFO {
        let mut certs: *mut sys::curl_certinfo = ptr::null_mut();
        // SAFETY: a live handle and an out-pointer of the info's type.
        let rc = unsafe { sys::curl_easy_getinfo(raw, code, &raw mut certs) };
        return (rc == sys::CURLE_OK).then(|| Info::Certs(certs_of(certs)));
    }
    match code & sys::CURLINFO_TYPEMASK {
        sys::CURLINFO_STRING => {
            let mut out: *const c_char = ptr::null();
            // SAFETY: as above.
            let rc = unsafe { sys::curl_easy_getinfo(raw, code, &raw mut out) };
            (rc == sys::CURLE_OK).then(|| Info::Str(bytes_of(out)))
        }
        sys::CURLINFO_LONG => {
            let mut out: c_long = 0;
            // SAFETY: as above.
            let rc = unsafe { sys::curl_easy_getinfo(raw, code, &raw mut out) };
            #[allow(clippy::useless_conversion)]
            (rc == sys::CURLE_OK).then(|| Info::Long(i64::from(out)))
        }
        sys::CURLINFO_DOUBLE => {
            let mut out: c_double = 0.0;
            // SAFETY: as above.
            let rc = unsafe { sys::curl_easy_getinfo(raw, code, &raw mut out) };
            (rc == sys::CURLE_OK).then_some(Info::Double(out))
        }
        sys::CURLINFO_SLIST => {
            let mut out: *mut sys::curl_slist = ptr::null_mut();
            // SAFETY: as above.
            let rc = unsafe { sys::curl_easy_getinfo(raw, code, &raw mut out) };
            if rc != sys::CURLE_OK {
                return None;
            }
            let lines = lines_of(out);
            // SAFETY: a list the library made for the caller to free.
            unsafe { sys::curl_slist_free_all(out) };
            Some(Info::List(lines))
        }
        sys::CURLINFO_OFF_T => {
            let mut out: sys::curl_off_t = 0;
            // SAFETY: as above.
            let rc = unsafe { sys::curl_easy_getinfo(raw, code, &raw mut out) };
            (rc == sys::CURLE_OK).then_some(Info::OffT(out))
        }
        _ => None,
    }
}

/// The certificates of `CURLINFO_CERTINFO`, each its lines.
fn certs_of(certs: *mut sys::curl_certinfo) -> Vec<Vec<Vec<u8>>> {
    // SAFETY: null, or the library's structure, live until its next call.
    let Some(certs) = (unsafe { certs.as_ref() }) else {
        return Vec::new();
    };
    let count = usize::try_from(certs.num_of_certs).unwrap_or(0);
    (0..count)
        .map(|i| {
            // SAFETY: `num_of_certs` lists at `certinfo`.
            let at = unsafe { certs.certinfo.add(i) };
            // SAFETY: as above, one of them.
            let list = unsafe { *at };
            lines_of(list)
        })
        .collect()
}

/// A MIME tree: `curl_mime *` and the arguments of its file parts'
/// callbacks, which live as long as it does.
pub(crate) struct Mime {
    raw: NonNull<curl_mime>,
    files: Vec<NonNull<FilePart>>,
    /// The request's link, which a [`Session`] holding the tree keeps.
    link: Lent,
}

// SAFETY: as for `Session`: one request's, held by one `CURL *`, by a
// builder, or by the request's link.
unsafe impl Send for Mime {}

impl Mime {
    /// The tree freed with the request reachable, so its file parts'
    /// `free_cb` closes what they opened.
    fn free(self, req: &mut Request) -> Option<Thrown> {
        let link = self.link;
        let ((), thrown) = link.enter(req, || drop(self));
        thrown
    }

    /// The tree freed ([`Mime::free`]) as php frees one, under what
    /// stands in `pending` ([`Pending::release`]), so a file part's
    /// `free_cb` closing a stream through a wrapper calls nothing while a
    /// throwable stands, as `zend_call_function` returns at once under
    /// `EG(exception)`. What a callback the free served threw is left
    /// pending. A wrapper's `stream_close` that throws is not among that:
    /// the stream layer keeps it and raises it as the native returns, so
    /// it does not yet stand over the steps after the free (#3708).
    fn let_go(self, req: &mut Request, pending: &mut Pending) {
        let mut tree = Some(self);
        let mut threw = None;
        pending.release(req, |req| {
            if let Some(tree) = tree.take() {
                threw = tree.free(req);
            }
        });
        if let Some(thrown) = threw {
            pending.park(req, thrown);
        }
    }

    /// Each file part's source closed, as its `free_cb` closes it, the
    /// tree left standing: a kept tree's, at the request's end.
    pub(super) fn close_files(&self, req: &mut Request) {
        for file in &self.files {
            // SAFETY: an argument `add_file` leaked, live while its tree
            // is; no callback of the part runs during this close, the
            // request serving no transfer.
            let source = unsafe { &mut (*file.as_ptr()).source };
            source.close(req);
        }
    }
}

impl Drop for Mime {
    fn drop(&mut self) {
        // SAFETY: the tree this value owns, freed once; its file parts'
        // `free_cb` is served before the arguments below are.
        unsafe { sys::curl_mime_free(self.raw.as_ptr()) };
        for file in self.files.drain(..) {
            // SAFETY: an argument `add_file` leaked, freed once the tree
            // that pointed at it is gone.
            drop(unsafe { Box::from_raw(file.as_ptr()) });
        }
    }
}

/// `struct mime_data_cb_arg`: the file a part reads, with the request's
/// link, named and not held: the tree that holds the part is held by an
/// owner of the link.
struct FilePart {
    link: Lent,
    source: Source,
}

/// Where [`EasyId::post_form`] left the tree it built.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Posted {
    /// Pointed at by `CURLOPT_MIMEPOST`, with the library's code.
    Set(i32),
    /// Freed, the form given up.
    Freed,
}

/// A MIME tree being built, lent to the fill of
/// [`EasyId::post_form`] and to nothing else.
pub(crate) struct MimeBuilder {
    mime: Mime,
    session: Session,
}

impl MimeBuilder {
    fn part(&self) -> Option<NonNull<curl_mimepart>> {
        // SAFETY: a live tree.
        NonNull::new(unsafe { sys::curl_mime_addpart(self.mime.raw.as_ptr()) })
    }

    /// `add_simple_field`: a part named `name` holding `data`.
    pub(crate) fn add_data(&mut self, name: &[u8], data: &[u8]) -> i32 {
        match self.part() {
            Some(part) => code(named_data(part, name, data)),
            None => code(sys::CURLE_OUT_OF_MEMORY),
        }
    }

    /// A `CURLStringFile` part: its data, file name and type.
    pub(crate) fn add_string_file(
        &mut self,
        name: &[u8],
        data: &[u8],
        filename: &[u8],
        mime: &[u8],
    ) -> Option<i32> {
        let part = self.part()?;
        let rc = named_data(part, name, data);
        Some(code(if rc == sys::CURLE_OK {
            labels(part, filename, mime)
        } else {
            rc
        }))
    }

    /// A `CURLFile` part: read from `source` by the callbacks, of `size`
    /// bytes where it is known, seekable where its stream opened.
    /// A part not made, or not named, closes `source` here, as no
    /// `free_cb` will.
    pub(crate) fn add_file(
        &mut self,
        req: &mut Request,
        name: &[u8],
        mut source: Source,
        size: Option<i64>,
        filename: &[u8],
        mime: &[u8],
    ) -> Option<i32> {
        let seekable = source.is_open();
        let Some(part) = self.part() else {
            source.close(req);
            return None;
        };
        let name = c_string(name);
        // SAFETY: a live part and a NUL-terminated name the library copies.
        let rc = unsafe { sys::curl_mime_name(part.as_ptr(), name.as_ptr()) };
        if rc != sys::CURLE_OK {
            source.close(req);
            return Some(code(rc));
        }
        let file = Box::into_raw(Box::new(FilePart {
            link: self.session.lent(),
            source,
        }));
        // SAFETY: `into_raw` never answers null.
        let file = unsafe { NonNull::new_unchecked(file) };
        self.mime.files.push(file);
        let seek: Option<sys::curl_seek_callback> = if seekable { Some(file_seek) } else { None };
        // SAFETY: a live part, the trampolines and their argument, which
        // the tree holds until after `curl_mime_free`.
        let rc = unsafe {
            sys::curl_mime_data_cb(
                part.as_ptr(),
                size.unwrap_or(-1),
                Some(file_read),
                seek,
                Some(file_free),
                arg(file),
            )
        };
        Some(code(if rc == sys::CURLE_OK {
            labels(part, filename, mime)
        } else {
            rc
        }))
    }
}

/// `curl_mime_name` then `curl_mime_data`.
fn named_data(part: NonNull<curl_mimepart>, name: &[u8], data: &[u8]) -> sys::CURLcode {
    let name = c_string(name);
    // SAFETY: a live part and a NUL-terminated name the library copies.
    let rc = unsafe { sys::curl_mime_name(part.as_ptr(), name.as_ptr()) };
    if rc != sys::CURLE_OK {
        return rc;
    }
    // SAFETY: a live part and `len` bytes the library copies.
    unsafe { sys::curl_mime_data(part.as_ptr(), data.as_ptr().cast(), data.len()) }
}

/// `curl_mime_filename` then `curl_mime_type`.
fn labels(part: NonNull<curl_mimepart>, filename: &[u8], mime: &[u8]) -> sys::CURLcode {
    let filename = c_string(filename);
    let mime = c_string(mime);
    // SAFETY: a live part and a NUL-terminated name the library copies.
    let rc = unsafe { sys::curl_mime_filename(part.as_ptr(), filename.as_ptr()) };
    if rc != sys::CURLE_OK {
        return rc;
    }
    // SAFETY: as above.
    unsafe { sys::curl_mime_type(part.as_ptr(), mime.as_ptr()) }
}

/// The tag a trampoline was handed, with its object, served through
/// the request of the call in progress.
fn served<T>(
    tag: *mut c_void,
    idle: T,
    abort: T,
    f: impl FnOnce(&mut Cx<'_>, PhpObject) -> T,
) -> T {
    let tag: *mut Tag = tag.cast();
    if tag.is_null() {
        return idle;
    }
    // SAFETY: the data pointer `point` set, a live tag while its handle
    // is; the field is read alone.
    let Some(object) = (unsafe { (*tag).object }) else {
        return idle;
    };
    // SAFETY: as above; the session field is never written after the tag
    // is made.
    let link = unsafe { (*tag).session.lent() };
    link.serve(idle, abort, |cx| f(cx, object))
}

/// `curl_write`.
extern "C" fn write_body(data: *mut c_char, size: usize, nmemb: usize, tag: *mut c_void) -> usize {
    let len = size.saturating_mul(nmemb);
    let bytes = slice(data, len);
    served(tag, len, 0, |cx, object| {
        callbacks::write(cx, object, bytes)
    })
}

/// `curl_write_header`.
extern "C" fn write_header(
    data: *mut c_char,
    size: usize,
    nmemb: usize,
    tag: *mut c_void,
) -> usize {
    let len = size.saturating_mul(nmemb);
    let bytes = slice(data, len);
    served(tag, len, 0, |cx, object| {
        callbacks::write_header(cx, object, bytes)
    })
}

/// `CURL_READFUNC_ABORT`.
const READ_ABORT: usize = 0x1000_0000;

/// `curl_read`.
extern "C" fn read_body(data: *mut c_char, size: usize, nmemb: usize, tag: *mut c_void) -> usize {
    let len = size.saturating_mul(nmemb);
    let buffer: &mut [u8] = if data.is_null() || len == 0 {
        &mut []
    } else {
        // SAFETY: the library hands `len` writable bytes at `data` for the
        // length of the callback.
        unsafe { std::slice::from_raw_parts_mut(data.cast(), len) }
    };
    served(tag, 0, READ_ABORT, |cx, object| {
        callbacks::read(cx, object, buffer)
    })
}

/// `curl_progress`.
extern "C" fn progress(
    tag: *mut c_void,
    dltotal: c_double,
    dlnow: c_double,
    ultotal: c_double,
    ulnow: c_double,
) -> c_int {
    served(tag, 0, 1, |cx, object| {
        callbacks::progress(cx, object, [dltotal, dlnow, ultotal, ulnow])
    })
}

/// `curl_xferinfo`.
extern "C" fn xferinfo(
    tag: *mut c_void,
    dltotal: i64,
    dlnow: i64,
    ultotal: i64,
    ulnow: i64,
) -> c_int {
    served(tag, 0, 1, |cx, object| {
        callbacks::xferinfo(cx, object, [dltotal, dlnow, ultotal, ulnow])
    })
}

/// `curl_fnmatch`.
extern "C" fn fnmatch(tag: *mut c_void, pattern: *const c_char, string: *const c_char) -> c_int {
    let pattern = bytes_of(pattern).unwrap_or_default();
    let string = bytes_of(string).unwrap_or_default();
    served(tag, 2, 2, |cx, object| {
        callbacks::fnmatch(cx, object, &pattern, &string)
    })
}

/// `curl_debug`.
extern "C" fn debug(
    _: *mut sys::CURL,
    kind: sys::curl_infotype,
    data: *mut c_char,
    size: usize,
    tag: *mut c_void,
) -> c_int {
    let bytes = slice(data, size);
    served(tag, 0, 0, |cx, object| {
        callbacks::debug(cx, object, i64::from(kind), bytes)
    })
}

/// `curl_prereqfunction`.
extern "C" fn prereq(
    tag: *mut c_void,
    primary: *mut c_char,
    local: *mut c_char,
    primary_port: c_int,
    local_port: c_int,
) -> c_int {
    let primary = bytes_of(primary).unwrap_or_default();
    let local = bytes_of(local).unwrap_or_default();
    served(tag, 0, 1, |cx, object| {
        callbacks::prereq(cx, object, (&primary, &local), [primary_port, local_port])
    })
}

/// `curl_ssh_hostkeyfunction`.
extern "C" fn ssh_hostkey(
    tag: *mut c_void,
    keytype: c_int,
    key: *const c_char,
    keylen: usize,
) -> c_int {
    let key = slice(key, keylen);
    served(tag, 1, 1, |cx, object| {
        callbacks::ssh_hostkey(cx, object, keytype, key)
    })
}

/// The file part a trampoline was handed.
fn file_served<T>(
    arg: *mut c_void,
    idle: T,
    abort: T,
    f: impl FnOnce(&mut Cx<'_>, &mut Source) -> T,
) -> T {
    let file: *mut FilePart = arg.cast();
    if file.is_null() {
        return idle;
    }
    // SAFETY: the argument `add_file` set, live while its tree is; the
    // link field is never written after it is made.
    let link = unsafe { (*file).link };
    // SAFETY: as above; the source field is borrowed alone, and only
    // while no other callback of the same part runs, the library calling
    // one at a time.
    let source = unsafe { &mut (*file).source };
    link.serve(idle, abort, |cx| f(cx, source))
}

/// `read_cb`.
extern "C" fn file_read(data: *mut c_char, size: usize, nitems: usize, arg: *mut c_void) -> usize {
    let len = size.saturating_mul(nitems);
    let buffer: &mut [u8] = if data.is_null() || len == 0 {
        &mut []
    } else {
        // SAFETY: as in `read_body`.
        unsafe { std::slice::from_raw_parts_mut(data.cast(), len) }
    };
    file_served(arg, READ_ABORT, READ_ABORT, |cx, source| {
        source.read(cx.req, buffer).unwrap_or(READ_ABORT)
    })
}

/// `seek_cb`: `CURL_SEEKFUNC_OK` or `CURL_SEEKFUNC_CANTSEEK`.
extern "C" fn file_seek(arg: *mut c_void, offset: sys::curl_off_t, origin: c_int) -> c_int {
    file_served(arg, 2, 2, |cx, source| {
        if source.seek(cx.req, offset, origin) {
            0
        } else {
            2
        }
    })
}

/// `free_cb`.
extern "C" fn file_free(arg: *mut c_void) {
    file_served(arg, (), (), |cx, source| source.close(cx.req));
}
