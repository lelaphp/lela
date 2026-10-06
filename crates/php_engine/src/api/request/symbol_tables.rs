//! The symbol tables of a request: `EG(symbol_table)` and the table
//! `zend_rebuild_symbol_table` gives a function frame that included a
//! file, a frame's compiled variables attached to a table's names and
//! detached as it leaves, the writes, unsets and lookups `$GLOBALS`,
//! `compact`, `extract` and `get_defined_vars` make through them, and
//! their release. The tables and the cache of rebuilt tables' bytes stay
//! on the request, since `hash_bytes` keeps the bytes beside them and
//! the tier face reads them.

use php_heap::{FrameRef, Slot, Zval, ZvalRef};

use crate::api::raising::Thrown;
use crate::api::values::{Key, PhpArray, PhpRef, PhpString, TypeTag, Value};
use crate::exec::{self, TableId};

use super::{Request, Symbol, SymbolTable};

impl Request {
    /// What a symbol table holds that is no CV of its frame, for the
    /// collector: the frame's walk names the CVs themselves, which is
    /// where php's `IS_INDIRECT` buckets point.
    pub(crate) fn gc_symbol_table(&self, table: TableId, visit: &mut dyn FnMut(Value)) {
        let Some(table) = self.tables.get(table.0 as usize) else {
            return;
        };
        for (_, symbol) in &table.entries {
            match symbol {
                Symbol::Value(value) => visit(*value),
                Symbol::Held(cell) => visit(cell.as_value()),
                Symbol::Indirect(..) => {}
            }
        }
    }

    /// `zend_attach_symbol_table` (`Z/Zend/zend_execute_API.c:1857`) on
    /// the table `frame` names: every CV of the frame's op array, in the
    /// order the compiler numbered them, takes what the table holds
    /// under its name, moved out of the bucket or out of the CV of the
    /// frame the bucket points at (`ZVAL_COPY_VALUE`, no owner taken),
    /// and the bucket points at this frame's CV from here; a name the
    /// table has not is added, its CV left `UNDEF`. What the main op
    /// array's activation does with `EG(symbol_table)`, what a code
    /// frame's does with its includer's, and what the includer does
    /// again when the code frame leaves, so a variable the included
    /// file wrote is the includer's.
    pub(crate) fn attach_symbol_table(&mut self, frame: FrameRef) {
        let Some(table) = self.vm.at(frame).and_then(|state| state.symbol_table) else {
            return;
        };
        for (at, name) in self.frame_var_names(frame).into_iter().enumerate() {
            let at = u32::try_from(at).unwrap_or(u32::MAX);
            let found = self.table_mut(table).and_then(|entries| {
                let index = entries.iter().position(|(key, _)| *key == name)?;
                Some(std::mem::replace(
                    &mut entries[index].1,
                    Symbol::Indirect(frame, at),
                ))
            });
            let value = match found {
                Some(Symbol::Value(value)) => value,
                Some(Symbol::Held(cell)) => self.unhold(cell),
                Some(Symbol::Indirect(other, slot)) => self.take_slot(other, slot),
                None => {
                    if let Some(entries) = self.table_mut(table) {
                        entries.push((name, Symbol::Indirect(frame, at)));
                        self.table_bytes(table, true, false);
                    }
                    continue;
                }
            };
            // The CV was `UNDEF` when the frame was pushed, or is its
            // own value taken back, so the slot gives up nothing.
            let previous = self.set_slot(frame, at, value);
            self.release(previous);
        }
    }

    /// `zend_detach_symbol_table` (`Z/Zend/zend_execute_API.c:1892`),
    /// what php does when it leaves a frame that has a symbol table, the
    /// main op array's frame, the one it marks `ZEND_CALL_TOP_CODE` and
    /// gives `EG(symbol_table)` as its own, and every code frame an
    /// `INCLUDE_OR_EVAL` pushed (design §5.6): every CV that holds
    /// something is moved into the table under the name the compiler
    /// numbered it by, the bucket keeping its place, the slot left
    /// `UNDEF` so the pop releases nothing, and a name whose CV holds
    /// nothing is deleted from the table. The variables are the
    /// table's from here: the request's for the main frame, which is
    /// why `$a = new A;` is still an owner while the shutdown
    /// functions are served and the destructor waits for
    /// [`Request::shutdown_destructors`], and the includer's for a code
    /// frame, whose re-attach takes them back.
    pub(crate) fn detach_symbol_table(&mut self, frame: FrameRef) {
        let Some(table) = self.vm.at(frame).and_then(|state| state.symbol_table) else {
            return;
        };
        for (at, name) in self.frame_var_names(frame).into_iter().enumerate() {
            let value = self.take_slot(frame, u32::try_from(at).unwrap_or(u32::MAX));
            if value.tag() == TypeTag::Undef {
                self.symbol_del(table, &name);
            } else {
                self.symbol_update(table, name, value);
            }
        }
    }

