//! `mysqlnd_alloc.c`: `mnd_pemalloc` and its kin, an allocation on the
//! request's heap for what a request's connection and its statements
//! hold, and `malloc`'s for a persistent connection's, which no request's
//! `memory_get_usage()` counts (design §3.1, §5.13); and the sizes of the
//! structures php allocates through them.

use php_ext::raising::Thrown;
use php_ext::request::Request;
use php_ext::values::Emalloc;

/// `mysqlnd_plugin_count() * sizeof(void *)`: a pointer of room for each
/// name `php --ri mysqlnd` lists as loaded, which every extensible
/// structure carries past its own size (`PLUGIN_ROOM`).
pub(crate) const PLUGIN_ROOM: usize = 8 * listed(crate::auth::Plugin::LOADED);

/// `sizeof(MYSQLND)`.
pub(crate) const MYSQLND: usize = 24;
/// `sizeof(MYSQLND_CONN_DATA)`.
pub(crate) const CONN_DATA: usize = 1088;
/// `sizeof(MYSQLND_STATS)` and its `STAT_LAST` counters, one allocation.
pub(crate) const STATS: usize = 8 + 8 * crate::stats::COUNT;
/// `ZEND_MM_ALIGNED_SIZE(sizeof(MYSQLND_PFC) + room)` and
/// `sizeof(MYSQLND_PFC_DATA) + room`, `get_pfc`'s one allocation.
pub(crate) const PFC: usize = 32 + PLUGIN_ROOM + 128 + PLUGIN_ROOM;
/// `get_vio`'s `MYSQLND_VIO` and `MYSQLND_VIO_DATA`, likewise.
pub(crate) const VIO: usize = 16 + PLUGIN_ROOM + 248 + PLUGIN_ROOM;
/// `sizeof(MYSQLND_PROTOCOL_PAYLOAD_DECODER_FACTORY)`.
pub(crate) const PAYLOAD_DECODER_FACTORY: usize = 176;
/// `sizeof(MYSQLND_STMT)`.
pub(crate) const STMT: usize = 16;
/// `sizeof(MYSQLND_STMT_DATA)`, which carries no `PLUGIN_ROOM`.
pub(crate) const STMT_DATA: usize = 800;
/// `sizeof(MYSQLND_PARAM_BIND)`, and `sizeof(MYSQLND_RESULT_BIND)`.
pub(crate) const BIND: usize = 24;

/// How many names a comma-separated list holds.
const fn listed(names: &str) -> usize {
    let bytes = names.as_bytes();
    let mut n = 1;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b',' {
            n += 1;
        }
        i += 1;
    }
    n
}

/// One `mnd_pecalloc(1, len, persistent)`: `emalloc`'s for a request's
/// connection, counted on the request's heap until [`Alloc::free`] gives
/// it back where php calls `mnd_pefree`; `malloc`'s for a persistent one,
/// which outlives every request and which none counts, kept as its length
/// alone.
#[derive(Debug)]
pub(crate) enum Alloc {
    Heap(Emalloc),
    Malloc(usize),
}

impl Alloc {
    /// `mnd_pecalloc(1, len, persistent)`; the `Err` is the memory limit's
    /// fatal.
    pub(crate) fn new(req: &mut Request, len: usize, persistent: bool) -> Result<Alloc, Thrown> {
        Ok(if persistent {
            Alloc::Malloc(len)
        } else {
            Alloc::Heap(Emalloc::new(req, len)?)
        })
    }

    /// The bytes asked for.
    pub(crate) fn len(&self) -> usize {
        match self {
            Alloc::Heap(block) => block.len(),
            Alloc::Malloc(len) => *len,
        }
    }

    /// `mnd_perealloc(ptr, len, persistent)`; the `Err` is the memory
    /// limit's fatal.
    pub(crate) fn resize(self, req: &mut Request, len: usize) -> Result<Alloc, Thrown> {
        Ok(match self {
            Alloc::Heap(block) => Alloc::Heap(block.resize(req, len)?),
            Alloc::Malloc(_) => Alloc::Malloc(len),
        })
    }

