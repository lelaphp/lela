//! The `cdb` and `cdb_make` handlers: `dba_cdb.c` over php's bundled
//! `libcdb` (`cdb.c` and `cdb_make.c`, D. J. Bernstein's constant
//! database 0.75 as php carries it), with every read and write through
//! the link's stream (`DBA_CDB_BUILTIN`). One open handler serves both
//! names, as `DBA_NAMED_HND(cdb_make, cdb, ...)` has it: a database
//! opened `r` is read, one opened `n` is made, and the name decides only
//! what `dba_handlers(true)` and `dba_fetch`'s skip notice say.

use php_ext::abi::EngineState;
use php_ext::raising::{Level, docref};
use php_ext::request::{Request, Stream, Whence};

/// `CDB_HASHSTART`.
const HASH_START: u32 = 5381;

/// `sizeof(c->final)`: the table of 256 bucket heads at the file's start.
const HEAD: u32 = 2048;

/// `cdb_hash`: djb's `h = (h + (h << 5)) ^ c` over the key's bytes.
fn hash(key: &[u8]) -> u32 {
    key.iter()
        .fold(HASH_START, |h, &b| h.wrapping_add(h << 5) ^ u32::from(b))
}

/// `uint32_unpack`: four bytes, little-endian.
fn unpack(bytes: &[u8]) -> u32 {
    let mut word = [0u8; 4];
    word.copy_from_slice(&bytes[..4]);
    u32::from_le_bytes(word)
}

/// `php_stream_read` into `buf`, the count read; a failed read is `0`.
fn read(req: &mut Request, file: Stream, buf: &mut [u8]) -> usize {
    file.read_into(req, buf).unwrap_or(0)
}

/// `struct cdb`, `cdb_findnext`'s search state, with what `dba_cdb.c`
/// keeps beside it for `firstkey` and `nextkey`.
#[derive(Default, EngineState)]
pub(crate) struct Reader {
    /// `loop`: the hash slots searched under the key so far.
    looped: u32,
    khash: u32,
    kpos: u32,
    hpos: u32,
    hslots: u32,
    /// `dpos` and `dlen`: where the last found record's data is.
    dpos: u32,
    dlen: u32,
    /// `eod`: the end of the records, the first bucket's position.
    eod: u32,
    /// `pos`: where `nextkey` reads the next record.
    pos: u32,
}

impl Reader {
    /// `cdb_read`: `buf.len()` bytes at `pos`, `None` for php's `-1`,
    /// a short file among them.
    fn read_at(req: &mut Request, file: Stream, buf: &mut [u8], pos: u32) -> Option<()> {
        if !file.seek(req, i64::from(pos), Whence::Set) {
            return None;
        }
        let mut at = 0;
        while at < buf.len() {
            match file.read_into(req, &mut buf[at..]) {
                Some(0) | None => return None,
                Some(n) => at += n,
            }
        }
        Some(())
    }

    /// `cdb_match`: whether the `key.len()` bytes at `pos` are `key`,
    /// compared thirty-two at a time.
    fn matches(req: &mut Request, file: Stream, key: &[u8], mut pos: u32) -> Option<bool> {
        let mut buf = [0u8; 32];
        for chunk in key.chunks(buf.len()) {
            let buf = &mut buf[..chunk.len()];
            Self::read_at(req, file, buf, pos)?;
            if buf != chunk {
                return Some(false);
            }
            pos = pos.wrapping_add(u32::try_from(chunk.len()).unwrap_or(0));
        }
        Some(true)
    }

    /// `cdb_findnext`: the next record under `key`, `Some(true)` with
    /// `dpos` and `dlen` set, `Some(false)` past the last, `None` for a
    /// read that failed.
    fn find_next(&mut self, req: &mut Request, file: Stream, key: &[u8]) -> Option<bool> {
        let mut buf = [0u8; 8];
        let len = u32::try_from(key.len()).unwrap_or(u32::MAX);
        if self.looped == 0 {
            let u = hash(key);
            Self::read_at(req, file, &mut buf, (u << 3) & 2047)?;
            self.hslots = unpack(&buf[4..]);
            if self.hslots == 0 {
                return Some(false);
            }
            self.hpos = unpack(&buf);
            self.khash = u;
            self.kpos = self.hpos.wrapping_add(((u >> 8) % self.hslots) << 3);
        }
        while self.looped < self.hslots {
            Self::read_at(req, file, &mut buf, self.kpos)?;
            let pos = unpack(&buf[4..]);
            if pos == 0 {
                return Some(false);
            }
            self.looped += 1;
            self.kpos = self.kpos.wrapping_add(8);
            if self.kpos == self.hpos.wrapping_add(self.hslots << 3) {
                self.kpos = self.hpos;
            }
            if unpack(&buf) == self.khash {
                Self::read_at(req, file, &mut buf, pos)?;
                if unpack(&buf) == len && Self::matches(req, file, key, pos.wrapping_add(8))? {
                    self.dlen = unpack(&buf[4..]);
                    self.dpos = pos.wrapping_add(8).wrapping_add(len);
                    return Some(true);
                }
            }
        }
        Some(false)
    }

