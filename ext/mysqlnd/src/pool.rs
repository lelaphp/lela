//! `mysqlnd_block_alloc.c` over `zend_arena.h`: a set of rows' memory pool,
//! the arena its structures, its column definitions and its rows' packets
//! are carved from, each of the arena's blocks one `emalloc` on the
//! request's heap, so `memory_get_usage()` and `memory_limit` see a
//! set of rows where php's see them (design §3.1, §5.13).

use php_ext::raising::Thrown;
use php_ext::request::Request;
use php_ext::values::Emalloc;

/// `ZEND_MM_ALIGNED_SIZE(sizeof(zend_arena))`: the arena's header at the
/// head of each of its blocks.
const ARENA_HEADER: usize = 24;
/// `ZEND_MM_ALIGNMENT`.
const ALIGNMENT: usize = 8;
/// `sizeof(MYSQLND_MEMORY_POOL)`, which the pool carves out of itself.
const POOL_SIZE: usize = 24;

/// `ZEND_MM_ALIGNED_SIZE`.
fn aligned(size: usize) -> usize {
    size.saturating_add(ALIGNMENT - 1) & !(ALIGNMENT - 1)
}

/// One stretch of a pool's bytes, what `get_chunk` answers: the pool's
/// until a restore releases the block or the place it lies past.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Chunk {
    block: usize,
    at: usize,
    len: usize,
}

impl Chunk {
    /// The first `len` bytes alone, as php's row buffer counts the
    /// packet's bytes and not the byte allocated past them.
    pub(crate) fn prefix(self, len: usize) -> Chunk {
        Chunk {
            len: len.min(self.len),
            ..self
        }
    }

    /// The record `MYSQLND_ROW_BUFFER` keeps of it, sixteen bytes; `None`
    /// for a chunk whose place does not fit one, which no packet the
    /// protocol frames makes.
    pub(crate) fn record(self) -> Option<[u8; 16]> {
        let block = u32::try_from(self.block).ok()?;
        let at = u32::try_from(self.at).ok()?;
        let len = u64::try_from(self.len).ok()?;
        let mut out = [0; 16];
        out[..4].copy_from_slice(&block.to_le_bytes());
        out[4..8].copy_from_slice(&at.to_le_bytes());
        out[8..].copy_from_slice(&len.to_le_bytes());
        Some(out)
    }

    /// The chunk a [`Chunk::record`] keeps.
    pub(crate) fn from_record(record: &[u8]) -> Option<Chunk> {
        let block = u32::from_le_bytes(record.get(..4)?.try_into().ok()?);
        let at = u32::from_le_bytes(record.get(4..8)?.try_into().ok()?);
        let len = u64::from_le_bytes(record.get(8..16)?.try_into().ok()?);
        Some(Chunk {
            block: usize::try_from(block).ok()?,
            at: usize::try_from(at).ok()?,
            len: usize::try_from(len).ok()?,
        })
    }
}

/// `zend_arena_checkpoint`: where the pool stood.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Mark {
    block: usize,
    ptr: usize,
}

/// The checkpoint a scratch read put aside, given back by
/// [`Pool::leave`].
#[derive(Debug, Clone, Copy)]
#[must_use]
pub(crate) struct Saved(Option<Mark>);

/// `MYSQLND_MEMORY_POOL`: a `zend_arena` and its checkpoint. A chunk is
/// cut from the newest block where it fits; one that does not opens a
/// block of the newest's size, or of the chunk's and the header's where
/// that is larger, so a set of rows' blocks grow as php's do. The blocks go
/// back through [`Pool::destroy`], where php calls
/// `mysqlnd_mempool_destroy`; a pool its holder drops is the request's
/// end's.
#[derive(Debug)]
pub(crate) struct Pool {
    /// The arena's blocks, oldest first: php's `prev` chain, read back.
    blocks: Vec<Emalloc>,
    /// `arena->ptr` of the newest block.
    ptr: usize,
    /// `pool->checkpoint`.
    checkpoint: Option<Mark>,
}

