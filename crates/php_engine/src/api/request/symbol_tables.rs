//! The symbol tables of a request (design §5.6): `EG(symbol_table)` and
//! the table `zend_rebuild_symbol_table` gives a function frame that
//! included a file, each a `php_heap` array laid down and grown as
//! `zend_hash` lays down and grows php's, a frame's compiled variables
//! attached to a table's names as `IS_INDIRECT` buckets and detached as
//! it leaves, the writes, unsets and lookups `$GLOBALS`, `compact`,
//! `extract` and `get_defined_vars` make through them,
//! `EG(symtable_cache)`, and their release.

use php_heap::{Arr, FrameRef, Heap, Slot, Zval, ZvalRef};

use crate::api::raising::Thrown;
use crate::api::values::{Key, Layout, Owner, PhpArray, PhpRef, PhpString, TypeTag, Value};
use crate::exec::{self, TableId};

use super::{Hooks, Request, Rows, SymbolTable};

/// `SYMTABLE_CACHE_SIZE` (`Z/Zend/zend_globals.h:61`).
const SYMTABLE_CACHE_SIZE: usize = 32;

impl SymbolTable {
    /// `EG(symbol_table)` as `init_executor` lays it down,
    /// `zend_hash_init(&EG(symbol_table), 64, ...)`: the header `EG`'s
    /// own, its data laid down at the first name written,
    /// sixty-four buckets wide, and grown as php's grows.
    pub(super) fn global() -> SymbolTable {
        SymbolTable {
            owner: None,
            rows: Rows::Embedded(php_heap::EmbeddedArr::new(64)),
            changes: 0,
        }
    }

    fn arr(&self) -> Option<Arr> {
        match &self.rows {
            Rows::Embedded(table) => Some(table.arr()),
            Rows::Own(arr) => Some(*arr),
            Rows::Gone => None,
        }
    }
}

/// What a bucket holds, an `IS_INDIRECT` one followed to the CV it
/// names (`ZVAL_DEINDIRECT`).
fn deindirect(z: Zval) -> Zval {
    z.as_indirect().map_or(z, ZvalRef::read)
}

/// What stands at `at` taken, the place left `UNDEF`.
fn take(at: ZvalRef) -> Zval {
    let z = at.read();
    at.write(Zval::UNDEF);
    z
}

impl Request {
    /// What each row of a frame's symbol table holds, in order, for the
    /// collector, as php's walk reads the table `get_gc` returns: an
    /// `IS_INDIRECT` row followed to the CV it stands for.
    pub(crate) fn gc_symbol_rows(&self, table: TableId, visit: &mut dyn FnMut(Value)) {
        for (_, z) in &self.symbol_arr(table).unwrap_or_else(Arr::empty) {
            visit(Value::of_zval(deindirect(z)));
        }
    }

    /// The array of `table`, none for one freed in its place.
    pub(super) fn symbol_arr(&self, table: TableId) -> Option<Arr> {
        self.tables.get(table.0 as usize)?.arr()
    }

    /// The array of `table`, its buckets reached to be written.
    fn table_mut(&mut self, table: TableId) -> Option<Arr> {
        let table = self.tables.get_mut(table.0 as usize)?;
        table.changes = table.changes.wrapping_add(1);
        table.arr()
    }

    /// The bucket of `table` named `name`, reached to be written.
    fn bucket(&mut self, table: TableId, name: &[u8]) -> Option<ZvalRef> {
        self.table_mut(table)?.str_value_ref(name)
    }

    /// CV `at` of `frame` in place, php's `EX_VAR_NUM(at)`, which an
    /// attached name's bucket points at: the page or the generator's
    /// allocation it stands on never moves while the frame lives.
    fn cv_ptr(&mut self, frame: FrameRef, at: usize) -> Option<ZvalRef> {
        let at = Slot(u32::try_from(at).ok()?);
        self.vm.stack.frame_mut(frame)?.slot_ptr(at)
    }