    /// `cdb_find`: `cdb_findstart`, then the first record under `key`.
    fn find(&mut self, req: &mut Request, file: Stream, key: &[u8]) -> Option<bool> {
        self.looped = 0;
        self.find_next(req, file, key)
    }

    /// `DBA_FETCH_FUNC(cdb)`: the data of the record under `key` past
    /// `skip` others.
    pub(crate) fn fetch(
        &mut self,
        req: &mut Request,
        file: Stream,
        key: &[u8],
        skip: i64,
    ) -> Option<Vec<u8>> {
        if self.find(req, file, key) != Some(true) {
            return None;
        }
        for _ in 0..skip.max(0) {
            if self.find_next(req, file, key) != Some(true) {
                return None;
            }
        }
        let mut data = vec![0u8; usize::try_from(self.dlen).unwrap_or(0)];
        Self::read_at(req, file, &mut data, self.dpos)?;
        Some(data)
    }

    /// `DBA_EXISTS_FUNC(cdb)`.
    pub(crate) fn exists(&mut self, req: &mut Request, file: Stream, key: &[u8]) -> bool {
        self.find(req, file, key) == Some(true)
    }

    /// `CSEEK(n)`: refused at or past the end of the records, and where
    /// the stream does not stand at `n` after the seek.
    fn seek(&self, req: &mut Request, file: Stream, n: u32) -> Option<()> {
        if n >= self.eod {
            return None;
        }
        file.seek(req, i64::from(n), Whence::Set);
        (file.tell(req) == i64::from(n)).then_some(())
    }

    /// The record at the stream's position: its key, the lengths read
    /// first; `pos` moves past it whether or not the key could be read.
    fn record(&mut self, req: &mut Request, file: Stream) -> Option<Vec<u8>> {
        let mut buf = [0u8; 8];
        if read(req, file, &mut buf) < 8 {
            return None;
        }
        let klen = unpack(&buf);
        let dlen = unpack(&buf[4..]);
        let mut key = vec![0u8; usize::try_from(klen).unwrap_or(0)];
        let had = read_full(req, file, &mut key);
        self.pos = self
            .pos
            .wrapping_add(8)
            .wrapping_add(klen)
            .wrapping_add(dlen);
        (had == key.len()).then_some(key)
    }

    /// `DBA_FIRSTKEY_FUNC(cdb)`: the end of the records read from the
    /// first bucket head, then the record at 2048.
    pub(crate) fn first_key(&mut self, req: &mut Request, file: Stream) -> Option<Vec<u8>> {
        self.eod = u32::MAX;
        self.seek(req, file, 0)?;
        let mut buf = [0u8; 4];
        if read(req, file, &mut buf) < 4 {
            return None;
        }
        self.eod = unpack(&buf);
        self.seek(req, file, HEAD)?;
        self.pos = HEAD;
        self.record(req, file)
    }

    /// `DBA_NEXTKEY_FUNC(cdb)`: the record at `pos`.
    pub(crate) fn next_key(&mut self, req: &mut Request, file: Stream) -> Option<Vec<u8>> {
        self.seek(req, file, self.pos)?;
        self.record(req, file)
    }
}

/// `php_stream_read` repeated until `buf` is full or the stream ends,
/// which is what one read of a plain file does.
fn read_full(req: &mut Request, file: Stream, buf: &mut [u8]) -> usize {
    let mut at = 0;
    while at < buf.len() {
        match file.read_into(req, &mut buf[at..]) {
            Some(0) | None => break,
            Some(n) => at += n,
        }
    }
    at
}

/// `struct cdb_hp`: a record's hash and position.
#[derive(Clone, Copy, Default, EngineState)]
struct Slot {
    h: u32,
    p: u32,
}

/// `struct cdb_make`: the records written so far, in the order added,
/// and where the next one goes.
#[derive(EngineState)]
pub(crate) struct Maker {
    slots: Vec<Slot>,
    pos: u32,
}

