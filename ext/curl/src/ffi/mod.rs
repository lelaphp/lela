//! The binding of the platform's libcurl (design §7.2, `unsafe.homes`'
//! `ext/*/src/ffi/`), declared by `curl-sys` through [`sys`]: the calls `interface.c`, `multi.c`, `share.c` and
//! `curl_file.c` make and nothing beside them, over the platform's
//! library the oracle links.
//!
//! **Owners.** The request keeps each `CURL *`, `CURLM *` and
//! request-scoped `CURLSH *` ([`Memory::adopt`], ADR-0013): the
//! object's payload stores the one owner, an [`Easy`], a [`Multi`] or a
//! [`Share`], a native reaches the library's pointer through its copied name, and a
//! call that serves PHP code (a transfer, a pause, a MIME tree's free)
//! holds what it calls in with ([`Memory::holding`]), so a callback
//! that frees the object of a `CURL *` leaves the cleanup to the call's end.
//! What the library keeps a pointer to past the call that set it (the
//! error buffer, the `CURLOPT_STDERR` file, a header list, a MIME tree)
//! is held by the `CURL *`'s `Tag`, freed after `curl_easy_cleanup`.
//! `curl_easy_duphandle` copies the pointers to the lists and the MIME
//! tree's file parts, so the lists and trees are shared by a `CURL *`
//! and its copies, as php's copies share `to_free` with its `clone`
//! count: a set lets go of what it replaces only where no copy shares
//! it, and the last `CURL *` of them given up frees them all. A
//! persistent share is not the request's: it passes from one request to
//! the next through the engine's pool, one request at a time, and only
//! once no `CURL *` of the request that held it points at it
//! ([`Persistent::alone`]).
//!
//! **Callbacks.** libcurl calls back from inside `curl_easy_perform`,
//! `curl_multi_perform`, `curl_easy_pause`, the calls that free a MIME
//! tree, and `curl_easy_setopt` and `curl_multi_remove_handle`, whose
//! `infof` and `failf` lines reach the debug callable and the
//! `CURLOPT_STDERR` file: a `CURL *` is set only through `Live`, which
//! `EasyId::call` makes holding it, and a `CURLM *` adds, removes and
//! performs only through `LiveMulti`, which `MultiId::call` makes
//! holding it and the `CURL *`s it reaches. Each such
//! call is made through [`Session::enter`], which leaves
//! the request where a trampoline finds it for the length of the call and
//! hands back what a callback threw, which the library cannot carry. A
//! trampoline outside such a call reaches no request and answers as php
//! answers a callback that served nothing.
//!
//! **Waits.** `curl_easy_perform` (`curl_exec`) and `curl_multi_wait`
//! (`curl_multi_select`) block inside libcurl without lending the request
//! ([`Limits::blocking`]), unlike the other waits of the engine's natives:
//! lending there would let PHP code on a callback's thread call libcurl
//! on the same handles, shares and session while the request's thread is
//! inside libcurl, which php's one thread never does. A callback another
//! thread makes during them is served at the request's next interrupt
//! check after the native returns (Mitchell ruled 2026-10-10).
#![allow(unsafe_code)]
#![deny(
    clippy::undocumented_unsafe_blocks,
    clippy::multiple_unsafe_ops_per_block
)]

mod easy;
mod multi;
mod stdio;
mod sys;

use std::ffi::{CStr, CString, c_char, c_long, c_void};
use std::ptr::{self, NonNull};

use php_ext::abi::Callable;
use php_ext::call::call;
use php_ext::raising::{Cause, Thrown};
use php_ext::request::{ForeignId, Held, Request};
use php_ext::values::{Owner, Value};

pub(crate) use easy::{Address, Called, Easy, EasyId, Hook, Info, MimeBuilder, Posted};
pub(crate) use multi::{Multi, MultiId, Persistent, Share, ShareRef};
use stdio::CFile;

use sys::CURL_ERROR_SIZE as ERROR_SIZE;

/// `curl_global_init(CURL_GLOBAL_DEFAULT)`: libcurl's global state set
/// up, whether it took. The process start alone calls it.
pub(crate) fn global_init() -> bool {
    // SAFETY: no pointer; called once for the process, from the process
    // start the engine holds a once for (ADR-0010), before any handle.
    let rc = unsafe { sys::curl_global_init(sys::CURL_GLOBAL_DEFAULT) };
    rc == sys::CURLE_OK
}

/// A `CURLcode` as the `int` php keeps it in, `ch->err.no`.
const fn code(rc: sys::CURLcode) -> i32 {
    rc.cast_signed()
}

