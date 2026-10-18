//! The binding of the platform's libcurl (design §7.2, `unsafe.homes`'
//! `ext/*/src/ffi/`), declared by `curl-sys` through [`sys`]: the calls `interface.c`, `multi.c`, `share.c` and
//! `curl_file.c` make and nothing beside them, over the platform's
//! library the oracle links.
//!
//! **Owners.** The request keeps each `CURL *`, `CURLM *` and
//! request-scoped `CURLSH *` ([`Request::adopt`], ADR-0013): the
//! object's payload stores the one owner, an [`Easy`], a [`Multi`] or a
//! [`Share`], a native reaches the library's pointer through its copied name, and a
//! call that serves PHP code (a transfer, a pause, a MIME tree's free)
//! holds what it calls in with ([`Request::holding`]), so a callback
//! that frees the object of a `CURL *` leaves the cleanup to the call's end.
//! What the library keeps a pointer to past the call that set it (the
//! error buffer, the `CURLOPT_STDERR` file, a header list, a MIME tree)
//! is held by the `CURL *`'s `Tag`, freed after `curl_easy_cleanup`.
//! `curl_easy_duphandle` copies the pointers to the lists and the MIME
//! tree's file parts, as php's copies share `to_free` with its `clone`
//! count: from then on the original lets nothing of the kind go before
//! the request's [`Session`] does, which outlives every `CURL *`. A
//! persistent share is not the request's: it passes from one request to
//! the next through the engine's pool, one request at a time, and only
//! once no `CURL *` of the request that held it points at it
//! ([`Persistent::alone`]).
//!
//! **Callbacks.** libcurl calls back from inside `curl_easy_perform`,
//! `curl_multi_perform`, `curl_easy_pause` and the calls that free a MIME
//! tree. Each such call is made through [`Session::enter`], which leaves
//! the request where a trampoline finds it for the length of the call and
//! hands back what a callback threw, which the library cannot carry. A
//! trampoline outside such a call reaches no request and answers as php
//! answers a callback that served nothing.
//!
//! **Waits.** `curl_easy_perform` (`curl_exec`) and `curl_multi_wait`
//! (`curl_multi_select`) block inside libcurl without lending the request
//! ([`Request::blocking`]), unlike the other waits of the engine's natives:
//! lending there would let PHP code on a callback's thread call libcurl
//! on the same handles, shares and session while the request's thread is
//! inside libcurl, which php's one thread never does. A callback another
//! thread makes during them is served at the request's next interrupt
//! check after the native returns (Mitchell's ruling on #2595).
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

use php_ext::abi::Callable;
use php_ext::call::call;
use php_ext::raising::{Cause, Thrown};
use php_ext::request::{ForeignId, Held, Request};
use php_ext::values::{Owner, Value};

pub(crate) use easy::{Address, Easy, EasyId, Hook, Info, MimeBuilder, Posted};
pub(crate) use multi::{Multi, MultiId, Persistent, Share, ShareRef, Shared};

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
    /// The lists and MIME trees a `CURL *` let go of where a copy of it
    /// may still point at them, or where the file parts could not be
    /// closed for want of a request, freed with the link.
    kept: Kept,
}

/// The lists and trees [`Link`] keeps to its own end.
#[derive(Default)]
struct Kept {
    lists: Vec<Slist>,
    trees: Vec<easy::Mime>,
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

    /// `trees` and `lists` let go of by a `CURL *` whose copies may still
    /// point at them, or with no request to close a file part through:
    /// kept to the link's end, their file parts closed by
    /// [`Session::close_kept`].
    fn keep(&self, lists: Vec<Slist>, trees: Vec<easy::Mime>) {
        // SAFETY: an owner's allocation; the field is written alone.
        let kept = unsafe { &mut (*self.0.0.as_ptr()).kept };
        kept.lists.extend(lists);
        kept.trees.extend(trees);
    }

