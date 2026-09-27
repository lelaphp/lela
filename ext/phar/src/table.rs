//! php's `HashTable` as phar uses it: string keys in insertion order, a
//! key added again after its deletion going to the end, an update of a
//! present key keeping its place. The manifest, the virtual and mounted
//! directories and the name and alias maps are each one; phar's flush,
//! its directory listings and its iterators walk them in this order,
//! which the archive bytes and the listings observe.
//!
//! A slot is never reused, so the index of a live row is a stable name
//! for as long as the row is there: an open entry stream and a
//! `PharFileInfo` hold one, as php holds the `phar_entry_info` pointer.

use std::collections::HashMap;

/// One table; `V` is the row's payload.
pub(crate) struct Table<V> {
    slots: Vec<Option<(Vec<u8>, V)>>,
    index: HashMap<Vec<u8>, usize>,
}

impl<V> Default for Table<V> {
    fn default() -> Table<V> {
        Table {
            slots: Vec::new(),
            index: HashMap::new(),
        }
    }
}

impl<V> Table<V> {
    /// `zend_hash_num_elements`.
    pub(crate) fn len(&self) -> usize {
        self.index.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.index.is_empty()
    }

    /// The slot of `key`, if present.
    pub(crate) fn slot(&self, key: &[u8]) -> Option<usize> {
        self.index.get(key).copied()
    }

    pub(crate) fn contains(&self, key: &[u8]) -> bool {
        self.index.contains_key(key)
    }

    pub(crate) fn get(&self, key: &[u8]) -> Option<&V> {
        self.at(self.slot(key)?)
    }

    pub(crate) fn get_mut(&mut self, key: &[u8]) -> Option<&mut V> {
        let slot = self.slot(key)?;
        self.at_mut(slot)
    }

    /// The row at a slot, `None` once it was deleted.
    pub(crate) fn at(&self, slot: usize) -> Option<&V> {
        self.slots.get(slot)?.as_ref().map(|(_, v)| v)
    }

    pub(crate) fn at_mut(&mut self, slot: usize) -> Option<&mut V> {
        self.slots.get_mut(slot)?.as_mut().map(|(_, v)| v)
    }

    /// The key at a slot.
    pub(crate) fn key_at(&self, slot: usize) -> Option<&[u8]> {
        self.slots.get(slot)?.as_ref().map(|(k, _)| k.as_slice())
    }

    /// `zend_hash_add`: the new row's slot, or the value back where the
    /// key is taken.
    pub(crate) fn add(&mut self, key: &[u8], value: V) -> Result<usize, V> {
        if self.index.contains_key(key) {
            return Err(value);
        }
        let slot = self.slots.len();
        self.slots.push(Some((key.to_vec(), value)));
        self.index.insert(key.to_vec(), slot);
        Ok(slot)
    }

    /// `zend_hash_update`: the row replaced in place, or added at the end;
    /// the value it replaced comes back.
    pub(crate) fn update(&mut self, key: &[u8], value: V) -> (usize, Option<V>) {
        match self.slot(key) {
            Some(slot) => {
                let old = self.slots[slot].replace((key.to_vec(), value));
                (slot, old.map(|(_, v)| v))
            }
            None => (self.add(key, value).unwrap_or(usize::MAX), None),
        }
    }

    /// `zend_hash_del`: the row taken out.
    pub(crate) fn remove(&mut self, key: &[u8]) -> Option<V> {
        let slot = self.index.remove(key)?;
        self.slots.get_mut(slot)?.take().map(|(_, v)| v)
    }

    /// The row at a slot taken out.
    pub(crate) fn remove_at(&mut self, slot: usize) -> Option<V> {
        let (key, value) = self.slots.get_mut(slot)?.take()?;
        self.index.remove(&key);
        Some(value)
    }

    /// A row's key changed where it stands, as `phar_wrapper_rename`
    /// rewrites a bucket's key and rehashes; a row already under the new
    /// key is dropped, as the rehash drops the second of two equal keys.
    pub(crate) fn rekey(&mut self, slot: usize, key: &[u8]) {
        let Some(Some((old, _))) = self.slots.get(slot) else {
            return;
        };
        let old = old.clone();
        if let Some(other) = self.index.get(key).copied()
            && other != slot
        {
            self.slots[other] = None;
        }
        self.index.remove(&old);
        self.index.insert(key.to_vec(), slot);
        if let Some(Some((k, _))) = self.slots.get_mut(slot) {
            *k = key.to_vec();
        }
    }

    /// The live slots in order.
    pub(crate) fn slots(&self) -> Vec<usize> {
        (0..self.slots.len())
            .filter(|s| self.slots[*s].is_some())
            .collect()
    }

    /// The rows in order.
    pub(crate) fn iter(&self) -> impl Iterator<Item = (&[u8], &V)> {
        self.slots
            .iter()
            .filter_map(|s| s.as_ref().map(|(k, v)| (k.as_slice(), v)))
    }

    /// The keys in order.
    pub(crate) fn keys(&self) -> Vec<Vec<u8>> {
        self.iter().map(|(k, _)| k.to_vec()).collect()
    }

    /// `zend_hash_clean`.
    pub(crate) fn clear(&mut self) -> Vec<V> {
        self.index.clear();
        self.slots.drain(..).flatten().map(|(_, v)| v).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::Table;

    #[test]
    fn order_is_insertion_and_a_readded_key_goes_last() {
        let mut t = Table::default();
        assert!(t.add(b"a", 1).is_ok());
        assert!(t.add(b"b", 2).is_ok());
        assert!(t.add(b"a", 3).is_err());
        t.update(b"a", 4);
        assert_eq!(t.keys(), vec![b"a".to_vec(), b"b".to_vec()]);
        t.remove(b"a");
        let _ = t.add(b"a", 5);
        assert_eq!(t.keys(), vec![b"b".to_vec(), b"a".to_vec()]);
        let slot = t.slot(b"b").unwrap_or(0);
        t.rekey(slot, b"c");
        assert_eq!(t.keys(), vec![b"c".to_vec(), b"a".to_vec()]);
        assert_eq!(t.get(b"c"), Some(&2));
    }
}