/// A C string of the bytes up to the first NUL, as php hands `ZSTR_VAL`
/// to the library.
fn c_string(bytes: &[u8]) -> CString {
    CString::new(php_ext::coerce::c_str(bytes)).unwrap_or_default()
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
/// call in progress and what a callback threw during it, and what a
/// `CURL *` let go of that the library may still read. One per request,
/// held by its state and by every `CURL *`, `CURLM *` and MIME tree
/// being built in it; a file part's callback argument names it without
/// holding it, as the part is held by a tree held by one of them.
struct Link {
    owners: usize,
    /// The request of the call in progress, where a callback reaches it;
    /// null between calls.
    req: *mut Request,
    /// What a callback threw during the call in progress, the first one.
    pending: Option<Thrown>,
    /// The MIME trees let go of where the file parts could not be closed
    /// for want of a request, and the `CURLOPT_STDERR` files let go of
    /// during a call, freed with the link.
    kept: Kept,
}

/// What [`Link`] keeps to its own end.
#[derive(Default)]
struct Kept {
    trees: Vec<easy::Mime>,
    /// A file let go of while the library may be inside a write to it, a
    /// cookie's that runs PHP code which sets the option again.
    files: Vec<CFile>,
    /// The stream values let go of at such a time, which the cookie of a
    /// kept file names until its write ends, given up by
    /// [`Session::close_kept`].
    streams: Vec<Owner<Value>>,
    /// The `to_free` and `clone` blocks, with what their lists laid
    /// down, of lists whose last share was dropped with no request at
    /// hand, freed by [`Session::close_kept`].
    counted: Vec<easy::Counted>,
}

/// One owner of the request's [`Link`].
pub(crate) struct Session(Lent);

/// The request's [`Link`], named, not owned: what a file part's callback
/// argument and a callback's [`Cx`] reach it through.
#[derive(Clone, Copy)]
struct Lent(NonNull<Link>);

// SAFETY: every owner of one `Session` belongs to the request that made
// it, which is served by one thread at a time and moves between threads
// only whole, so the count and the fields behind the pointer are never
// touched from two threads at once. It is not `Sync`: nothing shares one.
unsafe impl Send for Session {}

impl Default for Session {
    fn default() -> Session {
        let link = Box::into_raw(Box::new(Link {
            owners: 1,
            req: ptr::null_mut(),
            pending: None,
            kept: Kept::default(),
        }));
        // SAFETY: `into_raw` never answers null.
        Session(Lent(unsafe { NonNull::new_unchecked(link) }))
    }
}

impl Clone for Session {
    fn clone(&self) -> Session {
        // SAFETY: `self` is an owner, so the allocation is live, and no
        // other reference to it is held across this write.
        unsafe { (*self.0.0.as_ptr()).owners += 1 };
        Session(self.0)
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        let link = self.0.0.as_ptr();
        // SAFETY: as in `clone`.
        let left = unsafe {
            let link = &mut *link;
            link.owners -= 1;
            link.owners
        };
        if left > 0 {
            return;
        }
        // The kept trees are freed while the link stands, so a file
        // part's `free_cb` finds it; with no call in progress it serves
        // nothing.
        // SAFETY: the last owner's allocation; the field is taken alone.
        let kept = unsafe { std::mem::take(&mut (*link).kept) };
        drop(kept);
        // SAFETY: the allocation `default` leaked, freed by its last
        // owner.
        drop(unsafe { Box::from_raw(link) });
    }
}

impl Session {
    fn lent(&self) -> Lent {
        self.0
    }

    /// MIME trees let go of: freed now where a call in progress lends the
    /// request their file parts close through, and kept to the link's
    /// end otherwise, their file parts closed by [`Session::close_kept`].
    fn let_trees_go(&self, trees: Vec<easy::Mime>) {
        if self.entered() {
            drop(trees);
        } else {
            self.keep(trees);
        }
    }

    /// `trees` kept to the link's end.
    fn keep(&self, trees: Vec<easy::Mime>) {
        // SAFETY: an owner's allocation; the field is written alone.
        unsafe { (*self.0.0.as_ptr()).kept.trees.extend(trees) };
    }

    /// `counted` kept to the request's end, for [`Session::close_kept`].
    fn keep_counted(&self, counted: easy::Counted) {
        // SAFETY: an owner's allocation; the field is written alone.
        unsafe { (*self.0.0.as_ptr()).kept.counted.push(counted) };
    }

    /// A `CURLOPT_STDERR` file let go of: closed now between calls, and
    /// kept to the link's end during one, which may be inside a write to
    /// it ([`stdio`]).
    fn let_file_go(&self, file: CFile) {
        if self.entered() {
            // SAFETY: an owner's allocation; the field is written alone.
            unsafe { (*self.0.0.as_ptr()).kept.files.push(file) };
        } else {
            drop(file);
        }
    }

    /// A `CURLOPT_STDERR` stream's value let go of: given up now between
    /// calls, and during one kept to the request's end, since a kept
    /// file's cookie may still be inside a write to the stream.
    pub(crate) fn let_stream_go(&self, req: &mut Request, value: Owner<Value>) {
        if self.entered() {
            // SAFETY: an owner's allocation; the field is written alone.
            unsafe { (*self.0.0.as_ptr()).kept.streams.push(value) };
        } else {
            value.give_up(req);
        }
    }

    /// What `curl_easy_cleanup` and `curl_mime_free` would have closed of
    /// the kept trees' file parts, closed with the request at its end,
    /// once no `CURL *` that may read them is left (`post_deactivate`),
    /// the kept stream values given up and the kept blocks freed; the
    /// memory goes with the link.
    pub(crate) fn close_kept(&self, req: &mut Request) {
        // SAFETY: an owner's allocation; the blocks are taken off it alone.
        let counted = unsafe { std::mem::take(&mut (*self.0.0.as_ptr()).kept.counted) };
        for counted in counted {
            counted.free(req);
        }
        // SAFETY: an owner's allocation; the values are taken off it
        // alone, so giving one up reaches no field.
        let streams = unsafe { std::mem::take(&mut (*self.0.0.as_ptr()).kept.streams) };
        for value in streams {
            value.give_up(req);
        }
        // SAFETY: an owner's allocation; the trees are taken off it alone
        // and put back after, so closing a stream reaches no field.
        let trees = unsafe { std::mem::take(&mut (*self.0.0.as_ptr()).kept.trees) };
        for tree in &trees {
            tree.close_files(req);
        }
        self.keep(trees);
    }

    /// `f` served with the request left where a callback finds it, and
    /// what a callback threw meanwhile handed back beside `f`'s answer.
    /// `f` cannot reach the request itself: it is borrowed here for the
    /// length of the call.
    pub(crate) fn enter<R>(&self, req: &mut Request, f: impl FnOnce() -> R) -> (R, Option<Thrown>) {
        self.0.enter(req, f)
    }

    /// `body` served with the object `id` names held
    /// ([`Memory::holding`]) and no request where a callback finds it,
    /// whatever call into the library this one is made inside, that
    /// call's request put back after. The outermost hold of every call
    /// into the library that can call back: the holds such a call takes
    /// beside it, [`holding_all`]'s of the shares and of the `CURL *`s a
    /// `CURLM *` reaches, are taken inside `body`, so each hold ends with
    /// no request where a callback finds it. One given up during a hold
    /// is dropped as the hold ends, inside [`Memory::holding`], which
    /// still borrows the request, so a trampoline its cleanup reaches (a
    /// file part's `free_cb` under `curl_easy_cleanup` or
    /// `curl_mime_free`) finds no request, as at the top level, and not
    /// the one an enclosing transfer lent. What `body` calls the library
    /// with lends the request again ([`Session::enter`]), inside every
    /// hold.
    fn holding<T: Send + 'static, R>(
        &self,
        req: &mut Request,
        id: ForeignId<T>,
        body: impl for<'h> FnOnce(&mut Request, Held<'h, T>) -> R,
    ) -> Option<R> {
        let slot = self.0.0.as_ptr();
        // SAFETY: an owner's allocation; the write is the only access.
        let outer = unsafe { std::mem::replace(&mut (*slot).req, ptr::null_mut()) };
        let answered = req.memory_mut().holding(id, body);
        // SAFETY: as above; every `enter` inside `body` restored what it
        // found before this write.
        unsafe { (*slot).req = outer };
        answered
    }

    /// Whether a call is in progress, so a callback reaches the request.
    fn entered(&self) -> bool {
        self.0.request().is_some()
    }
}