    /// `zend_attach_symbol_table` (`Z/Zend/zend_execute_API.c:1857`) on
    /// the table `frame` names: every CV of the frame's op array, in the
    /// order the compiler numbered them, takes what the table holds
    /// under its name, moved out of the bucket or out of the CV of the
    /// frame the bucket points at (`ZVAL_COPY_VALUE`, no owner taken),
    /// and the bucket is an `IS_INDIRECT` to this frame's CV from here; a
    /// name the table has not is added, its CV left `UNDEF`. What the
    /// main op array's activation does with `EG(symbol_table)`, what a
    /// code frame's does with its includer's, and what the includer does
    /// again when the code frame leaves, so a variable the included
    /// file wrote is the includer's.
    pub(crate) fn attach_symbol_table(&mut self, frame: FrameRef) {
        let Some((table, _)) = self.cv_table(frame) else {
            return;
        };
        let Some((unit, body)) = self.frame_unit(frame) else {
            return;
        };
        let Some(code) = exec::body_of(&unit, body) else {
            return;
        };
        for (at, name) in code.vars.iter().enumerate() {
            let Some(cv) = self.cv_ptr(frame, at) else {
                return;
            };
            let Some(bucket) = self.bucket(table, &name.0) else {
                // The string the unit's compile made of the name
                // ([`Request::frame_var_names`]), the table's owner of it
                // taken; a refusal has ended the request.
                let Ok(name) = self.declared_string(&name.0) else {
                    return;
                };
                self.table_add(table, *name, Zval::indirect(cv));
                name.give_up(self);
                continue;
            };
            let held = bucket.read();
            // The bucket names this CV already: the value is where it
            // stands.
            if held.as_indirect() == Some(cv) {
                continue;
            }
            bucket.write(Zval::indirect(cv));
            let value = held.as_indirect().map_or(held, take);
            // The CV was `UNDEF` when the frame was pushed, so the slot
            // gives up nothing.
            let at = u32::try_from(at).unwrap_or(u32::MAX);
            self.replace_slot(frame, at, Value::of_zval(value));
        }
        self.note_attached(frame, table);
    }

    /// `frame` attached to `table` with every CV's bucket naming the CV,
    /// noted against the count of the table's writes
    /// ([`exec::Annex::attached`]).
    fn note_attached(&mut self, frame: FrameRef, table: TableId) {
        let changes = self.tables.get(table.0 as usize).map(|table| table.changes);
        if let Some(id) = self.vm.at(frame).and_then(|header| header.annex)
            && let Some(annex) = self.annexes.get_mut(id)
        {
            annex.attached = changes;
        }
    }

    /// Whether `frame`'s CVs are attached to its table as it left them:
    /// nothing has reached the table to write since
    /// ([`exec::Annex::attached`]). So an `eval` that writes nothing to
    /// its includer's table leaves the includer's buckets unread, and one
    /// that writes to it has the includer attached again:
    ///
    /// ```php
    /// $a = 1; $b = 2;
    /// eval('unset($GLOBALS["a"]); $GLOBALS["c"] = 3;');
    /// var_dump(isset($a), $b, $c ?? null);
    /// $a = 4;
    /// eval('return 1;');
    /// var_dump($GLOBALS["a"], array_slice(array_keys($GLOBALS), -3));
    /// ```
    /// ```expect
    /// bool(false)
    /// int(2)
    /// int(3)
    /// int(4)
    /// array(3) {
    ///   [0]=>
    ///   string(1) "a"
    ///   [1]=>
    ///   string(1) "b"
    ///   [2]=>
    ///   string(1) "c"
    /// }
    /// ```
    pub(crate) fn still_attached(&self, frame: FrameRef) -> bool {
        let Some(annex) = (self.vm.at(frame)).and_then(|header| self.annexes.get(header.annex?))
        else {
            return false;
        };
        let changes = (annex.symbol_table)
            .and_then(|table| self.tables.get(table.0 as usize))
            .map(|table| table.changes);
        annex.attached.is_some() && annex.attached == changes
    }

