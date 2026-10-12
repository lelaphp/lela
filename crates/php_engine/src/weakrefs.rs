//! `WeakReference` and `WeakMap` (`Z/Zend/zend_weakrefs.c` at the tag),
//! the two handler adapters design §11 names over the object store's
//! release path (design §4.3 seam 2, §5.4). A weak holder names an
//! object without owning it: the object carries
//! `IS_OBJ_WEAKLY_REFERENCED`, the request keeps php's `EG(weakrefs)`,
//! the object's id against every holder that names it, and the heap's
//! free of such an object tells the request first
//! ([`php_heap::HeapHooks::weakrefs_notify`]), where php's
//! `zend_object_std_dtor` calls `zend_weakrefs_notify`: after the
//! destructor ran and before the members are released, every
//! `WeakReference` naming it is cleared and every `WeakMap` entry keyed
//! by it removed, the entry's value released on the spot.
//!
//! A `WeakMap`'s entries are an array of the request's keyed by the
//! key object's id, php's `zend_object_to_weakref_key`, which an object
//! keeps for as long as it lives and which its free takes out of every
//! map before the id can be reused; the order is the array's, php's
//! insertion order.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use php_heap::Obj;
use php_macros::php_class;

use crate::api::abi::{
    ClassIterator, CloneObj, DebugInfo, DimensionRead, GcTable, Instance, IteratorElement,
    ObjectHandlers, Refusals, This, Untyped,
};
use crate::api::raising::{Thrown, throw_error};
use crate::api::request::Request;
use crate::api::values::{
    HashIterator, Key, Owner, PhpArray, PhpObject, PhpString, Place, Value, walk,
};
use crate::classes::ArrayAccess;
use crate::interfaces::{Countable, Iterator, IteratorAggregate};

/// One holder `EG(weakrefs)` lists against an object: php's tagged
/// pointer, `ZEND_WEAKREF_TAG_REF` or `ZEND_WEAKREF_TAG_MAP`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Holder {
    Reference(u32),
    Map(u32),
}

/// `EG(weakrefs)`: every weakly referenced object's id against the
/// holders that name it, in the order they were registered.
#[derive(Default, Debug)]
pub(crate) struct Table {
    rows: Vec<(u32, Vec<Holder>)>,
}

impl Table {
    fn row(&mut self, id: u32) -> Option<&mut Vec<Holder>> {
        self.rows
            .iter_mut()
            .find(|(at, _)| *at == id)
            .map(|(_, holders)| holders)
    }

    fn take(&mut self, id: u32) -> Vec<Holder> {
        let Some(at) = self.rows.iter().position(|(held, _)| *held == id) else {
            return Vec::new();
        };
        self.rows.swap_remove(at).1
    }
}

/// A `WeakReference`'s payload, `zend_weakref`: the object it names,
/// none once that object was freed.
#[derive(Default, Debug)]
pub(crate) struct WeakRef {
    referent: Option<PhpObject>,
}

/// A `WeakMap`'s payload, `zend_weakmap`: its entries, each value keyed
/// by the key object's id, laid down by the first write.
#[derive(Default, Debug)]
pub(crate) struct ZendWeakMap {
    entries: Option<PhpArray>,
}

/// `zend_weakref_register` (`Z/Zend/zend_weakrefs.c:99`): the object
/// flagged and `holder` listed against it.
fn register(req: &mut Request, object: PhpObject, holder: Holder) {
    object.heap().set_weakly_referenced(true);
    let id = object.index();
    let table = &mut req.weakrefs;
    if let Some(holders) = table.row(id) {
        holders.push(holder);
    } else {
        table.rows.push((id, vec![holder]));
    }
}

/// `zend_weakref_unregister` (`Z/Zend/zend_weakrefs.c:126`): `holder`
/// taken off the object's list, `IS_OBJ_WEAKLY_REFERENCED` and the row dropped with the
/// last one, and then, where `unref`, what the holder had of the object
/// given up, which may free it and so comes last.
fn unregister(req: &mut Request, object: PhpObject, holder: Holder, unref: bool) {
    let id = object.index();
    if let Some(holders) = req.weakrefs.row(id) {
        holders.retain(|held| *held != holder);
        if holders.is_empty() {
            drop(req.weakrefs.take(id));
            object.heap().set_weakly_referenced(false);
        }
    }
    if unref {
        unref_single(req, object.index(), holder);
    }
}

/// `zend_weakref_unref_single` (`Z/Zend/zend_weakrefs.c:77`): a
/// reference cleared, or the map's entry for the object removed and its
/// value released.
fn unref_single(req: &mut Request, id: u32, holder: Holder) {
    match holder {
        Holder::Reference(at) => {
            if let Some(reference) = stored(req, at)
                && let Some(payload) = reference.native_mut::<WeakRef>(req)
            {
                payload.referent = None;
            }
        }
        Holder::Map(at) => {
            let entries = stored(req, at)
                .and_then(|map| map.native::<ZendWeakMap>(req).and_then(|m| m.entries));
            if let Some(mut entries) = entries {
                entries.unset(req, &Key::Int(i64::from(id))).ok();
            }
        }
    }
}

