//! A `HashTable` with no destructor as the request's heap sees it, the
//! model php's libxml glue and the extensions that include it share
//! (design §5.13): `libxml_doc_props`' class map here, dom's namespace
//! mapper, its walks' tables and its template table.

use php_ext::raising::Thrown;
use php_ext::request::Request;
use php_ext::values::Emalloc;

/// `HT_MIN_SIZE`, the size `zend_hash_real_init_mixed` lays a table down
/// at.
const HT_MIN_SIZE: usize = 8;

/// `sizeof(HashTable)`, what `ALLOC_HASHTABLE` `emalloc`s.
const HASH_TABLE: usize = 56;

/// `HT_SIZE_EX(size, HT_SIZE_TO_MASK(size))`: a bucket a slot and two
/// hash words a slot.
const fn hash_size(size: usize) -> usize {
    size * 40
}

/// A `HashTable` with no destructor as the request's heap sees it: its
/// rows in their order, each at the bucket it was added at, its data
/// laid down at `HT_MIN_SIZE` by `zend_hash_real_init_mixed`, and the
/// 56-byte struct where php `ALLOC_HASHTABLE`s it rather than embedding
/// it. A full table is compacted in place where deletions left more than
/// a thirty-second of its buckets empty, and doubled otherwise, the new
/// data allocated before the old is freed (`zend_hash_do_resize`); a row
/// deleted from the end gives back the empty buckets before it
/// (`_zend_hash_del_el_ex`). So what a table costs is php's, the lookup
/// a walk of the rows. What a row holds is its owner's to give up: take
/// the rows ([`Table::take_rows`]) before [`Table::free`].
#[derive(Debug)]
pub struct Table<R> {
    /// The rows in order, each with its bucket.
    rows: Vec<(usize, R)>,
    /// `nNumUsed`: one past the last bucket a row stands in.
    used: usize,
    data: Option<Emalloc>,
    /// `ALLOC_HASHTABLE`'s struct, `None` for a table php embeds.
    header: Option<Emalloc>,
}

impl<R> Default for Table<R> {
    /// `zend_hash_init`: an embedded table, nothing laid down.
    fn default() -> Table<R> {
        Table {
            rows: Vec::new(),
            used: 0,
            data: None,
            header: None,
        }
    }
}

impl<R> Table<R> {
    /// `ALLOC_HASHTABLE`, `zend_hash_init` and
    /// `zend_hash_real_init_mixed`; the `Err` is the memory limit.
    pub fn allocated(req: &mut Request) -> Result<Table<R>, Thrown> {
        let mut table = Table::header(req)?;
        if let Err(thrown) = table.real_init(req) {
            table.free(req);
            return Err(thrown);
        }
        Ok(table)
    }

    /// `ALLOC_HASHTABLE` and `zend_hash_init`, the data laid down at the
    /// first row; the `Err` is the memory limit.
    pub(crate) fn header(req: &mut Request) -> Result<Table<R>, Thrown> {
        Ok(Table {
            header: Some(Emalloc::new(req, HASH_TABLE)?),
            ..Table::default()
        })
    }

    /// `zend_hash_real_init_mixed`; the `Err` is the memory limit.
    pub fn real_init(&mut self, req: &mut Request) -> Result<(), Thrown> {
        if self.data.is_none() {
            self.data = Some(Emalloc::new(req, hash_size(HT_MIN_SIZE))?);
        }
        Ok(())
    }

    /// `ZEND_HASH_IF_FULL_DO_RESIZE`, before a row is added: the data laid
    /// down at the first, and a full table compacted or doubled; the `Err`
    /// is the memory limit.
    pub fn make_room(&mut self, req: &mut Request) -> Result<(), Thrown> {
        let size = match &self.data {
            None => return self.real_init(req),
            Some(data) => data.len() / hash_size(1),
        };
        if self.used < size {
            return Ok(());
        }
        let len = self.rows.len();
        if self.used > len + (len >> 5) {
            for (bucket, row) in self.rows.iter_mut().enumerate() {
                row.0 = bucket;
            }
            self.used = len;
            return Ok(());
        }
        let grown = Emalloc::new(req, hash_size(size * 2))?;
        if let Some(old) = self.data.replace(grown) {
            old.free(req);
        }
        Ok(())
    }

    /// A row added in the room [`Table::make_room`] made.
    pub fn push(&mut self, row: R) {
        self.rows.push((self.used, row));
        self.used += 1;
    }

    /// `zend_hash_add_new`: room made, then the row; the `Err` is the
    /// memory limit, the row not added.
    pub fn add(&mut self, req: &mut Request, row: R) -> Result<(), Thrown> {
        self.make_room(req)?;
        self.push(row);
        Ok(())
    }

    /// The rows, in order.
    pub fn rows(&self) -> impl Iterator<Item = &R> {
        self.rows.iter().map(|(_, row)| row)
    }

    /// The rows, in order, to change in place, as `zend_hash_update`
    /// writes a row's value in its bucket.
    pub fn rows_mut(&mut self) -> impl Iterator<Item = &mut R> {
        self.rows.iter_mut().map(|(_, row)| row)
    }

    /// `zend_hash_num_elements`.
    #[must_use]
    pub fn len(&self) -> usize {
        self.rows.len()
    }

    /// Whether no row stands.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// `zend_hash_del_bucket` of the row at `at` in order: nothing is
    /// freed.
    pub fn remove(&mut self, at: usize) -> R {
        let (bucket, row) = self.rows.remove(at);
        if bucket + 1 == self.used {
            self.used = self.rows.last().map_or(0, |(b, _)| b + 1);
        }
        row
    }

    /// The rows taken out in order, for `zend_hash_destroy`'s walk.
    pub fn take_rows(&mut self) -> Vec<R> {
        self.used = 0;
        self.rows.drain(..).map(|(_, row)| row).collect()
    }

    /// `zend_hash_destroy`'s `efree` of the data, then `FREE_HASHTABLE`'s
    /// of the struct; the rows are the caller's, taken first.
    pub fn free(&mut self, req: &mut Request) {
        if let Some(data) = self.data.take() {
            data.free(req);
        }
        if let Some(header) = self.header.take() {
            header.free(req);
        }
    }
}
