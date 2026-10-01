//! The easy transfer: `CURL *` with what the library points into, the
//! trampolines `interface.c` installs, and the MIME tree
//! `build_mime_structure_from_hash` builds.

use std::ffi::{c_char, c_double, c_int, c_void};
use std::ptr::{self, NonNull};
use std::sync::Arc;

use php_ext::raising::Thrown;
use php_ext::request::Request;
use php_ext::values::PhpObject;

use super::sys::{self, curl_mime, curl_mimepart};
use super::{
    CFile, Cx, ERROR_SIZE, Session, Slist, arg, bytes_of, c_long, c_string, code, lines_of, slice,
};
use crate::callbacks;
use crate::constants as c;
use crate::file::Source;

/// An option number as the library's `CURLoption`.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
const fn opt(option: i64) -> sys::CURLoption {
    option as sys::CURLoption
}

/// What the library keeps a pointer to from one `CURL *`, freed after
/// the `CURL *` is: its object, the error buffer, the `CURLOPT_STDERR`
/// file, the lists, the MIME trees and the share.
struct Tag {
    session: Session,
    /// The `CurlHandle` a callback is served for, `None` once it is being
    /// freed.
    object: Option<PhpObject>,
    error: [c_char; ERROR_SIZE + 1],
    stderr: Option<Arc<CFile>>,
    slists: Vec<(i64, Arc<Slist>)>,
    mimes: Vec<Arc<Mime>>,
    share: Option<Arc<super::multi::ShareInner>>,
}

struct Inner {
    raw: NonNull<sys::CURL>,
    tag: NonNull<Tag>,
    /// False for a `CURL *` the library frees itself, a pushed one it was
    /// told to deny.
    owned: bool,
}

impl Drop for Inner {
    fn drop(&mut self) {
        if self.owned {
            // SAFETY: the live handle this value owns, cleaned up once.
            unsafe { sys::curl_easy_cleanup(self.raw.as_ptr()) };
        }
        // SAFETY: the tag `Easy::make` leaked, freed once the handle
        // that pointed into it is gone.
        drop(unsafe { Box::from_raw(self.tag.as_ptr()) });
    }
}

/// `CURL *`, one owner of it.
#[derive(Clone)]
pub(crate) struct Easy(Arc<Inner>);

// SAFETY: as for `Session`: one request's.
unsafe impl Send for Inner {}
// SAFETY: as for `Session`.
unsafe impl Sync for Inner {}

/// Which `CURL *` a message or a push names, compared and nothing more.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct EasyId(usize);