/// The live object the store holds under `id`, the holder a row names.
fn stored(req: &Request, id: u32) -> Option<PhpObject> {
    req.stored_object(id)
}

/// `zend_weakrefs_notify` (`Z/Zend/zend_weakrefs.c:212`), which the
/// heap's free of a flagged object calls before it releases the
/// members: the row taken, and every holder in it cleared.
pub(crate) fn notify(req: &mut Request, object: Obj) {
    let id = object.id().0;
    for holder in req.weakrefs.take(id) {
        unref_single(req, id, holder);
    }
}

/// A `WeakMap`'s entries for the cycle collector
/// (`zend_weakmap_get_key_entry_gc`, `Z/Zend/zend_weakrefs.c:509`):
/// each key object beside its entry in place, in the entries' order;
/// `false` for an object whose table is not `WeakMap`'s, which php
/// tells by `get_gc == zend_weakmap_get_gc`.
pub(crate) fn gc_entries(
    req: &Request,
    map: Obj,
    visit: &mut dyn FnMut(Obj, php_heap::ZvalRef),
) -> bool {
    let table = req.object_handlers(map);
    if !std::ptr::addr_eq(table, &raw const WEAK_MAP) {
        return false;
    }
    let Some(entries) = entries_of(req, PhpObject::of(map)) else {
        return true;
    };
    let entries = entries.heap();
    for (key, _) in &entries {
        if let php_heap::Key::Long(id) = key
            && let Some(owner) = u32::try_from(id).ok().and_then(|id| stored(req, id))
            && let Some(entry) = entries.value_ref(key)
        {
            visit(owner.heap(), entry);
        }
    }
    true
}

/// The `WeakMap` entries keyed by `key` for the cycle collector
/// (`zend_weakmap_get_object_key_entry_gc`, `Z/Zend/zend_weakrefs.c:536`):
/// every map `EG(weakrefs)` lists against the object, in the order they
/// were registered, and its entry for the object in place; a
/// `WeakReference` names none.
pub(crate) fn gc_keyed(req: &Request, key: Obj, visit: &mut dyn FnMut(php_heap::ZvalRef, Obj)) {
    let id = key.id().0;
    let Some((_, holders)) = req.weakrefs.rows.iter().find(|(at, _)| *at == id) else {
        return;
    };
    for holder in holders {
        if let Holder::Map(at) = *holder
            && let Some(map) = stored(req, at)
            && let Some(entries) = entries_of(req, map)
            && let Some(entry) = entries.heap().value_ref(php_heap::Key::Long(i64::from(id)))
        {
            visit(entry, map.heap());
        }
    }
}

/// The `WeakReference` handler table: `free_obj` unregisters, the
/// debug table is the one row php shows, and the class is uncloneable.
struct WeakReferenceHandlers;

impl ObjectHandlers for WeakReferenceHandlers {
    /// `zend_weakref_free`: the referent, where it still lives, no
    /// longer listed against this reference.
    fn free_obj(&self, req: &mut Request, object: PhpObject) {
        let referent = object
            .native_mut::<WeakRef>(req)
            .and_then(|payload| payload.referent.take());
        if let Some(referent) = referent {
            unregister(req, referent, Holder::Reference(object.index()), false);
        }
    }

    /// `zend_weakref_handlers.clone_obj = NULL`.
    fn refusals(&self) -> Refusals {
        Refusals::NONE.cloning()
    }

    /// `zend_weakref_get_debug_info` (`Z/Zend/zend_weakrefs.c:283`):
    /// `object`, the referent or `null` once it is gone.
    ///
    /// ```php
    /// $s = new stdClass;
    /// $s->hello = 'world';
    /// $weak = WeakReference::create($s);
    /// var_dump($weak);
    /// unset($s);
    /// var_dump($weak);
    /// ```
    /// ```expect
    /// object(WeakReference)#2 (1) {
    ///   ["object"]=>
    ///   object(stdClass)#1 (1) {
    ///     ["hello"]=>
    ///     string(5) "world"
    ///   }
    /// }
    /// object(WeakReference)#2 (1) {
    ///   ["object"]=>
    ///   NULL
    /// }
    /// ```
    fn get_debug_info(
        &self,
        req: &mut Request,
        object: PhpObject,
    ) -> Result<Option<DebugInfo>, Thrown> {
        let referent = object.native::<WeakRef>(req).and_then(|p| p.referent);
        let value = referent.map_or(Value::null(), |referent| referent.as_value());
        let table = PhpArray::filled(req, |req, table| {
            let key = PhpString::new(req, b"object")?.into_key();
            table.set(req, key, Owner::take(req, value))
        })?;
        Ok(Some(table.into()))
    }
}

