//! An object's property table read and written as a table, php's
//! `zobj->properties` under `zend_hash_find_ind`, `zend_hash_update_ind`
//! and `zend_hash_del_ind` and their `zend_hash_index_*` twins: no handler, no
//! `__get` or `__set`, no hook, no visibility and no declared type in the
//! way, which is how `spl_array.c` reads and writes an `ArrayObject`'s
//! backing object. php's table holds an `IS_INDIRECT` row per declared
//! slot, keyed by the name `zend_mangle_property_name` makes, then the
//! dynamic properties; here the slots stay on the object and the table
//! holds the dynamic ones alone, so a key is looked for among the slots'
//! mangled names first and in the dynamic table second. A slot nothing
//! holds is no row, as `zend_hash_find_ind` skips an `IS_INDIRECT` to an
//! `UNDEF`, and a write lands in it all the same. A table that holds
//! copies of the declared rows ([`php_heap::DeclaredRows::Copied`]) is
//! read and written as it stands, every key its own row.
//!
//! A key is the table's as it is handed over, `zend_hash_update` for a
//! string and `zend_hash_index_update` for an integer, so an integer key
//! is a row of its own beside the string that spells it, as
//! `object_properties_load` and `zend_hash_copy` leave php's table. A
//! caller that reads a key as a property name spells an integer in
//! decimal first, as `spl_array`'s `get_hash_key` does for an object.
//!
//! Every access first initialises a lazy object and goes on with what
//! that answers, the ghost or the proxy's instance, since php reaches the
//! table through `get_properties`, `zend_std_get_properties`, which
//! calls `zend_lazy_object_init`.

use php_heap::{Direction, Heap, Obj, PropertyTable, TableRow};
use php_unit::decl::Visibility;

use super::{Hooks, Request};
use crate::api::raising::Thrown;
use crate::api::values::{
    ClassId, Key, PhpArray, PhpObject, PhpRef, PhpString, PointerMove, Value,
};

/// Where a key of the table stands: a declared slot, or a key of the
/// dynamic table.
enum Row {
    Slot(u32),
    Dynamic(php_heap::Key),
}

impl Request {
    /// The object whose table an access reads, `zend_std_get_properties`'s
    /// `zend_lazy_object_init` first.
    fn table_owner(&mut self, object: PhpObject) -> Result<PhpObject, Thrown> {
        self.lazy_init(object)
    }

    /// The row `key` names on `object`: the declared slot
    /// whose mangled name is the key, `\0*\0p` for a protected property
    /// and `\0C\0p` for a private one declared by `C`, or the dynamic
    /// table's key, an integer one an index row.
    fn table_row_of(&self, object: PhpObject, key: &Key) -> Row {
        let class = object.class();
        let object_rows = object.heap().declared_rows();
        let name = match key {
            Key::Str(name) => *name,
            Key::Int(n) => return Row::Dynamic(php_heap::Key::Long(*n)),
        };
        let bytes = name.as_bytes();
        // A table that holds copies of the declared rows answers every
        // key itself, as php's plain values do.
        if object_rows == php_heap::DeclaredRows::Copied {
            return Row::Dynamic(php_heap::Key::Str(name.heap()));
        }
        for (at, (declaring, property)) in self.placed_properties(class).iter().enumerate() {
            let prefix: Vec<u8> = match property.visibility {
                Visibility::Public => Vec::new(),
                Visibility::Protected => b"\0*\0".to_vec(),
                Visibility::Private => self
                    .class_decl(*declaring)
                    .map(|d| [&b"\0"[..], &d.name.0, &b"\0"[..]].concat())
                    .unwrap_or_default(),
            };
            if bytes.len() == prefix.len() + property.name.0.len()
                && bytes.starts_with(&prefix)
                && bytes[prefix.len()..] == *property.name.0
            {
                return Row::Slot(u32::try_from(at).unwrap_or(u32::MAX));
            }
        }
        Row::Dynamic(php_heap::Key::Str(name.heap()))
    }

    /// [`PhpObject::row`]: `zend_symtable_find_ind`, a view.
    pub(crate) fn table_row(
        &mut self,
        object: PhpObject,
        key: &Key,
    ) -> Result<Option<Value>, Thrown> {
        let object = self.table_owner(object)?;
        let o = object.heap();
        Ok(match self.table_row_of(object, key) {
            Row::Slot(i) => {
                let z = o.slot(i);
                (!z.is_undef()).then(|| Value::of_zval(z))
            }
            Row::Dynamic(k) => o
                .properties()
                .and_then(|table| table.get(k))
                .map(Value::of_zval),
        })
    }

