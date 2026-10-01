//! The binding of the platform's libcurl (design §7.2, `unsafe.homes`'
//! `ext/*/src/ffi/`), declared by `curl-sys` through [`sys`]: the calls `interface.c`, `multi.c`, `share.c` and
//! `curl_file.c` make and nothing beside them, over the platform's
//! library the oracle links.
//!
//! **Owners.** An `CURL *` is an [`Easy`], a `CURLM *` a [`Multi`],
//! a `CURLSH *` a [`Share`], each an `Arc` over the library's pointer
//! so a native holds one while it calls in with the request. What the
//! library keeps a pointer to past the call that set it (the error buffer,
//! the `CURLOPT_STDERR` file, a header list, a MIME tree, a share) is
//! held by the `CURL *`'s [`Tag`], which is freed after `curl_easy_cleanup`,
//! and a copy made by `curl_easy_duphandle` holds what the library copied
//! the pointer of. Nothing here is shared with another request: every
//! owner belongs to one request, which is single-threaded and moves
//! between threads only whole (ADR-0005), which is what the `Send` and
//! `Sync` below rest on.
//!
//! **Callbacks.** libcurl calls back from inside `curl_easy_perform`,
//! `curl_multi_perform`, `curl_easy_pause` and the calls that free a MIME
//! tree. Each such call is made through [`Session::enter`], which leaves
//! the request where a trampoline finds it for the length of the call and
//! hands back what a callback threw, which the library cannot carry. A
//! trampoline outside such a call reaches no request and answers as php
//! answers a callback that served nothing.
#![allow(unsafe_code)]
#![deny(
    clippy::undocumented_unsafe_blocks,
    clippy::multiple_unsafe_ops_per_block
)]

mod easy;
mod multi;
mod sys;

use std::ffi::{CStr, CString, c_char, c_long, c_void};
use std::ptr::{self, NonNull};
use std::sync::Arc;

use php_ext::abi::Callable;
use php_ext::call::call;
use php_ext::raising::{Cause, Thrown};
use php_ext::request::Request;
use php_ext::values::Value;

pub(crate) use easy::{Easy, EasyId, Hook, Info};
pub(crate) use multi::{Multi, Share};

use sys::CURL_ERROR_SIZE as ERROR_SIZE;

/// A `CURLcode` as the `int` php keeps it in, `ch->err.no`.
const fn code(rc: sys::CURLcode) -> i32 {
    rc.cast_signed()
}

/// A C string of the bytes up to the first NUL, as php hands `ZSTR_VAL`
/// to the library.
fn c_string(bytes: &[u8]) -> CString {
    let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    CString::new(&bytes[..end]).unwrap_or_default()
}

/// The bytes of a NUL-terminated string the library owns, `None` for its
/// null.
fn bytes_of(raw: *const c_char) -> Option<Vec<u8>> {
    if raw.is_null() {
        return None;
    }
    // SAFETY: a non-null string the library answered, NUL-terminated and
    // live until its next call, which comes after the copy.
    let text = unsafe { CStr::from_ptr(raw) };
    Some(text.to_bytes().to_vec())
}

/// `len` bytes the library handed a callback at `data`.
fn slice<'a>(data: *const c_char, len: usize) -> &'a [u8] {
    if data.is_null() || len == 0 {
        return &[];
    }
    // SAFETY: the library hands `len` readable bytes at `data` for the
    // length of the callback.
    unsafe { std::slice::from_raw_parts(data.cast(), len) }
}

/// What the request's calls into the library share: the request of the
/// call in progress and what a callback threw during it. One per request,
/// held by its state and by every `CURL *`, MIME part and callback argument
/// made in it.
struct Link {
    owners: usize,
    /// The request of the call in progress, where a callback reaches it;
    /// null between calls.
    req: *mut Request,
    /// What a callback threw during the call in progress, the first one.
    pending: Option<Thrown>,
}

/// One owner of the request's [`Link`].
pub(crate) struct Session(NonNull<Link>);