static WEAK_REFERENCE: WeakReferenceHandlers = WeakReferenceHandlers;

/// The `WeakMap` handler table, `zend_weakmap_handlers`: the dimension
/// slots over the entries, `count_elements`, the debug table of
/// `key`/`value` pairs, `get_gc` over the values, the clone that
/// registers the copy, and `free_obj`, which takes the map off every
/// key's list before the entries go.
struct WeakMapHandlers;

/// The key a `WeakMap` dimension names, `ZVAL_DEREF(offset)` and php's
/// `TypeError` for one that is no object.
fn map_key(req: &mut Request, offset: &Value) -> Result<PhpObject, Thrown> {
    match offset.deref().as_object().copied() {
        Some(object) => Ok(object),
        None => Err(throw_error(
            req,
            "TypeError",
            "WeakMap key must be an object",
        )),
    }
}

/// The object ids a map's `entries` are keyed by, in order, read in one
/// pass before any is registered or unlisted: [`register`] and
/// [`unregister`] without `unref` write the request's list of weak
/// holders and no row of the table, so these are the ids php's
/// `ZEND_HASH_MAP_FOREACH_NUM_KEY` reads as it goes.
fn keyed_ids(req: &Request, entries: PhpArray) -> Vec<u32> {
    walk::rows(req, entries)
        .filter_map(|(key, _)| match key {
            Key::Int(id) => u32::try_from(id).ok(),
            Key::Str(_) => None,
        })
        .collect()
}

fn entries_of(req: &Request, map: PhpObject) -> Option<PhpArray> {
    map.native::<ZendWeakMap>(req)
        .and_then(|payload| payload.entries)
}

impl ObjectHandlers for WeakMapHandlers {
    /// `zend_weakmap_iterator`: the iterator and its `ht_iter`.
    fn iterator_size(&self) -> usize {
        96
    }

    /// `zend_weakmap_get_properties_for`, php's slot, which the debug
    /// info serves here.
    fn properties_overloaded(&self) -> bool {
        true
    }

    /// `zend_weakmap_free_obj` (`Z/Zend/zend_weakrefs.c:338`): every key
    /// unlisted without its entry being touched
    /// (`zend_weakrefs_hash_clean_ex`), then the entries released in
    /// order.
    fn free_obj(&self, req: &mut Request, object: PhpObject) {
        let entries = object
            .native_mut::<ZendWeakMap>(req)
            .and_then(|payload| payload.entries.take());
        let Some(entries) = entries else {
            return;
        };
        for id in keyed_ids(req, entries) {
            if let Some(owner) = stored(req, id) {
                unregister(req, owner, Holder::Map(object.index()), false);
            }
        }
        req.release(Value::array(entries));
    }

    /// `zend_weakmap_clone_obj` (`Z/Zend/zend_weakrefs.c:614`): a map of
    /// the same entries, each value one more owner, registered against
    /// every key.
    fn clone_obj(&self) -> Option<CloneObj> {
        Some(weakmap_clone)
    }

    /// `zend_weakmap_read_dimension` (`Z/Zend/zend_weakrefs.c:346`):
    /// php's `Error` for `$map[]`, its `TypeError` for a key that is no
    /// object, the entry's value with one more owner, and `Object C#n
    /// not contained in WeakMap` for a key it has not, which under
    /// `BP_VAR_IS` is `null` in silence.
    ///
    /// ```php
    /// $m = new WeakMap; $k = new stdClass; $m[$k] = ['a' => 1];
    /// var_dump(isset($m[$k]['a']), $m[new stdClass] ?? 'none', isset($m[new stdClass]['a']));
    /// ```
    /// ```expect
    /// bool(true)
    /// string(4) "none"
    /// bool(false)
    /// ```
    fn read_dimension(
        &self,
        req: &mut Request,
        object: PhpObject,
        offset: Option<&Value>,
        read: DimensionRead,
    ) -> Result<Owner<Value>, Thrown> {
        let Some(offset) = offset else {
            return Err(throw_error(req, "Error", "Cannot append to WeakMap"));
        };
        let key = map_key(req, offset)?;
        let found = entries_of(req, object).and_then(|e| e.get(&Key::Int(i64::from(key.index()))));
        let Some(value) = found else {
            if read == DimensionRead::Silent {
                return Ok(Owner::null());
            }
            let class = req.class_name(key.class());
            return Err(throw_error(
                req,
                "Error",
                &format!("Object {class}#{} not contained in WeakMap", key.index()),
            ));
        };
        Ok(Owner::take(req, value.deref()))
    }

