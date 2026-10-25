//! The shutdown functions of a request: `register_shutdown_function()`
//! and the named entries an extension sets, kept as php keeps
//! `BG(user_shutdown_function_names)`, what `php_call_shutdown_functions`
//! reads, and their release. The table is [`ShutdownFunctions`], whose
//! fields no file but this one reads.

use crate::api::abi::Callable;
use crate::api::raising::Thrown;
use crate::api::values::{Key, Owner, PhpArray, PhpString, Value, walk};

use super::Code;
use super::Request;

/// `sizeof(zval)`, what `safe_emalloc` asks per argument a shutdown
/// entry keeps.
const ZVAL_SIZE: usize = 16;

/// `sizeof(php_shutdown_function_entry)` at 8.5
/// (`Z/ext/standard/basic_functions.h:125`): the `zend_fcall_info_cache`'s
/// five pointers, `params` and `param_count`.
const SHUTDOWN_RECORD: usize = 56;

/// `BG(user_shutdown_function_names)`: the table php keeps the entries
/// in, one of the request's own arrays, laid down at the first entry,
/// grown as a packed table grows and turned into a hash by the first
/// entry registered under a name, so what `memory_get_usage()` counts
/// and where the memory limit refuses are php's; and the entries its
/// elements name by index, each with the blocks php allocates for it. An
/// entry's slot stays empty once its entry is gone, the table having
/// dropped the element that named it.
///
/// The first entry lays the table's header down and its eight packed
/// slots beside the entry and its one argument; the ninth grows the
/// slots to sixteen:
///
/// ```php
/// $f = function (...$a) {};
/// $m = [memory_get_usage()];
/// register_shutdown_function($f, 'a');
/// $m[] = memory_get_usage();
/// register_shutdown_function($f);
/// $m[] = memory_get_usage();
/// for ($i = 0; $i < 7; $i++) register_shutdown_function($f);
/// $m[] = memory_get_usage();
/// register_shutdown_function($f, 1, 2, 3);
/// $m[] = memory_get_usage();
/// for ($i = 1; $i < count($m); $i++) echo $m[$i] - $m[$i - 1], "\n";
/// ```
/// ```expect
/// 504
/// 56
/// 552
/// 104
/// ```
#[derive(Default)]
pub(super) struct ShutdownFunctions {
    table: Option<PhpArray>,
    entries: Vec<Option<ShutdownEntry>>,
}

/// One `php_shutdown_function_entry`: its callback and arguments, which
/// it owns, the block the entry is, and the arguments' `params` block.
struct ShutdownEntry {
    callback: Owner<Callable>,
    arguments: Vec<Value>,
    record: php_heap::Block,
    params: Option<php_heap::Block>,
}

impl ShutdownFunctions {
    /// The entry kept, and the index the table's element names it by.
    fn slot(&mut self, entry: ShutdownEntry) -> i64 {
        self.entries.push(Some(entry));
        i64::try_from(self.entries.len() - 1).unwrap_or(i64::MAX)
    }

    /// The entry a table element names.
    fn entry(&self, index: &Value) -> Option<&ShutdownEntry> {
        let index = usize::try_from(index.as_int()?).ok()?;
        self.entries.get(index)?.as_ref()
    }

    /// The entry at `index`, taken out of its slot.
    fn take(&mut self, index: i64) -> Option<ShutdownEntry> {
        self.entries.get_mut(usize::try_from(index).ok()?)?.take()
    }
}

impl Request {
    /// `zend_hash_str_update_mem`: the entry made and written under
    /// `key`, whose owner the table takes, or given up with the key
    /// where the memory limit refuses it.
    fn set_named_shutdown_entry(
        &mut self,
        key: Owner<Key>,
        callback: Owner<Callable>,
        arguments: Vec<Value>,
    ) -> Result<(), Thrown> {
        let (entry, mut table) = match self.shutdown_entry(callback, arguments) {
            Ok(made) => made,
            Err(thrown) => {
                key.give_up(self);
                return Err(thrown);
            }
        };
        let index = self.shutdown_functions.slot(entry);
        let set = table.set(self, key, Owner::int(index));
        if set.is_err() {
            self.drop_shutdown_entry(index);
        }
        set
    }

    /// The entry at or after the table's position `at`, as
    /// `zend_hash_apply` reaches it: the position it stands at, and its
    /// callback and arguments as views, the table keeping its owners, so
    /// the walk serves what it reads and an entry appended while it runs
    /// is reached by the same walk. `None` past the end is what ends it.
    pub(crate) fn shutdown_function(&self, at: u32) -> Option<(u32, Callable, Vec<Value>)> {
        let mut at = at;
        loop {
            let (position, _, index) = self.shutdown_functions.table?.live_from(at)?;
            if let Some(entry) = self.shutdown_functions.entry(&index) {
                return Some((position, (*entry.callback).clone(), entry.arguments.clone()));
            }
            at = position + 1;
        }
    }

    /// `php_free_shutdown_functions` (`Z/ext/standard/basic_functions.c:1660`),
    /// step 7 of `php_request_shutdown`, behind step 6 and its
    /// `php_output_deactivate`: `zend_hash_destroy` over the table, each
    /// entry's arguments, callback and record given up in the table's
    /// order by `user_shutdown_function_dtor`, then the table.
    pub(crate) fn free_shutdown_functions(&mut self) {
        let Some(table) = self.shutdown_functions.table.take() else {
            return;
        };
        let indexes: Vec<i64> = walk::rows(self, table)
            .filter_map(|(_, index)| index.deref().as_int())
            .collect();
        for index in indexes {
            self.drop_shutdown_entry(index);
        }
        self.release(Value::array(table));
        self.shutdown_functions.entries.clear();
    }

