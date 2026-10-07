//! `intern->storage` as `zend_hash` lays it down (`Z/Zend/zend_hash.c`):
//! the data of a packed or a hash table at php's sizes, grown,
//! converted and compacted where php's is, and the element
//! `spl_object_storage_create_element` `emalloc`s for each row, so
//! `memory_get_usage()` reads php's storage (design §5.13). The rows,
//! their order and the positions over them are
//! [`StorageState`](super::StorageState)'s; this holds what php's heap
//! holds for them and nothing else.

use std::collections::HashMap;

use php_ext::raising::Thrown;
use php_ext::request::Request;
use php_ext::values::{Emalloc, Owner, Value};

use super::RowKey;

/// `HT_MIN_SIZE`, the size `zend_hash_init(&intern->storage, 0, ...)`
/// starts at.
const MIN_SIZE: u32 = 8;

/// `sizeof(spl_SplObjectStorageElement)`: the object's pointer and the
/// information's zval.
const ELEMENT: usize = 24;

/// `HT_PACKED_SIZE_EX(size, HT_MIN_MASK)`: a zval a slot and the two
/// hash words of a packed table.
fn packed_size(size: u32) -> usize {
    size as usize * 16 + 8
}

/// `HT_SIZE_EX(size, HT_SIZE_TO_MASK(size))`: a bucket a slot and two
/// hash words a slot.
fn hash_size(size: u32) -> usize {
    size as usize * 40
}

/// One row's place in the table, its element, and the `getHash()`
/// string its bucket keeps as the key, the bucket's owner of it.
struct Entry {
    slot: u32,
    element: Emalloc,
    key: Option<Owner<Value>>,
}

/// The table's shape: `arData` once a row was placed, `HASH_FLAG_PACKED`,
/// `nTableSize`, which of the `nNumUsed` slots are live, and
/// `nNumOfElements`; with each row's entry by its key.
#[derive(Default)]
pub(super) struct Table {
    data: Option<Emalloc>,
    packed: bool,
    size: u32,
    live: Vec<bool>,
    count: u32,
    entries: HashMap<RowKey, Entry>,
}

impl Table {
    /// A new row's place, `zend_hash_index_lookup` for an object's id
    /// or `zend_hash_update_mem` for a `getHash()` string, `key_string`
    /// the string the bucket keeps; and its element, allocated after
    /// the table moves, or before it where `copied` says the row goes
    /// through `zend_hash_*update_mem`, which `pemalloc`s its copy
    /// first. The `Err` is the memory limit's fatal, the string given up.
    pub(super) fn place(
        &mut self,
        req: &mut Request,
        key: RowKey,
        key_string: Option<Owner<Value>>,
        copied: bool,
    ) -> Result<(), Thrown> {
        let laid = self.lay(req, &key, copied);
        let (slot, element) = match laid {
            Ok(laid) => laid,
            Err(thrown) => {
                if let Some(string) = key_string {
                    string.give_up(req);
                }
                return Err(thrown);
            }
        };
        self.entries.insert(
            key,
            Entry {
                slot,
                element,
                key: key_string,
            },
        );
        Ok(())
    }

    /// [`Table::place`]'s slot and element, in php's order.
    fn lay(
        &mut self,
        req: &mut Request,
        key: &RowKey,
        copied: bool,
    ) -> Result<(u32, Emalloc), Thrown> {
        let early = if copied {
            Some(Emalloc::new(req, ELEMENT)?)
        } else {
            None
        };
        let slot = match key {
            RowKey::Id(h) => self.place_index(req, *h)?,
            RowKey::Hash(_) => self.place_string(req)?,
        };
        let element = match early {
            Some(element) => element,
            None => Emalloc::new(req, ELEMENT)?,
        };
        Ok((slot, element))
    }

    /// `zend_hash_index_del` or `zend_hash_del`: the row's slot given
    /// up, `nNumUsed` drawn back past the holes at its end, and what
    /// the row held for its caller to give back, the key string and the
    /// element.
    pub(super) fn remove(&mut self, key: &RowKey) -> Option<(Option<Owner<Value>>, Emalloc)> {
        let entry = self.entries.remove(key)?;
        self.count -= 1;
        let slot = entry.slot as usize;
        if let Some(live) = self.live.get_mut(slot) {
            *live = false;
        }
        if slot + 1 == self.live.len() {
            while self.live.last() == Some(&false) {
                self.live.pop();
            }
        }
        Some((entry.key, entry.element))
    }