    /// [`PhpObject::set_row`]: `zend_symtable_update_ind`, the slot or
    /// the dynamic entry replaced, a reference it held given up rather
    /// than written through, and the dynamic table separated where a
    /// cast shares it. `value` is the table's from here, and what the
    /// row held is released after the write.
    pub(crate) fn table_set_row(
        &mut self,
        object: PhpObject,
        key: &Key,
        value: Value,
    ) -> Result<(), Thrown> {
        let placed = self
            .table_owner(object)
            .map(|object| (object, self.table_row_of(object, key)));
        let (object, row) = match placed {
            Ok(placed) => placed,
            Err(thrown) => {
                self.release(value);
                return Err(thrown);
            }
        };
        let o = object.heap();
        match row {
            Row::Slot(i) => self.write_declared_slot(o, i, value),
            Row::Dynamic(k) => match self.separated_properties(o) {
                Ok(table) => self.array_update(table, k, value),
                Err(thrown) => {
                    self.release(value);
                    Err(thrown)
                }
            },
        }
    }

    /// [`PhpObject::unset_row`]: `zend_symtable_del_ind`, a slot left
    /// `UNDEF` and a dynamic key removed, what either held released.
    pub(crate) fn table_unset_row(&mut self, object: PhpObject, key: &Key) -> Result<(), Thrown> {
        let object = self.table_owner(object)?;
        let o = object.heap();
        match self.table_row_of(object, key) {
            Row::Slot(i) => {
                if !o.slot(i).is_undef() {
                    Heap::object_set_slot(&mut Hooks(self), o, i, php_heap::Zval::UNDEF);
                    // `zend_hash_del_ind`: the table marked once what the
                    // slot held is given up.
                    if let Some(table) = o.properties() {
                        table.set_has_empty_ind(true);
                    }
                }
            }
            Row::Dynamic(k) => {
                if o.properties().is_some_and(|table| table.contains(k)) {
                    let table = self.separated_properties(o)?;
                    self.array_del(table, k);
                }
            }
        }
        Ok(())
    }

    /// [`PhpObject::row_ref`]: the row made a reference in place
    /// (`ZVAL_MAKE_REF`), `null` laid down where the table has none,
    /// and the cell handed back with one more owner, the caller's.
    pub(crate) fn table_row_ref(&mut self, object: PhpObject, key: &Key) -> Result<PhpRef, Thrown> {
        let place = self.table_row_place(object, key)?;
        let held = Value::of_zval(place.read());
        let cell = if let Some(cell) = held.as_php_ref() {
            *cell
        } else {
            let cell = self.reference(held.undef_as_null())?;
            place.write(cell.as_value().to_zval());
            cell
        };
        self.addref(&cell.as_value());
        Ok(cell)
    }

    /// [`PhpObject::row_place`]: where the row stands, a dynamic entry
    /// laid down as `null` where the table has none (`array_lookup`); a
    /// slot nothing wrote is left `UNDEF` for the caller, which reads it
    /// as `null`.
    pub(crate) fn table_row_place(
        &mut self,
        object: PhpObject,
        key: &Key,
    ) -> Result<php_heap::ZvalRef, Thrown> {
        let object = self.table_owner(object)?;
        let o = object.heap();
        let place = match self.table_row_of(object, key) {
            Row::Slot(i) => o.slot_ref(i),
            Row::Dynamic(k) => {
                let table = self.separated_properties(o)?;
                self.array_lookup(table, k)?
            }
        };
        place.ok_or_else(|| {
            crate::api::raising::fatal(
                self,
                "InternalError: a row of a property table with no place to stand",
            )
        })
    }

    /// [`PhpObject::typed_row_ref`]: [`Request::table_row_ref`] over a
    /// row whose slot a typed property owns taken as `ZEND_FETCH_REF`
    /// takes it first ([`Request::fetch_typed_slot_ref`]): a slot that holds
    /// no cell yet made one the property is a type source of
    /// (`ZEND_REF_ADD_TYPE_SOURCE` over
    /// `zend_get_typed_property_info_for_slot`), and a slot nothing has
    /// written refused where the type takes no `null`. A dynamic row, an
    /// untyped slot and a slot holding a cell already are `row_ref`'s.
    pub(crate) fn table_typed_row_ref(
        &mut self,
        object: PhpObject,
        key: &Key,
    ) -> Result<PhpRef, Thrown> {
        let object = self.table_owner(object)?;
        if let Row::Slot(i) = self.table_row_of(object, key)
            && let Some((typed, name)) = self.slot_typed_property(object.class(), i)
            && let Some(at) = object.heap().slot_ref(i)
        {
            self.fetch_typed_slot_ref(at, &typed, &name)?;
        }
        self.table_row_ref(object, key)
    }