    /// `zend_rebuild_symbol_table` (`Z/Zend/zend_execute_API.c:1810`) for
    /// `frame`: the table it already has, or a new one of its own with
    /// every CV of its op array appended as `IS_INDIRECT` to the CV
    /// (`_zend_hash_append_ind`), the values staying where they are, and
    /// the frame marked `ZEND_CALL_HAS_SYMBOL_TABLE`. What a function
    /// frame gets when it includes a file, so the included code reads
    /// and writes the function's variables; freed when the frame pops
    /// (`zend_clean_and_cache_symbol_table`). Its bytes are php's, on
    /// the request's heap ([`Request::rebuilt_table_bytes`]).
    pub(crate) fn rebuild_symbol_table(&mut self, frame: FrameRef) -> TableId {
        if let Some(table) = self.vm.at(frame).and_then(|state| state.symbol_table) {
            return table;
        }
        let entries = self
            .frame_var_names(frame)
            .into_iter()
            .enumerate()
            .map(|(at, name)| {
                (
                    name,
                    Symbol::Indirect(frame, u32::try_from(at).unwrap_or(u32::MAX)),
                )
            })
            .collect::<Vec<_>>();
        let last_var = u32::try_from(entries.len()).unwrap_or(u32::MAX);
        let bytes = self.rebuilt_table_bytes(last_var);
        let table = TableId(u32::try_from(self.tables.len()).unwrap_or(u32::MAX));
        self.tables.push(SymbolTable {
            owner: Some(frame),
            entries,
            bytes,
        });
        if let Some(state) = self.vm.at_mut(frame) {
            state.symbol_table = Some(table);
        }
        table
    }

    /// The names of the CVs of the op array `frame` serves, in the
    /// order the compiler numbered them, `op_array->vars`.
    pub(super) fn frame_var_names(&self, frame: FrameRef) -> Vec<Vec<u8>> {
        self.vm
            .at(frame)
            .and_then(|state| exec::body_of(&state.unit, state.body))
            .map(|code| code.vars.iter().map(|name| name.0.to_vec()).collect())
            .unwrap_or_default()
    }

    pub(super) fn table_mut(&mut self, table: TableId) -> Option<&mut Vec<(Vec<u8>, Symbol)>> {
        self.tables
            .get_mut(table.0 as usize)
            .map(|table| &mut table.entries)
    }

    /// `zend_clean_and_cache_symbol_table` when the frame that rebuilt a
    /// table pops: its values released and the table gone, its bytes
    /// kept in `EG(symtable_cache)`. A bucket still pointing at a CV
    /// points at a slot the pop is giving up, and owns nothing.
    ///
    /// The table is the one the frame names, php's `EX(symbol_table)`,
    /// and only where it was rebuilt for this frame: a code frame shares
    /// its includer's. A frame is named by its place on a stack, which a
    /// frame on a fiber's stack shares with one on the request's, and
    /// which a generator's frame leaves at every `yield`, so the table
    /// is never found by the place alone:
    ///
    /// ```php
    /// function inFiber() { $n = 'a'; $$n = 'A'; $m = 'n'; Fiber::suspend(); echo $a, ' ', $$m, "\n"; }
    /// function inFiber2() { inFiber(); }
    /// $f = new Fiber('inFiber'); $f->start();
    /// $g = new Fiber('inFiber2'); $g->start();
    /// function other() { $q = 'q'; }
    /// function other2() { other(); }
    /// other(); other2();
    /// $f->resume(); $g->resume();
    /// ```
    /// ```expect
    /// A a
    /// A a
    /// ```
    ///
    /// A table is named by its place among the request's, so one freed
    /// below a table that still stands is left in its place, owned by no
    /// frame and empty, and taken off with the last one above it: tables
    /// are built in the order frames are pushed, except where a frame
    /// below the innermost is given one, as an evaluation in a paused
    /// frame gives it ([`Request::eval_in_frame`]).
    pub(super) fn free_rebuilt_table(&mut self, frame: FrameRef) {
        let Some(table) = self.vm.at(frame).and_then(|state| state.symbol_table) else {
            return;
        };
        let at = table.0 as usize;
        if self
            .tables
            .get(at)
            .is_some_and(|rebuilt| rebuilt.owner == Some(frame))
        {
            self.free_table_at(at);
        }
    }

