//! `php_zip_archive` (php 8.5.12, GH-17787 and GH-23276): the open
//! archive a `ZipArchive` and each stream over one of its entries hold
//! together, by count, so freeing the object leaves the archive open
//! under its streams and the last of them to close writes its changes.
//! A stream holds the archive and not the object, so an object holding
//! its own stream is garbage as php collects it. The holder keeps what
//! libzip keeps for the archive besides: its stdio `FILE`, which the
//! archive and its streams read through, the progress and cancel
//! callbacks, the count of streams reading the archive's own file, and
//! php's `close` member, which refuses a second close from a callback the
//! first serves. Under its close the archive stays in its holder, as
//! php's `archive->za` stays live under `zip_close`, so a callback reaches
//! it as it reaches an open one.

use std::collections::HashMap;

use php_ext::call::call;
use php_ext::coerce::value_name;
use php_ext::raising::{Level, Thrown, docref, leave_pending, type_error, while_pending};
use php_ext::request::Request;
use php_ext::values::{Owner, Value, ValueRef};

use php_ext::abi::Callable;

use crate::archive::Archive;
use crate::c;
use crate::error::{Er, Status};
use crate::read::{Reader, Stdio};
use crate::write::{self, Program};

/// A callable kept past the call that registered it, `zend_fcc_dup`.
pub(crate) type Kept = Box<Owner<Callable>>;

/// An archive's number in the request, which its holders reach it by.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct ArchiveId(u64);

/// Where a holder's archive stands.
enum Za {
    /// Open, with its stdio `FILE`.
    Open(Box<Archive>, Option<Stdio>),
    /// Under its close, php's `close` member set, with its `FILE` lent to
    /// it for the close's length.
    Closing(Box<Archive>),
    /// Closed under the streams that still hold it, whose reads fail as
    /// libzip's invalidated sources fail.
    Closed,
}

/// `php_zip_archive`.
struct Holder {
    za: Za,
    refcount: u32,
    /// The streams whose `zip_file_t` reads the archive's own file, each
    /// one of libzip's opens of `za->src` past the archive's own.
    readers: u32,
    progress: Option<Kept>,
    cancel: Option<Kept>,
}

/// One of the two callbacks a close serves.
#[derive(Clone, Copy)]
pub(crate) enum Callback {
    Progress,
    Cancel,
}

/// What a close that fails does with the archive: `zip_discard` it, as
/// `ZipArchive::close()` and the last release do, or keep it open, as
/// `ZipArchive::open()` keeps the archive it was to close first.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum OnFailure {
    Discard,
    Keep,
}

/// What a close came to: the archive's error where it failed to write,
/// and what a callback threw meanwhile.
pub(crate) struct Closed {
    pub(crate) error: Option<Status>,
    pub(crate) thrown: Option<Thrown>,
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

fn get(req: &mut Request, id: ArchiveId) -> Option<&mut Holder> {
    holders(req).by_id.get_mut(&id)
}

/// `php_zip_archive_create`: `za` held once, by the object opening it.
pub(crate) fn create(req: &mut Request, mut za: Archive) -> ArchiveId {
    let file = za.data.as_mut().and_then(|d| d.stdio.take());
    let all = holders(req);
    all.last += 1;
    let id = ArchiveId(all.last);
    let holder = Holder {
        za: Za::Open(Box::new(za), file),
        refcount: 1,
        readers: 0,
        progress: None,
        cancel: None,
    };
    all.by_id.insert(id, holder);
    id
}

/// A stream's count of archive `id` (`self->archive`), and whether its
/// `zip_file_t` reads the archive's own file, which a close's commit is
/// refused under: taken by [`hold`] once the stream is made, given back
/// by [`Hold::let_go`].
pub(crate) struct Hold {
    pub(crate) id: ArchiveId,
    reads_file: bool,
}

impl Hold {
    pub(crate) fn new(id: ArchiveId, reads_file: bool) -> Hold {
        Hold { id, reads_file }
    }

