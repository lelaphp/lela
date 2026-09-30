//! An object's property table read and written as a table, php's
//! `zobj->properties` under `zend_symtable_find_ind`,
//! `zend_symtable_update_ind` and `zend_symtable_del_ind`: no handler, no
//! `__get` or `__set`, no hook, no visibility and no declared type in the
//! way, which is how `spl_array.c` reads and writes an `ArrayObject`'s
//! backing object. php's table holds an `IS_INDIRECT` row per declared
//! slot, keyed by the name `zend_mangle_property_name` makes, then the
//! dynamic properties; here the slots stay on the object and the table
//! holds the dynamic ones alone, so a key is looked for among the slots'
//! mangled names first and in the dynamic table second. A slot nothing
//! holds is no row, as `zend_hash_find_ind` skips an `IS_INDIRECT` to an
//! `UNDEF`, and a write lands in it all the same.
//!
//! The dynamic table keys a numeric name by its decimal string where
//! php's `zend_symtable_update` makes it an integer key, since every
//! reader of an object's dynamic table here reads string keys, as php's
//! `zend_symtable_to_proptable` leaves the table a cast makes.
//!
//! Every access first initialises a lazy object and goes on with what
//! that answers, the ghost or the proxy's instance, since php reaches the
//! table through `get_properties`, `zend_std_get_properties`, which
//! calls `zend_lazy_object_init`.

use php_heap::{Heap, Obj};
use php_unit::decl::Visibility;

use super::{Hooks, Request};
use crate::api::raising::Thrown;
use crate::api::values::{ClassId, Key, PhpObject, PhpRef, PhpString, Value};

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

    /// The row `key` names on an object of `class`: the declared slot
    /// whose mangled name is the key, `\0*\0p` for a protected property
    /// and `\0C\0p` for a private one declared by `C`, or the dynamic
    /// table's key, a number spelt in decimal.
    fn table_row_of(&mut self, class: ClassId, key: &Key) -> Result<Row, Thrown> {
        let name = match key {
            Key::Str(name) => *name,
            Key::Int(n) => {
                let spelt = PhpString::new(self, n.to_string().as_bytes())?;
                return Ok(Row::Dynamic(php_heap::Key::Str(spelt.heap())));
            }
        };
        let bytes = name.as_bytes();
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
                return Ok(Row::Slot(u32::try_from(at).unwrap_or(u32::MAX)));
            }
        }
        Ok(Row::Dynamic(php_heap::Key::Str(name.heap())))
    }

    /// [`PhpObject::row`]: `zend_symtable_find_ind`, a view.
    pub(crate) fn table_row(
        &mut self,
        object: PhpObject,
        key: &Key,
    ) -> Result<Option<Value>, Thrown> {
        let object = self.table_owner(object)?;
        let o = object.heap();
        Ok(match self.table_row_of(object.class(), key)? {
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
            .and_then(|object| Ok((object, self.table_row_of(object.class(), key)?)));
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
            Row::Dynamic(k) => match self.dynamic_table(o) {
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
        match self.table_row_of(object.class(), key)? {
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
                    let table = self.dynamic_table(o)?;
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
        let place = match self.table_row_of(object.class(), key)? {
            Row::Slot(i) => o.slot_ref(i),
            Row::Dynamic(k) => {
                let table = self.dynamic_table(o)?;
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
        if let Row::Slot(i) = self.table_row_of(object.class(), key)?
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

    /// `rebuild_object_properties` then `SEPARATE_ARRAY` over the
    /// object's dynamic table: the table with one owner, the object's.
    fn dynamic_table(&mut self, o: Obj) -> Result<php_heap::Arr, Thrown> {
        let table = self.heap.object_properties(o);
        let table = self.fatal_of(table)?;
        let table = self.array_separate(table)?;
        self.heap.object_set_properties(o, table);
        Ok(table)
    }
}