    /// `zend_weakmap_read_dimension` under `BP_VAR_W` and `BP_VAR_RW`
    /// (`Z/Zend/zend_weakrefs.c:370`): the entry's own slot, so
    /// `$map[$k][] = 1` writes into the entry and `$map[$k] =& $v` binds
    /// it; php's refusals as the read raises them. php makes the slot a
    /// reference (`ZVAL_MAKE_REF`) and the fetch unwraps it again where the
    /// map is its one owner (`ZVAL_UNREF`), which leaves the slot as it
    /// was.
    ///
    /// ```php
    /// $obj = new stdClass;
    /// $map = new WeakMap;
    /// $integer = 1;
    /// $map[$obj] = [];
    /// $map[$obj][] = 42;
    /// var_dump($map[$obj]);
    /// $map[$obj] =& $integer;
    /// $integer++;
    /// var_dump($map[$obj], $map->offsetGet($obj));
    /// ```
    /// ```expect
    /// array(1) {
    ///   [0]=>
    ///   int(42)
    /// }
    /// int(2)
    /// int(2)
    /// ```
    fn dimension_place(
        &self,
        req: &mut Request,
        object: PhpObject,
        offset: Option<&Value>,
        _fetch: DimensionRead,
    ) -> Result<Option<Place>, Thrown> {
        weakmap_place(req, object, offset).map(|at| Some(Place::of(at)))
    }

    /// `zend_weakmap_write_dimension` (`Z/Zend/zend_weakrefs.c:378`): an
    /// entry the map already has takes the value in place, the old one
    /// released after the write; a new key is registered against the map
    /// and appended.
    fn write_dimension(
        &self,
        req: &mut Request,
        object: PhpObject,
        offset: Option<&Value>,
        value: Owner<Value>,
    ) -> Result<(), Thrown> {
        let Some(offset) = offset else {
            value.give_up(req);
            return Err(throw_error(req, "Error", "Cannot append to WeakMap"));
        };
        let key = match map_key(req, offset) {
            Ok(key) => key,
            Err(thrown) => {
                value.give_up(req);
                return Err(thrown);
            }
        };
        let mut entries = if let Some(entries) = entries_of(req, object) {
            entries
        } else {
            let made = PhpArray::new(req)?.hand_on();
            if let Some(payload) = object.native_mut::<ZendWeakMap>(req) {
                payload.entries = Some(made);
            }
            made
        };
        let at = Key::Int(i64::from(key.index()));
        if !entries.contains(&at) {
            register(req, key, Holder::Map(object.index()));
        }
        entries.set(req, Owner::take(req, at), value)
    }

    /// `zend_weakmap_has_dimension` (`Z/Zend/zend_weakrefs.c:409`): the
    /// entry there and not `null`, or under `empty()` there and true.
    fn has_dimension(
        &self,
        req: &mut Request,
        object: PhpObject,
        offset: &Value,
        not_empty: bool,
    ) -> Result<bool, Thrown> {
        let key = map_key(req, offset)?;
        let Some(value) =
            entries_of(req, object).and_then(|e| e.get(&Key::Int(i64::from(key.index()))))
        else {
            return Ok(false);
        };
        if not_empty {
            return crate::ops::is_true(req, &value.deref());
        }
        Ok(value.deref().tag() != crate::api::values::TypeTag::Null)
    }

    /// `zend_weakmap_unset_dimension` (`Z/Zend/zend_weakrefs.c:429`): a
    /// key the map has unlisted and its entry removed; any other, the
    /// `TypeError` aside, left as it is.
    fn unset_dimension(
        &self,
        req: &mut Request,
        object: PhpObject,
        offset: &Value,
    ) -> Result<(), Thrown> {
        let key = map_key(req, offset)?;
        let held = entries_of(req, object)
            .is_some_and(|entries| entries.contains(&Key::Int(i64::from(key.index()))));
        if held {
            unregister(req, key, Holder::Map(object.index()), true);
        }
        Ok(())
    }

    /// `zend_weakmap_get_iterator` (`Z/Zend/zend_weakrefs.c:709`): a
    /// hash iterator over the entries, which a removal during the walk
    /// moves on as it moves `foreach` over an array.
    fn get_iterator(
        &self,
        req: &mut Request,
        object: PhpObject,
        by_ref: bool,
    ) -> Result<Option<Box<dyn ClassIterator>>, Thrown> {
        let _ = (req, object);
        Ok(Some(Box::new(MapIter { at: None, by_ref })))
    }

    /// The entry `index` steps into the walk, which is what an
    /// `InternalIterator` over the map reads: the object keying it and
    /// its value, one owner each the caller's.
    fn iterator_element(
        &self,
        req: &mut Request,
        object: PhpObject,
        index: usize,
    ) -> Result<Option<IteratorElement>, Thrown> {
        let entry = entries_of(req, object).and_then(|entries| {
            crate::api::values::walk::rows(req, entries)
                .filter_map(|(key, value)| match key {
                    Key::Int(id) => u32::try_from(id)
                        .ok()
                        .and_then(|id| stored(req, id))
                        .map(|o| (o, value)),
                    Key::Str(_) => None,
                })
                .nth(index)
        });
        Ok(entry.map(|(key, value)| {
            (
                Owner::take(req, key.as_value()),
                Owner::take(req, value.deref()),
            )
        }))
    }

