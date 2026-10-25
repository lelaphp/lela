//! `php_zip_archive` (php 8.5.12, GH-17787 and GH-23276): the open
//! archive a `ZipArchive` and each stream over one of its entries hold
//! together, by count, so freeing the object leaves the archive open
//! under its streams and the last of them to close writes its changes.
//! A stream holds the archive and not the object, so an object holding
//! its own stream is garbage as php collects it. The holder keeps what
//! libzip keeps for the archive besides: its stdio `FILE`, which the
//! archive and its streams read through, the progress and cancel
//! callbacks, and php's `close` member, which refuses a second close from
//! a callback the first serves.

use std::collections::HashMap;

use php_ext::call::call;
use php_ext::coerce::value_name;
use php_ext::raising::{Level, Thrown, docref, leave_pending, type_error, while_pending};
use php_ext::request::Request;
use php_ext::values::{Owner, Value, ValueRef};

use php_ext::abi::Callable;

use crate::archive::Archive;
use crate::c;
use crate::error::Status;
use crate::read::{Reader, Stdio};
use crate::write::Hooks;

/// A callable kept past the call that registered it, `zend_fcc_dup`.
pub(crate) type Kept = Box<Owner<Callable>>;

/// An archive's number in the request, which its holders reach it by.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct ArchiveId(u64);

/// `php_zip_archive`.
pub(crate) struct Holder {
    /// `za`: the archive, out of the holder for the length of its close,
    /// and `None` once closed under the streams that still hold it,
    /// whose reads then fail as libzip's invalidated sources fail.
    pub(crate) za: Option<Box<Archive>>,
    /// The archive's stdio `FILE`.
    file: Option<Stdio>,
    refcount: u32,
    /// `close`: a close of the archive is under way.
    pub(crate) closing: bool,
    pub(crate) progress: Option<Kept>,
    pub(crate) cancel: Option<Kept>,
}

/// Request-scoped (ADR-0005): the open archives by number.
#[derive(Default)]
pub(crate) struct Holders {
    last: u64,
    by_id: HashMap<ArchiveId, Holder>,
}

fn holders(req: &mut Request) -> &mut Holders {
    &mut req.state::<crate::State>().holders
}

/// `php_zip_archive_create`: `za` held once, by the object opening it.
pub(crate) fn create(req: &mut Request, mut za: Archive) -> ArchiveId {
    let file = za.data.as_mut().and_then(|d| d.stdio.take());
    let all = holders(req);
    all.last += 1;
    let id = ArchiveId(all.last);
    let holder = Holder {
        za: Some(Box::new(za)),
        file,
        refcount: 1,
        closing: false,
        progress: None,
        cancel: None,
    };
    all.by_id.insert(id, holder);
    id
}

/// The holder of archive `id`.
pub(crate) fn get(req: &mut Request, id: ArchiveId) -> Option<&mut Holder> {
    holders(req).by_id.get_mut(&id)
}

/// `php_zip_archive_addref`.
pub(crate) fn addref(req: &mut Request, id: ArchiveId) {
    if let Some(h) = get(req, id) {
        h.refcount += 1;
    }
}

/// `op` on archive `id` with its `FILE` lent to it; `None` where it is
/// closed.
pub(crate) fn with_file<T>(
    req: &mut Request,
    id: ArchiveId,
    op: impl FnOnce(&mut Archive) -> T,
) -> Option<T> {
    let h = get(req, id)?;
    let za = h.za.as_deref_mut()?;
    za.lend(&mut h.file);
    let out = op(za);
    za.lend(&mut h.file);
    Some(out)
}

/// `zip_fread` on an entry of archive `id` through its `FILE`, which
/// fails with `ZIP_ER_ZIPCLOSED` from the archive's close on.
pub(crate) fn read(
    req: &mut Request,
    id: ArchiveId,
    zf: &mut Reader,
    len: usize,
) -> Result<Vec<u8>, Status> {
    if let Some(Holder {
        za: Some(_), file, ..
    }) = get(req, id)
    {
        return zf.read_through(len, file);
    }
    *zf = Reader::closed();
    zf.read(len)
}

/// `zip_close` on archive `id`, taken out of its holder as `za`, with
/// its callbacks served with php's `close` member set; the throwable one of
/// them left. The `FILE` is lent to the archive for the close and put
/// back after.
pub(crate) fn close(
    req: &mut Request,
    id: ArchiveId,
    za: &mut Archive,
) -> (Result<(), ()>, Option<Thrown>) {
    let (progress, cancel, mut file) = match get(req, id) {
        Some(h) => {
            h.closing = true;
            (
                h.progress.as_deref().map(|k| (**k).clone()),
                h.cancel.as_deref().map(|k| (**k).clone()),
                h.file.take(),
            )
        }
        None => (None, None, None),
    };
    let umask = req.environment().umask();
    za.lend(&mut file);
    let mut hooks = PhpHooks {
        req,
        progress,
        cancel,
        thrown: None,
    };
    let result = za.close(&mut hooks, umask).map_err(|_| ());
    let thrown = hooks.thrown;
    za.lend(&mut file);
    if let Some(h) = get(req, id) {
        h.closing = false;
        h.file = file;
    }
    (result, thrown)
}