    /// The hold given back: the stream's `zip_fclose`, then its count
    /// released.
    pub(crate) fn let_go(self, req: &mut Request) {
        if let Some(h) = get(req, self.id) {
            h.readers = h.readers.saturating_sub(u32::from(self.reads_file));
        }
        release(req, self.id);
    }
}

/// `php_zip_archive_addref` for a stream of archive `id`, counted among
/// its readers where it reads the archive's own file.
pub(crate) fn hold(req: &mut Request, id: ArchiveId, reads_file: bool) {
    if let Some(h) = get(req, id) {
        h.refcount += 1;
        h.readers += u32::from(reads_file);
    }
}

/// Archive `id` where it is open, or under its close.
pub(crate) fn archive(req: &mut Request, id: ArchiveId) -> Option<&mut Archive> {
    match &mut get(req, id)?.za {
        Za::Open(za, _) | Za::Closing(za) => Some(za),
        Za::Closed => None,
    }
}

/// php's `close` member: whether archive `id` is under its close.
pub(crate) fn closing(req: &mut Request, id: ArchiveId) -> bool {
    get(req, id).is_some_and(|h| matches!(h.za, Za::Closing(_)))
}

/// `op` on archive `id` with its `FILE` lent to it, which it holds
/// already under its close; `None` where it is closed.
pub(crate) fn with_file<T>(
    req: &mut Request,
    id: ArchiveId,
    op: impl FnOnce(&mut Archive) -> T,
) -> Option<T> {
    match &mut get(req, id)?.za {
        Za::Open(za, file) => {
            za.lend(file);
            let out = op(za);
            za.lend(file);
            Some(out)
        }
        Za::Closing(za) => Some(op(za)),
        Za::Closed => None,
    }
}

/// `zip_fread` on an entry of archive `id` through its `FILE`, which
/// fails with `ZIP_ER_ZIPCLOSED` once the archive is closed. Under its
/// close the read is one a callback makes, through the `FILE` the close
/// holds.
pub(crate) fn read(
    req: &mut Request,
    id: ArchiveId,
    zf: &mut Reader,
    len: usize,
) -> Result<Vec<u8>, Status> {
    match get(req, id).map(|h| &mut h.za) {
        Some(Za::Open(_, file)) => zf.read_through(len, file),
        Some(Za::Closing(za)) => za.read_entry(zf, len),
        Some(Za::Closed) | None => {
            *zf = Reader::closed();
            zf.read(len)
        }
    }
}

/// Callback `which` of archive `id` replaced by `kept`, the one it
/// displaces handed back to be given up, or `kept` itself where the
/// archive has no holder.
pub(crate) fn keep(req: &mut Request, id: ArchiveId, which: Callback, kept: Kept) -> Option<Kept> {
    let Some(h) = get(req, id) else {
        return Some(kept);
    };
    match which {
        Callback::Progress => h.progress.replace(kept),
        Callback::Cancel => h.cancel.replace(kept),
    }
}

/// `zip_close` on archive `id` where it is open, its callbacks served
/// under php's `close` member with the archive in its holder and its
/// `FILE` lent to it. Written or discarded, the archive is closed and
/// libzip's callback state freed with it; under [`OnFailure::Keep`] one
/// that failed to write stays open. `None` where the archive is not open.
///
/// A callback reaches the archive being written, and a stream of the
/// archive's own data open at the commit fails it, as libzip refuses
/// `zip_source_commit_write` while another `zip_file_t` reads the file:
///
/// ```php
/// $z = new ZipArchive; $z->open('live-witness.zip', ZipArchive::CREATE | ZipArchive::OVERWRITE); $z->addFromString('a', str_repeat('a', 9000)); $z->close();
/// $z->open('live-witness.zip'); $z->addFromString('b', 'bee'); $s = $z->getStream('a');
/// $z->registerProgressCallback(0.5, function ($r) use ($z, $s) { echo "progress $r\n"; static $d; if ($d) return; $d = 1;
///   try { var_dump($z->numFiles, $z->count(), strlen($z->getFromName('a')), strlen(fread($s, 10))); } catch (Throwable $e) { echo get_class($e), ': ', $e->getMessage(), "\n"; } });
/// var_dump($z->close(), $z->status); fclose($s);
/// $z->open('live-witness.zip'); var_dump($z->numFiles); $z->addFromString('c', 'cee'); $s = $z->getStream('a');
/// $z->registerProgressCallback(0.5, function ($r) use ($z, &$s) { if ($s) { fclose($s); $s = null; $z->addFromString('late', 'x'); echo "closed at $r\n"; } });
/// var_dump($z->close()); $z->open('live-witness.zip'); var_dump($z->numFiles); $z->close(); unlink('live-witness.zip');
/// ```
/// ```expect
/// progress 0
/// int(2)
/// int(2)
/// int(9000)
/// int(10)
/// progress 1
///
/// Warning: ZipArchive::close(): Resource still in use in %s on line %d
/// bool(false)
/// int(29)
/// int(1)
/// closed at 0
/// bool(true)
/// int(2)
/// ```
pub(crate) fn close(req: &mut Request, id: ArchiveId, on_failure: OnFailure) -> Option<Closed> {
    let h = get(req, id)?;
    let Za::Open(mut za, mut file) = std::mem::replace(&mut h.za, Za::Closed) else {
        return None;
    };
    za.lend(&mut file);
    h.za = Za::Closing(za);
    let umask = req.environment().umask();
    let mut serving = Serving {
        req,
        id,
        thrown: None,
    };
    let done = write::close(&mut serving, umask);
    let thrown = serving.thrown;
    // The archive the close left in its holder, taken out to be closed
    // or kept.
    let taken = get(req, id).map(|h| (std::mem::replace(&mut h.za, Za::Closed), h));
    let Some((Za::Closing(mut za), h)) = taken else {
        let error = Some(Status::new(Er::Internal));
        return Some(Closed { error, thrown });
    };
    let error = done.err().map(|_| za.error);
    let mut file = None;
    za.lend(&mut file);
    if error.is_some() && on_failure == OnFailure::Keep {
        h.za = Za::Open(za, file);
        return Some(Closed { error, thrown });
    }
    let kept = [h.progress.take(), h.cancel.take()];
    for k in kept.into_iter().flatten() {
        k.give_up(req);
    }
    Some(Closed { error, thrown })
}

/// A diagnostic raised beside what a close's callback threw, which stays
/// pending.
pub(crate) fn diagnose(req: &mut Request, thrown: Option<&Thrown>, level: Level, text: &str) {
    match thrown {
        Some(t) => {
            while_pending(req, t, |req| docref(req, level, text));
        }
        None => docref(req, level, text),
    }
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
    // A count at zero is its last release's, under way.
    let Some(left) = h.refcount.checked_sub(1) else {
        return;
    };
    h.refcount = left;
    if left != 0 {
        return;
    }
    if let Some(closed) = close(req, id, OnFailure::Discard) {
        if let Some(error) = closed.error {
            let text = format!("Cannot destroy the zip context: {}", error.text(req));
            diagnose(req, closed.thrown.as_ref(), Level::Warning, &text);
        }
        if let Some(thrown) = closed.thrown {
            leave_pending(req, thrown);
        }
    }
    if let Some(h) = holders(req).by_id.remove(&id) {
        for k in [h.progress, h.cancel].into_iter().flatten() {
            k.give_up(req);
        }
    }
}

/// A close under way on archive `id`: the request its callbacks are
/// served with, each read from the holder as it is called, so one a
/// callback registers serves the next point, and the first throwable
/// either left.
struct Serving<'r> {
    req: &'r mut Request,
    id: ArchiveId,
    thrown: Option<Thrown>,
}

impl Serving<'_> {
    /// Callback `which` as the holder keeps it now.
    fn callback(&mut self, which: Callback) -> Option<Callable> {
        let h = get(self.req, self.id)?;
        let kept = match which {
            Callback::Progress => &h.progress,
            Callback::Cancel => &h.cancel,
        };
        kept.as_deref().map(|k| (**k).clone())
    }
}

impl Program for Serving<'_> {
    fn archive(&mut self) -> Option<&mut Archive> {
        match &mut get(self.req, self.id)?.za {
            Za::Closing(za) => Some(za),
            Za::Open(..) | Za::Closed => None,
        }
    }

    fn in_use(&mut self) -> bool {
        get(self.req, self.id).is_some_and(|h| h.readers > 0)
    }

    /// `php_zip_progress_callback`.
    fn progress(&mut self, current: f64) {
        if self.thrown.is_some() {
            return;
        }
        let Some(cb) = self.callback(Callback::Progress) else {
            return;
        };
        match call(self.req, &cb, &[Value::float(current)]) {
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
        let Some(cb) = self.callback(Callback::Cancel) else {
            return false;
        };
        let v = match call(self.req, &cb, &[]) {
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
