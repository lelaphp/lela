//! The symbol tables of a request: `EG(symbol_table)` and the table
//! `zend_rebuild_symbol_table` gives a function frame that included a
//! file, a frame's compiled variables attached to a table's names and
//! detached as it leaves, the writes, unsets and lookups `$GLOBALS`,
//! `compact`, `extract` and `get_defined_vars` make through them, and
//! their release. The tables and the cache of rebuilt tables' bytes stay
//! on the request, since `hash_bytes` keeps the bytes beside them and
//! the native face reads a frame's through them.

use php_heap::{FrameRef, Slot, Zval, ZvalRef};

use crate::api::raising::Thrown;
use crate::api::values::{Key, Layout, Owner, PhpArray, PhpRef, PhpString, TypeTag, Value};
use crate::exec::{self, TableId};

use super::{Request, Symbol, SymbolTable};

impl Request {
    /// What each row of a frame's symbol table holds, in order, for the
    /// collector, as php's walk reads the table `get_gc` returns: an
    /// `IS_INDIRECT` row followed to the CV it stands for, which `cv`
    /// reads off the frames of the execution that holds the table.
    pub(crate) fn gc_symbol_rows(
        &self,
        table: TableId,
        cv: &dyn Fn(FrameRef, u32) -> Zval,
        visit: &mut dyn FnMut(Value),
    ) {
        let Some(table) = self.tables.get(table.0 as usize) else {
            return;
        };
        for (_, symbol) in &table.entries {
            visit(match *symbol {
                Symbol::Value(value) => value,
                Symbol::Held(cell) => cell.as_value(),
                Symbol::Indirect(frame, at) => Value::of_zval(cv(frame, at)),
            });
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
        let Some(table) = self.annex_of(frame).and_then(|extra| extra.symbol_table) else {
            return;
        };
        let Some((unit, body)) = self.frame_unit(frame) else {
            return;
        };
        let Some(code) = exec::body_of(&unit, body) else {
            return;
        };
        for (at, name) in code.vars.iter().enumerate() {
            let at = u32::try_from(at).unwrap_or(u32::MAX);
            let found = self.table_mut(table).and_then(|entries| {
                let index = position(entries, &name.0)?;
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
                    // The string the unit's compile made of the name
                    // ([`Request::frame_var_names`]), the table's owner
                    // of it taken; a refusal has ended the request.
                    let Ok(name) = self.declared_string(&name.0) else {
                        return;
                    };
                    self.table_push(table, *name, Symbol::Indirect(frame, at));
                    name.give_up(self);
                    continue;
                }
            };
            // The CV was `UNDEF` when the frame was pushed, or is its
            // own value taken back, so the slot gives up nothing.
            self.replace_slot(frame, at, value);
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
        let Some(table) = self.annex_of(frame).and_then(|extra| extra.symbol_table) else {
            return;
        };
        let Some((unit, body)) = self.frame_unit(frame) else {
            return;
        };
        let Some(code) = exec::body_of(&unit, body) else {
            return;
        };
        for (at, name) in code.vars.iter().enumerate() {
            let value = self.take_slot(frame, u32::try_from(at).unwrap_or(u32::MAX));
            if value.tag() == TypeTag::Undef {
                self.symbol_del(table, &name.0);
                continue;
            }
            // The bucket still names the CV, as the attach left it: the
            // value takes its place with no name to look up again.
            if let Some(entries) = self.table_mut(table)
                && let Some(index) = position(entries, &name.0)
                && matches!(entries[index].1, Symbol::Indirect(..))
            {
                entries[index].1 = Symbol::Value(value);
                continue;
            }
            match self.declared_string(&name.0) {
                Ok(name) => {
                    self.symbol_update(table, *name, value);
                    name.give_up(self);
                }
                Err(_) => self.release(value),
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
        if let Some(table) = self.annex_of(frame).and_then(|extra| extra.symbol_table) {
            return table;
        }
        // The names are the strings the unit's compile made, an owner of
        // each the table's; a refusal has ended the request, and the
        // table it leaves holds nothing.
        let entries = self
            .frame_var_names(frame)
            .unwrap_or_default()
            .into_iter()
            .enumerate()
            .map(|(at, name)| {
                (
                    name.hand_on(),
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
        if let Some(extra) = self.annex_mut(frame) {
            extra.symbol_table = Some(table);
        }
        table
    }

    /// The names of the CVs of the op array `frame` serves, in the
    /// order the compiler numbered them, `op_array->vars`, one owner of
    /// each the caller's: the strings its unit's compile made of them
    /// ([`php_compile::intern_declared`]), found where it made them, in
    /// the engine's table for a published unit and in the unit for one
    /// of the request's own ([`Request::declared_string`]), so a symbol
    /// table keys a variable by its CV's name, as php's
    /// `zend_attach_symbol_table` and `zend_rebuild_symbol_table` key it
    /// by `op_array->vars[i]`. The `Err` is the memory limit, met only
    /// where nothing holds a name.
    pub(super) fn frame_var_names(
        &mut self,
        frame: FrameRef,
    ) -> Result<Vec<Owner<PhpString>>, Thrown> {
        let Some((unit, body)) = self.frame_unit(frame) else {
            return Ok(Vec::new());
        };
        let Some(code) = exec::body_of(&unit, body) else {
            return Ok(Vec::new());
        };
        code.vars
            .iter()
            .map(|name| self.declared_string(&name.0))
            .collect()
    }

    /// The number of the CV named `name` in the op array `frame` serves,
    /// `lookup_cv` over `op_array->vars`; `None` where it has none.
    pub(super) fn frame_var_at(&self, frame: FrameRef, name: &[u8]) -> Option<u32> {
        let (unit, body) = self.frame_unit(frame)?;
        let code = exec::body_of(&unit, body)?;
        let at = code.vars.iter().position(|var| &*var.0 == name)?;
        u32::try_from(at).ok()
    }

    /// The unit and the body of the op array `frame` serves, one more
    /// share of the unit so the frame's names are read while the
    /// request is written.
    fn frame_unit(&self, frame: FrameRef) -> Option<(php_unit::PublishedUnit, exec::Body)> {
        self.frame_at(frame)
            .and_then(exec::FrameAt::unit)
            .map(|(unit, body)| (php_unit::PublishedUnit::clone(unit), body))
    }

    pub(super) fn table_mut(&mut self, table: TableId) -> Option<&mut Vec<(PhpString, Symbol)>> {
        self.tables
            .get_mut(table.0 as usize)
            .map(|table| &mut table.entries)
    }

    /// A name added to `table` holding `symbol`, `zend_hash_add_new`:
    /// the table takes an owner of the name, as php's bucket takes one
    /// of a key that is not interned, and its bytes grow as php's do.
    pub(super) fn table_push(&mut self, table: TableId, name: PhpString, symbol: Symbol) {
        let Some(entries) = self.table_mut(table) else {
            return;
        };
        entries.push((name, symbol));
        self.addref(&Value::string(name));
        self.table_bytes(table, true, false);
    }

    /// The bucket at `at` taken out of `table`, `_zend_hash_del_el`:
    /// its name given up, what it holds handed back for the caller to
    /// release.
    pub(super) fn table_remove(&mut self, table: TableId, at: usize) -> Option<Symbol> {
        let entries = self.table_mut(table)?;
        if at >= entries.len() {
            return None;
        }
        let (name, symbol) = entries.remove(at);
        let last = at == entries.len();
        self.table_bytes(table, false, last);
        self.release(Value::string(name));
        Some(symbol)
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
        let Some(table) = self.annex_of(frame).and_then(|extra| extra.symbol_table) else {
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
        // `zend_hash_destroy`'s order: a bucket's value, then its key.
        for (name, symbol) in std::mem::take(&mut table.entries) {
            self.release_symbol(symbol);
            self.release(Value::string(name));
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
    pub(crate) fn global_cell(&mut self, name: PhpString) -> Result<PhpRef, Thrown> {
        let at = if let Some(at) = position(self.globals(), name.as_bytes()) {
            at
        } else {
            self.table_push(TableId::GLOBAL, name, Symbol::Value(Value::null()));
            self.globals().len() - 1
        };
        let cell = match self.globals()[at].1 {
            Symbol::Held(held) => self.make_ref(held.heap().val_ref())?,
            Symbol::Indirect(frame, slot) => {
                let Some(place) = self.attached_ptr(TableId::GLOBAL, frame, slot) else {
                    return Err(crate::api::raising::fatal(
                        self,
                        "InternalError: a global bound to a CV the frame does not have",
                    ));
                };
                // The CV's owner of what it held is the cell's now.
                self.make_ref(place)?
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

    /// One symbol written, `zend_hash_update`: `value` is the table's,
    /// the name keeping the position it had, as php's update keeps a
    /// bucket's and only a name the table has not appends. What stood
    /// there is released where it stands and the write comes after
    /// (`pDestructor` then `ZVAL_COPY_VALUE`, `Z/Zend/zend_hash.c:871`),
    /// [`Overwrite::InPlace`]. A bucket that pointed at a CV owned
    /// nothing, and is replaced by the value, as php's update replaces
    /// the `IS_INDIRECT`. `name` is the caller's, and a name the table
    /// adds it takes an owner of.
    pub(super) fn symbol_update(&mut self, table: TableId, name: PhpString, value: Value) {
        let Some(entries) = self.table_mut(table) else {
            self.release(value);
            return;
        };
        let Some(at) = position(entries, name.as_bytes()) else {
            self.table_push(table, name, Symbol::Value(value));
            return;
        };
        let held = match entries[at].1 {
            Symbol::Value(held) => held,
            // The bucket's own place, which a fetch by name holds: the
            // write goes there, as php's goes into the bucket.
            Symbol::Held(_) => {
                let place = |req: &mut Request| req.held_place(table, name.as_bytes());
                self.overwrite_place(place, value, Overwrite::InPlace);
                return;
            }
            Symbol::Indirect(..) => {
                entries[at].1 = Symbol::Value(value);
                return;
            }
        };
        // A destructor the release serves can delete the name, the
        // table's owner of it with it, before the settle reads it.
        self.addref(&Value::string(name));
        let released = self.release_in_place(held);
        self.symbol_settle(table, name, value, Some(released));
        self.release(Value::string(name));
    }

    /// The place a [`Symbol::Held`] bucket keeps its value in, found by
    /// name; none where the name is gone or holds its value otherwise.
    fn held_place(&mut self, table: TableId, name: &[u8]) -> Option<ZvalRef> {
        let entries = self.table_mut(table)?;
        match entries.get(position(entries, name)?)?.1 {
            Symbol::Held(cell) => Some(cell.heap().val_ref()),
            Symbol::Value(_) | Symbol::Indirect(..) => None,
        }
    }

    /// The write after a release, which can have served a destructor
    /// that wrote or deleted the name: `value` written where the name
    /// stands now, and what the program left there released; a name it
    /// deleted is added again. After a release in place, `released`,
    /// what that release left in the bucket is no write of the
    /// program's and is not released again; after a release that nulled
    /// the bucket first ([`Overwrite::Nulled`]), anything there is the
    /// program's.
    fn symbol_settle(
        &mut self,
        table: TableId,
        name: PhpString,
        value: Value,
        released: Option<Released>,
    ) {
        let entries = self.table_mut(table);
        let at = entries
            .as_ref()
            .and_then(|entries| position(entries, name.as_bytes()));
        let old = match (entries, at) {
            (None, _) => {
                if let Some(released) = released {
                    released.let_go(self);
                }
                self.release(value);
                return;
            }
            (Some(_), None) => {
                self.table_push(table, name, Symbol::Value(value));
                None
            }
            (Some(entries), Some(at)) => {
                Some(std::mem::replace(&mut entries[at].1, Symbol::Value(value)))
            }
        };
        let by_program = match (old, released) {
            (Some(Symbol::Value(left)), Some(released)) => released
                .wrote_over(self, left)
                .then_some(Symbol::Value(left)),
            (old, released) => {
                if let Some(released) = released {
                    released.let_go(self);
                }
                old
            }
        };
        if let Some(old) = by_program {
            self.release_symbol(old);
        }
    }

    /// `held`, what a write finds where it writes, released where it
    /// stands, before the write ([`Overwrite::InPlace`]), so a destructor
    /// the release serves reads the value it is serving there. An object
    /// is pinned across the release and the settle after it
    /// ([`php_heap::Obj::pin`]): where the release serves code that
    /// releases it again through the variable and so frees it, its
    /// bytes stand until [`Released::wrote_over`] lets them go, so
    /// nothing the program makes meanwhile is laid where it
    /// stood, and the settle compares against no block the heap has
    /// freed. A string, a table or a cell is freed by its release only
    /// after every destructor that release serves.
    fn release_in_place(&mut self, held: Value) -> Released {
        if let Some(object) = held.as_object() {
            object.heap().pin();
        }
        self.release(held);
        Released { held }
    }

    /// `value` written into the place `place` finds, what it held given
    /// up in `order`. The place is found again after a release that can
    /// have served program code, and what the program left there, other
    /// than what a release in place left ([`Released::wrote_over`]), is
    /// released after the write; where the place is gone, `value` is
    /// given up.
    pub(super) fn overwrite_place(
        &mut self,
        place: impl Fn(&mut Request) -> Option<ZvalRef>,
        value: Value,
        order: Overwrite,
    ) {
        let Some(at) = place(self) else {
            self.release(value);
            return;
        };
        let held = Value::of_zval(at.read());
        let released = match order {
            Overwrite::After => {
                at.write(value.to_zval());
                self.release(held);
                return;
            }
            Overwrite::InPlace => Some(self.release_in_place(held)),
            Overwrite::Nulled => {
                at.write(Value::null().to_zval());
                self.release(held);
                None
            }
        };
        let Some(at) = place(self) else {
            if let Some(released) = released {
                released.let_go(self);
            }
            self.release(value);
            return;
        };
        let left = Value::of_zval(at.read());
        at.write(value.to_zval());
        let by_program = match released {
            Some(released) => released.wrote_over(self, left),
            None => true,
        };
        if by_program {
            self.release(left);
        }
    }

    /// One symbol deleted, `zend_hash_del`: the name gone and what it
    /// held released; a name the table has not is no error, as php's
    /// delete of a missing key is none.
    pub(super) fn symbol_del(&mut self, table: TableId, name: &[u8]) {
        let Some(at) = self
            .table_mut(table)
            .and_then(|entries| position(entries, name))
        else {
            return;
        };
        if let Some(symbol) = self.table_remove(table, at) {
            self.release_symbol(symbol);
        }
    }

    /// `zend_unwrap_reference` (`Z/Zend/zend_operators.h:1009`): a
    /// reference a call handed back, which a method declared
    /// `&__toString` or `&__debugInfo` does, given up for the value it
    /// holds, one owner of that the caller's; any other value as it
    /// stands.
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
        // `ZVAL_UNREF` where the cell had this one owner, and `Z_DELREF`
        // with the value copied out otherwise: neither makes anything a
        // possible root of the collector.
        let held = cell.get();
        self.addref(&held);
        self.release_nogc(value);
        held
    }

    /// `zend_hash_del_ind` (`Z/Zend/zend_hash.c:1572`), what `UNSET_VAR`
    /// deletes a name with: a bucket attached to a CV stays and the CV is
    /// left `UNDEF`, what it held released after it is gone; any other
    /// bucket is deleted as [`Request::symbol_del`] deletes it.
    pub(crate) fn symbol_unset(&mut self, table: TableId, name: &[u8]) {
        let found = self
            .table_mut(table)
            .and_then(|entries| Some(entries.get(position(entries, name)?)?.1));
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
        let entries = &self.tables.get(table.0 as usize)?.entries;
        let symbol = entries.get(position(entries, name)?)?.1;
        Some(match symbol {
            Symbol::Value(value) => value,
            Symbol::Held(cell) => cell.get(),
            Symbol::Indirect(frame, at) => {
                let stack = match self.fibers.request_stack() {
                    Some(own) if table == TableId::GLOBAL => own,
                    _ => &self.vm.stack,
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
        let entries = self.table_mut(table)?;
        let symbol = entries.get(position(entries, name)?)?.1;
        Some(self.symbol_read(table, symbol))
    }

    /// What one bucket of `table` holds, an `IS_INDIRECT` bucket followed
    /// to the CV of the frame attached to it. A view: no owner taken.
    pub(super) fn symbol_read(&mut self, table: TableId, symbol: Symbol) -> Value {
        match symbol {
            Symbol::Value(value) => value,
            Symbol::Held(cell) => cell.get(),
            Symbol::Indirect(frame, at) => self.attached(table, frame, at),
        }
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
            _ => &mut self.vm.stack,
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

    /// One symbol written, `zend_hash_update_ind`: into the CV a frame
    /// attached to the name where one is, and as
    /// [`Request::symbol_update`] writes it otherwise; `value` is the
    /// table's. What stood there is given up in `order`: php's
    /// `zend_hash_update_ind` releases in place, [`Overwrite::InPlace`],
    /// and a caller that writes the variable as php's natives do through
    /// the `zval *` they found names its own.
    pub(super) fn symbol_update_ind(
        &mut self,
        table: TableId,
        name: PhpString,
        value: Value,
        order: Overwrite,
    ) {
        let bytes = name.as_bytes();
        let found = self
            .table_mut(table)
            .and_then(|entries| Some(entries.get(position(entries, bytes)?)?.1));
        match found {
            Some(Symbol::Indirect(frame, at)) => {
                self.overwrite_place(|req| req.attached_ptr(table, frame, at), value, order);
            }
            Some(Symbol::Held(_)) => {
                self.overwrite_place(|req| req.held_place(table, bytes), value, order);
            }
            Some(Symbol::Value(held)) if order != Overwrite::InPlace => {
                let Some(entry) = self.table_mut(table).and_then(|entries| {
                    entries.iter_mut().find(|(key, _)| key.as_bytes() == bytes)
                }) else {
                    self.release(value);
                    return;
                };
                if order == Overwrite::After {
                    entry.1 = Symbol::Value(value);
                    self.release(held);
                    return;
                }
                entry.1 = Symbol::Value(Value::null());
                // The release can serve a destructor that deletes the
                // name, as [`Request::symbol_update`]'s can.
                self.addref(&Value::string(name));
                self.release(held);
                self.symbol_settle(table, name, value, None);
                self.release(Value::string(name));
            }
            _ => self.symbol_update(table, name, value),
        }
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
        name: PhpString,
        add: bool,
    ) -> Result<Option<php_heap::ZvalRef>, Thrown> {
        let Some(entries) = self.table_mut(table) else {
            return Ok(None);
        };
        let (at, symbol) = match position(entries, name.as_bytes()) {
            Some(at) => (at, entries[at].1),
            None if add => {
                let at = entries.len();
                self.table_push(table, name, Symbol::Value(Value::null()));
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
    pub(super) fn globals(&mut self) -> &mut Vec<(PhpString, Symbol)> {
        if self.tables.is_empty() {
            self.tables.push(SymbolTable::global());
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
        // php's fast shutdown frees none of it, so no chunk it empties
        // leaves the heap php's child keeps (#2940).
        self.heap.fast_shutdown();
        while let Some((name, symbol)) = self.globals().pop() {
            self.release_symbol(symbol);
            self.release(Value::string(name));
        }
        if let Some(bytes) = self.tables.first_mut().and_then(|table| table.bytes.take()) {
            bytes.free(self);
        }
    }

    /// `zend_proptable_to_symtable(&EG(symbol_table), 1)`
    /// (`Z/Zend/zend_hash.c:3396`), what `FETCH_GLOBALS` hands the
    /// program as `$GLOBALS`: a copy of the global table, every name
    /// that has a value in the table's order, each held as
    /// [`Request::dup_value`] holds an element, and a name that reads as
    /// an integer that integer (`ZEND_HANDLE_NUMERIC_STR`).
    ///
    /// ```php
    /// $a = [3, 1, 2]; sort($a);
    /// $kept = [1]; $alias = &$kept;
    /// $all = $GLOBALS; $all['a'][] = 9; $all['kept'][] = 9;
    /// ${'7'} = 1;
    /// var_dump(count($a), count($kept), array_key_last($GLOBALS));
    /// ```
    /// ```expect
    /// int(3)
    /// int(2)
    /// int(7)
    /// ```
    ///
    /// The copy is `zend_array_dup` at the global table's own size,
    /// sixty-four as `init_executor` lays it down, where no name reads as
    /// an integer, and `zend_new_array` over every bucket the table
    /// holds, an attached name whose variable holds nothing among them,
    /// where one does, a variable written `${'7'}` among them, which is
    /// the CV `7` from the compile on; each name is the table's string
    /// with one more owner, a variable's the interned name of its CV:
    ///
    /// ```php
    /// $m = memory_get_usage(); $g = $GLOBALS; echo memory_get_usage() - $m, "\n";
    /// $n = '7'; $$n = 1;
    /// $m = memory_get_usage(); $h = $GLOBALS; echo memory_get_usage() - $m, "\n";
    /// ```
    /// ```expect
    /// 2616
    /// 696
    /// ```
    pub(crate) fn symbol_table_array(&mut self) -> Result<PhpArray, Thrown> {
        let copied = self.symbol_rows(TableId::GLOBAL);
        let size = if copied.numeric {
            u32::try_from(copied.buckets).unwrap_or(u32::MAX)
        } else {
            self.table_size(TableId::GLOBAL)
        };
        let mut array = self.table_copy(copied.buckets, size)?;
        for (name, value) in copied.rows {
            let key = Key::from_string(self, Owner::take(self, name));
            let value = self.dup_value(value, None);
            let value = Owner::adopt(value);
            array.set(self, key, value)?;
        }
        Ok(array)
    }

    /// `zend_rebuild_symbol_table` for `frame` and `zend_array_dup` of
    /// it (`Z/Zend/zend_builtin_functions.c:1517`): every name the
    /// frame's table holds that has a value, in the table's order, each
    /// held as [`Request::dup_value`] holds an element and each name the
    /// string it is; what `get_defined_vars()` answers.
    ///
    /// The copy is laid down at the table's own size, a function's
    /// rebuilt table's or the global table's sixty-four, and each name
    /// is the table's string with one more owner, a variable's the
    /// interned name of its CV, so it costs the request nothing:
    ///
    /// ```php
    /// function f() { $aa = 1; $bb = 2; $cc = 3; $m = memory_get_usage(); $v = get_defined_vars(); echo memory_get_usage() - $m, "\n"; }
    /// f();
    /// $m = memory_get_usage(); $v = get_defined_vars(); echo memory_get_usage() - $m, "\n";
    /// ```
    /// ```expect
    /// 752
    /// 2616
    /// ```
    pub(crate) fn defined_vars(&mut self, frame: FrameRef) -> Result<PhpArray, Thrown> {
        let table = self.rebuild_symbol_table(frame);
        let copied = self.symbol_rows(table);
        let size = self.table_size(table);
        let mut array = self.table_copy(copied.buckets, size)?;
        for (name, value) in copied.rows {
            let key = Owner::take(self, name).into_key();
            let value = self.dup_value(value, None);
            let value = Owner::adopt(value);
            array.set(self, key, value)?;
        }
        Ok(array)
    }

    /// `nTableSize` of `table`, eight where nothing sizes it.
    fn table_size(&self, table: TableId) -> u32 {
        self.tables
            .get(table.0 as usize)
            .and_then(SymbolTable::size)
            .unwrap_or(8)
    }

    /// The array a copy of a symbol table of `buckets` lays down, `size`
    /// buckets wide: `zend_array_dup`'s uninitialised table where the
    /// source holds none, else a hash of that size, as `zend_array_dup`
    /// lays it down and `zend_new_array` does at its first write.
    fn table_copy(&mut self, buckets: usize, size: u32) -> Result<PhpArray, Thrown> {
        if buckets == 0 {
            return self.array();
        }
        Ok(PhpArray::sized(self, size, Layout::Hash)?.hand_on())
    }

    /// What a copy of `table` reads, the half of `zend_array_dup_value`
    /// that is the table's: each name in the table's order with what it
    /// holds, an `IS_INDIRECT` bucket followed to its CV and one whose
    /// CV holds nothing left out. Views; no owner taken.
    fn symbol_rows(&mut self, table: TableId) -> Copied {
        let entries: Vec<(PhpString, Symbol)> = self
            .tables
            .get(table.0 as usize)
            .map(|table| table.entries.clone())
            .unwrap_or_default();
        let mut copied = Copied {
            rows: Vec::with_capacity(entries.len()),
            buckets: entries.len(),
            numeric: false,
        };
        for (name, symbol) in entries {
            copied.numeric |= php_heap::canonical_int(name.as_bytes()).is_some();
            let value = match symbol {
                Symbol::Value(value) => value,
                Symbol::Held(cell) => cell.get(),
                Symbol::Indirect(frame, slot) => self.attached(table, frame, slot),
            };
            if value.tag() != TypeTag::Undef {
                copied.rows.push((name, value));
            }
        }
        copied
    }
}

/// What a copy of a symbol table reads ([`Request::symbol_rows`]): the
/// rows that hold a value, the buckets the table holds,
/// `zend_hash_num_elements`, and whether a name reads as an integer,
/// which `zend_proptable_to_symtable` converts the copy for.
struct Copied {
    rows: Vec<(PhpString, Value)>,
    buckets: usize,
    numeric: bool,
}

/// The place in `entries` of the bucket named `name`.
fn position(entries: &[(PhpString, Symbol)], name: &[u8]) -> Option<usize> {
    entries.iter().position(|(key, _)| key.as_bytes() == name)
}

/// The order a write gives up what the variable it writes held, each
/// one php's: what a destructor that the release serves reads under the
/// name.
///
/// ```php
/// class D { function __construct(public $n) {} function __destruct() { echo "dtor {$this->n} sees: "; var_dump($GLOBALS[$this->n]); } }
/// $x = new D('x'); extract(['x' => 5]);
/// $z = new D('z'); $src = ['z' => 7]; extract($src, EXTR_REFS);
/// ```
/// ```expect
/// dtor x sees: NULL
/// dtor z sees: int(7)
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Overwrite {
    /// `zend_hash_update`'s `pDestructor` and `zend_set_local_var`'s
    /// `zval_ptr_dtor`: released where it stands, so the destructor reads
    /// the value it is serving, and the write after.
    InPlace,
    /// `zval_ptr_safe_dtor`, which `ZEND_TRY_ASSIGN_COPY_EX` calls
    /// (`Z/Zend/zend_API.h:1457`): the variable `null` while the old value
    /// is released, and the write after.
    Nulled,
    /// `php_extract_ref_overwrite`'s `garbage`: the write, and the old
    /// value released after it.
    After,
}

/// What a release in place ([`Request::release_in_place`]) left where
/// it released: the value, one object of it pinned, until the settle
/// after the write lets it go ([`Released::wrote_over`] or
/// [`Released::let_go`]).
#[must_use = "the pin a release in place set is let go by wrote_over or let_go"]
struct Released {
    held: Value,
}

impl Released {
    /// Whether `left`, what the place holds once the release in place
    /// is over, is a write of the program's: anything but what the
    /// release left there. The pin [`Request::release_in_place`] set is
    /// let go first, giving back the bytes of an object the release
    /// freed.
    fn wrote_over(self, req: &mut Request, left: Value) -> bool {
        let wrote = left.to_zval() != self.held.to_zval();
        self.let_go(req);
        wrote
    }

    /// The pin the release in place set let go, where it set one.
    fn let_go(self, req: &mut Request) {
        if let Some(object) = self.held.as_object() {
            req.heap.unpin(object.heap());
        }
    }
}
