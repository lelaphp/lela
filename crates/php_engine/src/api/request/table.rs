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
use crate::api::abi::{PropertiesFor, PropertyPurpose};
use crate::api::raising::Thrown;
use crate::api::values::walk::{ObjectTable, Table};
use crate::api::values::{
    ClassId, Key, Owner, PhpArray, PhpObject, PhpRef, PhpString, PointerMove, Value,
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
        let name = match key {
            Key::Str(name) => *name,
            Key::Int(n) => return Row::Dynamic(php_heap::Key::Long(*n)),
        };
        match self.slot_named(object, name.as_bytes()) {
            Some(at) => Row::Slot(at),
            None => Row::Dynamic(php_heap::Key::Str(name.heap())),
        }
    }

    /// The declared slot of `object` whose mangled name is `bytes`; none
    /// where no slot is named so, or where the table holds copies of the
    /// declared rows, which answers every key itself, as php's plain
    /// values do.
    fn slot_named(&self, object: PhpObject, bytes: &[u8]) -> Option<u32> {
        if object.heap().declared_rows() == php_heap::DeclaredRows::Copied {
            return None;
        }
        for (at, declaring, property) in self.placed(object.class()) {
            let prefix = self
                .property_prefix(declaring, property.visibility)
                .unwrap_or_default();
            if bytes.len() == prefix.len() + property.name.0.len()
                && bytes.starts_with(&prefix)
                && bytes[prefix.len()..] == *property.name.0
            {
                return Some(u32::try_from(at).unwrap_or(u32::MAX));
            }
        }
        None
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

    /// [`PhpObject::set_str`]: `zend_hash_str_update` over the table, the
    /// slot these bytes name written as [`Request::table_set_row`] writes
    /// it, and any other name the dynamic table's, separated first where a
    /// cast shares it, its key made by `zend_string_init` only where the
    /// table has none ([`Request::array_str_update`]). `value` is the
    /// table's from here.
    pub(crate) fn table_set_str(
        &mut self,
        object: PhpObject,
        name: &[u8],
        value: Value,
    ) -> Result<(), Thrown> {
        let object = match self.table_owner(object) {
            Ok(object) => object,
            Err(thrown) => {
                self.release(value);
                return Err(thrown);
            }
        };
        let o = object.heap();
        if let Some(at) = self.slot_named(object, name) {
            return self.write_declared_slot(o, at, value);
        }
        match self.separated_properties(o) {
            Ok(table) => self.array_str_update(table, name, value),
            Err(thrown) => {
                self.release(value);
                Err(thrown)
            }
        }
    }

    /// [`PhpObject::append_row`]: `zend_hash_next_index_insert` over the
    /// dynamic table, separated first where a cast shares it, at its
    /// `nNextFreeElement`, which php's declared rows, all string keys,
    /// never move. `value` is the table's from here, or given up with
    /// `false` where the next element is taken.
    pub(crate) fn table_append_row(
        &mut self,
        object: PhpObject,
        value: Value,
    ) -> Result<bool, Thrown> {
        let table = match self
            .table_owner(object)
            .and_then(|object| self.separated_properties(object.heap()))
        {
            Ok(table) => table,
            Err(thrown) => {
                self.release(value);
                return Err(thrown);
            }
        };
        self.array_append(table, value)
    }

    /// [`PhpObject::update_row`]: `zend_hash_update`, a declared row
    /// written raw and marked plain ([`php_heap::PropBit::PlainRow`]),
    /// any other key [`Request::table_set_row`]'s.
    pub(crate) fn table_update_row(
        &mut self,
        object: PhpObject,
        key: &Key,
        value: Value,
    ) -> Result<(), Thrown> {
        let slot = match self.table_owner(object) {
            Ok(owner) => match self.table_row_of(owner, key) {
                Row::Slot(i) => Some((owner, i)),
                Row::Dynamic(_) => None,
            },
            Err(thrown) => {
                self.release(value);
                return Err(thrown);
            }
        };
        let Some((owner, i)) = slot else {
            return self.table_set_row(object, key, value);
        };
        let o = owner.heap();
        self.write_declared_slot(o, i, value)?;
        o.set_slot_bit(i, php_heap::PropBit::PlainRow, true);
        Ok(())
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
        let cell = self.make_ref(place)?;
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
                Some(self.array_lookup(table, k)?)
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

    /// `get_ht_for_iap` (`Z/ext/standard/array.c:993`) past its
    /// deprecation: the object's table separated first where another
    /// holder shares it and `separate` asks, as `zend_array_dup` does,
    /// then the class's `get_properties`, which lays the table down,
    /// initialises a lazy object, whose instance's table is then the one
    /// the pointer moves over, and at `DateInterval` writes the fields
    /// into it. The object whose table it is rides beside it; a class
    /// that keeps a table in place of the object's has the pointer
    /// moved over the object's own all the same.
    pub(crate) fn pointer_table(
        &mut self,
        object: PhpObject,
        separate: bool,
    ) -> Result<(PhpObject, PropertyTable), Thrown> {
        if separate && object.heap().properties().is_some_and(|t| t.refcount() > 1) {
            self.separated_properties(object.heap())?;
        }
        let handlers = self.object_handlers(object.heap());
        if let Table::Object(table) = handlers.get_properties(self, object)? {
            return Ok((table.owner(), table.positions()));
        }
        let object = self.table_owner(object)?;
        let table = self.heap.object_property_table(object.heap());
        Ok((object, self.fatal_of(table)?))
    }

    /// `zend_std_get_properties`, `Std`'s `get_properties`: the class's
    /// table laid down for the object a lazy one initialises to, in
    /// php's positions.
    pub(crate) fn std_properties(&mut self, object: PhpObject) -> Result<Table, Thrown> {
        let owner = self.table_owner(object)?;
        let table = self.heap.object_property_table(owner.heap());
        let table = self.fatal_of(table)?;
        self.object_table(owner, table).map(Table::Object)
    }

    /// `zend_get_properties_no_lazy_init`: the class's `get_properties`
    /// over an object that is not lazy, and over a lazy one the table of
    /// the object its reader reads, laid down and nothing initialised
    /// (`rebuild_object_properties_internal`, through
    /// [`Request::lay_properties_no_lazy_init`]).
    ///
    /// ```php
    /// class C { public $a = 1; }
    /// $r = new ReflectionClass('C');
    /// $o = $r->newLazyGhost(function () { echo "init\n"; }, ReflectionClass::SKIP_INITIALIZATION_ON_SERIALIZE);
    /// echo serialize($o), "\n";
    /// var_dump(get_object_vars($o));
    /// $p = $r->newLazyGhost(function () { echo "init p\n"; });
    /// var_export($p);
    /// echo "\n";
    /// ```
    /// ```expect
    /// O:1:"C":0:{}
    /// init
    /// array(1) {
    ///   ["a"]=>
    ///   int(1)
    /// }
    /// init p
    /// \C::__set_state(array(
    ///    'a' => 1,
    /// ))
    /// ```
    pub(crate) fn properties_no_lazy_init(&mut self, object: PhpObject) -> Result<Table, Thrown> {
        if object.heap().laziness() == php_heap::Laziness::Eager {
            let handlers = self.object_handlers(object.heap());
            return handlers.get_properties(self, object);
        }
        let owner = self.lay_properties_no_lazy_init(object)?;
        let table = self.heap.object_property_table(owner.heap());
        let table = self.fatal_of(table)?;
        self.object_table(owner, table).map(Table::Object)
    }

    /// `zend_get_properties_for(obj, purpose)`: the class's
    /// `get_properties_for`, [`Request::std_properties_for`] at `Std`.
    pub(crate) fn properties_answered(
        &mut self,
        object: PhpObject,
        purpose: PropertyPurpose,
    ) -> Result<PropertiesFor, Thrown> {
        let handlers = self.object_handlers(object.heap());
        handlers.get_properties_for(self, object, purpose)
    }

    /// `zend_std_get_properties_for` (`Z/Zend/zend_object_handlers.c:2608`),
    /// `Std`'s `get_properties_for`: under [`PropertyPurpose::Debug`] the
    /// class's `get_debug_info`, and where that shows the members,
    /// `zend_std_get_debug_info`'s table, a lazy object's read as it
    /// stands (`zend_lazy_object_debug_info`); the hooked properties'
    /// table for the readers that call hooks; the table as it stands for
    /// the `(array)` cast and for a serialization a lazy object was made
    /// to skip initialising for; and `get_properties`' table otherwise.
    ///
    /// ```php
    /// class A { public $a = 1; protected $b = 2; private $c = 3; }
    /// print_r(new A);
    /// echo "\n";
    /// class B { public $hidden = 1; function __debugInfo() { return ['shown' => 2]; } }
    /// var_dump(new B);
    /// ```
    /// ```expect
    /// A Object
    /// (
    ///     [a] => 1
    ///     [b:protected] => 2
    ///     [c:A:private] => 3
    /// )
    ///
    /// object(B)#1 (1) {
    ///   ["shown"]=>
    ///   int(2)
    /// }
    /// ```
    pub(crate) fn std_properties_for(
        &mut self,
        object: PhpObject,
        purpose: PropertyPurpose,
    ) -> Result<PropertiesFor, Thrown> {
        let handlers = self.object_handlers(object.heap());
        match purpose {
            PropertyPurpose::Debug => match handlers.get_debug_info(self, object)? {
                Some(info) => Ok(PropertiesFor::Debug(info)),
                None => self
                    .properties_no_lazy_init(object)
                    .map(PropertiesFor::Table),
            },
            PropertyPurpose::Json | PropertyPurpose::GetObjectVars | PropertyPurpose::VarExport => {
                if let Some(hooked) = self.hooked_properties_for(object, purpose)? {
                    return Ok(hooked);
                }
                handlers
                    .get_properties(self, object)
                    .map(PropertiesFor::Table)
            }
            PropertyPurpose::ArrayCast => self
                .properties_no_lazy_init(object)
                .map(PropertiesFor::Table),
            PropertyPurpose::Serialize if self.lazy_skips_serialize(object.heap()) => self
                .properties_no_lazy_init(object)
                .map(PropertiesFor::Table),
            PropertyPurpose::Serialize => handlers
                .get_properties(self, object)
                .map(PropertiesFor::Table),
        }
    }

    /// `owner`'s table `table`, laid down, in php's positions, each
    /// declared slot named by the key [`Request::slot_names`] interns
    /// for it, the name `zend_mangle_property_name` makes, so a reader
    /// unmangles it as php's `print_hash` does and never asks the engine
    /// for a visibility beside it. The `Err` is the memory limit refusing
    /// a name.
    ///
    /// ```php
    /// class A { public $a = 1; protected $b = 2; private $c = 3; }
    /// class B extends A { private $c = 4; }
    /// print_r(new B);
    /// ```
    /// ```expect
    /// B Object
    /// (
    ///     [a] => 1
    ///     [b:protected] => 2
    ///     [c:A:private] => 3
    ///     [c:B:private] => 4
    /// )
    /// ```
    pub(crate) fn object_table(
        &mut self,
        owner: PhpObject,
        table: PropertyTable,
    ) -> Result<ObjectTable, Thrown> {
        let names = if table.declared() > 0 {
            self.slot_names(owner.class())?
        } else {
            Vec::new()
        };
        if names.len() < usize::try_from(table.declared()).unwrap_or(usize::MAX) {
            return self.fatal_of(Err(php_heap::Fatal::error(
                "InternalError: a declared slot of an object's table with no name",
            )));
        }
        Ok(ObjectTable::of(owner, table, names.into_boxed_slice()))
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
                    return self.fatal_of(Err(php_heap::Fatal::error(
                        "InternalError: a declared slot of an object's table with no name",
                    )));
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
        table: Owner<PhpArray>,
    ) -> Result<(), Thrown> {
        let object = match self.table_owner(object) {
            Ok(object) => object,
            Err(thrown) => {
                table.give_up(self);
                return Err(thrown);
            }
        };
        let o = object.heap();
        let mut table = table.hand_on();
        if crate::api::values::walk::rows(self, table).any(|(key, _)| matches!(key, Key::Int(_))) {
            let mut spelt = self.array()?;
            // `zend_symtable_to_proptable`'s copy, which no code of the
            // program reaches: the rows read under the borrowed request,
            // then written under their spelt keys.
            let rows: Vec<(Key, Value)> = crate::api::values::walk::rows(self, table).collect();
            for (key, value) in rows {
                let key = match key {
                    Key::Int(n) => {
                        PhpString::new(self, crate::c_number::Long::of(n).as_bytes())?.into_key()
                    }
                    Key::Str(_) => Owner::take(self, key),
                };
                spelt.set(self, key, Owner::take(self, value))?;
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
    /// the name itself for a public one, the string the declaration
    /// holds ([`Request::kept_string`]), views.
    pub(crate) fn slot_names(&mut self, class: ClassId) -> Result<Vec<php_heap::Str>, Thrown> {
        let slots = self.class_slots(class);
        let mut names = Vec::with_capacity(self.placed_properties(class).len());
        for slot in 0..slots {
            if let Some(name) = self.slot_name(class, slot)? {
                names.push(name);
            }
        }
        Ok(names)
    }

    /// [`Request::slot_names`]'s key for slot `slot` alone; `None` past
    /// the last slot.
    pub(crate) fn slot_name(
        &mut self,
        class: ClassId,
        slot: u32,
    ) -> Result<Option<php_heap::Str>, Thrown> {
        let Some((declaring, property)) = self.placed_property(class, slot) else {
            return Ok(None);
        };
        let prefix = self
            .property_prefix(declaring, property.visibility)
            .unwrap_or_default();
        let name = self.kept_string(&[prefix.as_slice(), &property.name.0].concat())?;
        Ok(Some(name.heap()))
    }

    /// `zend_mangle_property_name`'s prefix for a property `declaring`
    /// declares under `visibility`: none for a public one, `\0*\0` for a
    /// protected one, and for a private one `\0Class\0`, the class that
    /// declared it (`property_info->ce->name`), a class above for an
    /// inherited one; none where that class has no declaration.
    pub(crate) fn property_prefix(
        &self,
        declaring: ClassId,
        visibility: Visibility,
    ) -> Option<Vec<u8>> {
        match visibility {
            Visibility::Private => {
                php_compile::mangling(visibility, &self.class_decl(declaring)?.name.0)
            }
            Visibility::Public | Visibility::Protected => php_compile::mangling(visibility, &[]),
        }
    }
}