impl EasyId {
    pub(super) fn of(raw: *mut sys::CURL) -> EasyId {
        EasyId(raw as usize)
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

impl Easy {
    fn make(raw: NonNull<sys::CURL>, session: Session, object: PhpObject, owned: bool) -> Easy {
        let tag = Box::into_raw(Box::new(Tag {
            session,
            object: Some(object),
            error: [0; ERROR_SIZE + 1],
            stderr: None,
            slists: Vec::new(),
            mimes: Vec::new(),
            share: None,
        }));
        // SAFETY: `into_raw` never answers null.
        let tag = unsafe { NonNull::new_unchecked(tag) };
        let easy = Easy(Arc::new(Inner { raw, tag, owned }));
        easy.point();
        easy
    }

    /// `curl_easy_init()` for `object`, `None` where the library made none.
    pub(crate) fn init(session: Session, object: PhpObject) -> Option<Easy> {
        // SAFETY: no argument.
        let raw = NonNull::new(unsafe { sys::curl_easy_init() })?;
        Some(Easy::make(raw, session, object, true))
    }

    /// `curl_easy_duphandle(ch->cp)` for `object`, with what the copy's
    /// options point into held by it too.
    pub(crate) fn duplicate(&self, object: PhpObject) -> Option<Easy> {
        // SAFETY: a live handle.
        let raw = NonNull::new(unsafe { sys::curl_easy_duphandle(self.raw()) })?;
        let session = self.tag_session();
        let copy = Easy::make(raw, session, object, true);
        let from = self.0.tag.as_ptr();
        let to = copy.0.tag.as_ptr();
        // SAFETY: a live tag; the field is read and cloned, nothing held.
        let stderr = unsafe { (*from).stderr.clone() };
        // SAFETY: as above.
        let slists = unsafe { (*from).slists.clone() };
        // SAFETY: as above.
        let mimes = unsafe { (*from).mimes.clone() };
        // SAFETY: the copy's live tag; the field is written alone.
        unsafe { (*to).stderr = stderr };
        // SAFETY: as above.
        unsafe { (*to).slists = slists };
        // SAFETY: as above.
        unsafe { (*to).mimes = mimes };
        Some(copy)
    }

    /// A pushed `CURL *` the library made, served for `object`; `owned`
    /// false until the push is accepted.
    pub(super) fn adopt(raw: *mut sys::CURL, session: Session, object: PhpObject) -> Option<Easy> {
        let raw = NonNull::new(raw)?;
        Some(Easy::make(raw, session, object, false))
    }

    /// The pushed `CURL *` accepted: the multi owns it from here, as any.
    pub(crate) fn accept(self) -> Option<Easy> {
        let mut inner = Arc::into_inner(self.0)?;
        inner.owned = true;
        Some(Easy(Arc::new(inner)))
    }

    pub(super) fn raw(&self) -> *mut sys::CURL {
        self.0.raw.as_ptr()
    }

    fn tag_session(&self) -> Session {
        // SAFETY: a live tag; the field is read and cloned, nothing held.
        unsafe { (*self.0.tag.as_ptr()).session.clone() }
    }

    fn session(&self) -> &Session {
        // SAFETY: a live tag whose session field is written only when the
        // tag is made.
        unsafe { &(*self.0.tag.as_ptr()).session }
    }

    /// Which `CURL *` this is.
    pub(crate) fn id(&self) -> EasyId {
        EasyId::of(self.raw())
    }

    /// The `CURL *`'s object no longer served: `curl_free_obj` has begun.
    pub(crate) fn forget_object(&self) {
        // SAFETY: a live tag; the field is written alone.
        unsafe { (*self.0.tag.as_ptr()).object = None };
    }

    /// The error buffer, the three data pointers and the hooks' data
    /// pointed at this `CURL *`'s tag: what `_php_curl_set_default_options`
    /// and `_php_setup_easy_copy_handlers` point, and what a copy's options
    /// must stop pointing at the original's.
    fn point(&self) {
        let tag = arg(self.0.tag);
        // SAFETY: a live tag, whose array field is borrowed for the address.
        let error = unsafe { (*self.0.tag.as_ptr()).error.as_mut_ptr() };
        self.set_ptr(sys::CURLOPT_ERRORBUFFER.into(), error.cast());
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
            self.set_ptr(option, tag);
        }
    }

    fn set_ptr(&self, option: i64, value: *mut c_void) -> i32 {
        // SAFETY: a live handle and an option the library reads as a
        // pointer, which outlives the handle or is null.
        code(unsafe { sys::curl_easy_setopt(self.raw(), opt(option), value) })
    }

    /// `_php_curl_set_default_options`: the trampolines installed, the
    /// defaults php sets, `cainfo` where one is configured, and
    /// `CURLOPT_NOSIGNAL` as php's ZTS build sets it, since requests here
    /// are served on threads.
    pub(crate) fn defaults(&self, cainfo: Option<&[u8]>) {
        self.set_long(c::CURLOPT_NOPROGRESS, 1);
        self.set_long(c::CURLOPT_VERBOSE, 0);
        self.point();
        let write: sys::curl_write_callback = write_body;
        let read: sys::curl_read_callback = read_body;
        let header: sys::curl_write_callback = write_header;
        // SAFETY: a live handle and the trampolines, which take the tag
        // `point` set as their data.
        unsafe {
            sys::curl_easy_setopt(self.raw(), opt(c::CURLOPT_WRITEFUNCTION), write);
        }
        // SAFETY: as above.
        unsafe {
            sys::curl_easy_setopt(self.raw(), opt(c::CURLOPT_READFUNCTION), read);
        }
        // SAFETY: as above.
        unsafe {
            sys::curl_easy_setopt(self.raw(), opt(c::CURLOPT_HEADERFUNCTION), header);
        }
        self.set_long(c::CURLOPT_DNS_CACHE_TIMEOUT, 120);
        self.set_long(c::CURLOPT_MAXREDIRS, 20);
        if let Some(cainfo) = cainfo {
            self.set_str(c::CURLOPT_CAINFO, Some(cainfo));
        }
        self.set_long(c::CURLOPT_NOSIGNAL, 1);
    }

    /// `curl_easy_setopt(ch->cp, option, (long) value)`.
    pub(crate) fn set_long(&self, option: i64, value: i64) -> i32 {
        // SAFETY: a live handle and an option the library reads as a
        // `long`.
        code(unsafe { sys::curl_easy_setopt(self.raw(), opt(option), c_long(value)) })
    }

    /// `curl_easy_setopt(ch->cp, option, (curl_off_t) value)`.
    pub(crate) fn set_off(&self, option: i64, value: i64) -> i32 {
        // SAFETY: a live handle and an option the library reads as a
        // `curl_off_t`.
        code(unsafe { sys::curl_easy_setopt(self.raw(), opt(option), value) })
    }

    /// `curl_easy_setopt(ch->cp, option, str)`, `NULL` for `None`; the
    /// library copies the string.
    pub(crate) fn set_str(&self, option: i64, value: Option<&[u8]>) -> i32 {
        let value = value.map(c_string);
        let raw = value.as_ref().map_or(ptr::null(), |value| value.as_ptr());
        // SAFETY: a live handle, an option the library copies a string of,
        // and a NUL-terminated string that outlives the call, or null.
        code(unsafe { sys::curl_easy_setopt(self.raw(), opt(option), raw) })
    }

    /// A blob option under `CURL_BLOB_COPY`.
    pub(crate) fn set_blob(&self, option: i64, value: &[u8]) -> i32 {
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
    pub(crate) fn set_post_fields(&self, body: &[u8]) -> i32 {
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

    /// A list option: the list the library is pointed at, held by the
    /// `CURL *` in place of the one the option held.
    pub(crate) fn set_slist(&self, option: i64, list: Option<Arc<Slist>>) -> i32 {
        let raw = list
            .as_ref()
            .map_or(ptr::null_mut(), |list| list.0.as_ptr());
        let code = self.set_ptr(option, raw.cast());
        let slists = self.0.tag.as_ptr();
        // SAFETY: a live tag; the field is written alone.
        let held = unsafe { &mut (*slists).slists };
        held.retain(|(held, _)| *held != option);
        if let Some(list) = list {
            held.push((option, list));
        }
        code
    }

    /// `CURLOPT_STDERR`: the file the library writes its verbose lines
    /// to, the process's `stderr` for `None`.
    pub(crate) fn set_stderr(&self, file: Option<Arc<CFile>>) -> i32 {
        let raw = file
            .as_ref()
            .map_or(ptr::null_mut(), |file| file.0.as_ptr());
        let code = self.set_ptr(c::CURLOPT_STDERR, raw.cast());
        // SAFETY: a live tag; the field is written alone, after the
        // library stopped pointing at what it held.
        unsafe { (*self.0.tag.as_ptr()).stderr = file };
        code
    }

    /// The `CURLOPT_STDERR` file, flushed, as `php_stream_flush` flushes
    /// the stream `curl_exec` names.
    pub(crate) fn flush_stderr(&self) {
        // SAFETY: a live tag; the field is read and cloned, nothing held.
        let file = unsafe { (*self.0.tag.as_ptr()).stderr.clone() };
        if let Some(file) = file {
            file.flush();
        }
    }

    /// `CURLOPT_SHARE`, the share held by the `CURL *` while it points at it.
    pub(crate) fn set_share(&self, share: &super::Share) {
        let inner = Arc::clone(&share.0);
        self.set_ptr(c::CURLOPT_SHARE, inner.raw.as_ptr().cast());
        // SAFETY: a live tag; the field is written alone, after the
        // library stopped pointing at what it held.
        unsafe { (*self.0.tag.as_ptr()).share = Some(inner) };
    }

    /// A hook's `*FUNCTION` pointed at its trampoline and its data at the
    /// tag, as `HANDLE_CURL_OPTION_CALLABLE` does whatever the callable.
    pub(crate) fn set_hook(&self, hook: Hook) {
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

    /// `CURLOPT_DEBUGFUNCTION` at `curl_debug`, or at nothing.
    pub(crate) fn set_debug(&self, on: bool) {
        let f: Option<sys::curl_debug_callback> = if on { Some(debug) } else { None };
        // SAFETY: a live handle and the trampoline, or null.
        unsafe { sys::curl_easy_setopt(self.raw(), opt(c::CURLOPT_DEBUGFUNCTION), f) };
        let data = if on { arg(self.0.tag) } else { ptr::null_mut() };
        self.set_ptr(sys::CURLOPT_DEBUGDATA.into(), data);
    }

    /// `curl_easy_perform`, the request reachable by the callbacks.
    pub(crate) fn perform(&self, req: &mut Request) -> (i32, Option<Thrown>) {
        let raw = self.raw();
        // SAFETY: a live handle; its callbacks find the request `enter`
        // leaves.
        self.session()
            .enter(req, || code(unsafe { sys::curl_easy_perform(raw) }))
    }

    /// `curl_easy_pause`, which may deliver what was held back.
    pub(crate) fn pause(&self, req: &mut Request, bitmask: i64) -> (i32, Option<Thrown>) {
        let raw = self.raw();
        #[allow(clippy::cast_possible_truncation)]
        let bitmask = bitmask as c_int;
        self.session().enter(req, || {
            // SAFETY: as in `perform`.
            code(unsafe { sys::curl_easy_pause(raw, bitmask) })
        })
    }

    /// `curl_easy_upkeep`.
    pub(crate) fn upkeep(&self, req: &mut Request) -> (i32, Option<Thrown>) {
        let raw = self.raw();
        self.session().enter(req, || {
            // SAFETY: as in `perform`.
            code(unsafe { sys::curl_easy_upkeep(raw) })
        })
    }

    /// `curl_easy_reset`, the defaults to be laid again by the caller.
    pub(crate) fn reset(&self) {
        // SAFETY: a live handle.
        unsafe { sys::curl_easy_reset(self.raw()) };
    }

    /// The error buffer's text up to its NUL.
    pub(crate) fn error_text(&self) -> Vec<u8> {
        // SAFETY: a live tag; the field is read, nothing held.
        let error = unsafe { &(*self.0.tag.as_ptr()).error };
        let end = error.iter().position(|b| *b == 0).unwrap_or(ERROR_SIZE);
        #[allow(clippy::cast_sign_loss)]
        error[..end].iter().map(|b| *b as u8).collect()
    }

    /// `memset(ch->err.str, 0, CURL_ERROR_SIZE + 1)`.
    pub(crate) fn clear_error(&self) {
        // SAFETY: a live tag; the field is written alone.
        unsafe { (*self.0.tag.as_ptr()).error = [0; ERROR_SIZE + 1] };
    }

    /// `curl_easy_escape`.
    pub(crate) fn escape(&self, text: &[u8]) -> Option<Vec<u8>> {
        let len = c_int::try_from(text.len()).ok()?;
        // SAFETY: a live handle and `len` bytes at `text`.
        let out = unsafe { sys::curl_easy_escape(self.raw(), text.as_ptr().cast(), len) };
        let escaped = bytes_of(out)?;
        // SAFETY: the string the library allocated for this call.
        unsafe { sys::curl_free(out.cast()) };
        Some(escaped)
    }

    /// `curl_easy_unescape`.
    pub(crate) fn unescape(&self, text: &[u8]) -> Option<Vec<u8>> {
        let len = c_int::try_from(text.len()).ok()?;
        let mut out_len: c_int = 0;
        // SAFETY: a live handle, `len` bytes at `text` and a live out-length.
        let out = unsafe {
            sys::curl_easy_unescape(self.raw(), text.as_ptr().cast(), len, &raw mut out_len)
        };
        if out.is_null() {
            return None;
        }
        let bytes = slice(out, usize::try_from(out_len).unwrap_or(0)).to_vec();
        // SAFETY: the string the library allocated for this call.
        unsafe { sys::curl_free(out.cast()) };
        Some(bytes)
    }

    /// `curl_easy_getinfo(ch->cp, info, &out)` read as the type the
    /// number masks, `None` where the library refused.
    pub(crate) fn info(&self, info: i64) -> Option<Info> {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let code = info as sys::CURLINFO;
        let raw = self.raw();
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

    /// A MIME tree made on this `CURL *` and pointed at by
    /// `CURLOPT_MIMEPOST`, held by the `CURL *` in place of the ones it
    /// held; the code is the library's.
    pub(crate) fn set_mime(&self, req: &mut Request, mime: Arc<Mime>) -> (i32, Option<Thrown>) {
        let raw = self.raw();
        let tree = mime.raw.as_ptr();
        let (code, thrown) = self.session().enter(req, || {
            // SAFETY: a live handle and a tree that outlives the pointer,
            // held below; replacing the tree the handle had may free a
            // copy whose parts call back.
            code(unsafe { sys::curl_easy_setopt(raw, sys::CURLOPT_MIMEPOST, tree) })
        });
        let old = {
            // SAFETY: a live tag; the field is written alone.
            let held = unsafe { &mut (*self.0.tag.as_ptr()).mimes };
            std::mem::replace(held, vec![mime])
        };
        let ((), dropped) = self.session().enter(req, || drop(old));
        (code, thrown.or(dropped))
    }

    /// `curl_mime_init(ch->cp)`, `None` where the library made none.
    pub(crate) fn mime(&self) -> Option<MimeBuilder> {
        // SAFETY: a live handle.
        let raw = NonNull::new(unsafe { sys::curl_mime_init(self.raw()) })?;
        Some(MimeBuilder {
            mime: Mime {
                raw,
                files: Vec::new(),
            },
            session: self.tag_session(),
        })
    }

    /// The `CURL *` let go with the request reachable, so what the library
    /// frees with it calls back where php's would: the last owner cleans
    /// the `CURL *` up here.
    pub(crate) fn close(self, req: &mut Request) -> Option<Thrown> {
        let session = self.tag_session();
        let ((), thrown) = session.enter(req, || drop(self));
        thrown
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
}

// SAFETY: as for `Session`: one request's.
unsafe impl Send for Mime {}
// SAFETY: as for `Session`.
unsafe impl Sync for Mime {}

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
/// link.
struct FilePart {
    session: Session,
    source: Source,
}

/// A MIME tree being built.
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
    pub(crate) fn add_file(
        &mut self,
        name: &[u8],
        source: Source,
        size: Option<i64>,
        filename: &[u8],
        mime: &[u8],
    ) -> Option<i32> {
        let seekable = source.is_open();
        let part = self.part()?;
        let file = Box::into_raw(Box::new(FilePart {
            session: self.session.clone(),
            source,
        }));
        // SAFETY: `into_raw` never answers null.
        let file = unsafe { NonNull::new_unchecked(file) };
        self.mime.files.push(file);
        let name = c_string(name);
        // SAFETY: a live part and a NUL-terminated name the library copies.
        let rc = unsafe { sys::curl_mime_name(part.as_ptr(), name.as_ptr()) };
        if rc != sys::CURLE_OK {
            return Some(code(rc));
        }
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

    /// The tree, to be set.
    pub(crate) fn build(self) -> Arc<Mime> {
        Arc::new(self.mime)
    }

    /// The tree given up, freed with the request reachable so its file
    /// parts close what they opened.
    pub(crate) fn discard(self, req: &mut Request) -> Option<Thrown> {
        let session = self.session.clone();
        let ((), thrown) = session.enter(req, || drop(self));
        thrown
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
    // is made, so the shared borrow overlaps no write.
    let session = unsafe { &(*tag).session };
    session.serve(idle, abort, |cx| f(cx, object))
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
    // session field is never written after it is made.
    let session = unsafe { &(*file).session };
    // SAFETY: as above; the source field is borrowed alone, and only
    // while no other callback of the same part runs, the library calling
    // one at a time.
    let source = unsafe { &mut (*file).source };
    session.serve(idle, abort, |cx| f(cx, source))
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