impl Lent {
    /// [`Session::enter`].
    fn enter<R>(self, req: &mut Request, f: impl FnOnce() -> R) -> (R, Option<Thrown>) {
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
    fn request<'a>(self) -> Option<&'a mut Request> {
        // SAFETY: an owner's allocation; the field is read, nothing held.
        let req = unsafe { (*self.0.as_ptr()).req };
        // SAFETY: set by `enter` from the `&mut Request` it was given,
        // which that call does not touch until the library returns, so
        // this is the one live mutable borrow of it. Nothing reaches the
        // request through the link while a `Memory::holding` that ends a
        // hold is on the stack: `Session::holding` is the outermost hold
        // of every call that can call back, and `holding_all`, which holds
        // through `Memory::holding` itself, is called only inside one, so
        // where any hold ends and drops what was given up, outside the
        // `enter`s it serves, the link holds null and not an enclosing
        // call's request.
        unsafe { req.as_mut() }
    }

    fn pending(self) -> bool {
        // SAFETY: an owner's allocation; the field is read, nothing held.
        unsafe { (*self.0.as_ptr()).pending.is_some() }
    }

    fn hand_back(self, thrown: Thrown) {
        // SAFETY: an owner's allocation; no other reference to the field
        // is held across this write.
        let slot = unsafe { &mut (*self.0.as_ptr()).pending };
        if slot.is_none() {
            *slot = Some(thrown);
        }
    }

    /// Whether what a callback threw is a fatal, `zend_bailout`, which
    /// leaves php's transfer at once where a throwable lets it go on.
    fn fatal(self, req: &Request) -> bool {
        // SAFETY: an owner's allocation; the field is read, nothing held.
        let pending = unsafe { &(*self.0.as_ptr()).pending };
        pending
            .as_ref()
            .is_some_and(|thrown| matches!(thrown.cause(req), Cause::Fatal))
    }

    /// `f` served with the request of the call in progress: `idle` where
    /// there is none, `abort` where a callback left a fatal behind.
    fn serve<T>(self, idle: T, abort: T, f: impl FnOnce(&mut Cx<'_>) -> T) -> T {
        let Some(req) = self.request() else {
            return idle;
        };
        let mut cx = Cx { req, link: self };
        let answered = f(&mut cx);
        if self.fatal(cx.req) { abort } else { answered }
    }
}

/// A callback's view of the call in progress: the request, and php's
/// `zend_call_known_fcc`, which serves nothing while a throwable is
/// pending.
pub(crate) struct Cx<'a> {
    pub(crate) req: &'a mut Request,
    link: Lent,
}

impl Cx<'_> {
    /// `zend_call_known_fcc(fcc, &retval, ...)`: the value returned, or
    /// `None` for php's `UNDEF`, when a callback already threw or this
    /// one does, which is kept for the call to hand back.
    pub(crate) fn call(
        &mut self,
        callable: &Callable,
        arguments: &[Value],
    ) -> Option<Owner<Value>> {
        if self.link.pending() {
            return None;
        }
        match call(self.req, callable, arguments) {
            Ok(returned) => Some(returned),
            Err(thrown) => {
                self.link.hand_back(thrown);
                None
            }
        }
    }