    /// `mnd_pefree(ptr, persistent)`.
    pub(crate) fn free(self, req: &mut Request) {
        if let Alloc::Heap(block) = self {
            block.free(req);
        }
    }

    /// What a persistent connection keeps of the allocation while the
    /// engine's pool holds it: `malloc`'s length, which
    /// [`Alloc::Malloc`] takes up again. One on the request's heap, which
    /// a persistent connection never makes, is freed with the request
    /// that gives the connection back, and its length kept.
    pub(crate) fn idle(self, req: &mut Request) -> usize {
        let len = self.len();
        self.free(req);
        len
    }
}

/// A string the connection keeps as php keeps it, `mnd_pestrdup`'d or
/// `mnd_pemalloc`'d and copied: on the request's heap for a request's
/// connection, the allocation counting the NUL a strdup writes past the
/// bytes; the bytes alone for a persistent one.
#[derive(Debug)]
pub(crate) enum Pstr {
    Heap { held: Emalloc, len: usize },
    Malloc(Box<[u8]>),
}

impl Pstr {
    /// `mnd_pestrndup(bytes, len, persistent)`: the bytes and a NUL; the
    /// `Err` is the memory limit's fatal.
    pub(crate) fn dup(req: &mut Request, bytes: &[u8], persistent: bool) -> Result<Pstr, Thrown> {
        Pstr::alloc(req, bytes, bytes.len() + 1, persistent)
    }

    /// `mnd_pemalloc(len, persistent)` and `memcpy`: the bytes alone.
    pub(crate) fn copy(req: &mut Request, bytes: &[u8], persistent: bool) -> Result<Pstr, Thrown> {
        Pstr::alloc(req, bytes, bytes.len(), persistent)
    }

    fn alloc(
        req: &mut Request,
        bytes: &[u8],
        size: usize,
        persistent: bool,
    ) -> Result<Pstr, Thrown> {
        if persistent {
            return Ok(Pstr::Malloc(bytes.into()));
        }
        let mut held = Emalloc::new(req, size)?;
        if let Some(at) = held.bytes_mut().get_mut(..bytes.len()) {
            at.copy_from_slice(bytes);
        }
        Ok(Pstr::Heap {
            held,
            len: bytes.len(),
        })
    }

    /// The bytes, without the NUL.
    pub(crate) fn bytes(&self) -> &[u8] {
        match self {
            Pstr::Heap { held, len } => held.bytes().get(..*len).unwrap_or_default(),
            Pstr::Malloc(bytes) => bytes,
        }
    }

    /// `mnd_pefree(s, persistent)`.
    pub(crate) fn free(self, req: &mut Request) {
        if let Pstr::Heap { held, .. } = self {
            held.free(req);
        }
    }

    /// What a persistent connection keeps of the string while the
    /// engine's pool holds it: `malloc`'s bytes, which [`Pstr::Malloc`]
    /// takes up again. One on the request's heap, which a persistent
    /// connection never makes, is copied out and freed with the request
    /// that gives the connection back.
    pub(crate) fn idle(self, req: &mut Request) -> Box<[u8]> {
        match self {
            Pstr::Malloc(bytes) => bytes,
            heap @ Pstr::Heap { .. } => {
                let bytes = heap.bytes().into();
                heap.free(req);
                bytes
            }
        }
    }

    /// [`Pstr::idle`] of the field, where it holds a string.
    pub(crate) fn idle_field(req: &mut Request, field: Option<Pstr>) -> Option<Box<[u8]>> {
        field.map(|s| s.idle(req))
    }

    /// `field` set to `to`, what it held before freed: the
    /// `mnd_pefree`-then-assign every `MYSQLND_STRING` and option string
    /// is set by.
    pub(crate) fn put(req: &mut Request, field: &mut Option<Pstr>, to: Option<Pstr>) {
        if let Some(old) = std::mem::replace(field, to) {
            old.free(req);
        }
    }
}
