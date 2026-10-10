//! `stmt->bound_params` and `stmt->bound_columns`: each a `HashTable`
//! of `struct pdo_bound_param_data`, which `really_register_bound_param`
//! allocates on the request's heap as php does, `ALLOC_HASHTABLE` and
//! `zend_hash_init(hash, 13, ...)` at the first entry, the entry's
//! canonical name a `zend_string_init` of its own, and the entry itself
//! `zend_hash_update_mem`'s copy, so `memory_get_usage()` reads php's
//! table (design §3.1, §5.13).

use php_ext::raising::Thrown;
use php_ext::request::Request;
use php_ext::values::{Emalloc, Key, Owner, PhpArray, PhpString, Value};

use crate::connector::{BindType, Binding};

/// `sizeof(struct pdo_bound_param_data)`.
const BOUND_PARAM_DATA: usize = 80;

/// The key a bound parameter or column is held under: its position or
/// its name, php's `HashTable` over both.
#[derive(Clone, PartialEq, Eq)]
pub(crate) enum BoundKey {
    Position(i64),
    Name(Vec<u8>),
}

/// `struct pdo_bound_param_data`.
pub(crate) struct Bound {
    /// `paramno`, `-1` for a name not yet placed.
    pub(crate) position: i64,
    /// `name`: the caller's string until `really_register_bound_param`
    /// makes it the entry's own ([`Bound::own_name`]).
    pub(crate) name: Option<Owner<PhpString>>,
    pub(crate) ty: BindType,
    /// `parameter`: a reference for `bindParam()` and `bindColumn()`.
    pub(crate) value: Owner<Value>,
    /// `driver_params`, `null` for none.
    pub(crate) driver_params: Owner<Value>,
    pub(crate) max_len: i64,
    pub(crate) is_parameter: bool,
}

impl Bound {
    /// The entry as the binding callback sees it.
    pub(crate) fn binding(&mut self) -> Binding<'_> {
        Binding {
            position: &mut self.position,
            name: self.name.as_ref().map(|n| n.as_bytes()),
            ty: self.ty,
            is_parameter: self.is_parameter,
            value: &mut self.value,
        }
    }

    /// The name's bytes, if it has one.
    pub(crate) fn name_bytes(&self) -> Option<&[u8]> {
        self.name.as_ref().map(|n| n.as_bytes())
    }

    /// The key the table holds the entry under, its canonical name or
    /// its position.
    pub(crate) fn key(&self) -> BoundKey {
        match self.name_bytes() {
            Some(name) => BoundKey::Name(name.to_vec()),
            None => BoundKey::Position(self.position),
        }
    }

    /// Whether the entry is held under `key`.
    pub(crate) fn is_keyed(&self, key: &BoundKey) -> bool {
        match (self.name_bytes(), key) {
            (Some(name), BoundKey::Name(n)) => name == n.as_slice(),
            (None, BoundKey::Position(n)) => self.position == *n,
            _ => false,
        }
    }

    /// `really_register_bound_param`'s copy of the name: a parameter's
    /// given its `:` by `zend_string_alloc`, anything else
    /// `zend_string_init`'d, each an allocation of the entry's own
    /// whatever its length; the caller's string given up. The `Err` is
    /// the memory limit's fatal.
    pub(crate) fn own_name(&mut self, req: &mut Request) -> Result<(), Thrown> {
        let Some(given) = self.name.take() else {
            return Ok(());
        };
        let colon = self.is_parameter && given.as_bytes().first() != Some(&b':');
        let len = given.len() + usize::from(colon);
        let made = PhpString::filled(req, len, |to| {
            let (head, tail) = to.split_at_mut(usize::from(colon));
            head.fill(b':');
            tail.copy_from_slice(given.as_bytes());
        });
        given.give_up(req);
        self.name = Some(made?);
        Ok(())
    }

    /// `param_dtor` past the callback: the name, the value and the
    /// connector's options given up.
    pub(crate) fn give_up(self, req: &mut Request) {
        if let Some(name) = self.name {
            name.give_up(req);
        }
        self.value.give_up(req);
        self.driver_params.give_up(req);
    }
}

/// One entry stored: the entry, and the `pdo_bound_param_data`
/// `zend_hash_update_mem` allocated for it.
pub(crate) struct Row {
    pub(crate) bound: Bound,
    held: Emalloc,
}

/// `stmt->bound_params` or `stmt->bound_columns`: the entries in the
/// table's order, and the `HashTable` php keeps them in, its keys
/// php's and its values nothing, which is what the request's heap holds
/// for it: the header, and the table laid down, grown and converted as
/// `zend_hash` lays it.
pub(crate) struct BoundTable {
    table: Owner<PhpArray>,
    rows: Vec<Row>,
}

impl BoundTable {
    /// `ALLOC_HASHTABLE` and `zend_hash_init(hash, 13, NULL, param_dtor,
    /// 0)`; the `Err` is the memory limit's fatal.
    pub(crate) fn new(req: &mut Request) -> Result<BoundTable, Thrown> {
        Ok(BoundTable {
            table: PhpArray::init_size(req, 13)?,
            rows: Vec::new(),
        })
    }

