//! The `CURLM *` and the `CURLSH *`: `CURLM *` with the push
//! trampoline `multi.c` installs, and `CURLSH *`.

use std::ffi::{c_int, c_void};
use std::ptr::{self, NonNull};
use std::sync::Arc;

use php_ext::raising::Thrown;
use php_ext::request::Request;
use php_ext::values::PhpObject;

use super::header as h;
use super::sys::{self, PushHeaders, curl_pushheader_bynum};
use super::{Easy, EasyId, Session, arg, bytes_of, c_long};
use crate::constants as c;
use crate::multi as callbacks;

/// A multi option number as the library's `CURLMoption`.
#[allow(clippy::cast_possible_truncation)]
const fn opt(option: i64) -> c_int {
    option as c_int
}

/// What the push trampoline is handed: the request's link and the
/// `CurlMultiHandle`.
struct Tag {
    session: Session,
    object: Option<PhpObject>,
}

struct Inner {
    raw: NonNull<sys::CURLM>,
    tag: NonNull<Tag>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        // SAFETY: the live handle this value owns, cleaned up once; the
        // easy handles in it are detached, not freed.
        unsafe { sys::curl_multi_cleanup(self.raw.as_ptr()) };
        // SAFETY: the tag `init` leaked, freed after the handle.
        drop(unsafe { Box::from_raw(self.tag.as_ptr()) });
    }
}

// SAFETY: as for `Session`: one request's.
unsafe impl Send for Inner {}
// SAFETY: as for `Session`.
unsafe impl Sync for Inner {}

/// `CURLM *`, one owner of it.
#[derive(Clone)]
pub(crate) struct Multi(Arc<Inner>);

/// One `CURLMsg` of `curl_multi_info_read`: the message, the transfer's
/// code and which `CURLM *` it is, with how many more are queued.
pub(crate) struct Done {
    pub(crate) msg: i64,
    pub(crate) result: i64,
    pub(crate) easy: EasyId,
    pub(crate) queued: i64,
}

impl Multi {
    /// `curl_multi_init()` for `object`.
    pub(crate) fn init(session: Session, object: PhpObject) -> Option<Multi> {
        // SAFETY: no argument.
        let raw = NonNull::new(unsafe { sys::curl_multi_init() })?;
        let tag = Box::into_raw(Box::new(Tag {
            session,
            object: Some(object),
        }));
        // SAFETY: `into_raw` never answers null.
        let tag = unsafe { NonNull::new_unchecked(tag) };
        Some(Multi(Arc::new(Inner { raw, tag })))
    }

    fn raw(&self) -> *mut sys::CURLM {
        self.0.raw.as_ptr()
    }

    fn session(&self) -> &Session {
        // SAFETY: a live tag whose session field is written only when the
        // tag is made.
        unsafe { &(*self.0.tag.as_ptr()).session }
    }

    /// The `CURLM *`'s object no longer served: `curl_multi_free_obj` has
    /// begun.
    pub(crate) fn forget_object(&self) {
        // SAFETY: a live tag; the field is written alone.
        unsafe { (*self.0.tag.as_ptr()).object = None };
    }

    /// `curl_multi_add_handle`.
    pub(crate) fn add(&self, easy: &Easy) -> i64 {
        // SAFETY: two live handles; the multi does not outlive the easy
        // one's use, which `remove` or the easy's cleanup ends.
        i64::from(unsafe { sys::curl_multi_add_handle(self.raw(), easy.raw()) })
    }

    /// `curl_multi_remove_handle`.
    pub(crate) fn remove(&self, easy: &Easy) -> i64 {
        // SAFETY: two live handles.
        i64::from(unsafe { sys::curl_multi_remove_handle(self.raw(), easy.raw()) })
    }

    /// `curl_multi_perform`, the request reachable by every transfer's
    /// callbacks: the code and the transfers still running.
    pub(crate) fn perform(
        &self,
        req: &mut Request,
        running: c_int,
    ) -> ((i64, i64), Option<Thrown>) {
        let raw = self.raw();
        self.session().enter(req, || {
            let mut running = running;
            // SAFETY: a live handle and a live out-count; the callbacks
            // find the request `enter` leaves.
            let code = unsafe { sys::curl_multi_perform(raw, &raw mut running) };
            (i64::from(code), i64::from(running))
        })
    }

    /// `curl_multi_wait(mh->multi, NULL, 0, timeout, &numfds)`.
    pub(crate) fn wait(&self, timeout_ms: c_int) -> (i64, i64) {
        let mut numfds: c_int = 0;
        // SAFETY: a live handle, no extra fds and a live out-count.
        let code = unsafe {
            sys::curl_multi_wait(self.raw(), ptr::null_mut(), 0, timeout_ms, &raw mut numfds)
        };
        (i64::from(code), i64::from(numfds))
    }