    /// `zend_weakmap_count_elements`: the entries.
    fn count_elements(&self, req: &mut Request, object: PhpObject) -> Result<Option<i64>, Thrown> {
        let count = entries_of(req, object).map_or(0, |entries| entries.len());
        Ok(Some(i64::try_from(count).unwrap_or(i64::MAX)))
    }

    /// `zend_weakmap_get_properties_for` under `ZEND_PROP_PURPOSE_DEBUG`
    /// (`Z/Zend/zend_weakrefs.c:458`): one `['key' => $object, 'value'
    /// => $value]` pair per entry, in order.
    fn get_debug_info(
        &self,
        req: &mut Request,
        object: PhpObject,
    ) -> Result<Option<DebugInfo>, Thrown> {
        // The map's own table, read under the borrowed request: building
        // the pairs runs no code of the program.
        let entries: Vec<(Key, Value)> = match entries_of(req, object) {
            Some(entries) => crate::api::values::walk::rows(req, entries).collect(),
            None => Vec::new(),
        };
        let table = PhpArray::filled(req, |req, table| {
            for (key, value) in entries {
                let Key::Int(id) = key else { continue };
                let Some(owner) = u32::try_from(id).ok().and_then(|id| stored(req, id)) else {
                    continue;
                };
                let pair = PhpArray::filled(req, |req, pair| {
                    let at = PhpString::new(req, b"key")?.into_key();
                    pair.set(req, at, Owner::take(req, owner.as_value()))?;
                    let at = PhpString::new(req, b"value")?.into_key();
                    pair.set(req, at, Owner::take(req, value))
                })?;
                table.append(req, pair.into_value())?;
            }
            Ok(())
        })?;
        Ok(Some(table.into()))
    }

    /// `zend_weakmap_get_gc`: the values, which the map owns; the keys it
    /// does not. The cycle collector reads the map through
    /// [`gc_entries`] as well, since an entry is kept alive by its key:
    /// an entry whose value holds its key is garbage once nothing else
    /// holds the key, and the collection frees both.
    ///
    /// ```php
    /// class Value { public function __construct(public string $name) {} }
    /// $map = new WeakMap();
    /// $obj = new Value('a');
    /// $map[$obj] = [$obj];
    /// var_dump(gc_collect_cycles(), count($map));
    /// $obj = null;
    /// var_dump(gc_collect_cycles(), count($map));
    /// ```
    /// ```expect
    /// int(0)
    /// int(1)
    /// int(2)
    /// int(0)
    /// ```
    fn get_gc(
        &self,
        req: &mut Request,
        object: PhpObject,
        visit: &mut dyn FnMut(Value),
    ) -> GcTable {
        let Some(entries) = entries_of(req, object) else {
            return GcTable::Properties;
        };
        for (_, value) in walk::rows(req, entries) {
            visit(value);
        }
        GcTable::Properties
    }
}

static WEAK_MAP: WeakMapHandlers = WeakMapHandlers;

/// `zend_weakmap_iterator`: the entries' hash iterator, laid on the
/// table at the first step that finds one, and whether the walk hands
/// the value out by reference.
struct MapIter {
    at: Option<HashIterator>,
    by_ref: bool,
}

impl MapIter {
    /// The entry the iterator stands at or past, with its bucket:
    /// `zend_hash_get_current_data_ex` over `iter->ht_iter`.
    fn here(&mut self, req: &mut Request, map: PhpObject) -> Option<(u32, Key, Value)> {
        let entries = entries_of(req, map)?;
        let at = *self
            .at
            .get_or_insert_with(|| HashIterator::new(req, &entries, 0));
        let pos = at.position(req, &entries);
        entries.live_from(pos)
    }
}

impl ClassIterator for MapIter {
    /// `zend_weakmap_iterator_rewind`.
    fn rewind(&mut self, req: &mut Request, _map: PhpObject) -> Result<(), Thrown> {
        if let Some(at) = self.at {
            at.set_position(req, 0);
        }
        Ok(())
    }

    fn valid(&mut self, req: &mut Request, map: PhpObject) -> Result<bool, Thrown> {
        Ok(self.here(req, map).is_some())
    }

    /// `zend_weakmap_iterator_get_current_data`, made a reference in
    /// place under a walk by reference, as `FE_FETCH_RW` makes the zval
    /// it points at one.
    fn current(&mut self, req: &mut Request, map: PhpObject) -> Result<Owner<Value>, Thrown> {
        let Some((_, key, value)) = self.here(req, map) else {
            return Ok(Owner::null());
        };
        if !self.by_ref || value.as_php_ref().is_some() {
            return Ok(Owner::take(req, value));
        }
        let Some(mut entries) = entries_of(req, map) else {
            return Ok(Owner::null());
        };
        req.addref(&value);
        // The cell's one owner is the caller's, and the entry takes a
        // second.
        let cell = req.reference(value)?;
        let cell = Owner::adopt(cell.as_value());
        let key = Owner::take(req, key);
        if let Err(thrown) = entries.set(req, key, Owner::take(req, *cell)) {
            cell.give_up(req);
            return Err(thrown);
        }
        Ok(cell)
    }

