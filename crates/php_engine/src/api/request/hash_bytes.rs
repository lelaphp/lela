//! Two of php's `HashTable`s the engine keeps in a shape of its own,
//! with the bytes php's take on the request's heap held beside them
//! (design §5.13): `EG(included_files)`, keyed by the path of every file
//! the request opened, which goes with the heap at the request's end, as
//! `shutdown_executor`'s fast shutdown leaves it to the memory manager,
//! and `EG(modified_ini_directives)`, the ini area's, which lays it down
//! and frees it by `zend_ini_deactivate` (`super::ini`).

use super::Request;
use crate::api::raising::Thrown;
use crate::api::values::{Emalloc, PhpString};

/// `sizeof(zend_array)`, what `ALLOC_HASHTABLE` takes.
const HEADER: usize = 56;

/// `HT_SIZE_EX(n, HT_SIZE_TO_MASK(n))` over `n`: a 32-byte `Bucket`
/// and two `uint32_t` hash slots for each element of the table's size.
const PER_ELEMENT: usize = 40;

/// `zend_hash_check_size`: eight, or the power of two at or above `n`.
fn check_size(n: u32) -> u32 {
    n.max(8).checked_next_power_of_two().unwrap_or(0x8000_0000)
}

/// What one php hash table that is never packed holds on the request's
/// heap: the `zend_array` where it is `emalloc`'d and not embedded in
/// `EG`, and the data `zend_hash_real_init_mixed` lays down at its first
/// write, sized, grown and compacted as `zend_hash.c` sizes, grows and
/// compacts it. `used` and `count` are `nNumUsed` and `nNumOfElements`.
#[derive(Debug, Default)]
pub(crate) struct HashBytes {
    header: Option<Emalloc>,
    data: Option<Emalloc>,
    size: u32,
    used: u32,
    count: u32,
}

impl HashBytes {
    /// `zend_hash_init(ht, size, ...)` over a table embedded in `EG`.
    pub(super) fn embedded(size: u32) -> HashBytes {
        HashBytes {
            size: check_size(size),
            ..HashBytes::default()
        }
    }

    /// `zend_new_array(size)`: the header alone.
    pub(super) fn new_array(req: &mut Request, size: u32) -> Result<HashBytes, Thrown> {
        Ok(HashBytes {
            header: Some(Emalloc::new(req, HEADER)?),
            ..HashBytes::embedded(size)
        })
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
    pub(super) fn add(&mut self, req: &mut Request) -> Result<(), Thrown> {
        if self.data.is_none() {
            self.real_init(req)?;
        } else if self.used >= self.size {
            self.resize(req)?;
        }
        self.used += 1;
        self.count += 1;
        Ok(())
    }

    /// One bucket deleted (`_zend_hash_del_el_ex`): `nNumUsed` falls
    /// with it where it was the last, the tombstones before it kept.
    pub(super) fn del(&mut self, last: bool) {
        self.count = self.count.saturating_sub(1);
        if last {
            self.used = self.used.saturating_sub(1);
        }
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

    /// `zend_array_destroy`: the data, then the header.
    pub(super) fn free(self, req: &mut Request) {
        if let Some(data) = self.data {
            data.free(req);
        }
        if let Some(header) = self.header {
            header.free(req);
        }
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

    /// The table's string for `path`, `zend_string_copy`'d by the
    /// caller, as an op array's `filename` is the string its file was
    /// compiled under; `None` for a path the request did not open.
    pub(crate) fn held(&self, path: &[u8]) -> Option<PhpString> {
        self.paths
            .iter()
            .copied()
            .find(|held| held.as_bytes() == path)
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
        let held = self.string(path)?;
        self.note_included_held(held)
    }

    /// [`Request::note_included`] over `held`, a string whose one owner
    /// the table takes: one another place holds already, so no string is
    /// made; given up where the table has its path.
    pub(crate) fn note_included_held(&mut self, held: PhpString) -> Result<(), Thrown> {
        if self.included_files.contains(held.as_bytes()) {
            self.release(crate::api::values::Value::string(held));
            return Ok(());
        }
        let mut table = std::mem::take(&mut self.included_files.table);
        let added = table.add(self);
        self.included_files.table = table;
        self.included_files.paths.push(held);
        added
    }

    /// `shutdown_executor`'s frees of the two tables a debug build of php
    /// frees by name (`Z/Zend/zend_execute_API.c:496-502`): every
    /// `EG(symtable_cache)` kept, then `EG(included_files)` with the path
    /// strings it holds. A release build leaves them to the heap's mass
    /// release, as php's fast shutdown leaves them.
    #[cfg(debug_assertions)]
    pub(super) fn free_hash_tables(&mut self) {
        self.free_symbol_tables();
        let included = std::mem::take(&mut self.included_files);
        for path in included.paths {
            self.release(crate::api::values::Value::string(path));
        }
        included.table.free(self);
    }
}
