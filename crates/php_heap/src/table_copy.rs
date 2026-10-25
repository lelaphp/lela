//! Copying a table as php copies it (design §5.3, #2005): the one rule
//! beside [`Heap::array_dup`] for what an element becomes in a copy, and
//! php's two conversions between an array's table and an object's,
//! which share the table where its keys allow it.
//!
//! The rule is `zend_array_dup_value`'s (`Z/Zend/zend_hash.c:2293`),
//! which `zend_array_dup_element` (`:2330`) runs for each bucket:
//! a reference nothing but the table holds is copied as the value it
//! holds, so the copy shares no cell the original's last holder left
//! behind, unless that value is the table being copied; anything else
//! is one more owner of itself, and a hole is left out. `zval_add_ref`,
//! which a clone's members take, is the same rule with no table to
//! compare against. The conversions are `zend_symtable_to_proptable`,
//! what the `(object)` cast makes of an array, and
//! `zend_proptable_to_symtable`, what the `(array)` cast,
//! `get_object_vars()` and `get_mangled_object_vars()` make of an
//! object's table: each hands the table itself back with one more owner
//! where every key is already one the other kind reads, and otherwise
//! builds a copy under the rule with each key spelt as the other kind
//! spells it.
//!
//! Every copy of a table the engine makes calls here and keeps no rule
//! of its own: [`Heap::array_dup`], a clone's members, the separation of
//! an object's table ([`Heap::object_properties_copy`]), and the
//! engine's `$GLOBALS`, `get_defined_vars()`, `(object)` and `(array)`
//! casts, `get_object_vars()` and `get_mangled_object_vars()`, whose
//! symbol tables and declared rows are the engine's and whose elements
//! are each copied by [`Heap::dup_value`]. php writes the same unwrap
//! inline in algorithms that copy no table (an array spread, a
//! closure's debug statics, the array natives), and those keep php's
//! inline form; `ext/mysqli`'s `fetch_object` conversion moves here
//! with #2110.

use crate::array::{Arr, DeclaredRows, Key, canonical_int};
use crate::hooks::{Fatal, HeapHooks};
use crate::numeric::LongText;
use crate::string::{self, Str};
use crate::{Heap, Zval};

impl Heap {
    /// `zend_array_dup_value`'s copy of one element
    /// (`Z/Zend/zend_hash.c:2293`), and `zval_add_ref` where `source` is
    /// `None`, the rule every builder of a copy calls: a reference nothing but this holder
    /// owns is copied as the value it holds, one more owner of that, so
    /// the copy does not share a cell the original's last holder left
    /// behind (`foreach ($a as &$v) {} unset($v);` leaves one); a
    /// reference to the array being copied itself stays one, and every
    /// other value is one more owner of itself.
    ///
    /// ```php
    /// $a = [1, 2]; foreach ($a as &$v) {} unset($v);
    /// $b = $a; $b[1] = 9;
    /// $o = new stdClass; $o->p = 1; $r = &$o->p; unset($r);
    /// $c = clone $o; $c->p = 5;
    /// $k = [1]; $k[] = &$k; $d = $k; $d[0] = 3;
    /// var_dump($a[1], $o->p, $k[0]);
    /// ```
    /// ```expect
    /// int(2)
    /// int(1)
    /// int(1)
    /// ```
    pub fn dup_value(&self, z: Zval, source: Option<Arr>) -> Zval {
        if let Some(r) = z.as_reference()
            && r.refcount() == 1
        {
            let inner = r.val();
            let itself = inner
                .as_arr()
                .is_some_and(|held| source.is_some_and(|s| held.is_same(s)));
            if !itself {
                self.addref(inner);
                return inner;
            }
        }
        self.addref(z);
        z
    }

    /// `zend_symtable_to_proptable` (`Z/Zend/zend_hash.c:3343`), what
    /// the `(object)` cast makes of an array: `a` itself, one more owner
    /// of it unless it is immutable, where it is a hash whose keys are
    /// all strings, since a property table reads nothing else; and
    /// otherwise a new table of `a`'s size holding each element under
    /// [`Heap::dup_value`]'s rule, an integer key spelt in decimal
    /// (`zend_long_to_str`, a digit the process's own string). The cast
    /// then copies an immutable table it was handed, as
    /// `zend_cast_zval_to_object` does.
    ///
    /// ```php
    /// $x = 1; $a = [5 => 'five', 'r' => &$x, 'k' => 1]; unset($x);
    /// $o = (object) $a; $o->r = 2;
    /// var_dump($o, $a['r']);
    /// ```
    /// ```expect
    /// object(stdClass)#1 (3) {
    ///   ["5"]=>
    ///   string(4) "five"
    ///   ["r"]=>
    ///   int(2)
    ///   ["k"]=>
    ///   int(1)
    /// }
    /// int(1)
    /// ```
    pub fn symtable_to_proptable(hooks: &mut dyn HeapHooks, a: Arr) -> Result<Arr, Fatal> {
        if !a.is_packed() && a.iter().all(|(key, _)| matches!(key, Key::Str(_))) {
            a.addref();
            return Ok(a);
        }
        let size = u32::try_from(a.len()).unwrap_or(u32::MAX);
        let copy = hooks.heap().array_sized(size)?;
        for (key, z) in &a {
            let z = hooks.heap().dup_value(z, None);
            match key {
                Key::Str(name) => Heap::array_update(hooks, copy, Key::Str(name), z)?,
                Key::Long(n) => {
                    let name = hooks.heap().long_to_str(n)?;
                    let written = Heap::array_update(hooks, copy, Key::Str(name), z);
                    hooks.heap().release_string(name);
                    written?;
                }
            }
        }
        Ok(copy)
    }