impl Pool {
    /// `mysqlnd_mempool_create(MYSQLND_G(mempool_default_size))`: the
    /// first block, at least the header, with the pool carved out of it;
    /// the `Err` is the memory limit's fatal.
    pub(crate) fn create(req: &mut Request) -> Result<Pool, Thrown> {
        // `OnUpdateLong`'s `zend_long`, read as `size_t`.
        let size = usize::try_from(
            req.ini()
                .long(b"mysqlnd.mempool_default_size")
                .cast_unsigned(),
        )
        .unwrap_or(usize::MAX);
        let first = Emalloc::new(req, size.max(ARENA_HEADER))?;
        let mut pool = Pool {
            blocks: vec![first],
            ptr: ARENA_HEADER,
            checkpoint: None,
        };
        pool.get_chunk(req, POOL_SIZE)?;
        Ok(pool)
    }

    /// `pool->get_chunk`, `zend_arena_alloc`: `size` bytes, aligned, cut
    /// from the newest block or from a new one; the `Err` is the memory
    /// limit's fatal.
    pub(crate) fn get_chunk(&mut self, req: &mut Request, size: usize) -> Result<Chunk, Thrown> {
        let need = aligned(size);
        let end = self.blocks.last().map_or(0, Emalloc::len);
        if need <= end.saturating_sub(self.ptr) {
            let at = self.ptr;
            self.ptr += need;
            return Ok(Chunk {
                block: self.blocks.len() - 1,
                at,
                len: size,
            });
        }
        let whole = need.saturating_add(ARENA_HEADER).max(end);
        let block = Emalloc::new(req, whole)?;
        self.blocks.push(block);
        self.ptr = ARENA_HEADER + need;
        Ok(Chunk {
            block: self.blocks.len() - 1,
            at: ARENA_HEADER,
            len: size,
        })
    }

    /// A chunk's bytes.
    pub(crate) fn bytes(&self, chunk: Chunk) -> &[u8] {
        self.blocks
            .get(chunk.block)
            .and_then(|b| b.bytes().get(chunk.at..chunk.at + chunk.len))
            .unwrap_or_default()
    }

    /// A chunk's bytes, to write.
    pub(crate) fn bytes_mut(&mut self, chunk: Chunk) -> &mut [u8] {
        self.blocks
            .get_mut(chunk.block)
            .and_then(|b| b.bytes_mut().get_mut(chunk.at..chunk.at + chunk.len))
            .unwrap_or_default()
    }

    /// `mysqlnd_mempool_save_state`.
    pub(crate) fn save(&mut self) {
        self.checkpoint = Some(Mark {
            block: self.blocks.len().saturating_sub(1),
            ptr: self.ptr,
        });
    }

    /// `mysqlnd_mempool_restore_state`, `zend_arena_release`: every block
    /// newer than the checkpoint's freed, the checkpoint's block cut back
    /// to it, the checkpoint gone.
    pub(crate) fn restore(&mut self, req: &mut Request) {
        let Some(mark) = self.checkpoint.take() else {
            return;
        };
        while self.blocks.len() > mark.block + 1 {
            if let Some(block) = self.blocks.pop() {
                block.free(req);
            }
        }
        self.ptr = mark.ptr;
    }

    /// The checkpoint put aside and a new one saved, as a row read off the
    /// line is read: `checkpoint = pool->checkpoint;
    /// mysqlnd_mempool_save_state(pool)`.
    pub(crate) fn enter(&mut self) -> Saved {
        let saved = Saved(self.checkpoint);
        self.save();
        saved
    }

    /// What the read since [`Pool::enter`] cut released, and the checkpoint
    /// put aside back: `mysqlnd_mempool_restore_state(pool);
    /// pool->checkpoint = checkpoint`.
    pub(crate) fn leave(&mut self, req: &mut Request, saved: Saved) {
        self.restore(req);
        self.checkpoint = saved.0;
    }

    /// `mysqlnd_mempool_destroy`, `zend_arena_destroy`: every block freed,
    /// the newest first.
    pub(crate) fn destroy(self, req: &mut Request) {
        for block in self.blocks.into_iter().rev() {
            block.free(req);
        }
    }
}