    /// `zend_weakmap_iterator_get_current_key`: the object the entry is
    /// keyed by, `null` past the last.
    fn key(&mut self, req: &mut Request, map: PhpObject) -> Result<Owner<Value>, Thrown> {
        let object = match self.here(req, map) {
            Some((_, Key::Int(id), _)) => u32::try_from(id).ok().and_then(|id| stored(req, id)),
            _ => None,
        };
        let key = object.map_or_else(Value::null, |object| object.as_value());
        Ok(Owner::take(req, key))
    }

    /// `zend_weakmap_iterator_move_forward`: past the entry it stands at.
    fn move_forward(&mut self, req: &mut Request, map: PhpObject) -> Result<(), Thrown> {
        if let (Some((bucket, _, _)), Some(at)) = (self.here(req, map), self.at) {
            at.set_position(req, bucket.saturating_add(1));
        }
        Ok(())
    }

    /// `zend_weakmap_iterator_dtor`: the hash iterator given back.
    fn release(&mut self, req: &mut Request, map: PhpObject) {
        let _ = map;
        if let Some(at) = self.at.take() {
            at.release(req);
        }
    }
}

fn weakmap_place(
    req: &mut Request,
    object: PhpObject,
    offset: Option<&Value>,
) -> Result<php_heap::ZvalRef, Thrown> {
    let Some(offset) = offset else {
        return Err(throw_error(req, "Error", "Cannot append to WeakMap"));
    };
    let key = map_key(req, offset)?;
    let at = Key::Int(i64::from(key.index()));
    let entries = entries_of(req, object);
    let held = entries.and_then(|entries| entries.get(&at).map(|value| (entries, value)));
    let Some((entries, value)) = held else {
        let class = req.class_name(key.class());
        return Err(throw_error(
            req,
            "Error",
            &format!("Object {class}#{} not contained in WeakMap", key.index()),
        ));
    };
    let _ = value;
    match entries.heap().value_ref(at.heap()) {
        Some(place) => Ok(place),
        None => Err(throw_error(req, "Error", "Cannot append to WeakMap")),
    }
}

/// `zend_weakmap_clone_obj`: the new map's entries a copy, every value
/// one more owner, and the copy registered against every key.
fn weakmap_clone(req: &mut Request, object: PhpObject) -> Result<Owner<PhpObject>, Thrown> {
    let copy = match entries_of(req, object) {
        Some(entries) => Some(entries.copy(req)?.hand_on()),
        None => None,
    };
    let made = req.clone_with_payload(object, ZendWeakMap { entries: copy })?;
    for id in copy.map(|copy| keyed_ids(req, copy)).unwrap_or_default() {
        if let Some(owner) = stored(req, id) {
            register(req, owner, Holder::Map(made.index()));
        }
    }
    Ok(Owner::adopt(made))
}

/// `final class WeakReference`, `@strict-properties` and
/// `@not-serializable`, with the methods the stub gives, over the
/// handler table and the [`WeakRef`] payload `zend_register_weakref_ce`
/// sets.
///
/// ```php
/// $o = new stdClass;
/// $r = WeakReference::create($o);
/// var_dump($r->get() === $o, WeakReference::create($o) === $r);
/// unset($o);
/// var_dump($r->get());
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// NULL
/// ```
#[php_class(
    final,
    not_serializable,
    dynamic_properties = forbidden,
    size = 64, payload = WeakRef,
    handlers = WEAK_REFERENCE
)]
pub(crate) struct WeakReference;

#[php_class]
impl WeakReference {
    /// `WeakReference::__construct()` (`Z/Zend/zend_weakrefs.c:301`):
    /// `new WeakReference` is refused, the one way to make one being
    /// `create`.
    ///
    /// ```php
    /// new WeakReference;
    /// ```
    /// ```expect exit=255
    /// Fatal error: Uncaught Error: Direct instantiation of WeakReference is not allowed, use WeakReference::create instead in %s:2
    /// Stack trace:
    /// #0 %s(2): WeakReference->__construct()
    /// #1 {main}
    ///   thrown in %s on line 2
    /// ```
    #[php(arity = manual)]
    fn __construct(req: &mut Request, this: This<Self>) -> Result<(), Thrown> {
        let _ = this;
        Err(throw_error(
            req,
            "Error",
            "Direct instantiation of WeakReference is not allowed, use WeakReference::create instead",
        ))
    }

