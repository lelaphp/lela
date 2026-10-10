//! `HashTable` as `php_sdl.c` and `php_schema.c` use it, for values that
//! are no PHP values: string and integer keys in insertion order, an add
//! that refuses a key already there (`zend_hash_add`), an update that
//! replaces in place (`zend_hash_update`), the next free integer key
//! (`zend_hash_next_index_insert`) and removal (`zend_hash_del`), so the
//! order a WSDL is read in is the order `__getFunctions()`, `__getTypes()`
//! and the encoders walk it in.

use std::collections::HashMap;

/// A key: `zend_string` or `zend_ulong`.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum Key {
    Str(Vec<u8>),
    Int(u64),
}

impl Key {
    pub(crate) fn str(bytes: &[u8]) -> Key {
        Key::Str(bytes.to_vec())
    }
}

/// One table: the rows in order, an index by key, and the next free
/// integer key.
#[derive(Debug, Clone)]
pub(crate) struct Table<V> {
    rows: Vec<Option<(Key, V)>>,
    index: HashMap<Key, usize>,
    next_free: u64,
    live: usize,
}

impl<V> Default for Table<V> {
    fn default() -> Table<V> {
        Table {
            rows: Vec::new(),
            index: HashMap::new(),
            next_free: 0,
            live: 0,
        }
    }
}

impl<V> Table<V> {
    pub(crate) fn new() -> Table<V> {
        Table::default()
    }

    /// `zend_hash_num_elements`.
    pub(crate) fn len(&self) -> usize {
        self.live
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.live == 0
    }

    fn note_int(&mut self, key: &Key) {
        if let Key::Int(i) = key
            && *i >= self.next_free
        {
            self.next_free = i.saturating_add(1);
        }
    }

    /// `zend_hash_add`: `false`, and `value` dropped, where `key` is
    /// already there.
    pub(crate) fn add(&mut self, key: Key, value: V) -> bool {
        if self.index.contains_key(&key) {
            return false;
        }
        self.note_int(&key);
        self.index.insert(key.clone(), self.rows.len());
        self.rows.push(Some((key, value)));
        self.live += 1;
        true
    }

    /// `zend_hash_update`: the row replaced where it stands, or appended;
    /// the value it replaced, for the caller to destroy as the table's
    /// destructor would.
    pub(crate) fn update(&mut self, key: Key, value: V) -> Option<V> {
        if let Some(&at) = self.index.get(&key) {
            let row = self.rows.get_mut(at)?;
            return row.replace((key, value)).map(|(_, old)| old);
        }
        self.add(key, value);
        None
    }

    /// `zend_hash_str_update`.
    pub(crate) fn update_str(&mut self, key: &[u8], value: V) -> Option<V> {
        self.update(Key::str(key), value)
    }

    /// `zend_hash_next_index_insert`: the key it took.
    pub(crate) fn push(&mut self, value: V) -> u64 {
        let key = self.next_free;
        self.add(Key::Int(key), value);
        key
    }

    /// `zend_hash_find`.
    pub(crate) fn get(&self, key: &Key) -> Option<&V> {
        let at = *self.index.get(key)?;
        self.rows.get(at)?.as_ref().map(|(_, v)| v)
    }

    /// `zend_hash_str_find`.
    pub(crate) fn get_str(&self, key: &[u8]) -> Option<&V> {
        self.get(&Key::Str(key.to_vec()))
    }

    /// `zend_hash_index_find`.
    pub(crate) fn get_int(&self, key: u64) -> Option<&V> {
        self.get(&Key::Int(key))
    }

    pub(crate) fn contains(&self, key: &Key) -> bool {
        self.index.contains_key(key)
    }

    /// `zend_hash_del`: the value, where the key was there.
    pub(crate) fn remove(&mut self, key: &Key) -> Option<V> {
        let at = self.index.remove(key)?;
        let row = self.rows.get_mut(at)?.take()?;
        self.live -= 1;
        Some(row.1)
    }

    /// The rows in order.
    pub(crate) fn iter(&self) -> impl Iterator<Item = (&Key, &V)> {
        self.rows.iter().flatten().map(|(k, v)| (k, v))
    }

    /// The values in order.
    pub(crate) fn values(&self) -> impl Iterator<Item = &V> {
        self.rows.iter().flatten().map(|(_, v)| v)
    }

    pub(crate) fn values_mut(&mut self) -> impl Iterator<Item = &mut V> {
        self.rows.iter_mut().flatten().map(|(_, v)| v)
    }

    /// The values in order, the table consumed.
    pub(crate) fn into_values(self) -> impl Iterator<Item = V> {
        self.rows.into_iter().flatten().map(|(_, v)| v)
    }

    /// The same table over `f` of each value: the rows, their keys and
    /// their order kept.
    pub(crate) fn map<U>(&self, f: impl Fn(&V) -> U) -> Table<U> {
        Table {
            rows: self
                .rows
                .iter()
                .map(|row| row.as_ref().map(|(k, v)| (k.clone(), f(v))))
                .collect(),
            index: self.index.clone(),
            next_free: self.next_free,
            live: self.live,
        }
    }

    /// The first row, `zend_hash_internal_pointer_reset` and the current
    /// value.
    pub(crate) fn first(&self) -> Option<&V> {
        self.values().next()
    }

    /// The value at `n` rows from the start, in order.
    pub(crate) fn nth(&self, n: usize) -> Option<&V> {
        self.values().nth(n)
    }
}
