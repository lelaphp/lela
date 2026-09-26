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
use php_unit::decl::{
    ClassDecl, ClassFlags, ClassKind, DynamicProperties, FunctionDecl, MethodDecl, Name, Parameter,
    ParameterDefault, Type, Visibility,
};

use crate::api::abi::{CloneObj, DimensionRead, Entry, NativeClassDecl, ObjectHandlers};
use crate::api::coerce::coerce_param;
use crate::api::raising::{Thrown, arg_count_error, throw};
use crate::api::request::Request;
use crate::api::values::{Key, PhpArray, PhpObject, PhpString, Value};

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
pub(crate) struct WeakMap {
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
            let _ = req.weakrefs.take(id);
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
            let entries =
                stored(req, at).and_then(|map| map.native::<WeakMap>(req).and_then(|m| m.entries));
            if let Some(mut entries) = entries {
                let _ = entries.unset(req, &Key::Int(i64::from(id)));
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
    fn clone_obj(&self) -> Option<CloneObj> {
        None
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
    ) -> Result<Option<PhpArray>, Thrown> {
        let referent = object.native::<WeakRef>(req).and_then(|p| p.referent);
        let mut table = PhpArray::new(req)?;
        let value = referent.map_or(Value::null(), |referent| referent.as_value());
        req.addref(&value);
        let key = Key::Str(PhpString::new(req, b"object")?);
        table.set(req, key, value)?;
        Ok(Some(table))
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
        None => Err(throw(req, "TypeError", "WeakMap key must be an object")),
    }
}

fn entries_of(req: &Request, map: PhpObject) -> Option<PhpArray> {
    map.native::<WeakMap>(req)
        .and_then(|payload| payload.entries)
}

impl ObjectHandlers for WeakMapHandlers {
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
            .native_mut::<WeakMap>(req)
            .and_then(|payload| payload.entries.take());
        let Some(entries) = entries else {
            return;
        };
        for (key, _) in entries.snapshot() {
            if let Key::Int(id) = key
                && let Some(owner) = u32::try_from(id).ok().and_then(|id| stored(req, id))
            {
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
    ) -> Result<Value, Thrown> {
        let Some(offset) = offset else {
            return Err(throw(req, "Error", "Cannot append to WeakMap"));
        };
        let key = map_key(req, offset)?;
        let found = entries_of(req, object).and_then(|e| e.get(&Key::Int(i64::from(key.index()))));
        let Some(value) = found else {
            if read == DimensionRead::Silent {
                return Ok(Value::null());
            }
            let class = req.class_name(key.class());
            return Err(throw(
                req,
                "Error",
                &format!("Object {class}#{} not contained in WeakMap", key.index()),
            ));
        };
        let value = value.deref();
        req.addref(&value);
        Ok(value)
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
        value: Value,
    ) -> Result<(), Thrown> {
        let Some(offset) = offset else {
            req.release(value);
            return Err(throw(req, "Error", "Cannot append to WeakMap"));
        };
        let key = match map_key(req, offset) {
            Ok(key) => key,
            Err(thrown) => {
                req.release(value);
                return Err(thrown);
            }
        };
        let mut entries = if let Some(entries) = entries_of(req, object) {
            entries
        } else {
            let made = PhpArray::new(req)?;
            if let Some(payload) = object.native_mut::<WeakMap>(req) {
                payload.entries = Some(made);
            }
            made
        };
        let at = Key::Int(i64::from(key.index()));
        if !entries.contains(&at) {
            register(req, key, Holder::Map(object.index()));
        }
        entries.set(req, at, value)
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
            return Ok(crate::ops::is_true(req, &value.deref()));
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
    ) -> Result<Option<PhpArray>, Thrown> {
        let mut table = PhpArray::new(req)?;
        for (key, value) in entries_of(req, object)
            .map(|e| e.snapshot())
            .unwrap_or_default()
        {
            let Key::Int(id) = key else { continue };
            let Some(owner) = u32::try_from(id).ok().and_then(|id| stored(req, id)) else {
                continue;
            };
            let mut pair = PhpArray::new(req)?;
            req.addref(&owner.as_value());
            let at = Key::Str(PhpString::new(req, b"key")?);
            pair.set(req, at, owner.as_value())?;
            req.addref(&value);
            let at = Key::Str(PhpString::new(req, b"value")?);
            pair.set(req, at, value)?;
            table.append(req, Value::array(pair))?;
        }
        Ok(Some(table))
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
    fn get_gc(&self, req: &mut Request, object: PhpObject, visit: &mut dyn FnMut(Value)) {
        for (_, value) in entries_of(req, object)
            .map(|e| e.snapshot())
            .unwrap_or_default()
        {
            visit(value);
        }
    }
}

static WEAK_MAP: WeakMapHandlers = WeakMapHandlers;

/// `zend_weakmap_read_dimension` under `BP_VAR_W` and `BP_VAR_RW`
/// (`Z/Zend/zend_weakrefs.c:370`), which the dimension fetch for a write
/// asks of a `WeakMap` where it asks `read_dimension` of any other
/// class: the entry's own slot, so `$map[$k][] = 1` writes into the
/// entry and `$map[$k] =& $v` binds it; php's refusals as the read
/// raises them. php makes the slot a reference (`ZVAL_MAKE_REF`) and the
/// fetch unwraps it again where the map is its one owner
/// (`ZVAL_UNREF`), which leaves the slot as it was. `None` for an object
/// of any other class.
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
pub(crate) fn dimension_place(
    req: &mut Request,
    table: &'static dyn ObjectHandlers,
    object: PhpObject,
    offset: Option<&Value>,
) -> Option<Result<php_heap::ZvalRef, Thrown>> {
    if !std::ptr::addr_eq(table, &raw const WEAK_MAP) {
        return None;
    }
    Some(weakmap_place(req, object, offset))
}

fn weakmap_place(
    req: &mut Request,
    object: PhpObject,
    offset: Option<&Value>,
) -> Result<php_heap::ZvalRef, Thrown> {
    let Some(offset) = offset else {
        return Err(throw(req, "Error", "Cannot append to WeakMap"));
    };
    let key = map_key(req, offset)?;
    let at = Key::Int(i64::from(key.index()));
    let entries = entries_of(req, object);
    let held = entries.and_then(|entries| entries.get(&at).map(|value| (entries, value)));
    let Some((entries, value)) = held else {
        let class = req.class_name(key.class());
        return Err(throw(
            req,
            "Error",
            &format!("Object {class}#{} not contained in WeakMap", key.index()),
        ));
    };
    let _ = value;
    match entries.heap().value_ref(at.heap()) {
        Some(place) => Ok(place),
        None => Err(throw(req, "Error", "Cannot append to WeakMap")),
    }
}

/// `zend_weakmap_clone_obj`: the new map's entries a copy, every value
/// one more owner, and the copy registered against every key.
fn weakmap_clone(req: &mut Request, object: PhpObject) -> Result<PhpObject, Thrown> {
    let copy = match entries_of(req, object) {
        Some(entries) => Some(entries.copy(req)?),
        None => None,
    };
    let made = req.clone_with_payload(object, WeakMap { entries: copy })?;
    for (key, _) in copy.map(|e| e.snapshot()).unwrap_or_default() {
        if let Key::Int(id) = key
            && let Some(owner) = u32::try_from(id).ok().and_then(|id| stored(req, id))
        {
            register(req, owner, Holder::Map(made.index()));
        }
    }
    Ok(made)
}

/// The two classes as `zend_weakrefs.stub.php` declares them, each
/// `final` and `@strict-properties`, with the methods the stub gives
/// and the handler table and payload `zend_register_weakref_ce` sets.
/// `WeakMap` implements `ArrayAccess` and `Countable`; the stub's
/// `IteratorAggregate` and its `getIterator` wait on the engine's
/// `Traversable` (#548) and a `get_iterator` for `foreach` to reach.
pub(crate) fn weakref_classes() -> Vec<NativeClassDecl> {
    vec![
        (
            final_class(b"WeakReference", Vec::new()),
            vec![
                (
                    method(b"__construct", Vec::new(), None, true),
                    weakref_construct as Entry,
                ),
                (
                    method(
                        b"create",
                        vec![parameter(b"object", Type::Object)],
                        Some(named(b"WeakReference")),
                        false,
                    ),
                    weakref_create as Entry,
                ),
                (
                    method(
                        b"get",
                        Vec::new(),
                        Some(Type::Nullable(Box::new(Type::Object))),
                        true,
                    ),
                    weakref_get as Entry,
                ),
            ],
            Some(&WEAK_REFERENCE),
            Some(crate::api::abi::new_payload::<WeakRef>()),
        ),
        (
            final_class(
                b"WeakMap",
                vec![Name::from("ArrayAccess"), Name::from("Countable")],
            ),
            vec![
                (
                    method(
                        b"offsetGet",
                        vec![untyped(b"object")],
                        Some(Type::Mixed),
                        true,
                    ),
                    weakmap_offset_get as Entry,
                ),
                (
                    method(
                        b"offsetSet",
                        vec![untyped(b"object"), parameter(b"value", Type::Mixed)],
                        Some(Type::Void),
                        true,
                    ),
                    weakmap_offset_set as Entry,
                ),
                (
                    method(
                        b"offsetExists",
                        vec![untyped(b"object")],
                        Some(Type::Bool),
                        true,
                    ),
                    weakmap_offset_exists as Entry,
                ),
                (
                    method(
                        b"offsetUnset",
                        vec![untyped(b"object")],
                        Some(Type::Void),
                        true,
                    ),
                    weakmap_offset_unset as Entry,
                ),
                (
                    method(b"count", Vec::new(), Some(Type::Int), true),
                    weakmap_count as Entry,
                ),
            ],
            Some(&WEAK_MAP),
            Some(crate::api::abi::new_payload::<WeakMap>()),
        ),
    ]
}

fn named(name: &[u8]) -> Type {
    Type::Named(Name(name.into()))
}

fn final_class(name: &[u8], interfaces: Vec<Name>) -> ClassDecl {
    ClassDecl {
        name: Name(name.into()),
        kind: ClassKind::Class,
        flags: ClassFlags {
            is_final: true,
            dynamic_properties: DynamicProperties::Forbidden,
            ..ClassFlags::default()
        },
        parent: None,
        interfaces,
        traits: Vec::new(),
        adaptations: Vec::new(),
        backing: php_unit::decl::EnumBacking::Pure,
        constants: Vec::new(),
        properties: Vec::new(),
        methods: Vec::new(),
        attributes: Vec::new(),
        doc_comment: None,
        file: Name(Box::new([])),
        line: 0,
        end_line: 0,
    }
}

fn method(
    name: &[u8],
    parameters: Vec<Parameter>,
    returns: Option<Type>,
    is_instance: bool,
) -> MethodDecl {
    MethodDecl {
        function: FunctionDecl {
            name: Name(name.into()),
            parameters,
            return_type: returns,
            returns_by_ref: false,
            is_generator: false,
            attributes: Vec::new(),
            doc_comment: None,
            file: Name(Box::new([])),
            line: 0,
            end_line: 0,
            frameless: Vec::new(),
        },
        visibility: Visibility::Public,
        is_instance,
        is_abstract: false,
        is_final: false,
        tentative_return: false,
    }
}

fn parameter(name: &[u8], ty: Type) -> Parameter {
    Parameter {
        ty: Some(ty),
        ..untyped(name)
    }
}

fn untyped(name: &[u8]) -> Parameter {
    Parameter {
        name: Name(name.into()),
        ty: None,
        by_ref: false,
        prefer_ref: false,
        promoted: false,
        variadic: false,
        default: ParameterDefault::None,
        attributes: Vec::new(),
    }
}

/// `zend_parse_parameters` over exactly `count` arguments: php's
/// `ArgumentCountError` for any other number.
fn exactly(req: &mut Request, count: usize) -> Result<(), Thrown> {
    let given = req.argument_count();
    if given == count {
        return Ok(());
    }
    let what = match count {
        0 => format!("exactly 0 arguments, {given} given"),
        1 => format!("exactly 1 argument, {given} given"),
        n => format!("exactly {n} arguments, {given} given"),
    };
    Err(arg_count_error(req, &what))
}

/// The receiver a method of these classes is served on.
fn receiver(req: &mut Request) -> Result<PhpObject, Thrown> {
    match req.this() {
        Some(this) => Ok(this),
        None => Err(throw(req, "Error", "Non-static method called statically")),
    }
}

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
fn weakref_construct(req: &mut Request) -> Result<Value, Thrown> {
    Err(throw(
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
fn weakref_create(req: &mut Request) -> Result<Value, Thrown> {
    exactly(req, 1)?;
    let given = req.argument(1).unwrap_or_default();
    let given = coerce_param(req, &given, &Type::Object, 1)?;
    let Some(referent) = given.as_object().copied() else {
        return Err(crate::api::raising::fatal(
            req,
            "InternalError: coerce_param held a value that is no object to `object`",
        ));
    };
    let existing = req.weakrefs.row(referent.index()).and_then(|holders| {
        holders.iter().find_map(|holder| match holder {
            Holder::Reference(at) => Some(*at),
            Holder::Map(_) => None,
        })
    });
    if let Some(at) = existing
        && let Some(reference) = stored(req, at)
    {
        req.addref(&reference.as_value());
        return Ok(reference.as_value());
    }
    let Some(class) = req.class_named(b"WeakReference") else {
        return Err(throw(req, "Error", "Class \"WeakReference\" not found"));
    };
    let reference = req.new_object_with(
        class,
        WeakRef {
            referent: Some(referent),
        },
    )?;
    register(req, referent, Holder::Reference(reference.index()));
    Ok(reference.as_value())
}

/// `WeakReference::get(): ?object`: the referent with one more owner,
/// or `null` once it is gone.
fn weakref_get(req: &mut Request) -> Result<Value, Thrown> {
    exactly(req, 0)?;
    let this = receiver(req)?;
    let referent = this
        .native::<WeakRef>(req)
        .and_then(|payload| payload.referent);
    let value = referent.map_or(Value::null(), |referent| referent.as_value());
    req.addref(&value);
    Ok(value)
}

/// `WeakMap::offsetGet($object): mixed`, the read dimension handler's
/// answer under `BP_VAR_R`.
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
fn weakmap_offset_get(req: &mut Request) -> Result<Value, Thrown> {
    exactly(req, 1)?;
    let this = receiver(req)?;
    let key = req.argument(1).unwrap_or_default();
    WEAK_MAP.read_dimension(req, this, Some(&key), DimensionRead::Warning)
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
fn weakmap_offset_set(req: &mut Request) -> Result<Value, Thrown> {
    exactly(req, 2)?;
    let this = receiver(req)?;
    let key = req.argument(1).unwrap_or_default();
    let value = req.argument(2).unwrap_or_default().deref();
    req.addref(&value);
    WEAK_MAP.write_dimension(req, this, Some(&key), value)?;
    Ok(Value::null())
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
fn weakmap_offset_exists(req: &mut Request) -> Result<Value, Thrown> {
    exactly(req, 1)?;
    let this = receiver(req)?;
    let key = req.argument(1).unwrap_or_default();
    WEAK_MAP
        .has_dimension(req, this, &key, false)
        .map(Value::bool)
}

/// `WeakMap::offsetUnset($object): void`: the unset dimension handler.
fn weakmap_offset_unset(req: &mut Request) -> Result<Value, Thrown> {
    exactly(req, 1)?;
    let this = receiver(req)?;
    let key = req.argument(1).unwrap_or_default();
    WEAK_MAP.unset_dimension(req, this, &key)?;
    Ok(Value::null())
}

/// `WeakMap::count(): int`: `count_elements`.
fn weakmap_count(req: &mut Request) -> Result<Value, Thrown> {
    exactly(req, 0)?;
    let this = receiver(req)?;
    let count = WEAK_MAP.count_elements(req, this)?.unwrap_or(0);
    Ok(Value::int(count))
}