    /// `WeakReference::create(object $object): WeakReference`
    /// (`Z/Zend/zend_weakrefs.c:306`): the one reference the object has
    /// already, `zend_weakref_find`, so two creates hand back one object;
    /// otherwise a new one naming it, registered. `get()` answers the
    /// object while it lives and `null` from the moment it is freed, and
    /// the reference takes no owner of it.
    ///
    /// ```php
    /// $std = new stdClass;
    /// $wr = WeakReference::create($std);
    /// $wr2 = WeakReference::create($std);
    /// var_dump($wr === $wr2, $wr->get() === $std);
    /// unset($std);
    /// var_dump($wr->get(), $wr2->get());
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(true)
    /// NULL
    /// NULL
    /// ```
    ///
    /// The reference is cleared when the object is freed, after its
    /// destructor ran and before its properties are released, so the
    /// destructor still reaches it through the reference and a property's
    /// own destructor no longer does.
    ///
    /// ```php
    /// class D { public $wr; public $p; function __destruct() { echo "dtor: ", var_export($this->wr->get() === $this, true), "\n"; } }
    /// class P { public $wr; function __destruct() { echo "prop: ", var_export($this->wr->get(), true), "\n"; } }
    /// $d = new D; $d->wr = WeakReference::create($d);
    /// $d->p = new P; $d->p->wr = $d->wr;
    /// $d = null;
    /// echo "end\n";
    /// ```
    /// ```expect
    /// dtor: true
    /// prop: NULL
    /// end
    /// ```
    ///
    /// ```php
    /// WeakReference::create(1);
    /// ```
    /// ```expect exit=255
    /// Fatal error: Uncaught TypeError: WeakReference::create(): Argument #1 ($object) must be of type object, int given in %s:2
    /// Stack trace:
    /// #0 %s(2): WeakReference::create(1)
    /// #1 {main}
    ///   thrown in %s on line 2
    /// ```
    fn create(
        req: &mut Request,
        object: PhpObject,
    ) -> Result<Owner<Instance<WeakReference>>, Thrown> {
        let referent = object;
        let existing = req.weakrefs.row(referent.index()).and_then(|holders| {
            holders.iter().find_map(|holder| match holder {
                Holder::Reference(at) => Some(*at),
                Holder::Map(_) => None,
            })
        });
        if let Some(at) = existing
            && let Some(reference) = stored(req, at)
        {
            return Ok(Owner::take(req, reference).into_instance());
        }
        let Some(class) = req.class_named(b"WeakReference") else {
            return Err(throw_error(
                req,
                "Error",
                "Class \"WeakReference\" not found",
            ));
        };
        let reference = req.new_object_with(
            class,
            WeakRef {
                referent: Some(referent),
            },
        )?;
        register(req, referent, Holder::Reference(reference.index()));
        Ok(Owner::adopt(reference).into_instance())
    }

    /// `WeakReference::get(): ?object`: the referent with one more owner,
    /// or `null` once it is gone.
    fn get(req: &mut Request, this: This<Self>) -> Option<PhpObject> {
        let referent = this
            .object()
            .native::<WeakRef>(req)
            .and_then(|payload| payload.referent)?;
        Some(referent)
    }
}

/// `final class WeakMap implements ArrayAccess, Countable, IteratorAggregate`,
/// `@strict-properties` and `@not-serializable`, over the handler table
/// and the [`ZendWeakMap`] payload `zend_register_weakref_ce` sets. The
/// entries are walked by its own iterator. Its methods are its dimension
/// handlers and `count_elements`, served by name:
///
/// ```php
/// $map = new WeakMap;
/// $o = new stdClass;
/// $map[$o] = 1;
/// $map->offsetSet($o, 2);
/// var_dump($map[$o], $map->offsetGet($o), isset($map[$o]), $map->offsetExists($o), count($map), $map->count());
/// $map[$o] = null;
/// var_dump(isset($map[$o]), $map->offsetExists($o));
/// $map->offsetUnset($o);
/// var_dump(count($map));
/// try { $map[$o]; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// try { $map[1] = 1; } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { $map[] = 1; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(2)
/// int(2)
/// bool(true)
/// bool(true)
/// int(1)
/// int(1)
/// bool(false)
/// bool(false)
/// int(0)
/// Object stdClass#2 not contained in WeakMap
/// WeakMap key must be an object
/// Cannot append to WeakMap
/// ```
///
/// `zend_weakmap_create_object` allocates `sizeof(zend_weakmap)`, and
/// `zend_weakref_new` its `zend_weakref`, which `memory_get_usage()`
/// counts (design §5.13):
///
/// ```php
/// $o = new stdClass;
/// foreach ([false, true] as $shown) foreach (['WeakMap' => fn() => new WeakMap, 'WeakReference' => fn() => WeakReference::create($o)] as $c => $make) {
///     $m = memory_get_usage();
///     $w = $make();
///     $used = memory_get_usage() - $m;
///     $w = null;
///     if ($shown) echo $c, ' ', $used, "\n";
/// }
/// ```
/// ```expect
/// WeakMap 96
/// WeakReference 48
/// ```
#[php_class(
    final,
    not_serializable,
    dynamic_properties = forbidden,
    implements(ArrayAccess, Countable, IteratorAggregate),
    size = 112, payload = ZendWeakMap,
    handlers = WEAK_MAP
)]
pub(crate) struct WeakMap;