// SAFETY: every owner of one `Session` belongs to the request that made
// it, which is served by one thread at a time and moves between threads
// only whole, so the count and the fields behind the pointer are never
// touched from two threads at once.
unsafe impl Send for Session {}
// SAFETY: as for `Send`; a shared `Session` is only read or written by
// the thread serving its request.
unsafe impl Sync for Session {}

impl Default for Session {
    fn default() -> Session {
        let link = Box::into_raw(Box::new(Link {
            owners: 1,
            req: ptr::null_mut(),
            pending: None,
        }));
        // SAFETY: `into_raw` never answers null.
        Session(unsafe { NonNull::new_unchecked(link) })
    }
}

impl Clone for Session {
    fn clone(&self) -> Session {
        // SAFETY: `self` is an owner, so the allocation is live, and no
        // other reference to it is held across this write.
        unsafe { (*self.0.as_ptr()).owners += 1 };
        Session(self.0)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        // SAFETY: as in `clone`.
        let left = unsafe {
            let link = &mut *self.0.as_ptr();
            link.owners -= 1;
            link.owners
        };
        if left == 0 {
            // SAFETY: the allocation `default` leaked, freed by its last
            // owner.
            drop(unsafe { Box::from_raw(self.0.as_ptr()) });
        }
    }
}

impl Session {
    /// `f` served with the request left where a callback finds it, and
    /// what a callback threw meanwhile handed back beside `f`'s answer.
    /// `f` cannot reach the request itself: it is borrowed here for the
    /// length of the call.
    pub(crate) fn enter<R>(&self, req: &mut Request, f: impl FnOnce() -> R) -> (R, Option<Thrown>) {
        let slot = self.0.as_ptr();
        // SAFETY: an owner's allocation; the write is the only access.
        let outer = unsafe { std::mem::replace(&mut (*slot).req, ptr::from_mut(req)) };
        // SAFETY: as above.
        let held = unsafe { (*slot).pending.take() };
        let answered = f();
        // SAFETY: as above; a callback's own `enter` restored what it
        // found before this read.
        let thrown = unsafe { std::mem::replace(&mut (*slot).pending, held) };
        // SAFETY: as above.
        unsafe { (*slot).req = outer };
        (answered, thrown)
    }

    /// The request of the call in progress, `None` outside one.
    fn request<'a>(&self) -> Option<&'a mut Request> {
        // SAFETY: an owner's allocation; the field is read, nothing held.
        let req = unsafe { (*self.0.as_ptr()).req };
        // SAFETY: set by `enter` from the `&mut Request` it was given,
        // which that call does not touch until the library returns, so
        // this is the one live mutable borrow of it.
        unsafe { req.as_mut() }
    }

    fn pending(&self) -> bool {
        // SAFETY: an owner's allocation; the field is read, nothing held.
        unsafe { (*self.0.as_ptr()).pending.is_some() }
    }

    fn keep(&self, thrown: Thrown) {
        // SAFETY: an owner's allocation; no other reference to the field
        // is held across this write.
        let slot = unsafe { &mut (*self.0.as_ptr()).pending };
        if slot.is_none() {
            *slot = Some(thrown);
        }
    }

    /// Whether what a callback threw is a fatal, `zend_bailout`, which
    /// leaves php's transfer at once where a throwable lets it go on.
    fn fatal(&self, req: &Request) -> bool {
        // SAFETY: an owner's allocation; the field is read, nothing held.
        let pending = unsafe { &(*self.0.as_ptr()).pending };
        pending
            .as_ref()
            .is_some_and(|thrown| matches!(thrown.cause(req), Cause::Fatal))
    }

    /// `f` served with the request of the call in progress: `idle` where
    /// there is none, `abort` where a callback left a fatal behind.
    fn serve<T>(&self, idle: T, abort: T, f: impl FnOnce(&mut Cx<'_>) -> T) -> T {
        let Some(req) = self.request() else {
            return idle;
        };
        let mut cx = Cx { req, session: self };
        let answered = f(&mut cx);
        if self.fatal(cx.req) { abort } else { answered }
    }
}

/// A callback's view of the call in progress: the request, and php's
/// `zend_call_known_fcc`, which serves nothing while a throwable is
/// pending.
pub(crate) struct Cx<'a> {
    pub(crate) req: &'a mut Request,
    session: &'a Session,
}