    /// [`PhpObject::entries`]: the rows in the table's order, the slots
    /// that hold a value by their mangled names and then the dynamic
    /// table, each key as it stands; views, no owner taken.
    pub(crate) fn table_entries(&mut self, object: PhpObject) -> Result<Vec<(Key, Value)>, Thrown> {
        let object = self.table_owner(object)?;
        crate::ops::properties_for(self, object, crate::api::abi::PropertyPurpose::Serialize)
    }

    /// `get_ht_for_iap` (`Z/ext/standard/array.c:993`) past its
    /// deprecation: the object's table separated first where another
    /// holder shares it and `separate` asks, as `zend_array_dup` does,
    /// then the class's `get_properties`, which lays the table down,
    /// initialises a lazy object, whose instance's table is then the one
    /// the pointer moves over, and at `DateInterval` writes the fields
    /// into it. The object whose table it is rides beside it.
    fn pointer_table(
        &mut self,
        object: PhpObject,
        separate: bool,
    ) -> Result<(PhpObject, PropertyTable), Thrown> {
        if separate && object.heap().properties().is_some_and(|t| t.refcount() > 1) {
            self.separated_properties(object.heap())?;
        }
        let object = self.table_owner(object)?;
        let handlers = self.object_handlers(object.heap());
        handlers.get_properties(self, object)?;
        let table = self.heap.object_property_table(object.heap());
        Ok((object, self.fatal_of(table)?))
    }

    /// [`PhpObject::pointer_move`]: `reset`, `end`, `next` or `prev`
    /// over the table `get_ht_for_iap` answers with its separation,
    /// nothing moved and `false` for a table with no row, php's
    /// `zend_hash_num_elements(array) == 0`, which counts a declared
    /// slot nothing holds.
    pub(crate) fn pointer_move(
        &mut self,
        object: PhpObject,
        how: PointerMove,
    ) -> Result<bool, Thrown> {
        let (_, table) = self.pointer_table(object, true)?;
        if table.is_empty() {
            return Ok(false);
        }
        match how {
            PointerMove::Reset => table.reset(),
            PointerMove::End => table.end(),
            PointerMove::Forward => {
                table.forward();
            }
            PointerMove::Backward => {
                table.backward();
            }
        }
        Ok(true)
    }

    /// [`PhpObject::pointer_row`]: `php_array_iter_seek_current` over the
    /// table `get_ht_for_iap` answers without its separation, stepping
    /// back over an uninitialised slot after `end` and `prev` and on
    /// otherwise, the pointer left where it stops; the row's key, a
    /// declared one by its mangled name, and its value, views.
    pub(crate) fn pointer_row(
        &mut self,
        object: PhpObject,
        after: Option<PointerMove>,
    ) -> Result<Option<(Key, Value)>, Thrown> {
        let (owner, table) = self.pointer_table(object, false)?;
        let toward = match after {
            Some(PointerMove::End | PointerMove::Backward) => Direction::Backward,
            Some(PointerMove::Reset | PointerMove::Forward) | None => Direction::Forward,
        };
        let Some((row, z)) = table.seek(toward) else {
            return Ok(None);
        };
        let key = match row {
            TableRow::Entry(key) => Key::of(key),
            TableRow::Slot(i) => {
                let names = self.slot_names(owner.class())?;
                let Some(name) = usize::try_from(i).ok().and_then(|i| names.get(i)) else {
                    return self.fatal_of(Err(php_heap::Fatal::Error(String::from(
                        "InternalError: a declared slot of an object's table with no name",
                    ))));
                };
                Key::Str(PhpString::of(*name))
            }
        };
        Ok(Some((key, Value::of_zval(z))))
    }