    /// The table a generator's frame was given, as the frame comes back
    /// on the stack at `frame`, wherever it stood before: the frame's at
    /// its new place, each bucket that pointed at a CV where the frame
    /// stood before pointing at it where it stands now, as php's
    /// `IS_INDIRECT` buckets point into the `zend_execute_data` the
    /// generator keeps.
    pub(crate) fn resume_generator_table(&mut self, table: TableId, frame: FrameRef) {
        let Some(rebuilt) = self.tables.get_mut(table.0 as usize) else {
            return;
        };
        let Some(before) = rebuilt.owner else {
            return;
        };
        rebuilt.owner = Some(frame);
        for (_, symbol) in &mut rebuilt.entries {
            if let Symbol::Indirect(at, slot) = *symbol
                && at == before
            {
                *symbol = Symbol::Indirect(frame, slot);
            }
        }
    }

    /// `zend_clean_and_cache_symbol_table` in `zend_generator_close`,
    /// over the table a generator's frame was given, before its CVs are
    /// freed, wherever the frame stands, on the stack or off it.
    pub(crate) fn free_generator_table(&mut self, table: TableId) {
        let at = table.0 as usize;
        if self
            .tables
            .get(at)
            .is_some_and(|rebuilt| rebuilt.owner.is_some())
        {
            self.free_table_at(at);
        }
    }

    /// The rebuilt table at `at` freed: its values released and its
    /// bytes kept in `EG(symtable_cache)`, the place left empty and owned
    /// by none, and every such place on top taken off.
    fn free_table_at(&mut self, at: usize) {
        let freed = self.tables.get_mut(at).map(|table| SymbolTable {
            owner: table.owner.take(),
            entries: std::mem::take(&mut table.entries),
            bytes: table.bytes.take(),
        });
        // `EG(symbol_table)`, the first, is never one a frame rebuilt.
        while self.tables.len() > 1 && self.tables.last().is_some_and(|t| t.owner.is_none()) {
            self.tables.pop();
        }
        let Some(mut table) = freed else {
            return;
        };
        for (_, symbol) in std::mem::take(&mut table.entries) {
            self.release_symbol(symbol);
        }
        self.cache_symbol_table(table);
    }

    /// `ZEND_BIND_GLOBAL`'s half over `EG(symbol_table)`
    /// (`Z/Zend/zend_vm_def.h:8679`): the bucket `name` has, laid down
    /// holding `null` where the table has none (`zend_hash_lookup`); an
    /// `IS_INDIRECT` bucket followed to the CV of the frame attached to
    /// it, an `UNDEF` there taken as `null`; what stands there made a
    /// reference in place where it is none (`ZVAL_MAKE_REF_EX`); and the
    /// cell handed back with one more owner, the binding's.
    pub(crate) fn global_cell(&mut self, name: &[u8]) -> Result<PhpRef, Thrown> {
        let found = self.globals().iter().position(|(key, _)| key == name);
        let at = found.unwrap_or_else(|| {
            self.globals()
                .push((name.to_vec(), Symbol::Value(Value::null())));
            self.globals().len() - 1
        });
        let cell = match self.globals()[at].1 {
            Symbol::Held(held) => {
                let place = held.heap().val_ref();
                let value = Value::of_zval(place.read());
                if let Some(cell) = value.as_php_ref() {
                    *cell
                } else {
                    let cell = self.reference(value.undef_as_null())?;
                    place.write(cell.as_value().to_zval());
                    cell
                }
            }
            Symbol::Indirect(frame, slot) => {
                let Some(place) = self.attached_ptr(TableId::GLOBAL, frame, slot) else {
                    return Err(crate::api::raising::fatal(
                        self,
                        "InternalError: a global bound to a CV the frame does not have",
                    ));
                };
                let held = Value::of_zval(place.read());
                if let Some(cell) = held.as_php_ref() {
                    *cell
                } else {
                    let cell = self.reference(held.undef_as_null())?;
                    // The CV's owner of what it held is the cell's now.
                    place.write(cell.as_value().to_zval());
                    cell
                }
            }
            Symbol::Value(held) => {
                if let Some(cell) = held.as_php_ref() {
                    *cell
                } else {
                    let cell = self.reference(held.undef_as_null())?;
                    self.globals()[at].1 = Symbol::Value(cell.as_value());
                    cell
                }
            }
        };
        self.addref(&cell.as_value());
        Ok(cell)
    }