impl Maker {
    /// `cdb_make_start`: the records start past the 2048 bytes of bucket
    /// heads, which `finish` writes last; a seek that fails is php's
    /// notice and the maker made all the same.
    pub(crate) fn start(req: &mut Request, file: Stream) -> Maker {
        if !file.seek(req, i64::from(HEAD), Whence::Set) {
            docref(req, Level::Notice, "Fseek failed");
        }
        Maker {
            slots: Vec::new(),
            pos: HEAD,
        }
    }

    /// `cdb_posplus`: `pos` moved on, `None` where it would wrap.
    fn advance(&mut self, len: u32) -> Option<()> {
        let next = self.pos.wrapping_add(len);
        if next < len {
            return None;
        }
        self.pos = next;
        Some(())
    }

    /// `cdb_make_add`: the lengths, the key and the data written, and
    /// the record's slot kept for the table; `None` for php's `-1`.
    pub(crate) fn add(
        &mut self,
        req: &mut Request,
        file: Stream,
        key: &[u8],
        data: &[u8],
    ) -> Option<()> {
        // `unsigned int` lengths, as `dba_update_cdb` hands them over.
        #[expect(
            clippy::cast_possible_truncation,
            reason = "php's `unsigned int` lengths"
        )]
        let (klen, dlen) = (key.len() as u32, data.len() as u32);
        let mut head = [0u8; 8];
        head[..4].copy_from_slice(&klen.to_le_bytes());
        head[4..].copy_from_slice(&dlen.to_le_bytes());
        for bytes in [&head[..], key, data] {
            if file.write(req, bytes) != Some(bytes.len()) {
                return None;
            }
        }
        self.slots.push(Slot {
            h: hash(key),
            p: self.pos,
        });
        self.advance(8)?;
        self.advance(klen)?;
        self.advance(dlen)
    }

    /// `cdb_make_finish`: the 256 hash tables written after the records,
    /// each twice its bucket's count with linear probing, then the
    /// bucket heads written over the first 2048 bytes.
    pub(crate) fn finish(mut self, req: &mut Request, file: Stream) -> Option<()> {
        let mut count = [0u32; 256];
        for slot in &self.slots {
            count[(slot.h & 255) as usize] += 1;
        }
        let mut start = [0u32; 256];
        let mut total = 0u32;
        for (i, n) in count.iter().enumerate() {
            total += n;
            start[i] = total;
        }
        // `c->split[--c->start[...]]` over the records newest first, so
        // each bucket's run is in the order the records were added.
        let mut split = vec![Slot::default(); self.slots.len()];
        for slot in self.slots.iter().rev() {
            let bucket = (slot.h & 255) as usize;
            start[bucket] -= 1;
            split[start[bucket] as usize] = *slot;
        }
        let mut heads = vec![0u8; HEAD as usize];
        for i in 0..256 {
            let n = count[i];
            let len = n * 2;
            heads[8 * i..8 * i + 4].copy_from_slice(&self.pos.to_le_bytes());
            heads[8 * i + 4..8 * i + 8].copy_from_slice(&len.to_le_bytes());
            let mut table = vec![Slot::default(); len as usize];
            let first = start[i] as usize;
            for hp in &split[first..first + n as usize] {
                let mut at = ((hp.h >> 8) % len) as usize;
                while table[at].p != 0 {
                    at += 1;
                    if at == table.len() {
                        at = 0;
                    }
                }
                table[at] = *hp;
            }
            for slot in &table {
                let mut buf = [0u8; 8];
                buf[..4].copy_from_slice(&slot.h.to_le_bytes());
                buf[4..].copy_from_slice(&slot.p.to_le_bytes());
                if file.write(req, &buf) != Some(8) {
                    return None;
                }
                self.advance(8)?;
            }
        }
        if !file.flush(req) {
            return None;
        }
        file.seek(req, 0, Whence::Set);
        if file.tell(req) != 0 || file.write(req, &heads) != Some(heads.len()) {
            return None;
        }
        file.flush(req).then_some(())
    }
}

/// `cdb_version` and `cdb_make_version`.
pub(crate) const READER_VERSION: &str = "0.75, $Id: e694d6a6eb12f2bbad27f8ec7ebf3788257532d9 $";
pub(crate) const MAKER_VERSION: &str = "0.75, $Id: 570e3625b0cea5c29455cecf40fdfd491c0eb3c6 $";
