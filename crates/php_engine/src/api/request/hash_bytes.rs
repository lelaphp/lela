//! php's `HashTable`s the engine keeps in a shape of its own, with the
//! bytes php's take on the request's heap held beside them (design
//! §5.13): `EG(included_files)`, keyed by the path of every file the
//! request opened, which goes with the heap at the request's end, as
//! `shutdown_executor`'s fast shutdown leaves it to the memory manager.

use super::Request;
use crate::api::raising::Thrown;
use crate::api::values::{Emalloc, PhpString};

/// `HT_SIZE_EX(n, HT_SIZE_TO_MASK(n))` over `n`: a 32-byte `Bucket`
/// and two `uint32_t` hash slots for each element of the table's size.
const PER_ELEMENT: usize = 40;

/// `zend_hash_check_size`: eight, or the power of two at or above `n`.
fn check_size(n: u32) -> u32 {
    n.max(8).checked_next_power_of_two().unwrap_or(0x8000_0000)
}

/// What one php hash table that is never packed and is embedded in
/// `EG` holds on the request's heap: the data `zend_hash_real_init_mixed`
/// lays down at its first write, sized, grown and compacted as
/// `zend_hash.c` sizes, grows and compacts it. `used` and `count` are
/// `nNumUsed` and `nNumOfElements`.
#[derive(Debug, Default)]
pub(crate) struct HashBytes {
    data: Option<Emalloc>,
    size: u32,
    used: u32,
    count: u32,
}

impl HashBytes {
    /// `zend_hash_init(ht, size, ...)` over a table embedded in `EG`.
    fn embedded(size: u32) -> HashBytes {
        HashBytes {
            size: check_size(size),
            ..HashBytes::default()
        }
    }

    /// `zend_hash_real_init_mixed`: the data laid down where it is not.
    fn real_init(&mut self, req: &mut Request) -> Result<(), Thrown> {
        if self.data.is_none() {
            self.data = Some(Emalloc::new(req, PER_ELEMENT * self.size as usize)?);
        }
        Ok(())
    }

    /// One bucket added (`_zend_hash_add_or_update_i`): the data laid
    /// down on the first, and `ZEND_HASH_IF_FULL_DO_RESIZE` before one
    /// that finds every bucket used.
    fn add(&mut self, req: &mut Request) -> Result<(), Thrown> {
        if self.data.is_none() {
            self.real_init(req)?;
        } else if self.used >= self.size {
            self.resize(req)?;
        }
        self.used += 1;
        self.count += 1;
        Ok(())
    }

    /// `zend_hash_do_resize`: compacted in place where more than a
    /// thirty-second of the used buckets are deleted, the size doubled
    /// otherwise.
    fn resize(&mut self, req: &mut Request) -> Result<(), Thrown> {
        if self.used > self.count + (self.count >> 5) {
            self.used = self.count;
            return Ok(());
        }
        self.grow(req, self.size.saturating_mul(2))
    }

    /// The data moved to a table of `size`: the new laid down before
    /// the old is freed, as `zend_hash_do_resize` and `zend_hash_extend`
    /// `pemalloc` and then `pefree`.
    fn grow(&mut self, req: &mut Request, size: u32) -> Result<(), Thrown> {
        let data = Emalloc::new(req, PER_ELEMENT * size as usize)?;
        if let Some(old) = self.data.replace(data) {
            old.free(req);
        }
        self.size = size;
        Ok(())
    }
}

/// `EG(included_files)`: the path of every file the request compiled,
/// the primary one first, each a string on the request's heap the table
/// holds (`zend_hash_add_empty_element` over `opened_path`), and the
/// table php initialises at eight in `init_executor`.
#[derive(Debug)]
pub(crate) struct IncludedFiles {
    table: HashBytes,
    paths: Vec<PhpString>,
}

impl Default for IncludedFiles {
    fn default() -> IncludedFiles {
        IncludedFiles {
            table: HashBytes::embedded(8),
            paths: Vec::new(),
        }
    }
}

impl IncludedFiles {
    /// `zend_hash_exists`.
    pub(crate) fn contains(&self, path: &[u8]) -> bool {
        self.paths.iter().any(|held| held.as_bytes() == path)
    }

    /// The paths in the table's order.
    pub(crate) fn paths(&self) -> Vec<Vec<u8>> {
        self.paths
            .iter()
            .map(|path| path.as_bytes().to_vec())
            .collect()
    }
}

impl Request {
    /// `zend_hash_add_empty_element(&EG(included_files), opened_path)`:
    /// `path` put in the table where it is not there yet, its string
    /// one more allocation the table holds and the table grown as
    /// php's grows. A path the table has is left alone. The `Err` is the
    /// memory limit, which has ended the request.
    ///
    /// Each first include of a file keeps its path's string, and the one
    /// that finds eight paths in the table the doubled table too, as
    /// php's `memory_get_usage()` reads them; the primary file is the
    /// table's first where the host named one:
    ///
    /// ```php
    /// $d = realpath(__DIR__) . '/'; $d .= str_repeat('f', 120 - strlen($d));
    /// for ($i = 0; $i < 10; $i++) { file_put_contents("$d$i.inc", '<?php $x = 1;'); }
    /// for ($i = 0; count(get_included_files()) < 8; $i++) { include_once "$d$i.inc"; }
    /// for ($n = $i + 2; $i < $n; $i++) { $m = memory_get_usage(); include_once "$d$i.inc"; echo memory_get_usage() - $m, "\n"; }
    /// for ($i = 0; $i < 10; $i++) { unlink("$d$i.inc"); }
    /// ```
    /// ```expect
    /// 480
    /// 160
    /// ```
    pub(crate) fn note_included(&mut self, path: &[u8]) -> Result<(), Thrown> {
        if self.included_files.contains(path) {
            return Ok(());
        }
        let held = PhpString::new(self, path)?;
        let mut table = std::mem::take(&mut self.included_files.table);
        let added = table.add(self);
        self.included_files.table = table;
        self.included_files.paths.push(held);
        added
    }
}