    /// [`PhpObject::replace_table`]: `*ht_ptr = Z_ARRVAL_P(ht_zv)` over
    /// an object, the table a lazy object's instance keeps; `table` laid
    /// down as the object's with its numeric keys spelt in decimal, as
    /// every reader of the table here reads them.
    pub(crate) fn replace_table(
        &mut self,
        object: PhpObject,
        table: PhpArray,
    ) -> Result<(), Thrown> {
        let object = self.table_owner(object)?;
        let o = object.heap();
        let mut table = table;
        if table.iter().any(|(key, _)| matches!(key, Key::Int(_))) {
            let mut spelt = PhpArray::new(self)?;
            for (key, value) in table.snapshot_refs() {
                let key = match key {
                    Key::Int(n) => Key::Str(PhpString::new(self, n.to_string().as_bytes())?),
                    Key::Str(name) => {
                        self.addref(&Value::string(name));
                        Key::Str(name)
                    }
                };
                self.addref(&value);
                spelt.set(self, key, value)?;
            }
            self.release(Value::array(table));
            table = spelt;
        }
        let table = table.heap();
        table.set_declared_rows(php_heap::DeclaredRows::Copied);
        let old = o.properties();
        self.heap.object_set_properties(o, table);
        if let Some(old) = old {
            self.release(Value::array(PhpArray::of(old)));
        }
        Ok(())
    }

    /// `rebuild_object_properties` then `SEPARATE_ARRAY` over the
    /// object's table, what every write to a dynamic property does before
    /// it writes: the table with one owner, the object's. One a lazy
    /// object's snapshot shares while it stands for the declared slots is
    /// separated as `zend_array_dup` separates php's, which copies what
    /// each `IS_INDIRECT` points at, so the copy holds the declared rows
    /// as they stood from then on ([`php_heap::DeclaredRows::Copied`])
    /// and every reader of the table reads them there, apart from the
    /// slots.
    ///
    /// ```php
    /// #[AllowDynamicProperties]
    /// class C { public int $b = 1; protected $p = 'p';
    ///     function __construct() { $this->a = 1; $this->b = 3; $this->p = 'q'; } }
    /// $o = (new ReflectionClass(C::class))->newLazyGhost(function ($o) { $o->__construct(); });
    /// var_dump($o);
    /// $o->a += 1;
    /// var_dump($o, $o->b);
    /// echo json_encode(get_object_vars($o)), "\n";
    /// foreach ($o as $k => $v) { echo "$k => $v\n"; }
    /// $c = clone $o; $c->b = 7; $c->d = 1;
    /// var_dump($c);
    /// ```
    /// ```expect
    /// lazy ghost object(C)#3 (0) {
    ///   ["b"]=>
    ///   uninitialized(int)
    /// }
    /// object(C)#3 (3) {
    ///   ["b"]=>
    ///   int(1)
    ///   ["p":protected]=>
    ///   string(1) "p"
    ///   ["a"]=>
    ///   int(2)
    /// }
    /// int(3)
    /// {"b":1,"\u0000*\u0000p":"p","a":2}
    /// b => 1
    /// p => p
    /// a => 2
    /// object(C)#2 (4) {
    ///   ["b"]=>
    ///   int(1)
    ///   ["p":protected]=>
    ///   string(1) "p"
    ///   ["a"]=>
    ///   int(2)
    ///   ["d"]=>
    ///   int(1)
    /// }
    /// ```
    pub(crate) fn separated_properties(&mut self, o: Obj) -> Result<php_heap::Arr, Thrown> {
        let table = self.heap.object_properties(o);
        let table = self.fatal_of(table)?;
        if table.refcount() > 1
            && table.declared_rows() == php_heap::DeclaredRows::Slots
            && o.slots() > 0
        {
            let names = self.slot_names(ClassId(o.class()))?;
            let copy = self.heap.object_properties_copy(o, &names);
            return self.fatal_of(copy);
        }
        let rows = table.declared_rows();
        let table = self.array_separate(table)?;
        table.set_declared_rows(rows);
        self.heap.object_set_properties(o, table);
        Ok(table)
    }

    /// The key php's table holds each declared slot of `class` under,
    /// in slot order: `zend_mangle_property_name`'s `\0*\0p` for a
    /// protected property, `\0C\0p` for a private one `C` declared, and
    /// the name itself for a public one, interned as php's are.
    pub(crate) fn slot_names(&mut self, class: ClassId) -> Result<Vec<php_heap::Str>, Thrown> {
        let placed = self.placed_properties(class).to_vec();
        let mut names = Vec::with_capacity(placed.len());
        for (declaring, property) in &placed {
            let prefix: Vec<u8> = match property.visibility {
                Visibility::Public => Vec::new(),
                Visibility::Protected => b"\0*\0".to_vec(),
                Visibility::Private => self
                    .class_decl(*declaring)
                    .map(|d| [&b"\0"[..], &d.name.0, &b"\0"[..]].concat())
                    .unwrap_or_default(),
            };
            let name = self.interned(&[prefix.as_slice(), &property.name.0].concat())?;
            names.push(name.heap());
        }
        Ok(names)
    }
}