    /// `zend_proptable_to_symtable` (`Z/Zend/zend_hash.c:3396`), what
    /// the `(array)` cast, `get_object_vars()` and
    /// `get_mangled_object_vars()` make of an object's table: where no
    /// string key of it reads as an integer, a copy of it under
    /// `always_duplicate` ([`Heap::array_dup`]) and otherwise `a` itself,
    /// one more owner of it unless it is immutable; where one does, a
    /// new table of `a`'s size holding each element under
    /// [`Heap::dup_value`]'s rule, a decimal name the integer it reads
    /// as (`ZEND_HANDLE_NUMERIC`).
    ///
    /// ```php
    /// $o = new stdClass; $o->a = str_repeat('a', 40); $o->b = 2;
    /// $m = memory_get_usage(); $x = (array) $o; echo memory_get_usage() - $m, "\n";
    /// $x['a'] = 1; $o->b = 3; echo $o->a[0], $x['b'], "\n";
    /// $p = new stdClass; $p->{'7'} = 1; $v = 2; $p->r = &$v; unset($v);
    /// $y = (array) $p; $y['r'] = 3; var_dump($y, $p->r);
    /// ```
    /// ```expect
    /// 0
    /// a2
    /// array(2) {
    ///   [7]=>
    ///   int(1)
    ///   ["r"]=>
    ///   int(3)
    /// }
    /// int(2)
    /// ```
    pub fn proptable_to_symtable(
        hooks: &mut dyn HeapHooks,
        a: Arr,
        always_duplicate: bool,
    ) -> Result<Arr, Fatal> {
        let numeric = !a.is_packed()
            && a.iter().any(
                |(key, _)| matches!(key, Key::Str(name) if canonical_int(name.bytes()).is_some()),
            );
        if !numeric {
            if always_duplicate {
                return hooks.heap().array_dup(a);
            }
            a.addref();
            return Ok(a);
        }
        let size = u32::try_from(a.len()).unwrap_or(u32::MAX);
        let copy = hooks.heap().array_sized(size)?;
        for (key, z) in &a {
            let z = hooks.heap().dup_value(z, None);
            let key = match key {
                Key::Str(name) => canonical_int(name.bytes()).map_or(Key::Str(name), Key::Long),
                Key::Long(n) => Key::Long(n),
            };
            Heap::array_update(hooks, copy, key, z)?;
        }
        Ok(copy)
    }

    /// `zend_long_to_str`: an integer key spelt as a property name, one
    /// owner of it the caller's, the process's own string for a digit
    /// (`ZSTR_CHAR`).
    fn long_to_str(&mut self, n: i64) -> Result<Str, Fatal> {
        let spelt = LongText::long(n);
        match string::frozen_string(spelt.as_bytes()) {
            Some(digit) => Ok(digit),
            None => self.string(spelt.as_bytes()),
        }
    }

    /// `zend_array_dup` over an object's property table that stands for
    /// its declared slots and is shared, what `SEPARATE_ARRAY` over it
    /// does before a write (`Z/Zend/zend_hash.c:2503-2505`): a copy that
    /// holds each slot's value, a reference only this slot owns copied
    /// as the value it holds, under `names[i]`, the slot's mangled name,
    /// a slot nothing holds left out as php's copy skips an
    /// `IS_INDIRECT` to an `UNDEF`, and then the dynamic rows, and that
    /// holds them from then on ([`DeclaredRows::Copied`]). The object
    /// takes the copy and the table it had, which another holder shares,
    /// is one owner fewer. `names`
    /// has one name per slot; a slot past its end is left out.
    pub fn object_properties_copy(&mut self, o: crate::Obj, names: &[Str]) -> Result<Arr, Fatal> {
        let source = o.properties();
        let held = (0..o.slots()).filter(|i| !o.slot(*i).is_undef()).count();
        let dynamic = source.map_or(0, |table| table.len());
        let size = u32::try_from(held + dynamic).unwrap_or(u32::MAX);
        let copy = self.array_mixed(size)?;
        // `target->nInternalPointer`: the source's where it stands before
        // the end and zero past it, then moved with the row it stands at
        // as the holes close up; a pointer at a row the copy leaves out
        // keeps the number it had.
        let (from, used) = o
            .property_table()
            .map_or((0, 0), |table| (table.raw_pointer(), table.used()));
        let mut pointer = if from < used { from } else { 0 };
        let mut at = 0;
        for (i, name) in (0..o.slots()).zip(names) {
            let z = o.slot(i);
            if z.is_undef() {
                continue;
            }
            if i == from {
                pointer = at;
            }
            at += 1;
            let z = self.dup_value(z, source);
            self.array_write_str(copy, *name, z)?;
        }
        if let Some(table) = source {
            copy.set_next_free(table.next_free());
            for pos in 0..table.num_used() {
                let Some((key, z)) = table.entry_at(pos) else {
                    continue;
                };
                if o.slots() + pos == from {
                    pointer = at;
                }
                at += 1;
                let z = self.dup_value(z, Some(table));
                self.array_add_new(copy, key, z)?;
            }
            let _ = table.delref();
        }
        copy.set_raw_pointer(pointer);
        copy.set_declared_rows(DeclaredRows::Copied);
        self.object_set_properties(o, copy);
        Ok(copy)
    }
}