impl Cx<'_> {
    /// `zend_call_known_fcc(fcc, &retval, ...)`: the value returned, or
    /// `None` for php's `UNDEF`, when a callback already threw or this
    /// one does, which is kept for the call to hand back.
    pub(crate) fn call(&mut self, callable: &Callable, arguments: &[Value]) -> Option<Value> {
        if self.session.pending() {
            return None;
        }
        match call(self.req, callable, arguments) {
            Ok(returned) => Some(returned),
            Err(thrown) => {
                self.session.keep(thrown);
                None
            }
        }
    }

    /// A throwable raised by the callback's own code, `zend_throw_error`
    /// after the call, kept for the call to hand back.
    pub(crate) fn raise(&mut self, thrown: Thrown) {
        self.session.keep(thrown);
    }
}

/// `FILE *` over a duplicate of a stream's fd, what `php_stream_cast` to
/// `PHP_STREAM_AS_STDIO` makes of a plain file, for `CURLOPT_STDERR`.
pub(crate) struct CFile(NonNull<libc::FILE>);

// SAFETY: as for `Session`: one request's.
unsafe impl Send for CFile {}
// SAFETY: as for `Session`.
unsafe impl Sync for CFile {}

impl CFile {
    /// `fdopen(dup(fd), "a")`, `None` where either fails.
    pub(crate) fn open(fd: i32) -> Option<Arc<CFile>> {
        // SAFETY: `dup` of a descriptor the stream holds open.
        let own = unsafe { libc::dup(fd) };
        if own < 0 {
            return None;
        }
        // SAFETY: a descriptor this call owns and a NUL-terminated mode.
        let file = unsafe { libc::fdopen(own, c"a".as_ptr()) };
        if let Some(file) = NonNull::new(file) {
            return Some(Arc::new(CFile(file)));
        }
        // SAFETY: the descriptor `dup` made, which nothing else holds.
        unsafe { libc::close(own) };
        None
    }

    /// `fflush`.
    pub(crate) fn flush(&self) {
        // SAFETY: an open `FILE` this value owns.
        unsafe { libc::fflush(self.0.as_ptr()) };
    }
}

impl Drop for CFile {
    fn drop(&mut self) {
        // SAFETY: the `FILE` `open` made, closed once by its one owner.
        unsafe { libc::fclose(self.0.as_ptr()) };
    }
}

/// A `curl_slist`, the value of a list option, kept while any `CURL *` the
/// library copied its pointer to lives.
pub(crate) struct Slist(NonNull<sys::curl_slist>);

// SAFETY: as for `Session`: one request's, never changed once built.
unsafe impl Send for Slist {}
// SAFETY: as above.
unsafe impl Sync for Slist {}

impl Slist {
    /// `curl_slist_append` over each line; `Ok(None)` for none, `Err` for
    /// the append that failed.
    pub(crate) fn of(lines: &[Vec<u8>]) -> Result<Option<Arc<Slist>>, ()> {
        let mut list: *mut sys::curl_slist = ptr::null_mut();
        for line in lines {
            let line = c_string(line);
            // SAFETY: the list built so far, or null, and a NUL-terminated
            // line the library copies.
            let grown = unsafe { sys::curl_slist_append(list, line.as_ptr()) };
            if grown.is_null() {
                // SAFETY: the list built so far, which nothing else holds.
                unsafe { sys::curl_slist_free_all(list) };
                return Err(());
            }
            list = grown;
        }
        Ok(NonNull::new(list).map(|list| Arc::new(Slist(list))))
    }
}

impl Drop for Slist {
    fn drop(&mut self) {
        // SAFETY: the list `of` built, freed by its last owner.
        unsafe { sys::curl_slist_free_all(self.0.as_ptr()) };
    }
}

/// The lines of a list the library answered, freed with it where it is
/// the caller's.
fn lines_of(list: *mut sys::curl_slist) -> Vec<Vec<u8>> {
    let mut lines = Vec::new();
    let mut at = list;
    while !at.is_null() {
        // SAFETY: a node of a live list.
        let node = unsafe { &*at };
        lines.push(bytes_of(node.data).unwrap_or_default());
        at = node.next;
    }
    lines
}