    /// `zend_detach_symbol_table` (`Z/Zend/zend_execute_API.c:1892`),
    /// what php does when it leaves a frame that has a symbol table, the
    /// main op array's frame, the one it marks `ZEND_CALL_TOP_CODE` and
    /// gives `EG(symbol_table)` as its own, and every code frame an
    /// `INCLUDE_OR_EVAL` pushed (design §5.6): every CV that holds
    /// something is moved into the table under the name the compiler
    /// numbered it by (`zend_hash_update`), the bucket keeping its place,
    /// the slot left `UNDEF` so the pop releases nothing, and a name whose
    /// CV holds nothing is deleted from the table. The variables are the
    /// table's from here: the request's for the main frame, which is
    /// why `$a = new A;` is still an owner while the shutdown
    /// functions are served and the destructor waits for
    /// [`Request::shutdown_destructors`], and the includer's for a code
    /// frame, whose re-attach takes them back. A fatal in nested code
    /// leaves each frame's variables in the table for the shutdown
    /// functions, as php's leaves them:
    ///
    /// ```php
    /// register_shutdown_function(function () { $a = [1]; var_dump($GLOBALS['inner'], $GLOBALS['outer'], $GLOBALS['main']); });
    /// $main = 'm';
    /// eval('$outer = "o"; $main .= "2"; eval(\'$inner = "i"; $outer .= "!"; ini_set("memory_limit", "2M"); str_repeat("x", 4 << 20);\');');
    /// ```
    /// ```expect exit=255
    /// Fatal error: Allowed memory size of 2097152 bytes exhausted (tried to allocate 4194336 bytes) in %s on line 1
    /// string(1) "i"
    /// string(2) "o!"
    /// string(2) "m2"
    /// ```
    pub(crate) fn detach_symbol_table(&mut self, frame: FrameRef) {
        let Some((table, _)) = self.cv_table(frame) else {
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
            // value takes its place, as php's update replaces the
            // `IS_INDIRECT`, with no name to make.
            if let Some(bucket) = self.bucket(table, &name.0)
                && bucket.read().as_indirect().is_some()
            {
                bucket.write(value.to_zval());
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
        if self.bailout {
            self.forget_left(table, frame);
        }
    }

    /// Under a `zend_bailout`, the frames above `frame` that shared its
    /// table left the stack with no detach, as php's jump leaves them,
    /// so a bucket that pointed at one of their CVs points at nothing:
    /// deleted, as the detach deletes a name whose CV holds nothing,
    /// where it names no CV of a frame below that is attached to the
    /// table.
    fn forget_left(&mut self, table: TableId, frame: FrameRef) {
        let mut standing = Vec::new();
        for below in 0..frame.0 {
            let below = FrameRef(below);
            if let Some((attached, cvs)) = self.cv_table(below)
                && attached == table
            {
                standing.extend((0..cvs).filter_map(|at| self.cv_ptr(below, at)));
            }
        }
        let left: Vec<PhpString> = self
            .symbol_entries(table)
            .into_iter()
            .filter(|(_, z)| z.as_indirect().is_some_and(|cv| !standing.contains(&cv)))
            .map(|(name, _)| name)
            .collect();
        for name in left {
            self.symbol_del(table, name.as_bytes());
        }
    }

    /// Each CV of `frame` whose bucket in `table` still names it taken
    /// back into the bucket, `zend_detach_symbol_table`'s rule for those
    /// buckets alone: the value where the CV holds one, the name deleted
    /// where it holds none. Whether any bucket named one. What a frame
    /// that shares its includer's table leaves behind when it pops with
    /// no detach, its CVs released, and what the global table's names
    /// are freed of before a serving with nothing executing
    /// ([`Request::nothing_executing`]).
    pub(super) fn detach_named(&mut self, frame: FrameRef, table: TableId) -> bool {
        let Some((unit, body)) = self.frame_unit(frame) else {
            return false;
        };
        let Some(code) = exec::body_of(&unit, body) else {
            return false;
        };
        let mut named = false;
        for (at, name) in code.vars.iter().enumerate() {
            let Some(cv) = self.cv_ptr(frame, at) else {
                break;
            };
            let Some(bucket) = self.bucket(table, &name.0) else {
                continue;
            };
            if bucket.read().as_indirect() != Some(cv) {
                continue;
            }
            named = true;
            let value = take(cv);
            if value.is_undef() {
                self.symbol_del(table, &name.0);
            } else {
                bucket.write(value);
            }
        }
        named
    }

    /// The table `frame` is attached to, `EX(symbol_table)`, and how many
    /// CVs the op array it serves has, read with no share of its unit
    /// taken: none for a frame with no table, or whose op array has no
    /// CV, an `eval`'s of an expression among them, which attaching and
    /// detaching move nothing of.
    fn cv_table(&self, frame: FrameRef) -> Option<(TableId, usize)> {
        let header = self.vm.at(frame)?;
        let annex = self.annexes.get(header.annex?)?;
        let table = annex.symbol_table?;
        // A file's frame has its unit in the annex read already.
        let (unit, body) = match (header.function, &annex.unit) {
            (None, Some(unit)) => (unit, exec::Body::Main),
            _ => self.unit_of(header)?,
        };
        let cvs = exec::body_of(unit, body)?.vars.len();
        (cvs > 0).then_some((table, cvs))
    }

    /// `zend_rebuild_symbol_table` (`Z/Zend/zend_execute_API.c:1810`) for
    /// `frame`: the table it already has, or one of its own with every
    /// CV of its op array appended as an `IS_INDIRECT` to the CV
    /// (`_zend_hash_append_ind`), the values staying where they are, and
    /// the frame marked `ZEND_CALL_HAS_SYMBOL_TABLE`. What a function
    /// frame gets when it includes a file, so the included code reads
    /// and writes the function's variables; freed when the frame pops
    /// (`zend_clean_and_cache_symbol_table`).
    pub(crate) fn rebuild_symbol_table(&mut self, frame: FrameRef) -> TableId {
        if let Some(table) = self.annex_of(frame).and_then(|extra| extra.symbol_table) {
            return table;
        }
        // The names are the strings the unit's compile made, an owner of
        // each the table's; a refusal has ended the request, and the
        // table it leaves holds nothing.
        let names = self.frame_var_names(frame).unwrap_or_default();
        let rows = self.rebuilt_table(u32::try_from(names.len()).unwrap_or(u32::MAX));
        for (at, name) in names.into_iter().enumerate() {
            if let Some(arr) = rows
                && let Some(cv) = self.cv_ptr(frame, at)
            {
                let added = self.heap.array_add_new(
                    arr,
                    php_heap::Key::Str(name.heap()),
                    Zval::indirect(cv),
                );
                // The memory limit's fatal is recorded and ends the
                // request.
                self.fatal_of(added).ok();
            }
            name.give_up(self);
        }
        let table = TableId(u32::try_from(self.tables.len()).unwrap_or(u32::MAX));
        self.tables.push(SymbolTable {
            owner: Some(frame),
            rows: rows.map_or(Rows::Gone, Rows::Own),
            changes: 0,
        });
        if let Some(extra) = self.annex_mut(frame) {
            extra.symbol_table = Some(table);
        }
        table
    }

    /// `zend_rebuild_symbol_table`'s table for a function frame with
    /// `last_var` CVs, before they are appended: one `EG(symtable_cache)`
    /// kept, extended to them (`zend_hash_extend`), or a new one,
    /// `zend_new_array(last_var)` with its data laid down where there is
    /// a CV to append (`zend_hash_real_init_mixed`); none where the
    /// memory limit refused it, which has ended the request. A function
    /// that includes a file keeps the table in the cache when it
    /// returns, 56 bytes and the 320 of its data, and the next rebuild,
    /// here `get_defined_vars()`'s, takes it back:
    ///
    /// ```php
    /// $f = realpath(__DIR__) . '/rebuilt.inc'; file_put_contents($f, '<?php $x = 1;'); include $f;
    /// function g($f) { $a = 1; include $f; return $a; }
    /// function h() { $b = 1; return count(get_defined_vars()); }
    /// $m = memory_get_usage(); g($f); echo memory_get_usage() - $m, "\n";
    /// $m = memory_get_usage(); h(); echo memory_get_usage() - $m, "\n";
    /// unlink($f);
    /// ```
    /// ```expect
    /// 376
    /// 0
    /// ```
    ///
    /// Forty-one frames that each rebuild one before any returns keep
    /// the thirty-two the cache has room for, and free the rest:
    ///
    /// ```php
    /// function s($n) { $y = 2; get_defined_vars(); if ($n > 0) s($n - 1); return $n; }
    /// $m = memory_get_usage(); s(40); echo memory_get_usage() - $m, "\n";
    /// $m = memory_get_usage(); s(40); echo memory_get_usage() - $m, "\n";
    /// ```
    /// ```expect
    /// 12032
    /// 0
    /// ```
    fn rebuilt_table(&mut self, last_var: u32) -> Option<Arr> {
        let Some(arr) = self.symtable_cache.pop() else {
            // `zend_new_array(last_var)`, its data laid down at once
            // where there is a CV to append.
            let packed = last_var == 0;
            return self.array_sized(last_var, packed).ok().map(PhpArray::heap);
        };
        let extended = self.heap.array_extend(arr, last_var);
        if self.fatal_of(extended).is_err() {
            self.symtable_cache.push(arr);
            return None;
        }
        Some(arr)
    }

    /// The names of the CVs of the op array `frame` serves, in the
    /// order the compiler numbered them, `op_array->vars`, one owner of
    /// each the caller's: the strings its unit's compile made of them
    /// ([`php_compile::named`]), found where it made them, in
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

    /// A name added to `table` holding `z`, `zend_hash_add_new`: the
    /// table takes an owner of the name, as php's bucket takes one of a
    /// key that is not interned, and grows as php's does. Where the
    /// memory limit refuses the room, which has ended the request, `z`
    /// is given up.
    fn table_add(&mut self, table: TableId, name: PhpString, z: Zval) {
        let added = match self.table_mut(table) {
            Some(arr) => {
                let added = self
                    .heap
                    .array_add_new(arr, php_heap::Key::Str(name.heap()), z);
                self.fatal_of(added).is_ok()
            }
            None => false,
        };
        if !added {
            self.release_row(z);
        }
    }

    /// `zend_clean_and_cache_symbol_table` when the frame that rebuilt a
    /// table pops, `table` the one it names: its values released and the
    /// table gone, cleaned and kept in `EG(symtable_cache)`.
    ///
    /// The table is the one the frame names, php's `EX(symbol_table)`,
    /// and only where it was rebuilt for this frame: a code frame shares
    /// its includer's, whose buckets that still name the frame's CVs,
    /// which the pop has released, are taken back as the detach takes
    /// them ([`Request::detach_named`]). A frame is named by its place on
    /// a stack, which a frame on a fiber's stack shares with one on the
    /// request's, and which a generator's frame leaves at every `yield`,
    /// so the table is never found by the place alone:
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
    /// frame gives it ([`Frames::eval_in_frame`]).
    pub(super) fn free_rebuilt_table(&mut self, frame: FrameRef, table: TableId) {
        let at = table.0 as usize;
        if self
            .tables
            .get(at)
            .is_some_and(|rebuilt| rebuilt.owner == Some(frame))
        {
            self.free_table_at(at);
        } else {
            self.detach_named(frame, table);
        }
    }

    /// The table a generator's frame was given, as the frame comes back
    /// on the stack at `frame`, wherever it stood before: the frame's at
    /// its new place. Its buckets point at the CVs where they stand, in
    /// the allocation the generator's frame keeps from its creation,
    /// which no resumption moves, as php's `IS_INDIRECT` buckets point
    /// into the `zend_execute_data` the generator keeps.
    pub(crate) fn resume_generator_table(&mut self, table: TableId, frame: FrameRef) {
        if let Some(rebuilt) = self.tables.get_mut(table.0 as usize)
            && rebuilt.owner.is_some()
        {
            rebuilt.owner = Some(frame);
            rebuilt.changes = rebuilt.changes.wrapping_add(1);
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

    /// The rebuilt table at `at` freed, the place left empty and owned by
    /// none, and every such place on top taken off; then
    /// `zend_symtable_clean`, its values released in order, a bucket that
    /// points at a CV owning nothing, and the table kept in
    /// `EG(symtable_cache)` where it has room, `zend_array_destroy`'d
    /// where it has none.
    fn free_table_at(&mut self, at: usize) {
        let freed = self.tables.get_mut(at).map(|table| {
            table.owner = None;
            table.changes = 0;
            std::mem::replace(&mut table.rows, Rows::Gone)
        });
        // `EG(symbol_table)`, the first, is never one a frame rebuilt.
        while self.tables.len() > 1 && self.tables.last().is_some_and(|t| t.owner.is_none()) {
            self.tables.pop();
        }
        let Some(Rows::Own(arr)) = freed else {
            return;
        };
        Heap::symtable_clean(&mut Hooks(self), arr);
        if self.symtable_cache.len() >= SYMTABLE_CACHE_SIZE {
            self.release(Value::array(PhpArray::of(arr)));
        } else {
            self.symtable_cache.push(arr);
        }
    }

    /// `ZEND_BIND_GLOBAL`'s half over `EG(symbol_table)`
    /// (`Z/Zend/zend_vm_def.h:8679`): the bucket `name` has, laid down
    /// holding `null` where the table has none (`zend_hash_lookup`), as
    /// [`Request::symbol_place`] finds it for a write, an `IS_INDIRECT`
    /// bucket followed to the CV of the frame attached to it; what stands
    /// there made a reference in place where it is none, an `UNDEF` taken
    /// as `null` (`ZVAL_MAKE_REF_EX`); and the cell handed back with one
    /// more owner, the binding's. The bucket points at the CV itself,
    /// which no stack switch moves, so a `global` inside a fiber binds the
    /// main frame's CV:
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
    pub(crate) fn global_cell(&mut self, name: PhpString) -> Result<PhpRef, Thrown> {
        let Some(place) = self.symbol_place(TableId::GLOBAL, name, true) else {
            return Err(crate::api::raising::fatal(
                self,
                "InternalError: a global bound to a bucket the table does not have",
            ));
        };
        let cell = self.make_ref(place)?;
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
        let Some(arr) = self.table_mut(table) else {
            return self.release(value);
        };
        let Some(bucket) = arr.str_value_ref(name.as_bytes()) else {
            return self.table_add(table, name, value.to_zval());
        };
        let held = bucket.read();
        if held.as_indirect().is_some() {
            return bucket.write(value.to_zval());
        }
        // A destructor the release serves can delete the name, the
        // table's owner of it with it, before the settle reads it.
        self.addref(&Value::string(name));
        let released = self.release_in_place(Value::of_zval(held));
        self.symbol_settle(table, name, value, Some(released));
        self.release(Value::string(name));
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
        let old = match self.table_mut(table) {
            None => {
                if let Some(released) = released {
                    released.let_go(self);
                }
                return self.release(value);
            }
            Some(arr) => arr.str_value_ref(name.as_bytes()).map(|bucket| {
                let old = bucket.read();
                bucket.write(value.to_zval());
                old
            }),
        };
        if old.is_none() {
            self.table_add(table, name, value.to_zval());
        }
        let by_program = match (old, released) {
            (Some(left), Some(released)) if left.as_indirect().is_none() => released
                .wrote_over(self, Value::of_zval(left))
                .then_some(left),
            (old, released) => {
                if let Some(released) = released {
                    released.let_go(self);
                }
                old
            }
        };
        if let Some(old) = by_program {
            self.release_row(old);
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
        let held = Value::of_slot(at.read());
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
        let left = Value::of_slot(at.read());
        at.write(value.to_zval());
        let by_program = match released {
            Some(released) => released.wrote_over(self, left),
            None => true,
        };
        if by_program {
            self.release(left);
        }
    }

    /// One symbol deleted, `zend_hash_del`: the name gone, its key given
    /// up and then what it held released; a name the table has not is no
    /// error, as php's delete of a missing key is none.
    pub(super) fn symbol_del(&mut self, table: TableId, name: &[u8]) {
        let taken = self
            .table_mut(table)
            .and_then(|arr| self.heap.array_str_take(arr, name));
        if let Some(z) = taken {
            self.release_row(z);
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
        let Some(bucket) = self.bucket(table, name) else {
            return;
        };
        match bucket.read().as_indirect() {
            Some(cv) => {
                let value = take(cv);
                self.release_row(value);
            }
            None => self.symbol_del(table, name),
        }
    }

    /// What a bucket owns given up: its value; one that points at a CV
    /// owns nothing.
    pub(super) fn release_row(&mut self, z: Zval) {
        if z.as_indirect().is_none() {
            self.release(Value::of_zval(z));
        }
    }

    /// `zend_hash_find_ind` in a symbol table, what a fetch by name that
    /// reads finds: what the bucket `name` holds, an `IS_INDIRECT` bucket
    /// followed to the CV of the frame attached to it; `None` where the
    /// table has no such name. A view: no owner taken.
    pub(crate) fn symbol_value(&self, table: TableId, name: &[u8]) -> Option<Value> {
        let z = self.symbol_arr(table)?.str_find(name)?;
        Some(Value::of_zval(deindirect(z)))
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
        let Some(bucket) = self.bucket(table, name.as_bytes()) else {
            return self.symbol_update(table, name, value);
        };
        let held = bucket.read();
        if held.as_indirect().is_some() {
            let attached =
                |req: &mut Request| req.bucket(table, name.as_bytes())?.read().as_indirect();
            return self.overwrite_place(attached, value, order);
        }
        match order {
            Overwrite::InPlace => self.symbol_update(table, name, value),
            Overwrite::After => {
                bucket.write(value.to_zval());
                self.release(Value::of_zval(held));
            }
            Overwrite::Nulled => {
                bucket.write(Zval::null());
                // The release can serve a destructor that deletes the
                // name, as [`Request::symbol_update`]'s can.
                self.addref(&Value::string(name));
                self.release(Value::of_zval(held));
                self.symbol_settle(table, name, value, None);
                self.release(Value::string(name));
            }
        }
    }

    /// `zend_hash_find` in a symbol table for a fetch by name that
    /// writes, php's `zval *` into the table that the `VAR` holds as an
    /// `IS_INDIRECT`: the bucket itself, or the CV an attached name's
    /// bucket points at; with `add`, a name the table has not is laid
    /// down holding `null` (`zend_hash_add_new`). `None` where the name
    /// is not there and `add` is not asked. The place stands until the
    /// table is next resized, as php's does, and php's compiler writes
    /// through it in the opline after the fetch; the write costs what
    /// php's costs: nothing past a name the program made and the bucket
    /// a new name adds.
    ///
    /// ```php
    /// function f() { $m = memory_get_usage(); $GLOBALS['zzzz'] = 1; echo memory_get_usage() - $m, "\n"; }
    /// f();
    /// $i = 7;
    /// $m = memory_get_usage(); ${"x$i"} = 1; echo memory_get_usage() - $m, "\n";
    /// $k = "y$i";
    /// $m = memory_get_usage(); $$k = 1; echo memory_get_usage() - $m, "\n";
    /// $m = memory_get_usage(); $$k .= 'a'; ${"x$i"}++; echo memory_get_usage() - $m, "\n";
    /// var_dump($GLOBALS['zzzz'], ${"x$i"}, $$k);
    /// ```
    /// ```expect
    /// 0
    /// 32
    /// 0
    /// 32
    /// int(1)
    /// int(2)
    /// string(2) "1a"
    /// ```
    pub(crate) fn symbol_place(
        &mut self,
        table: TableId,
        name: PhpString,
        add: bool,
    ) -> Option<ZvalRef> {
        let bucket = match self.bucket(table, name.as_bytes()) {
            Some(bucket) => bucket,
            None if add => {
                self.table_add(table, name, Zval::null());
                self.bucket(table, name.as_bytes())?
            }
            None => return None,
        };
        Some(bucket.read().as_indirect().unwrap_or(bucket))
    }

    /// `zend_hash_graceful_reverse_destroy(&EG(symbol_table))` in
    /// `zend_shutdown_executor_values`
    /// (`Z/Zend/zend_execute_API.c:271`), after the open resources are
    /// closed and before the object storage is freed: every symbol left
    /// deleted from the last to the first, then the table's data. The
    /// destructors are behind the request by now,
    /// `zend_objects_store_call_destructors` having marked every object,
    /// so this frees and serves nothing.
    pub(crate) fn destroy_symbol_table(&mut self) {
        // php's fast shutdown frees none of it, so no chunk it empties
        // leaves the heap php's child keeps (#2940).
        self.heap.fast_shutdown();
        if let Some(arr) = self.table_mut(TableId::GLOBAL) {
            for at in (0..arr.num_used()).rev() {
                if let Some(z) = self.heap.array_take_at(arr, at) {
                    self.release_row(z);
                }
            }
        }
        self.free_global_data();
    }

    /// `EG(symbol_table)`'s data given back, where it holds no row.
    fn free_global_data(&mut self) {
        if let Some(SymbolTable {
            rows: Rows::Embedded(table),
            ..
        }) = self.tables.first()
        {
            table.free_data(&mut self.heap);
        }
    }

    /// `shutdown_executor`'s frees of `EG(symtable_cache)`, each table
    /// destroyed, and of `EG(symbol_table)`'s data where no destroy of
    /// the table freed it, which a debug build of php makes by name
    /// (`Z/Zend/zend_execute_API.c:496-502`).
    #[cfg(debug_assertions)]
    pub(super) fn free_symbol_tables(&mut self) {
        for arr in std::mem::take(&mut self.symtable_cache) {
            self.release(Value::array(PhpArray::of(arr)));
        }
        self.free_global_data();
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
            let value = Owner::adopt(self.dup_value(value, None));
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
            let value = Owner::adopt(self.dup_value(value, None));
            array.set(self, key, value)?;
        }
        Ok(array)
    }

    /// `nTableSize` of `table`, eight where nothing sizes it.
    fn table_size(&self, table: TableId) -> u32 {
        self.symbol_arr(table).map_or(8, |arr| arr.table_size())
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

    /// Each name of `table` in the table's order with its bucket as it
    /// stands, an `IS_INDIRECT` one not followed. Views; no owner taken.
    pub(super) fn symbol_entries(&self, table: TableId) -> Vec<(PhpString, Zval)> {
        let arr = self.symbol_arr(table).unwrap_or_else(Arr::empty);
        arr.iter()
            .filter_map(|(key, z)| match key {
                php_heap::Key::Str(name) => Some((PhpString::of(name), z)),
                php_heap::Key::Long(_) => None,
            })
            .collect()
    }

    /// Each name of `table` in the table's order with what it holds, an
    /// `IS_INDIRECT` bucket followed to its CV. Views; no owner taken.
    pub(super) fn symbol_vars(&self, table: TableId) -> Vec<(PhpString, Value)> {
        let entries = self.symbol_entries(table).into_iter();
        entries
            .map(|(name, z)| (name, Value::of_zval(deindirect(z))))
            .collect()
    }

    /// What a copy of `table` reads, the half of `zend_array_dup_value`
    /// that is the table's: each name in the table's order with what it
    /// holds, an `IS_INDIRECT` bucket followed to its CV and one whose
    /// CV holds nothing left out. Views; no owner taken.
    fn symbol_rows(&self, table: TableId) -> Copied {
        let vars = self.symbol_vars(table);
        let buckets = vars.len();
        let numeric =
            (vars.iter()).any(|(name, _)| php_heap::canonical_int(name.as_bytes()).is_some());
        let rows = (vars.into_iter())
            .filter(|(_, value)| value.tag() != TypeTag::Undef)
            .collect();
        Copied {
            rows,
            buckets,
            numeric,
        }
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