    /// One `php_shutdown_function_entry` out of the heap in php's order,
    /// with the table it goes into: the arguments' `params` first,
    /// `safe_emalloc(param_count, sizeof(zval))` where there are any, in
    /// `register_shutdown_function`; then the table, laid down at the
    /// first entry (`ALLOC_HASHTABLE`); then the entry itself, which
    /// `zend_hash_next_index_insert_mem` and `zend_hash_str_update_mem`
    /// copy into a block of its size. Where the memory limit refuses one
    /// of them, what was made for the entry is given up with the
    /// callback and the arguments.
    fn shutdown_entry(
        &mut self,
        callback: Owner<Callable>,
        arguments: Vec<Value>,
    ) -> Result<(ShutdownEntry, PhpArray), Thrown> {
        let params = if arguments.is_empty() {
            Ok(None)
        } else {
            let params = self.heap.block(arguments.len() * ZVAL_SIZE);
            self.fatal_of(params).map(Some)
        };
        let made = params.and_then(|params| {
            let table = self.shutdown_table();
            let record = table.and_then(|table| {
                let record = self.heap.block(SHUTDOWN_RECORD);
                self.fatal_of(record).map(|record| (record, table))
            });
            match record {
                Ok((record, table)) => Ok((record, params, table)),
                Err(thrown) => {
                    if let Some(params) = params {
                        self.heap.free_block(params);
                    }
                    Err(thrown)
                }
            }
        });
        match made {
            Ok((record, params, table)) => Ok((
                ShutdownEntry {
                    callback,
                    arguments,
                    record,
                    params,
                },
                table,
            )),
            Err(thrown) => {
                self.drop_callback(callback, arguments);
                Err(thrown)
            }
        }
    }

    /// A callback and its arguments given up where no entry was made of
    /// them.
    pub(super) fn drop_callback(&mut self, callback: Owner<Callable>, arguments: Vec<Value>) {
        callback.give_up(self);
        for value in arguments {
            self.release(value);
        }
    }

    /// `BG(user_shutdown_function_names)`, laid down at the first entry
    /// as `ALLOC_HASHTABLE` and `zend_hash_init` lay it down.
    fn shutdown_table(&mut self) -> Result<PhpArray, Thrown> {
        if let Some(table) = self.shutdown_functions.table {
            return Ok(table);
        }
        let table = self.array()?;
        self.shutdown_functions.table = Some(table);
        Ok(table)
    }

    /// `user_shutdown_function_dtor`: the entry's arguments released, its
    /// `params` freed, its callback released and its record freed.
    fn drop_shutdown_entry(&mut self, index: i64) {
        let Some(entry) = self.shutdown_functions.take(index) else {
            return;
        };
        for value in entry.arguments {
            self.release(value);
        }
        if let Some(params) = entry.params {
            self.heap.free_block(params);
        }
        entry.callback.give_up(self);
        self.heap.free_block(entry.record);
    }
}

impl Code<&mut Request> {
    /// `register_shutdown_function()`, `append_user_shutdown_function`
    /// (`Z/ext/standard/basic_functions.c:1680-1735`): one entry appended
    /// at the table's next index, which is the order
    /// [`crate::natives::call_shutdown_functions`] serves them in, out of
    /// the request's heap as php lays it down ([`ShutdownFunctions`]). The
    /// callback and the arguments are the entry's owners from here,
    /// given up by [`Request::free_shutdown_functions`]; where the memory
    /// limit refuses the entry they are given up at once and the `Err` is
    /// that fatal.
    pub fn register_shutdown_function(
        self,
        callback: Owner<Callable>,
        arguments: Vec<Value>,
    ) -> Result<(), Thrown> {
        let (entry, mut table) = self.0.shutdown_entry(callback, arguments)?;
        let index = self.0.shutdown_functions.slot(entry);
        if let Err(thrown) = table.append(self.0, Owner::int(index)) {
            self.0.drop_shutdown_entry(index);
            return Err(thrown);
        }
        Ok(())
    }

    /// `register_user_shutdown_function` and
    /// `remove_user_shutdown_function` (`Z/ext/standard/basic_functions.c:1705-1725`),
    /// the entry an extension keeps under a name of its own: `Some`
    /// replaces the entry of that name where it stands, which
    /// `zend_hash_str_update_mem` keeps in its place, or adds one; `None`
    /// removes it. The owners are the table's as
    /// [`Code::register_shutdown_function`]'s, and what an entry held
    /// is given up where it goes. The session's `session_shutdown` is the
    /// witness, in `ext/session`.
    pub fn set_shutdown_function(
        self,
        name: &[u8],
        entry: Option<(Owner<Callable>, Vec<Value>)>,
    ) -> Result<(), Thrown> {
        // `zend_hash_str_update_mem` keys by the bytes, never numerically.
        let key = match PhpString::new(self.0, name) {
            Ok(name) => name.into_key(),
            Err(thrown) => {
                if let Some((callback, arguments)) = entry {
                    self.0.drop_callback(callback, arguments);
                }
                return Err(thrown);
            }
        };
        let standing = self
            .0
            .shutdown_functions
            .table
            .and_then(|table| table.get(&key))
            .and_then(|index| index.as_int());
        let result = if let Some((callback, arguments)) = entry {
            self.0.set_named_shutdown_entry(key, callback, arguments)
        } else {
            let unset = match self.0.shutdown_functions.table {
                Some(mut table) => table.unset(self.0, &key).map(drop),
                None => Ok(()),
            };
            key.give_up(self.0);
            unset
        };
        if let Some(index) = standing {
            self.0.drop_shutdown_entry(index);
        }
        result
    }
}
