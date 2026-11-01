//! The `CURLM *` and the `CURLSH *`: `CURLM *` with the push
//! trampoline `multi.c` installs, and `CURLSH *`.

use std::ffi::{c_int, c_void};
use std::marker::PhantomData;
use std::ptr::{self, NonNull};
use std::sync::Arc;

use php_ext::raising::Thrown;
use php_ext::request::{Foreign, ForeignId, Request};
use php_ext::values::PhpObject;

use super::sys;
use super::{Address, Easy, EasyId, Session, arg, bytes_of, c_long, holding_all};
use crate::constants as c;
use crate::multi as callbacks;

/// A multi option number as the library's `CURLMoption`.
#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
const fn opt(option: i64) -> sys::CURLMoption {
    option as sys::CURLMoption
}

/// What the push trampoline is handed: the request's link and the
/// `CurlMultiHandle`.
struct Tag {
    session: Session,
    object: Option<PhpObject>,
}

/// One `CURLM *` the request keeps, with its tag.
pub(super) struct MultiHandle {
    raw: NonNull<sys::CURLM>,
    tag: NonNull<Tag>,
    /// Whether [`MultiId::cleanup`] cleaned the `CURLM *` up, so that
    /// nothing reaches it after and the drop frees only the tag.
    cleaned: bool,
}

impl Drop for MultiHandle {
    fn drop(&mut self) {
        if !self.cleaned {
            // SAFETY: the live handle this value owns, cleaned up once;
            // the easy handles in it are detached, not freed.
            unsafe { sys::curl_multi_cleanup(self.raw.as_ptr()) };
        }
        // SAFETY: the tag `init` leaked, freed after the handle.
        drop(unsafe { Box::from_raw(self.tag.as_ptr()) });
    }
}

// SAFETY: as for the easy handle: the request's, kept in its table of
// foreign objects (ADR-0013).
unsafe impl Send for MultiHandle {}

impl MultiHandle {
    fn raw(&self) -> *mut sys::CURLM {
        self.raw.as_ptr()
    }

    fn tag(&self) -> &Tag {
        // SAFETY: a live tag, written only through `tag_mut`.
        unsafe { self.tag.as_ref() }
    }

    fn tag_mut(&mut self) -> &mut Tag {
        // SAFETY: a live tag; the library hands it only to the push
        // trampoline, inside a call this borrow is not across.
        unsafe { self.tag.as_mut() }
    }
}

/// `CURLM *`'s one owner, what a `CurlMultiHandle`'s payload stores.
pub(crate) struct Multi(Foreign<MultiHandle>);

/// The name of a [`Multi`], copied freely.
#[derive(Clone, Copy)]
pub(crate) struct MultiId(ForeignId<MultiHandle>);

/// `CURLM_BAD_HANDLE`, what the library answers a `CURLM *` it was not
/// given, and what a `CURLM *` its owner gave up answers here.
const BAD_HANDLE: i64 = 1;

/// `CURLM_BAD_EASY_HANDLE`, what a `CURL *` its owner gave up answers.
const BAD_EASY: i64 = 2;

/// A `CURLM *` the library may call back from inside, held for one call
/// with the `CURL *`s it reaches and the shares they point at, the
/// request lent to the callbacks meanwhile ([`MultiId::call`]). It is the
/// only value `curl_multi_add_handle`, `curl_multi_remove_handle`,
/// `curl_multi_perform` and the free's `curl_multi_cleanup` are called
/// through. libcurl logs from inside the remove, `closing connection`
/// reaching the debug callable of the `CURL *` taken out, so a call made
/// inside [`MultiId::with`] would serve PHP code a second `&mut Request`
/// while the `&mut MultiHandle` borrowed from the first is live, over
/// objects that code could free.
/// `'h` is the span it is reached for.
#[derive(Clone, Copy)]
struct LiveMulti<'h> {
    raw: NonNull<sys::CURLM>,
    span: PhantomData<&'h mut MultiHandle>,
}

/// A `CURL *` held for the span of a [`LiveMulti`], what it adds and
/// removes.
#[derive(Clone, Copy)]
struct Member<'h> {
    raw: NonNull<sys::CURL>,
    span: PhantomData<&'h mut MultiHandle>,
}