    /// One symbol written, `zend_hash_update`: `value` is the table's
    /// and what stood under the name is released after the write, the
    /// name keeping the position it had, as php's update keeps a
    /// bucket's and only a name the table has not appends. A bucket
    /// that pointed at a CV owned nothing.
    pub(super) fn symbol_update(&mut self, table: TableId, name: Vec<u8>, value: Value) {
        let Some(entries) = self.table_mut(table) else {
            return;
        };
        if let Some(at) = entries.iter().position(|(key, _)| *key == name) {
            let old = std::mem::replace(&mut entries[at].1, Symbol::Value(value));
            self.release_symbol(old);
            return;
        }
        entries.push((name, Symbol::Value(value)));
        self.table_bytes(table, true, false);
    }

    /// One symbol deleted, `zend_hash_del`: the name gone and what it
    /// held released; a name the table has not is no error, as php's
    /// delete of a missing key is none.
    pub(super) fn symbol_del(&mut self, table: TableId, name: &[u8]) {
        let Some(entries) = self.table_mut(table) else {
            return;
        };
        let Some(at) = entries.iter().position(|(key, _)| key == name) else {
            return;
        };
        let (_, symbol) = entries.remove(at);
        let last = at == entries.len();
        self.table_bytes(table, false, last);
        self.release_symbol(symbol);
    }

    /// `zend_unwrap_reference`: a reference a call handed back, which a
    /// method declared `&__toString` or `&__debugInfo` does, given up for
    /// the value it holds, one owner of that the caller's; any other
    /// value as it stands.
    ///
    /// ```php
    /// class S { private $s = 'x'; function &__toString(): string { return $this->s; } }
    /// echo new S, "\n", (string) new S, "\n";
    /// class D { private $d = ['k' => 1]; function &__debugInfo(): array { return $this->d; } }
    /// var_dump(new D);
    /// ```
    /// ```expect
    /// x
    /// x
    /// object(D)#1 (1) {
    ///   ["k"]=>
    ///   int(1)
    /// }
    /// ```
    pub(crate) fn unwrap_reference(&mut self, value: Value) -> Value {
        let Some(cell) = value.as_php_ref() else {
            return value;
        };
        let held = cell.get();
        self.addref(&held);
        self.release(value);
        held
    }

    /// `zend_hash_del_ind` (`Z/Zend/zend_hash.c:1572`), what `UNSET_VAR`
    /// deletes a name with: a bucket attached to a CV stays and the CV is
    /// left `UNDEF`, what it held released after it is gone; any other
    /// bucket is deleted as [`Request::symbol_del`] deletes it.
    pub(crate) fn symbol_unset(&mut self, table: TableId, name: &[u8]) {
        let found = self
            .table_mut(table)
            .and_then(|entries| entries.iter().find(|(key, _)| key == name))
            .map(|(_, symbol)| *symbol);
        match found {
            Some(Symbol::Indirect(frame, slot)) => {
                let value = self.take_attached(table, frame, slot);
                self.release(value);
            }
            Some(_) => self.symbol_del(table, name),
            None => {}
        }
    }

    /// What a bucket owns given up: its value, or the cell a fetch by
    /// name moved it into with the value in it; an attached bucket owns
    /// nothing.
    pub(super) fn release_symbol(&mut self, symbol: Symbol) {
        match symbol {
            Symbol::Value(value) => self.release(value),
            Symbol::Held(cell) => self.release(cell.as_value()),
            Symbol::Indirect(..) => {}
        }
    }

    /// The value a [`Symbol::Held`] cell holds taken out of it, the owner
    /// the cell had the caller's, and the cell given up.
    fn unhold(&mut self, cell: PhpRef) -> Value {
        let place = cell.heap().val_ref();
        let value = Value::of_zval(place.read());
        place.write(Value::undef().to_zval());
        self.release(cell.as_value());
        value
    }

    /// `EG(symbol_table)`, laid down where nothing has written it yet.
    pub(crate) fn global_table(&mut self) -> TableId {
        let _ = self.globals();
        TableId::GLOBAL
    }