    /// What `curl_easy_cleanup` and `curl_mime_free` would have closed of
    /// the kept trees' file parts, closed with the request at its end,
    /// once no `CURL *` that may read them is left (`post_deactivate`);
    /// the memory goes with the link.
    pub(crate) fn close_kept(&self, req: &mut Request) {
        // SAFETY: an owner's allocation; the trees are taken off it alone
        // and put back after, so closing a stream reaches no field.
        let trees = unsafe { std::mem::take(&mut (*self.0.0.as_ptr()).kept.trees) };
        for tree in &trees {
            tree.close_files(req);
        }
        self.keep(Vec::new(), trees);
    }

    /// `f` served with the request left where a callback finds it, and
    /// what a callback threw meanwhile handed back beside `f`'s answer.
    /// `f` cannot reach the request itself: it is borrowed here for the
    /// length of the call.
    pub(crate) fn enter<R>(&self, req: &mut Request, f: impl FnOnce() -> R) -> (R, Option<Thrown>) {
        self.0.enter(req, f)
    }

    /// `body` served with the object `id` names held
    /// ([`Request::holding`]) and no request where a callback finds it,
    /// whatever call into the library this one is made inside, that
    /// call's request put back after. The only way the binding holds the
    /// library's objects: one given up during the hold is dropped as the
    /// hold ends, inside `holding`, which still borrows the request, so a
    /// trampoline its cleanup reaches (a file part's `free_cb` under
    /// `curl_easy_cleanup` or `curl_mime_free`) finds no request, as at
    /// the top level, and not the one an enclosing transfer lent. What
    /// `body` calls the library with lends the request again
    /// ([`Session::enter`]).
    fn holding<T: Send + 'static, R>(
        &self,
        req: &mut Request,
        id: ForeignId<T>,
        body: impl for<'h> FnOnce(&mut Request, Held<'h, T>) -> R,
    ) -> Option<R> {
        let slot = self.0.0.as_ptr();
        // SAFETY: an owner's allocation; the write is the only access.
        let outer = unsafe { std::mem::replace(&mut (*slot).req, ptr::null_mut()) };
        let answered = req.holding(id, body);
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
        // request through the link while a `Request::holding` that ends a
        // hold is on the stack: the binding holds the library's objects
        // through `Session::holding` alone, so between the `enter`s a hold
        // serves, where the hold's end drops what was given up, the link
        // holds null and not an enclosing call's request.
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

/// `body` served with each object `ids` names held ([`Request::holding`]),
/// the ones no owner keeps any more passed over: what a call into the
/// library holds of the handles it may reach, inside the
/// [`Session::holding`] of the `CURL *` or `CURLM *` it calls with.
fn holding_all<T: Send + 'static, R>(
    req: &mut Request,
    ids: &[ForeignId<T>],
    body: &mut Option<impl FnOnce(&mut Request) -> R>,
) -> Option<R> {
    let Some((first, rest)) = ids.split_first() else {
        return body.take().map(|body| body(req));
    };
    match req.holding(*first, |req, _| holding_all(req, rest, body)) {
        Some(answer) => answer,
        None => holding_all(req, rest, body),
    }
}

/// `FILE *` over a duplicate of a stream's fd, what `php_stream_cast` to
/// `PHP_STREAM_AS_STDIO` makes of a plain file, for `CURLOPT_STDERR`.
pub(crate) struct CFile(NonNull<libc::FILE>);

// SAFETY: as for `Session`: one request's, held by one `CURL *`.
unsafe impl Send for CFile {}

impl CFile {
    /// `fdopen(dup(fd), "a")`, `None` where either fails.
    pub(crate) fn open(fd: i32) -> Option<CFile> {
        // SAFETY: `dup` of a descriptor the stream holds open.
        let own = unsafe { libc::dup(fd) };
        if own < 0 {
            return None;
        }
        // SAFETY: a descriptor this call owns and a NUL-terminated mode.
        let file = unsafe { libc::fdopen(own, c"a".as_ptr()) };
        if let Some(file) = NonNull::new(file) {
            return Some(CFile(file));
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

    /// The same file opened again for a copy of the `CURL *`, which
    /// writes to it and flushes it as its own: what the original wrote
    /// flushed first, so the two keep their order.
    fn reopen(&self) -> Option<CFile> {
        self.flush();
        // SAFETY: an open `FILE` this value owns.
        let fd = unsafe { libc::fileno(self.0.as_ptr()) };
        CFile::open(fd)
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