#[php_class]
impl WeakMap {
    /// `WeakMap::offsetGet($object): mixed`, the read dimension handler's
    /// answer under `BP_VAR_R`.
    fn offsetGet(
        req: &mut Request,
        this: This<Self>,
        object: Untyped<Value>,
    ) -> Result<Owner<Value>, Thrown> {
        WEAK_MAP.read_dimension(req, this.object(), Some(&object.0), DimensionRead::Warning)
    }

    /// `WeakMap::offsetSet($object, mixed $value): void`: the write
    /// dimension handler, the value one more owner the map holds. An entry
    /// goes with its key: the object's free removes it and releases the
    /// value there, before anything after the `unset` runs.
    ///
    /// ```php
    /// class V { function __destruct() { echo "value freed\n"; } }
    /// $map = new WeakMap;
    /// $k = new stdClass;
    /// $map[$k] = new V;
    /// var_dump(count($map));
    /// unset($k);
    /// echo "after unset\n";
    /// var_dump(count($map), $map);
    /// ```
    /// ```expect
    /// int(1)
    /// value freed
    /// after unset
    /// int(0)
    /// object(WeakMap)#1 (0) {
    /// }
    /// ```
    fn offsetSet(
        req: &mut Request,
        this: This<Self>,
        object: Untyped<Value>,
        value: Value,
    ) -> Result<(), Thrown> {
        let value = Owner::take(req, value);
        WEAK_MAP.write_dimension(req, this.object(), Some(&object.0), value)
    }

    /// `WeakMap::offsetExists($object): bool`: the has dimension handler
    /// without `empty()`'s test.
    ///
    /// ```php
    /// $map = new WeakMap;
    /// $a = new stdClass; $b = new stdClass;
    /// $map[$a] = 'a'; $map[$b] = ['b'];
    /// var_dump($map);
    /// $c = clone $map;
    /// unset($a);
    /// var_dump(count($map), count($c));
    /// ```
    /// ```expect
    /// object(WeakMap)#1 (2) {
    ///   [0]=>
    ///   array(2) {
    ///     ["key"]=>
    ///     object(stdClass)#2 (0) {
    ///     }
    ///     ["value"]=>
    ///     string(1) "a"
    ///   }
    ///   [1]=>
    ///   array(2) {
    ///     ["key"]=>
    ///     object(stdClass)#3 (0) {
    ///     }
    ///     ["value"]=>
    ///     array(1) {
    ///       [0]=>
    ///       string(1) "b"
    ///     }
    ///   }
    /// }
    /// int(1)
    /// int(1)
    /// ```
    fn offsetExists(
        req: &mut Request,
        this: This<Self>,
        object: Untyped<Value>,
    ) -> Result<bool, Thrown> {
        WEAK_MAP.has_dimension(req, this.object(), &object.0, false)
    }

    /// `WeakMap::offsetUnset($object): void`: the unset dimension handler.
    fn offsetUnset(
        req: &mut Request,
        this: This<Self>,
        object: Untyped<Value>,
    ) -> Result<(), Thrown> {
        WEAK_MAP.unset_dimension(req, this.object(), &object.0)
    }

    /// `WeakMap::count(): int`: `count_elements`.
    fn count(req: &mut Request, this: This<Self>) -> Result<i64, Thrown> {
        Ok(WEAK_MAP.count_elements(req, this.object())?.unwrap_or(0))
    }

    /// `WeakMap::getIterator(): Iterator`, an `InternalIterator` over
    /// the map's own iterator (`zend_create_internal_iterator_zval`).
    ///
    /// ```php
    /// $map = new WeakMap;
    /// $it = $map->getIterator();
    /// var_dump($it->key(), $it->current(), $it->valid());
    /// $o = new stdClass; $p = new stdClass;
    /// $map[$o] = 1; $map[$p] = 2;
    /// foreach ($map as $k => &$v) { $v *= 10; }
    /// unset($v);
    /// $it = $map->getIterator();
    /// var_dump($it->key() === $o, $it->current());
    /// $it->next(); $it->next();
    /// var_dump($it->key(), $it->valid(), $map[$p]);
    /// ```
    /// ```expect
    /// NULL
    /// NULL
    /// bool(false)
    /// bool(true)
    /// int(10)
    /// NULL
    /// bool(false)
    /// int(20)
    /// ```
    fn getIterator(
        req: &mut Request,
        this: This<Self>,
    ) -> Result<Owner<Instance<Iterator>>, Thrown> {
        crate::api::call::internal_iterator(req, &this.object()).map(Owner::into_instance)
    }
}