    /// [`Request::symbol_value`] with nothing borrowed mutably: an
    /// `IS_INDIRECT` bucket followed to the CV on the stack the table's
    /// frames stand on, what `zend_hash_find_ex_ind` reads.
    pub(super) fn symbol_view(&self, table: TableId, name: &[u8]) -> Option<Value> {
        let symbol = self
            .tables
            .get(table.0 as usize)?
            .entries
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, symbol)| *symbol)?;
        Some(match symbol {
            Symbol::Value(value) => value,
            Symbol::Held(cell) => cell.get(),
            Symbol::Indirect(frame, at) => {
                let stack = match self.fibers.request_stack() {
                    Some(own) if table == TableId::GLOBAL => own,
                    _ => &self.stack,
                };
                Value::of_zval(stack.frame(frame)?.slot(php_heap::Slot(at)))
            }
        })
    }

    /// `zend_hash_find` in a symbol table for a fetch by name that
    /// reads: what the bucket `name` holds, an `IS_INDIRECT` bucket
    /// followed to the CV of the frame attached to it; `None` where the
    /// table has no such name. A view: no owner taken.
    pub(crate) fn symbol_value(&mut self, table: TableId, name: &[u8]) -> Option<Value> {
        let symbol = self
            .table_mut(table)?
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, symbol)| *symbol)?;
        Some(match symbol {
            Symbol::Value(value) => value,
            Symbol::Held(cell) => cell.get(),
            Symbol::Indirect(frame, at) => self.attached(table, frame, at),
        })
    }

    /// php's `IS_INDIRECT` bucket of `table`, the CV slot an attached name
    /// stands for, on the stack the table's frames stand on: the
    /// request's own for `EG(symbol_table)`, below every fiber running,
    /// and the running stack for a table rebuilt for a function frame.
    /// php's bucket points at the slot itself, which no stack switch
    /// moves, so a `global` inside a fiber binds the main frame's CV.
    ///
    /// ```php
    /// $f = new Fiber(function () { global $x; $x = 1; $GLOBALS["y"] = 2; });
    /// $f->start();
    /// var_dump($x, $y);
    /// ```
    /// ```expect
    /// int(1)
    /// int(2)
    /// ```
    fn attached_ptr(&mut self, table: TableId, frame: FrameRef, at: u32) -> Option<ZvalRef> {
        let stack = match self.fibers.request_stack_mut() {
            Some(own) if table == TableId::GLOBAL => own,
            _ => &mut self.stack,
        };
        stack.frame_mut(frame)?.slot_ptr(Slot(at))
    }

    /// What an attached name's CV holds, a view ([`Request::attached_ptr`]).
    pub(super) fn attached(&mut self, table: TableId, frame: FrameRef, at: u32) -> Value {
        self.attached_ptr(table, frame, at)
            .map_or(Value::undef(), |place| Value::of_zval(place.read()))
    }

    /// What an attached name's CV holds, taken, the CV left `UNDEF`.
    fn take_attached(&mut self, table: TableId, frame: FrameRef, at: u32) -> Value {
        let Some(place) = self.attached_ptr(table, frame, at) else {
            return Value::undef();
        };
        let held = Value::of_zval(place.read());
        place.write(Zval::UNDEF);
        held
    }

    /// `value` into an attached name's CV, what it held handed back.
    pub(crate) fn set_attached(
        &mut self,
        table: TableId,
        frame: FrameRef,
        at: u32,
        value: Value,
    ) -> Value {
        let Some(place) = self.attached_ptr(table, frame, at) else {
            return value;
        };
        let held = Value::of_zval(place.read());
        place.write(value.to_zval());
        held
    }

    /// `zend_hash_find` in a symbol table for a fetch by name that
    /// writes, php's `zval *` into the table that the `VAR` holds as an
    /// `IS_INDIRECT`: an attached name is its CV's slot, and a bucket
    /// that holds its value in the table is moved into a place of its
    /// own first ([`Symbol::Held`]), where php's bucket already is one;
    /// with `add`, a name the table has not is laid down holding `null`
    /// (`zend_hash_add_new`). `None` where the name is not there and
    /// `add` is not asked.
    pub(crate) fn symbol_place(
        &mut self,
        table: TableId,
        name: &[u8],
        add: bool,
    ) -> Result<Option<php_heap::ZvalRef>, Thrown> {
        let Some(entries) = self.table_mut(table) else {
            return Ok(None);
        };
        let (at, symbol) = match entries.iter().position(|(key, _)| key == name) {
            Some(at) => (at, entries[at].1),
            None if add => {
                entries.push((name.to_vec(), Symbol::Value(Value::null())));
                let at = entries.len() - 1;
                self.table_bytes(table, true, false);
                (at, Symbol::Value(Value::null()))
            }
            None => return Ok(None),
        };
        let cell = match symbol {
            Symbol::Indirect(frame, slot) => return Ok(self.attached_ptr(table, frame, slot)),
            Symbol::Held(cell) => cell,
            Symbol::Value(value) => {
                let cell = self.reference(value)?;
                if let Some(entries) = self.table_mut(table) {
                    entries[at].1 = Symbol::Held(cell);
                }
                cell
            }
        };
        Ok(Some(cell.heap().val_ref()))
    }

    /// `EG(symbol_table)`'s buckets, the first table's.
    pub(super) fn globals(&mut self) -> &mut Vec<(Vec<u8>, Symbol)> {
        if self.tables.is_empty() {
            self.tables.push(SymbolTable::default());
        }
        &mut self.tables[0].entries
    }

    /// `zend_hash_graceful_reverse_destroy(&EG(symbol_table))` in
    /// `zend_shutdown_executor_values`
    /// (`Z/Zend/zend_execute_API.c:271`), after the open resources are
    /// closed and before the object storage is freed: every symbol left
    /// released from the last to the first. The destructors are behind
    /// the request by now, `zend_objects_store_call_destructors` having
    /// marked every object, so this frees and serves nothing.
    pub(crate) fn destroy_symbol_table(&mut self) {
        while let Some((_, symbol)) = self.globals().pop() {
            self.release_symbol(symbol);
        }
    }

    /// `zend_proptable_to_symtable(&EG(symbol_table), 1)`
    /// (`Z/Zend/zend_hash.c:3396`), what `FETCH_GLOBALS` hands the
    /// program as `$GLOBALS`: a copy of the global table, every name
    /// that has a value in the table's order, a CV the name is attached
    /// to read where it stands and skipped where it holds nothing, a
    /// name that reads as an integer that integer (`ZEND_HANDLE_NUMERIC_STR`),
    /// and a reference nothing else holds its value, as
    /// `zend_array_dup` unwraps one; each value one more owner.
    pub(crate) fn symbol_table_array(&mut self) -> Result<PhpArray, Thrown> {
        let entries: Vec<(Vec<u8>, Symbol)> = self.globals().clone();
        let mut array = PhpArray::new(self)?;
        for (name, symbol) in entries {
            let value = match symbol {
                Symbol::Value(value) => value,
                Symbol::Held(cell) => cell.get(),
                Symbol::Indirect(frame, slot) => self.attached(TableId::GLOBAL, frame, slot),
            };
            if value.tag() == TypeTag::Undef {
                continue;
            }
            let value = match value.as_php_ref() {
                Some(cell) if value.refcount() == 1 => cell.get(),
                _ => value,
            };
            self.addref(&value);
            let key = Key::from_string(PhpString::new(self, &name)?);
            array.set(self, key, value)?;
        }
        Ok(array)
    }

    /// `zend_rebuild_symbol_table` for `frame` and `zend_array_dup` of
    /// it: every name the frame's table holds that has a value, in the
    /// table's order, each value one more owner, a reference kept as
    /// one; what `get_defined_vars()` answers.
    pub(crate) fn defined_vars(&mut self, frame: FrameRef) -> Result<PhpArray, Thrown> {
        let table = self.rebuild_symbol_table(frame);
        let entries: Vec<(Vec<u8>, Symbol)> = self
            .tables
            .get(table.0 as usize)
            .map(|table| table.entries.clone())
            .unwrap_or_default();
        // `zend_array_dup` lays the copy down at the table's own size,
        // which a rebuilt table taken from `EG(symtable_cache)` may have
        // grown past eight.
        let size = self
            .tables
            .get(table.0 as usize)
            .and_then(SymbolTable::size);
        let mut array = match size {
            Some(size) if size > 8 => {
                PhpArray::sized(self, size, crate::api::values::Layout::Hash)?
            }
            _ => PhpArray::new(self)?,
        };
        for (name, symbol) in entries {
            let value = match symbol {
                Symbol::Value(value) => value,
                Symbol::Held(cell) => cell.get(),
                Symbol::Indirect(frame, slot) => self.attached(table, frame, slot),
            };
            if value.tag() == TypeTag::Undef {
                continue;
            }
            self.addref(&value);
            let key = Key::from_string(PhpString::new(self, &name)?);
            array.set(self, key, value)?;
        }
        Ok(array)
    }
}