    /// A throwable raised by the callback's own code, `zend_throw_error`
    /// after the call, kept for the call to hand back.
    pub(crate) fn raise(&mut self, thrown: Thrown) {
        self.link.hand_back(thrown);
    }
}

/// `body` served with each object `ids` names held ([`Memory::holding`]),
/// the ones no owner keeps any more passed over: what a call into the
/// library holds of the handles it may reach. Called only inside the
/// [`Session::holding`] of the `CURL *` or `CURLM *` that call is made
/// with, never as its outermost hold, so each of these holds ends where
/// the link holds no request, as that one's does ([`Lent::request`]).
fn holding_all<T: Send + 'static, R>(
    req: &mut Request,
    ids: &[ForeignId<T>],
    body: &mut Option<impl FnOnce(&mut Request) -> R>,
) -> Option<R> {
    let Some((first, rest)) = ids.split_first() else {
        return body.take().map(|body| body(req));
    };
    match req
        .memory_mut()
        .holding(*first, |req, _| holding_all(req, rest, body))
    {
        Some(answer) => answer,
        None => holding_all(req, rest, body),
    }
}

/// A `curl_slist`, the value of a list option, kept while any `CURL *` the
/// library copied its pointer to lives.
pub(crate) struct Slist(NonNull<sys::curl_slist>);

// SAFETY: as for `Session`: one request's, never changed once built.
unsafe impl Send for Slist {}

impl Slist {
    /// `curl_slist_append` over each line; `Ok(None)` for none, `Err` for
    /// the append that failed.
    pub(crate) fn of(lines: &[Vec<u8>]) -> Result<Option<Slist>, ()> {
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
        Ok(NonNull::new(list).map(Slist))
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