impl<'h> LiveMulti<'h> {
    /// `curl_multi_add_handle`.
    fn add(self, easy: Member<'h>) -> i64 {
        // SAFETY: two objects held for the call; the multi does not
        // outlive the easy one's use, which `remove` or the easy's
        // cleanup ends.
        i64::from(unsafe { sys::curl_multi_add_handle(self.raw.as_ptr(), easy.raw.as_ptr()) })
    }

    /// `curl_multi_remove_handle`.
    fn remove(self, easy: Member<'h>) -> i64 {
        // SAFETY: two objects held for the call; the callbacks find the
        // request `enter` leaves.
        i64::from(unsafe { sys::curl_multi_remove_handle(self.raw.as_ptr(), easy.raw.as_ptr()) })
    }

    /// `curl_multi_cleanup`: whether the library cleaned the `CURLM *`
    /// up, which it refuses from inside one of its own callbacks.
    fn cleanup(self) -> bool {
        // SAFETY: an object held for the call; the easy handles in it are
        // detached, not freed, and the hold's owner reaches it no more
        // once this answers true.
        let code = unsafe { sys::curl_multi_cleanup(self.raw.as_ptr()) };
        i64::from(code) == c::CURLM_OK
    }

    /// `curl_multi_perform`: the code and the transfers still going.
    fn perform(self, running: c_int) -> (i64, i64) {
        let mut running = running;
        // SAFETY: an object held for the call and a live out-count; the
        // callbacks find the request `enter` leaves.
        let code = unsafe { sys::curl_multi_perform(self.raw.as_ptr(), &raw mut running) };
        (i64::from(code), i64::from(running))
    }
}

/// One `CURLMsg` of `curl_multi_info_read`: the message, the transfer's
/// code and which `CURL *` it is, with how many more are queued.
pub(crate) struct Done {
    pub(crate) msg: i64,
    pub(crate) result: i64,
    pub(crate) easy: Address,
    pub(crate) queued: i64,
}

impl Multi {
    /// `curl_multi_init()` for `object`, kept by the request.
    pub(crate) fn init(req: &mut Request, session: Session, object: PhpObject) -> Option<Multi> {
        // SAFETY: no argument.
        let raw = NonNull::new(unsafe { sys::curl_multi_init() })?;
        let tag = Box::into_raw(Box::new(Tag {
            session,
            object: Some(object),
        }));
        // SAFETY: `into_raw` never answers null.
        let tag = unsafe { NonNull::new_unchecked(tag) };
        Some(Multi(req.memory_mut().adopt(MultiHandle {
            raw,
            tag,
            cleaned: false,
        })))
    }

    pub(crate) fn id(&self) -> MultiId {
        MultiId(self.0.id())
    }

    /// The `CURLM *` given up, its object no longer served, as
    /// [`Easy::close`](super::Easy::close): cleaned up as it is dropped,
    /// where [`MultiId::cleanup`] has not cleaned it up already.
    pub(crate) fn close(self, req: &mut Request) -> Option<Thrown> {
        let session = req.memory_mut().foreign(self.0.id()).map(|handle| {
            let tag = handle.tag_mut();
            tag.object = None;
            tag.session.clone()
        })?;
        let handle = req.memory_mut().disown(self.0)?;
        let ((), thrown) = session.enter(req, || drop(handle));
        thrown
    }
}

impl MultiId {
    /// `f` over the `CURLM *` while its owner keeps it and it is not
    /// cleaned up, `gone` otherwise: for a call that serves no callback,
    /// which [`LiveMulti`] keeps out.
    fn with<R>(self, req: &mut Request, gone: R, f: impl FnOnce(&mut MultiHandle) -> R) -> R {
        let multi = req.memory_mut().foreign(self.0);
        multi.filter(|multi| !multi.cleaned).map_or(gone, f)
    }

    /// `curl_multi_cleanup(mh->multi)` as `curl_multi_free_obj` calls it,
    /// with the `CURLM *` still its owner's and so reachable by name, as
    /// php leaves `mh->multi` in place: a callable the cleanup calls back
    /// that makes a `curl_multi_*` call on this multi reaches the library,
    /// which answers `CURLM_RECURSIVE_API_CALL`. Made as [`MultiId::call`]
    /// makes every call that can call back, with the `CURL *`s of
    /// `easies` held, so a callable that gives one up meanwhile
    /// (`curl_multi_close`) leaves its cleanup to the call's end. What a
    /// callback threw is the answer; the owner's close then drops the
    /// tag alone.
    pub(crate) fn cleanup(self, req: &mut Request, easies: &[EasyId]) -> Option<Thrown> {
        let (cleaned, thrown) = self.call(req, easies, false, |multi, _| multi.cleanup());
        if let Some(multi) = req.memory_mut().foreign(self.0) {
            multi.cleaned = cleaned;
        }
        thrown
    }