    /// `zend_hash_num_elements`.
    pub(crate) fn len(&self) -> usize {
        self.rows.len()
    }

    /// The entries in the table's order.
    pub(crate) fn iter(&self) -> impl Iterator<Item = &Bound> {
        self.rows.iter().map(|r| &r.bound)
    }

    /// The entries in the table's order, to change in place.
    pub(crate) fn iter_mut(&mut self) -> impl Iterator<Item = &mut Bound> {
        self.rows.iter_mut().map(|r| &mut r.bound)
    }

    /// Where the entry held under `key` is.
    pub(crate) fn find(&self, key: &BoundKey) -> Option<usize> {
        self.rows.iter().position(|r| r.bound.is_keyed(key))
    }

    /// The entry at `at` lent out of the order for a callback that can
    /// call into PHP code, the table keeping its key; [`BoundTable::put_back`]
    /// returns it.
    pub(crate) fn lend(&mut self, at: usize) -> Option<Row> {
        (at < self.rows.len()).then(|| self.rows.remove(at))
    }

    /// A lent entry back at `at`, or at the end where the order shrank.
    pub(crate) fn put_back(&mut self, at: usize, row: Row) {
        self.rows.insert(at.min(self.rows.len()), row);
    }

    /// `zend_hash_index_del(hash, paramno)` where the position is one,
    /// then `zend_hash_update_mem` or `zend_hash_index_update_mem` under
    /// the entry's key: the copy allocated, stored in place of an entry
    /// so keyed or at the end. Answers where it is stored and the
    /// entries it displaced, for `param_dtor`'s callback, their copies
    /// given back. The `Err` is the memory limit's fatal, the entry and
    /// whatever it displaced given up.
    pub(crate) fn store(
        &mut self,
        req: &mut Request,
        bound: Bound,
    ) -> Result<(usize, Vec<Bound>), Thrown> {
        let mut displaced = Vec::new();
        if bound.position >= 0
            && let Some(row) = self.remove(req, &BoundKey::Position(bound.position))?
        {
            displaced.push(row);
        }
        let held = match self.lay(req, &bound) {
            Ok(held) => held,
            Err(thrown) => {
                bound.give_up(req);
                for row in displaced {
                    row.give_up(req);
                }
                return Err(thrown);
            }
        };
        let key = bound.key();
        let row = Row { bound, held };
        let at = if let Some(at) = self.find(&key)
            && let Some(slot) = self.rows.get_mut(at)
        {
            displaced.push(std::mem::replace(slot, row));
            at
        } else {
            self.rows.push(row);
            self.rows.len() - 1
        };
        let displaced = displaced
            .into_iter()
            .map(|row| row.release(req))
            .collect::<Vec<_>>();
        Ok((at, displaced))
    }

    /// `zend_hash_update_mem`'s `pemalloc` of the copy, then the key
    /// written into the table; the `Err` is the memory limit's fatal.
    fn lay(&mut self, req: &mut Request, bound: &Bound) -> Result<Emalloc, Thrown> {
        let held = Emalloc::new(req, BOUND_PARAM_DATA)?;
        let key = match &bound.name {
            Some(name) => Owner::take(req, **name).into_key(),
            None => Owner::index(bound.position),
        };
        match self.table.set(req, key, Owner::null()) {
            Ok(()) => Ok(held),
            Err(thrown) => {
                held.free(req);
                Err(thrown)
            }
        }
    }

    /// `zend_hash_del` or `zend_hash_index_del`: the entry held under
    /// `key` taken out of the table, its copy given back, for
    /// `param_dtor`'s callback.
    pub(crate) fn delete(
        &mut self,
        req: &mut Request,
        key: &BoundKey,
    ) -> Result<Option<Bound>, Thrown> {
        Ok(self.remove(req, key)?.map(|row| row.release(req)))
    }

    fn remove(&mut self, req: &mut Request, key: &BoundKey) -> Result<Option<Row>, Thrown> {
        let Some(at) = self.find(key) else {
            return Ok(None);
        };
        let row = self.rows.remove(at);
        let table_key = match &row.bound.name {
            Some(name) => Key::Str(**name),
            None => Key::Int(row.bound.position),
        };
        self.table.unset(req, &table_key)?;
        Ok(Some(row))
    }

    /// `zend_hash_destroy` and `FREE_HASHTABLE`: every entry, in order,
    /// for `param_dtor`'s callback, each copy given back, then the table.
    pub(crate) fn destroy(self, req: &mut Request) -> Vec<Bound> {
        let rows = self.rows.into_iter().map(|row| row.release(req)).collect();
        self.table.give_up(req);
        rows
    }
}

impl Row {
    /// `efree(param)`: the copy given back, the entry handed on.
    pub(crate) fn release(self, req: &mut Request) -> Bound {
        self.held.free(req);
        self.bound
    }

    fn give_up(self, req: &mut Request) {
        self.release(req).give_up(req);
    }
}