    /// `zend_hash_destroy`'s last step, the data freed once every row
    /// was given back.
    pub(super) fn free(self, req: &mut Request) {
        if let Some(data) = self.data {
            data.free(req);
        }
    }

    fn used(&self) -> u32 {
        u32::try_from(self.live.len()).unwrap_or(u32::MAX)
    }

    /// `_zend_hash_index_add_or_update_i` for a key no row holds.
    fn place_index(&mut self, req: &mut Request, h: u32) -> Result<u32, Thrown> {
        if self.data.is_none() {
            self.size = MIN_SIZE;
            self.packed = h < self.size;
            let bytes = if self.packed {
                packed_size(self.size)
            } else {
                hash_size(self.size)
            };
            self.data = Some(Emalloc::new(req, bytes)?);
            return Ok(if self.packed {
                self.add_packed(h)
            } else {
                self.append()
            });
        }
        if self.packed {
            if h < self.used() {
                // "we have to keep the order"
                self.make_hash(req)?;
                return Ok(self.append());
            }
            if h < self.size {
                return Ok(self.add_packed(h));
            }
            if (h >> 1) < self.size && (self.size >> 1) < self.count {
                // `zend_hash_packed_grow`, an `erealloc` of the data.
                self.size *= 2;
                if let Some(data) = self.data.take() {
                    self.data = Some(data.resize(req, packed_size(self.size))?);
                }
                return Ok(self.add_packed(h));
            }
            if self.used() >= self.size {
                self.size *= 2;
            }
            self.make_hash(req)?;
            return Ok(self.append());
        }
        self.make_room(req)?;
        Ok(self.append())
    }

    /// `_zend_hash_add_or_update_i` for a string no row holds.
    fn place_string(&mut self, req: &mut Request) -> Result<u32, Thrown> {
        if self.data.is_none() {
            self.size = MIN_SIZE;
            self.packed = false;
            self.data = Some(Emalloc::new(req, hash_size(self.size))?);
            return Ok(self.append());
        }
        if self.packed {
            self.make_hash(req)?;
        }
        self.make_room(req)?;
        Ok(self.append())
    }

    fn add_packed(&mut self, h: u32) -> u32 {
        self.live.resize(h as usize, false);
        self.live.push(true);
        self.count += 1;
        h
    }

    fn append(&mut self) -> u32 {
        self.live.push(true);
        self.count += 1;
        self.used() - 1
    }

    /// `ZEND_HASH_IF_FULL_DO_RESIZE` and `zend_hash_do_resize`: a full
    /// table with enough holes compacted in place, any other doubled.
    fn make_room(&mut self, req: &mut Request) -> Result<(), Thrown> {
        let used = self.used();
        if used < self.size {
            return Ok(());
        }
        if used > self.count + (self.count >> 5) {
            self.rehash();
            return Ok(());
        }
        self.size *= 2;
        self.replace(req, hash_size(self.size))?;
        self.rehash();
        Ok(())
    }

    /// `zend_hash_packed_to_hash`: the data of a hash table of the same
    /// size, made before the old is freed, then rehashed.
    fn make_hash(&mut self, req: &mut Request) -> Result<(), Thrown> {
        self.replace(req, hash_size(self.size))?;
        self.packed = false;
        self.rehash();
        Ok(())
    }

    fn replace(&mut self, req: &mut Request, bytes: usize) -> Result<(), Thrown> {
        let data = Emalloc::new(req, bytes)?;
        if let Some(old) = self.data.replace(data) {
            old.free(req);
        }
        Ok(())
    }

    /// `zend_hash_rehash`: the holes compacted out, each row at its rank
    /// among the live ones.
    fn rehash(&mut self) {
        if self.count as usize == self.live.len() {
            return;
        }
        let mut rank = Vec::with_capacity(self.live.len());
        let mut live = 0;
        for &here in &self.live {
            rank.push(live);
            if here {
                live += 1;
            }
        }
        for entry in self.entries.values_mut() {
            entry.slot = rank.get(entry.slot as usize).copied().unwrap_or(0);
        }
        self.live = vec![true; live as usize];
    }
}