/// `php_zip_object_detach_archive` past the close: the archive gone from
/// its holder, libzip's callback state freed with it, and the object's
/// count released.
pub(crate) fn detach(req: &mut Request, id: ArchiveId) {
    let kept = get(req, id).map(|h| {
        h.za = None;
        h.file = None;
        (h.progress.take(), h.cancel.take())
    });
    for k in kept.into_iter().flat_map(|(p, c)| [p, c]).flatten() {
        k.give_up(req);
    }
    release(req, id);
}

/// `php_zip_archive_release`: one count let go, and with the last an
/// archive still open closed, its changes written, php's warning where
/// that fails. What a progress or cancel callback threw meanwhile is
/// left pending, for whatever released it to raise.
///
/// An object freed under two streams of its archive leaves the archive
/// open, its change and its callback kept, until the second closes
/// (GH-17787); a stream the archive was closed under reads nothing:
///
/// ```php
/// $z = new ZipArchive; $z->open('held-witness.zip', ZipArchive::CREATE | ZipArchive::OVERWRITE);
/// $z->addFromString('a', str_repeat('a', 9000)); $z->close();
/// $z->open('held-witness.zip'); $z->addFromString('b', 'bee');
/// $z->registerProgressCallback(0.5, function ($r) { echo "progress $r\n"; });
/// $s = $z->getStream('a'); $t = $z->getStream('a'); $w = WeakReference::create($z); $z = null;
/// var_dump($w->get(), strlen(stream_get_contents($s)));
/// fclose($s); echo "one closed\n"; var_dump(strlen(fread($t, 10)));
/// fclose($t); echo "both closed\n";
/// $z = new ZipArchive; $z->open('held-witness.zip'); var_dump($z->numFiles, $z->getFromName('b'));
/// $s = $z->getStream('a'); $z->close(); var_dump(@fread($s, 10)); fclose($s); unlink('held-witness.zip');
/// ```
/// ```expect
/// NULL
/// int(9000)
/// one closed
/// int(10)
/// progress 0
/// progress 1
/// both closed
/// int(2)
/// string(3) "bee"
/// bool(false)
/// ```
pub(crate) fn release(req: &mut Request, id: ArchiveId) {
    let Some(h) = get(req, id) else {
        return;
    };
    h.refcount = h.refcount.saturating_sub(1);
    if h.refcount != 0 {
        return;
    }
    let mut za = h.za.take();
    let mut thrown = None;
    if let Some(za) = za.as_deref_mut() {
        let (result, t) = close(req, id, za);
        if result.is_err() {
            let text = za.error.text(req);
            let text = format!("Cannot destroy the zip context: {text}");
            match &t {
                Some(t) => {
                    while_pending(req, t, |req| docref(req, Level::Warning, &text));
                }
                None => docref(req, Level::Warning, &text),
            }
        }
        thrown = t;
    }
    drop(za);
    if let Some(h) = holders(req).by_id.remove(&id) {
        for k in [h.progress, h.cancel].into_iter().flatten() {
            k.give_up(req);
        }
    }
    if let Some(thrown) = thrown {
        leave_pending(req, thrown);
    }
}

/// The callbacks a close serves: the progress and cancel callables, and
/// the first throwable either left.
struct PhpHooks<'r> {
    req: &'r mut Request,
    progress: Option<Callable>,
    cancel: Option<Callable>,
    thrown: Option<Thrown>,
}

impl Hooks for PhpHooks<'_> {
    /// `php_zip_progress_callback`.
    fn progress(&mut self, current: f64) {
        let (Some(cb), None) = (&self.progress, &self.thrown) else {
            return;
        };
        match call(self.req, cb, &[Value::float(current)]) {
            Ok(v) => v.give_up(self.req),
            Err(t) => self.thrown = Some(t),
        }
    }

    /// `php_zip_cancel_callback`: a throw cancels, and so does a value
    /// that is no integer, after php's `TypeError`.
    fn cancel(&mut self) -> bool {
        if self.thrown.is_some() {
            return true;
        }
        let Some(cb) = &self.cancel else {
            return false;
        };
        let v = match call(self.req, cb, &[]) {
            Ok(v) => v,
            Err(t) => {
                self.thrown = Some(t);
                return true;
            }
        };
        let long = match v.deref().as_ref() {
            ValueRef::Int(n) => Some(n),
            ValueRef::Bool(b) => Some(i64::from(b)),
            ValueRef::Null => Some(0),
            ValueRef::Float(f) if f.is_finite() => Some(c::float_long(f)),
            ValueRef::Str(s) => std::str::from_utf8(s.as_bytes())
                .ok()
                .and_then(|t| t.trim().parse::<i64>().ok()),
            _ => None,
        };
        let Some(long) = long else {
            let name = value_name(self.req, &v);
            v.give_up(self.req);
            let message = format!(
                "Return value of callback provided to ZipArchive::registerCancelCallback() must be of type int, {name} returned"
            );
            self.thrown = Some(type_error(self.req, &message));
            return true;
        };
        v.give_up(self.req);
        c::i32(long) != 0
    }
}