    /// `curl_multi_info_read`.
    pub(crate) fn info_read(&self) -> Option<Done> {
        let mut queued: c_int = 0;
        // SAFETY: a live handle and a live out-count.
        let msg = unsafe { sys::curl_multi_info_read(self.raw(), &raw mut queued) };
        // SAFETY: null, or a message the library keeps until its next call.
        let msg = unsafe { msg.as_ref() }?;
        // The `data` union's `CURLcode result`, its first bytes on the
        // little-endian targets the engine is built for.
        #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)]
        let result = msg.data as usize as u32 as i32;
        Some(Done {
            msg: i64::from(msg.msg),
            result: i64::from(result),
            easy: EasyId::of(msg.easy_handle),
            queued: i64::from(queued),
        })
    }

    /// `curl_multi_setopt(mh->multi, option, (long) value)`.
    pub(crate) fn set_long(&self, option: i64, value: i64) -> i64 {
        // SAFETY: a live handle and an option the library reads as a
        // `long`.
        i64::from(unsafe { sys::curl_multi_setopt(self.raw(), opt(option), c_long(value)) })
    }

    /// `CURLMOPT_PUSHFUNCTION` at `_php_server_push_callback`, its data
    /// the tag.
    pub(crate) fn set_push(&self) -> i64 {
        let f: sys::PushFn = push;
        // SAFETY: a live handle and the trampoline.
        let code = unsafe { sys::curl_multi_setopt(self.raw(), opt(c::CURLMOPT_PUSHFUNCTION), f) };
        if code != 0 {
            return i64::from(code);
        }
        // SAFETY: a live handle and the tag, live while the handle is.
        i64::from(unsafe {
            sys::curl_multi_setopt(self.raw(), opt(h::CURLMOPT_PUSHDATA), arg(self.0.tag))
        })
    }

    /// The `CURLM *` let go with the request reachable, as [`Easy::close`].
    pub(crate) fn close(self, req: &mut Request) -> Option<Thrown> {
        let session = self.session().clone();
        let ((), thrown) = session.enter(req, || drop(self));
        thrown
    }
}

/// `_php_server_push_callback`: `CURL_PUSH_OK` or `CURL_PUSH_DENY`.
extern "C" fn push(
    parent: *mut sys::CURL,
    easy: *mut sys::CURL,
    count: usize,
    headers: *mut PushHeaders,
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
            bytes_of(unsafe { curl_pushheader_bynum(headers, i) }).unwrap_or_default()
        })
        .collect();
    // SAFETY: as above; the session field is never written after the tag
    // is made.
    let session = unsafe { &(*tag).session };
    session.serve(DENY, DENY, |cx| {
        let made = |object| Easy::adopt(easy, session.clone(), object);
        if callbacks::push(cx, object, EasyId::of(parent), made, &lines) {
            0
        } else {
            DENY
        }
    })
}

/// `CURLSH *`.
pub(crate) struct ShareInner {
    pub(super) raw: NonNull<sys::CURLSH>,
}

impl Drop for ShareInner {
    fn drop(&mut self) {
        // SAFETY: the live share this value owns, freed once, after every
        // easy handle that held it let go.
        unsafe { sys::curl_share_cleanup(self.raw.as_ptr()) };
    }
}

// SAFETY: as for `Session`: one request's.
unsafe impl Send for ShareInner {}
// SAFETY: as for `Session`.
unsafe impl Sync for ShareInner {}

/// A `CURLSH *`, held by its object and by every `CURL *` pointed
/// at it.
#[derive(Clone)]
pub(crate) struct Share(pub(super) Arc<ShareInner>);

impl Share {
    /// `curl_share_init()`.
    pub(crate) fn init() -> Option<Share> {
        // SAFETY: no argument.
        let raw = NonNull::new(unsafe { sys::curl_share_init() })?;
        Some(Share(Arc::new(ShareInner { raw })))
    }

    /// `curl_share_setopt(sh->share, option, (long) value)`.
    pub(crate) fn set_long(&self, option: i64, value: i64) -> i64 {
        #[allow(clippy::cast_possible_truncation)]
        let option = option as c_int;
        // SAFETY: a live share and an option the library reads as a `long`.
        i64::from(unsafe { sys::curl_share_setopt(self.0.raw.as_ptr(), option, c_long(value)) })
    }
}