/// `curl_easy_strerror`.
pub(crate) fn strerror(code: i64) -> Option<Vec<u8>> {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let code = code as sys::CURLcode;
    // SAFETY: any number; the library answers a static string.
    bytes_of(unsafe { sys::curl_easy_strerror(code) })
}

/// `curl_multi_strerror`.
pub(crate) fn multi_strerror(code: i64) -> Option<Vec<u8>> {
    #[allow(clippy::cast_possible_truncation)]
    let code = code as sys::CURLMcode;
    // SAFETY: as above.
    bytes_of(unsafe { sys::curl_multi_strerror(code) })
}

/// `curl_share_strerror`.
pub(crate) fn share_strerror(code: i64) -> Option<Vec<u8>> {
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let code = code as sys::CURLSHcode;
    // SAFETY: as above.
    bytes_of(unsafe { sys::curl_share_strerror(code) })
}

/// `curl_version_info(CURLVERSION_NOW)`, read as `curl_version()` and
/// `PHP_MINFO_FUNCTION(curl)` read it, each field named as the
/// library's is.
#[allow(clippy::struct_field_names)]
pub(crate) struct Version {
    pub(crate) version_num: i64,
    pub(crate) age: i64,
    pub(crate) features: i64,
    pub(crate) ssl_version_num: i64,
    pub(crate) version: Vec<u8>,
    pub(crate) host: Vec<u8>,
    pub(crate) ssl_version: Option<Vec<u8>>,
    pub(crate) libz_version: Option<Vec<u8>>,
    pub(crate) protocols: Vec<Vec<u8>>,
    pub(crate) ares: Option<Vec<u8>>,
    pub(crate) ares_num: i64,
    pub(crate) libidn: Option<Vec<u8>>,
    pub(crate) iconv_ver_num: i64,
    pub(crate) libssh_version: Option<Vec<u8>>,
    pub(crate) brotli_ver_num: i64,
    pub(crate) brotli_version: Option<Vec<u8>>,
}

#[allow(clippy::useless_conversion)]
pub(crate) fn version() -> Option<Version> {
    // SAFETY: the library answers a static structure, or null.
    let data = unsafe { sys::curl_version_info(sys::CURLVERSION_NOW) };
    // SAFETY: a static structure the library filled, or null.
    let data = unsafe { data.as_ref() }?;
    let mut protocols = Vec::new();
    let mut at = data.protocols;
    while !at.is_null() {
        // SAFETY: a NULL-terminated array of static strings.
        let name = unsafe { *at };
        let Some(name) = bytes_of(name) else {
            break;
        };
        protocols.push(name);
        // SAFETY: the next element of the same array, the terminator at
        // the latest.
        at = unsafe { at.add(1) };
    }
    Some(Version {
        version_num: i64::from(data.version_num),
        age: i64::from(data.age),
        features: i64::from(data.features),
        ssl_version_num: i64::from(data.ssl_version_num),
        version: bytes_of(data.version).unwrap_or_default(),
        host: bytes_of(data.host).unwrap_or_default(),
        ssl_version: bytes_of(data.ssl_version),
        libz_version: bytes_of(data.libz_version),
        protocols,
        ares: bytes_of(data.ares),
        ares_num: i64::from(data.ares_num),
        libidn: bytes_of(data.libidn),
        iconv_ver_num: i64::from(data.iconv_ver_num),
        libssh_version: bytes_of(data.libssh_version),
        brotli_ver_num: i64::from(data.brotli_ver_num),
        brotli_version: bytes_of(data.brotli_version),
    })
}

/// A `zend_long` as the C `long` a variadic option takes.
#[allow(clippy::cast_possible_truncation, clippy::useless_conversion)]
const fn c_long(n: i64) -> c_long {
    n as c_long
}

/// The trampolines' `void *` of a leaked box.
fn arg<T>(ptr: NonNull<T>) -> *mut c_void {
    ptr.as_ptr().cast()
}