    /// `f`, a call into the library that may call back, over the `CURLM *`
    /// as [`LiveMulti`] and each of `easies` its owner still keeps as a
    /// [`Member`], in order: the `CURLM *`, those `CURL *`s and the
    /// shares they point at held ([`Memory::holding`]), so a callback
    /// that frees one's object leaves the cleanup to the call's end, with
    /// no request lent ([`Session::holding`]), and the request lent to
    /// the callbacks meanwhile ([`Session::enter`]), what one threw
    /// handed back beside `f`'s answer; `gone` where the owner gave the
    /// `CURLM *` up. The one way the binding calls the library with a
    /// `CURLM *` that can call back, as [`EasyId::call`] is with a
    /// `CURL *`.
    ///
    /// [`Memory::holding`]: php_ext::request::Memory::holding
    fn call<R>(
        self,
        req: &mut Request,
        easies: &[EasyId],
        gone: R,
        f: impl for<'h> FnOnce(LiveMulti<'h>, &[Member<'h>]) -> R,
    ) -> (R, Option<Thrown>) {
        let Some(session) = self.with(req, None, |multi| Some(multi.tag().session.clone())) else {
            return (gone, None);
        };
        let (handles, shares) = EasyId::held_by(req, easies);
        let held = session.holding(req, self.0, |req, held| {
            let live = LiveMulti {
                raw: held.get(req)?.raw,
                span: PhantomData,
            };
            let mut body = Some(|req: &mut Request| {
                // Inside every hold, so each pointer stays live to the
                // call's end; one its owner gave up is not reached.
                let members: Vec<Member<'_>> = easies
                    .iter()
                    .filter_map(|easy| easy.raw(req))
                    .map(|raw| Member {
                        raw,
                        span: PhantomData,
                    })
                    .collect();
                session.enter(req, || f(live, &members))
            });
            let mut with_shares = Some(|req: &mut Request| holding_all(req, &shares, &mut body));
            holding_all(req, &handles, &mut with_shares).flatten()
        });
        match held.flatten() {
            Some(called) => called,
            None => (gone, None),
        }
    }

    /// `curl_multi_add_handle`, and what a callback threw during it.
    pub(crate) fn add(self, req: &mut Request, easy: EasyId) -> (i64, Option<Thrown>) {
        self.call(req, &[easy], BAD_HANDLE, |multi, members| match members {
            [easy] => multi.add(*easy),
            _ => BAD_EASY,
        })
    }

    /// `curl_multi_remove_handle`, and what a callback threw during it:
    /// libcurl's `closing connection` reaches the debug callable there.
    pub(crate) fn remove(self, req: &mut Request, easy: EasyId) -> (i64, Option<Thrown>) {
        self.call(req, &[easy], BAD_HANDLE, |multi, members| match members {
            [easy] => multi.remove(*easy),
            _ => BAD_EASY,
        })
    }

    /// `curl_multi_perform`, the request reachable by every transfer's
    /// callbacks, with the `CURLM *`, each of `easies` and the shares they
    /// point at held for the call: the code and the transfers still
    /// going.
    pub(crate) fn perform(
        self,
        req: &mut Request,
        easies: &[EasyId],
        running: c_int,
    ) -> ((i64, i64), Option<Thrown>) {
        let gone = (BAD_HANDLE, i64::from(running));
        self.call(req, easies, gone, |multi, _| multi.perform(running))
    }

    /// `curl_multi_wait(mh->multi, NULL, 0, timeout, &numfds)`, which
    /// waits without lending the request (#2595, `ffi`'s *Waits*).
    pub(crate) fn wait(self, req: &mut Request, timeout_ms: c_int) -> (i64, i64) {
        self.with(req, (BAD_HANDLE, 0), |multi| {
            let mut numfds: c_int = 0;
            // SAFETY: a live handle, no extra fds and a live out-count.
            let code = unsafe {
                sys::curl_multi_wait(multi.raw(), ptr::null_mut(), 0, timeout_ms, &raw mut numfds)
            };
            (i64::from(code), i64::from(numfds))
        })
    }

    /// `curl_multi_info_read`.
    pub(crate) fn info_read(self, req: &mut Request) -> Option<Done> {
        self.with(req, None, |multi| {
            let mut queued: c_int = 0;
            // SAFETY: a live handle and a live out-count.
            let msg = unsafe { sys::curl_multi_info_read(multi.raw(), &raw mut queued) };
            // SAFETY: null, or a message the library keeps until its next call.
            let msg = unsafe { msg.as_ref() }?;
            // The `data` union's `CURLcode result`, its first bytes on the
            // little-endian targets the engine is built for.
            #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
            let result = msg.data as usize as u32 as i32;
            Some(Done {
                msg: i64::from(msg.msg),
                result: i64::from(result),
                easy: Address::of(msg.easy_handle),
                queued: i64::from(queued),
            })
        })
    }

    /// `curl_multi_setopt(mh->multi, option, (long) value)`.
    pub(crate) fn set_long(self, req: &mut Request, option: i64, value: i64) -> i64 {
        self.with(req, BAD_HANDLE, |multi| {
            // SAFETY: a live handle and an option the library reads as a
            // `long`.
            i64::from(unsafe { sys::curl_multi_setopt(multi.raw(), opt(option), c_long(value)) })
        })
    }

    /// `CURLMOPT_PUSHFUNCTION` at `_php_server_push_callback`, its data
    /// the tag.
    pub(crate) fn set_push(self, req: &mut Request) -> i64 {
        self.with(req, BAD_HANDLE, |multi| {
            let f: sys::curl_push_callback = push;
            // SAFETY: a live handle and the trampoline.
            let code =
                unsafe { sys::curl_multi_setopt(multi.raw(), opt(c::CURLMOPT_PUSHFUNCTION), f) };
            if code != 0 {
                return i64::from(code);
            }
            // SAFETY: a live handle and the tag, live while the handle is.
            i64::from(unsafe {
                sys::curl_multi_setopt(multi.raw(), sys::CURLMOPT_PUSHDATA, arg(multi.tag))
            })
        })
    }
}

/// `_php_server_push_callback`: `CURL_PUSH_OK` or `CURL_PUSH_DENY`.
extern "C" fn push(
    parent: *mut sys::CURL,
    easy: *mut sys::CURL,
    count: usize,
    headers: *mut sys::curl_pushheaders,
    tag: *mut c_void,
) -> c_int {
    const DENY: c_int = 1;
    let tag: *mut Tag = tag.cast();
    if tag.is_null() {
        return DENY;
    }
    // SAFETY: the data `set_push` set, live while its handle is; the
    // field is read alone.
    let Some(object) = (unsafe { (*tag).object }) else {
        return DENY;
    };
    let lines: Vec<Vec<u8>> = (0..count)
        .map(|i| {
            // SAFETY: the push's headers, `count` of them.
            bytes_of(unsafe { sys::curl_pushheader_bynum(headers, i) }).unwrap_or_default()
        })
        .collect();
    // SAFETY: as above; the session field is never written after the tag
    // is made, and the clone is the pushed handle's own owner of it.
    let session = unsafe { (*tag).session.clone() };
    let link = session.lent();
    link.serve(DENY, DENY, |cx| {
        let made = |req: &mut Request, object| Easy::adopt(req, easy, session, object);
        if callbacks::push(cx, object, Address::of(parent), made, &lines) {
            0
        } else {
            DENY
        }
    })
}

/// `CURLSH *`, owned: cleaned up once, after every `CURL *` that
/// pointed at it let go (`curl_share_cleanup` refuses one still in use,
/// and the share is then left to the library).
pub(crate) struct ShareHandle {
    raw: NonNull<sys::CURLSH>,
}

impl Drop for ShareHandle {
    fn drop(&mut self) {
        // SAFETY: the live share this value owns, freed once.
        unsafe { sys::curl_share_cleanup(self.raw.as_ptr()) };
    }
}

// SAFETY: a request's share is the request's, kept in its table of
// foreign objects (ADR-0013); a persistent one is [`Pooled`]'s.
unsafe impl Send for ShareHandle {}

impl ShareHandle {
    /// `curl_share_init()`.
    fn init() -> Option<ShareHandle> {
        // SAFETY: no argument.
        let raw = NonNull::new(unsafe { sys::curl_share_init() })?;
        Some(ShareHandle { raw })
    }

    /// `curl_share_setopt(sh->share, option, (long) value)`.
    fn set_long(&self, option: i64, value: i64) -> i64 {
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let option = option as sys::CURLSHoption;
        // SAFETY: a live share and an option the library reads as a `long`.
        i64::from(unsafe { sys::curl_share_setopt(self.raw.as_ptr(), option, c_long(value)) })
    }
}

/// A request's `CURLSH *`'s one owner, what a `CurlShareHandle`'s payload
/// stores. A `CURL *` points at it while the `CurlHandle` keeps the share's
/// object, as php's `ch->share` keeps it, and a transfer holds it.
pub(crate) struct Share(Foreign<ShareHandle>);

/// The name of a [`Share`], copied freely.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ShareId(pub(super) ForeignId<ShareHandle>);

impl Share {
    /// `curl_share_init()`, kept by the request.
    pub(crate) fn init(req: &mut Request) -> Option<Share> {
        Some(Share(req.memory_mut().adopt(ShareHandle::init()?)))
    }

    /// Given up: cleaned up now, or as the transfer that holds it ends.
    pub(crate) fn close(self, req: &mut Request) {
        drop(req.memory_mut().disown(self.0));
    }

    /// The share as `CURLOPT_SHARE` points a `CURL *` at it.
    pub(crate) fn as_ref(&self) -> ShareRef {
        ShareRef::Request(ShareId(self.0.id()))
    }
}

impl ShareId {
    pub(super) fn raw(self, req: &mut Request) -> Option<*mut sys::CURLSH> {
        req.memory_mut()
            .foreign(self.0)
            .map(|share| share.raw.as_ptr())
    }
}

/// A persistent `CURLSH *`, `CURL_G(persistent_curlsh)`'s: not the
/// request's but the engine's pool's, lent to one request at a time
/// (ADR-0005's amendment). One owner is the pool's or the lending
/// request's, and one more is each `CURL *` pointing at it, so the count
/// says when nothing of the request that held it reaches it
/// ([`Persistent::alone`]). A `CurlSharePersistentHandle` owns none: it
/// names the lending request's by the options it was made with, and is
/// freed before the request gives its shares back.
#[derive(Clone)]
pub(crate) struct Persistent(Arc<Pooled>);

/// The share the pool keeps.
pub(crate) struct Pooled(ShareHandle);

// SAFETY: the pool lends a share to one request at a time and takes it
// back only once no handle of the request that held it remains
// (`Persistent::alone`), so libcurl never sees two threads in it at once.
unsafe impl Sync for Pooled {}

impl Persistent {
    /// `curl_share_init()`, for the pool.
    pub(crate) fn init() -> Option<Persistent> {
        Some(Persistent(Arc::new(Pooled(ShareHandle::init()?))))
    }

    /// The share where nothing else holds it, no `CURL *`:
    /// one that may be lent to another request, on another thread.
    pub(crate) fn alone(self) -> Option<Persistent> {
        (Arc::strong_count(&self.0) == 1).then_some(self)
    }

    pub(crate) fn set_long(&self, option: i64, value: i64) -> i64 {
        self.0.0.set_long(option, value)
    }

    pub(super) fn raw(&self) -> *mut sys::CURLSH {
        self.0.0.raw.as_ptr()
    }
}

/// A share as `CURLOPT_SHARE` points a `CURL *` at it.
#[derive(Clone)]
pub(crate) enum ShareRef {
    Request(ShareId),
    Persistent(Persistent),
}

impl ShareRef {
    /// `curl_share_setopt(sh->share, option, (long) value)`;
    /// `CURLSHE_BAD_OPTION` where the share is gone.
    pub(crate) fn set_long(&self, req: &mut Request, option: i64, value: i64) -> i64 {
        match self {
            ShareRef::Request(id) => req
                .memory_mut()
                .foreign(id.0)
                .map_or(1, |share| share.set_long(option, value)),
            ShareRef::Persistent(persistent) => persistent.set_long(option, value),
        }
    }
}
