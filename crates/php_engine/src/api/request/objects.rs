//! The objects of a request: an object made with its properties laid
//! down from their defaults, a native class's payload, the handler table
//! of its class, `ArrayAccess` and the dimension handlers, the cast, the
//! debug info, comparison as `zend_std_compare_objects` compares, cloning
//! with `__clone` and `clone with`, and destruction, `__destruct` served
//! on the last release and the walk `shutdown_destructors` makes at the
//! request's end. Where that walk was cut short is [`Objects`]'s, whose
//! field no file but this one reads.

use std::any::Any;

use php_heap::{Fatal, Heap, Obj, Zval};
use php_link::{LinkedClass, Row};
use php_unit::decl::{Bare, ClassDecl, ClassKind, PropertyDecl, Visibility};

use crate::api::abi::{DimensionRead, DtorObj, NativeNew, ObjectHandlers, Std};
use crate::api::call::Function;
use crate::api::raising::Thrown;
use crate::api::values::{
    ClassId, Owner, PhpArray, PhpObject, PhpString, TypeTag, Value, adopted, adopted_some,
};

use methods::visibility_string;
use pending::Saved;

use super::{Hooks, Placed, PropertyInfo, Request, RowPlace, methods, pending};

/// Where the destructors' walk was cut short, whose field no file but
/// this one reads.
#[derive(Default)]
pub(super) struct Objects {
    /// Where `shutdown_destructors` caught the `zend_bailout` a
    /// destructor's raise ends the walk with
    /// (`Z/Zend/zend_execute_API.c:258-266`): the walk over the symbol
    /// table stops, and the objects then standing are marked destructed
    /// in the heap, `zend_objects_store_mark_destructed`.
    shutdown_walk_ended: bool,
}

impl Request {
    /// `zend_objects_new` over `class` (`object_init_ex` without the
    /// constructor): a store entry out of this request's heap with one
    /// owner, a slot per declared instance property, as
    /// `zend_object_properties_size` counts them, and the memory limit
    /// as the fatal that ends the request.
    pub(crate) fn new_object(&mut self, class: ClassId) -> Result<PhpObject, Thrown> {
        self.update_class_constants(class)?;
        let made = self.made(class);
        // `ce->create_object`: a class that declares a native payload
        // lays it down here, with the object, as
        // `zend_object_alloc(sizeof(custom), ce)` does; every other
        // class is `zend_objects_new` (design §5.4).
        let allocated = match made.create {
            Some(new) => new(&mut self.heap, class.0, made.slots, made.custom),
            None => self.heap.object(class.0, made.slots, made.custom),
        };
        let object = self.fatal_of(allocated).map(PhpObject::of)?;
        self.init_properties(object);
        self.object_made(object, made)?;
        Ok(object)
    }

    /// [`Request::new_object`] carrying `payload` as the native slot of
    /// design §5.4, `zend_object_alloc(sizeof(custom), ce)`: what an
    /// internal class's methods reach through the receiver as `T`, and
    /// what its table's `free_obj` and `get_gc` release and walk.
    pub(crate) fn new_object_with<T: std::any::Any + Send>(
        &mut self,
        class: ClassId,
        payload: T,
    ) -> Result<PhpObject, Thrown> {
        let custom = self.object_custom(class);
        self.new_object_custom(class, payload, custom)
    }

    /// [`Request::new_object_with`] for an object php `emalloc`s whole at
    /// `size` bytes rather than at its class's `zend_object_alloc`: an
    /// iterator the engine's wrapper stands for, whose struct is the
    /// `get_iterator` that made it's ([`crate::interfaces`]).
    pub(crate) fn new_object_sized<T: std::any::Any + Send>(
        &mut self,
        class: ClassId,
        payload: T,
        size: usize,
    ) -> Result<PhpObject, Thrown> {
        const HEADER: usize = 40;
        self.new_object_custom(class, payload, size.saturating_sub(HEADER))
    }

    fn new_object_custom<T: std::any::Any + Send>(
        &mut self,
        class: ClassId,
        payload: T,
        custom: usize,
    ) -> Result<PhpObject, Thrown> {
        self.update_class_constants(class)?;
        let made = self.made(class);
        let allocated = self.heap.object_with(class.0, made.slots, custom, payload);
        let object = self.fatal_of(allocated).map(PhpObject::of)?;
        self.init_properties(object);
        self.object_made(object, made)?;
        Ok(object)
    }

    /// `ce->default_properties_table` beside `ce->properties_info_table`
    /// as `zend_do_inheritance` and `zend_build_properties_info_table`
    /// leave them (`Z/Zend/zend_inheritance.c:1889`, `:1686`): the
    /// declared instance properties in slot order, a parent's before the
    /// class's own, since inheritance copies the parent's table to the
    /// front of the child's and moves the child's own up behind it. A
    /// property the class redeclares keeps the slot the parent gave it,
    /// `do_inherit_property`'s `child_info->offset =
    /// parent_info->offset`, and the class's own declaration is what
    /// stands there. A parent's *private* property is not redeclarable:
    /// php leaves its slot alone and gives the child's a slot of its
    /// own, which is how `var_dump` shows both. Each row names the class
    /// that declared what stands in the slot, which is the scope a
    /// private name is measured against, `property_info->ce`.
    ///
    /// The order is what `rebuild_object_properties_internal` walks
    /// (`Z/Zend/zend_object_handlers.c:75`), so it is the order
    /// `var_dump`, the `(array)` cast and a `foreach` over an object
    /// read; `ReflectionMethod` declares `$class` over
    /// `ReflectionFunctionAbstract`'s `$name`, so the inherited name
    /// prints first. An inherited property has a slot from here on, so
    /// it takes the parent's default where php's
    /// `object_properties_init` copies one.
    ///
    /// ```php
    /// class P { public $a = 1; private $p = 2; protected $q = 3; }
    /// class C extends P { public $b = 4; public $p = 5; }
    /// var_dump(new C, new ReflectionMethod('Exception', 'getCode'), (new ReflectionClass('JsonException'))->newInstanceWithoutConstructor()->getMessage());
    /// ```
    /// ```expect
    /// object(C)#1 (5) {
    ///   ["a"]=>
    ///   int(1)
    ///   ["p":"P":private]=>
    ///   int(2)
    ///   ["q":protected]=>
    ///   int(3)
    ///   ["b"]=>
    ///   int(4)
    ///   ["p"]=>
    ///   int(5)
    /// }
    /// object(ReflectionMethod)#2 (2) {
    ///   ["name"]=>
    ///   string(7) "getCode"
    ///   ["class"]=>
    ///   string(9) "Exception"
    /// }
    /// string(0) ""
    /// ```
    ///
    /// The `(array)` cast mangles by the slot and not by the name, so
    /// the two `$p` slots are two keys, php's
    /// `zend_mangle_property_name` naming the private one with the class
    /// that declared it:
    ///
    /// ```php
    /// class P { private $p = 1; protected $q = 2; public $r = 3; }
    /// class C extends P { public $p = 9; public $s = 4; }
    /// var_dump((array)(new C));
    /// ```
    /// ```expect
    /// array(5) {
    ///   ["%0P%0p"]=>
    ///   int(1)
    ///   ["%0*%0q"]=>
    ///   int(2)
    ///   ["r"]=>
    ///   int(3)
    ///   ["p"]=>
    ///   int(9)
    ///   ["s"]=>
    ///   int(4)
    /// }
    /// ```
    ///
    /// The table is built where the class binds, and the engine's own
    /// classes' at startup ([`placed_of`]), as php builds it once at the
    /// link (`zend_build_properties_info_table`), and read as plain data
    /// after: what a linked class declares, and the chain above it,
    /// never changes.
    pub(crate) fn placed_properties(&self, class: ClassId) -> &[PropertyInfo] {
        self.placement(class).map_or(&[], Placed::slots)
    }

    /// Where `class`'s declared properties are on its objects
    /// ([`Placed`]), as the class's link built it.
    pub(crate) fn placement(&self, class: ClassId) -> Option<&Placed> {
        match self.native_class_at(class) {
            Some(native) => self.shared.placed.get(native),
            None => self.declared_row(class).map(|row| &row.bound.placed),
        }
    }

    /// `ce->properties_info` as the class's one row holds it
    /// ([`PropertiesInfo`]): what [`Request::find_property_info`] finds a
    /// name in. None for a class with no linked record. Inlined into
    /// its two callers, so the three references reach the find in
    /// registers rather than through memory: out of line it handed them
    /// back through the stack, thirteen instructions on a computed name's
    /// placement.
    #[inline(always)]
    pub(super) fn properties_info(&self, class: ClassId) -> Option<PropertiesInfo<'_>> {
        let (record, placed, decl) = if let Some(native) = self.native_class_at(class) {
            (
                self.shared.records.get(native)?,
                self.shared.placed.get(native)?,
                self.shared.classes.get(native)?,
            )
        } else {
            let row = self.declared_row(class)?;
            (row.bound.record.as_ref()?, &row.bound.placed, row.decl()?)
        };
        Some(PropertiesInfo {
            class,
            rows: &record.properties,
            places: &placed.rows,
            decl,
            hooks: record.hooks,
        })
    }

    /// `*prop_info`: the declaration a property's row names, read off
    /// the class that declared it.
    pub(crate) fn property_decl(&self, info: PropertyInfo) -> Option<&PropertyDecl> {
        self.symbols()
            .class_decl(info.declaring)?
            .properties
            .get(usize::try_from(info.at).ok()?)
    }

    /// `ZEND_TYPE_IS_SET(prop_info->type)`: whether the property is typed.
    pub(crate) fn property_typed(&self, info: PropertyInfo) -> bool {
        self.property_decl(info).is_some_and(|p| p.ty.is_some())
    }

    /// `zend_get_property_info_for_slot`: the property in slot `slot`
    /// of an object of `class`, beside the class that declared it.
    pub(crate) fn placed_property(
        &self,
        class: ClassId,
        slot: u32,
    ) -> Option<(ClassId, &PropertyDecl)> {
        let info = *self
            .placed_properties(class)
            .get(usize::try_from(slot).ok()?)?;
        Some((info.declaring, self.property_decl(info)?))
    }

    /// [`Request::placed_properties`] read through: each slot of `class`
    /// in order, its number beside the class that declared it and the
    /// declaration.
    pub(crate) fn placed(
        &self,
        class: ClassId,
    ) -> impl DoubleEndedIterator<Item = (usize, ClassId, &PropertyDecl)> + '_ {
        self.placed_properties(class)
            .iter()
            .enumerate()
            .filter_map(|(slot, info)| Some((slot, info.declaring, self.property_decl(*info)?)))
    }

    /// The slot on an object of `class` of the property `declaring`
    /// declares under `name`; none where it takes no slot there.
    pub(crate) fn slot_declared_by(
        &self,
        class: ClassId,
        declaring: ClassId,
        name: &[u8],
    ) -> Option<u32> {
        self.placed(class)
            .find(|(_, held, p)| *held == declaring && *p.name.0 == *name)
            .and_then(|(slot, _, _)| u32::try_from(slot).ok())
    }
}

/// `zend_build_properties_info_table` over what `zend_do_inheritance`
/// left (`Z/Zend/zend_inheritance.c:1686`): the placed properties of
/// `class`, declared by `decl`, read off its record's property rows and
/// its parent's placed properties, `parent`, with no walk of the chain.
/// The parent's slots come first, in their order, as inheritance copies
/// the parent's table to the front; a slot whose property is not
/// private is taken by the class's own property of that name, which
/// `do_inherit_property` gives the parent's offset (a virtual one
/// stops being virtual there); then each of the class's own instance
/// properties that took no slot and is backed, in declaration order.
/// A backed redeclaration of a parent's virtual property finds no slot
/// to keep, since the virtual one has none, and takes a new one. The
/// record's rows are `properties_info`, the class's own first, so its
/// order is not the slots'; it answers, in one lookup, whether the
/// class declares a slot's name itself. Without a record, which a class
/// bound with none has, the declaration answers instead.
///
/// Through three levels: the parent's redeclared `$a` keeps the
/// grandparent's slot, the grandparent's private `$p` keeps its own
/// beside the child's, the child's backed `$v` over a virtual one takes
/// a new slot, and a static takes none:
///
/// ```php
/// class G { public $a = 1; private $p = 2; public $v { get => 0; } }
/// class P extends G { public $b = 3; public $a = 4; }
/// class C extends P { public $v = 5; public $c = 6; public $p = 7; public static $s = 8; }
/// var_dump(new C);
/// ```
/// ```expect
/// object(C)#1 (6) {
///   ["a"]=>
///   int(4)
///   ["p":"G":private]=>
///   int(2)
///   ["b"]=>
///   int(3)
///   ["v"]=>
///   int(5)
///   ["c"]=>
///   int(6)
///   ["p"]=>
///   int(7)
/// }
/// ```
pub(crate) fn placed_of<'d>(
    class: ClassId,
    decl: &ClassDecl,
    record: Option<&LinkedClass<ClassId>>,
    above: &Placed,
    decl_of: impl Fn(PropertyInfo) -> Option<&'d PropertyDecl>,
) -> Placed {
    let parent = above.slots();
    let own_name = |row: &Row<ClassId>| match row.holder {
        None => decl
            .properties
            .get(usize::try_from(row.at).unwrap_or(usize::MAX))
            .map_or(&[][..], |property| &property.name.0),
        Some(_) => &[][..],
    };
    let own = |name: &[u8]| match record {
        Some(record) => record
            .properties
            .find(name, own_name)
            .filter(|row| row.holder.is_none())
            .and_then(|row| usize::try_from(row.at).ok()),
        None => decl.properties.iter().position(|p| &*p.name.0 == name),
    };
    let row = |at: usize| PropertyInfo {
        declaring: class,
        at: u32::try_from(at).unwrap_or(u32::MAX),
    };
    let mut took = vec![false; decl.properties.len()];
    let mut placed = Vec::with_capacity(parent.len().saturating_add(decl.properties.len()));
    let mut holes = above.holes;
    for &inherited in parent {
        let above =
            decl_of(inherited).filter(|property| property.visibility != Visibility::Private);
        let redeclared = above
            .and_then(|property| own(&property.name.0))
            .filter(|at| decl.properties.get(*at).is_some_and(|p| p.is_instance));
        match redeclared.and_then(|at| Some((at, took.get_mut(at)?))) {
            Some((at, took)) => {
                *took = true;
                placed.push(row(at));
                // `do_inherit_property` leaves a slot `IS_UNDEF`: the
                // backed property's own, its default moved to the
                // parent's, or the parent's, where hooks over a property
                // with none keep the child's slot (`use_child_prop`).
                let child = decl.properties.get(at);
                let hooked_over = above.is_some_and(|above| above.hooks.is_none())
                    && child.is_some_and(|child| child.hooks.is_some());
                if child.is_some_and(|child| !child.is_virtual) || hooked_over {
                    holes += 1;
                }
            }
            None => placed.push(inherited),
        }
    }
    for (at, (property, took)) in decl.properties.iter().zip(took).enumerate() {
        if property.is_instance && !property.is_virtual && !took {
            placed.push(row(at));
        }
    }
    let rows = record.map_or_else(Box::default, |record| {
        record
            .properties
            .rows()
            .iter()
            .map(|row| row_place(class, decl, &placed, row, &decl_of))
            .collect()
    });
    Placed {
        slots: placed.into_boxed_slice(),
        rows,
        holes,
    }
}

/// `ce->properties_info` of one class, read off its one row as php
/// reads it off one `zend_class_entry`: the linked record's rows, what
/// each places its name at, and the class's own declaration, which
/// names every row the class declares itself, so a find of one of those
/// reads no second class.
#[derive(Clone, Copy)]
pub(super) struct PropertiesInfo<'r> {
    pub(super) class: ClassId,
    pub(super) rows: &'r php_link::Rows<Row<ClassId>, php_link::Exact>,
    pub(super) places: &'r [RowPlace],
    pub(super) decl: &'r ClassDecl,
    /// Where the class's property hooks are, `ce->num_hooked_props`.
    pub(super) hooks: php_link::ClassHooks,
}

/// What `row` of `class`'s `properties_info` places its name at among
/// `placed`, the class's slots: the slot of its property,
/// `ZEND_ACC_CHANGED`, which `do_inherit_property` sets on a property
/// redeclared over a parent's private one and keeps down the chain, and
/// what a write to the slot is held to ([`crate::exec::SlotType`]). A
/// private instance property of a class above keeps its slot on the
/// objects of every class below it, so a slot of the name another class
/// declared private is that parent's property.
fn row_place<'d>(
    class: ClassId,
    decl: &ClassDecl,
    placed: &[PropertyInfo],
    row: &Row<ClassId>,
    decl_of: &impl Fn(PropertyInfo) -> Option<&'d PropertyDecl>,
) -> RowPlace {
    let info = PropertyInfo {
        declaring: row.holder_in(class),
        at: row.at,
    };
    let declared = |info: PropertyInfo| {
        if info.declaring == class {
            decl.properties.get(usize::try_from(info.at).ok()?)
        } else {
            decl_of(info)
        }
    };
    let slot = placed
        .iter()
        .position(|held| *held == info)
        .and_then(|slot| u32::try_from(slot).ok());
    let changed = declared(info).is_some_and(|property| {
        changed_over(
            placed.iter().map(|held| (held.declaring, declared(*held))),
            info.declaring,
            &property.name.0,
        )
    });
    let ty = declared(info).map_or(crate::exec::SlotType::UNTYPED, crate::exec::SlotType::of);
    RowPlace { slot, changed, ty }
}

/// `ZEND_ACC_CHANGED` of the property `name` that `declaring` declares,
/// read off the slots of a class's objects, each the class that declared
/// it beside its declaration: a slot of the name another class of the
/// chain declared private, which is a parent's private property keeping
/// its slot below the redeclaration `do_inherit_property` marks.
pub(super) fn changed_over<'d>(
    slots: impl IntoIterator<Item = (ClassId, Option<&'d PropertyDecl>)>,
    declaring: ClassId,
    name: &[u8],
) -> bool {
    slots.into_iter().any(|(held, property)| {
        held != declaring
            && property.is_some_and(|other| {
                other.visibility == Visibility::Private && *other.name.0 == *name
            })
    })
}

impl Request {
    /// [`Request::new_object`] by the class's name, what `object_init_ex`
    /// is over a class entry an extension holds. php's caller cannot
    /// name a class nothing registered, since it hands over the entry
    /// itself; here a name the bundle does not link is the
    /// `InternalError` fatal that says which, and no object is made.
    pub(crate) fn new_object_of(&mut self, class_name: &str) -> Result<PhpObject, Thrown> {
        let Some(class) = self.class_named(Bare::resolved(class_name.as_bytes())) else {
            return Err(crate::api::raising::fatal(
                self,
                &format!("InternalError: the class {class_name} is not linked into this engine"),
            ));
        };
        self.new_object(class)
    }

    /// The native payload of `o` as `T`, borrowed for as long as the
    /// request is.
    pub(crate) fn native<T: std::any::Any + Send>(&self, o: Obj) -> Option<&T> {
        self.heap.native(o)
    }

    /// The native payload of `o` as `T`, mutably.
    pub(crate) fn native_mut<T: std::any::Any + Send>(&mut self, o: Obj) -> Option<&mut T> {
        self.heap.native_mut(o)
    }

    /// [`Request::native_mut`], or the `InternalError` of a method served
    /// on an object without the payload. The first read answers whether
    /// it is there, so the fatal is raised with the request free; the
    /// second cannot miss, since nothing runs between the two.
    pub(crate) fn native_mut_or_fatal<T: std::any::Any + Send>(
        &mut self,
        o: Obj,
    ) -> Result<&mut T, Thrown> {
        if self.heap.native::<T>(o).is_none() {
            return Err(crate::api::raising::fatal(
                self,
                "InternalError: a method served on an object without its payload",
            ));
        }
        self.heap.native_mut(o).ok_or(Thrown::of(None))
    }

    /// `zend_objects_destroy_object` (`Z/Zend/zend_objects.c:114`): the
    /// class's `__destruct` served on the object, which the store holds
    /// at one owner meanwhile (`zend_objects_store_del` holds the count
    /// there, where php's `GC_ADDREF` and `OBJ_RELEASE` around the call
    /// hold it). A class with none serves nothing, and so does a lazy
    /// object, which is the first test php makes after finding the
    /// destructor (`zend_object_is_lazy`). The visibility check and the
    /// pending-throwable rules are the branches under this one.
    ///
    /// An array gives its elements up in element order
    /// (`zend_array_destroy` over `i_zval_ptr_dtor`), so the
    /// destructors of the objects it held are served in that order, and
    /// the array is gone before the statement after it runs.
    ///
    /// ```php
    /// class A {
    ///     public $n;
    ///     function __construct($n) { $this->n = $n; }
    ///     function __destruct() { echo "d", $this->n, "\n"; }
    /// }
    /// $a = [new A(1), new A(2)];
    /// $a = 0;
    /// echo "after\n";
    /// ```
    /// ```expect
    /// d1
    /// d2
    /// after
    /// ```
    ///
    /// A destructor that runs a collection, which finds the object
    /// reachable while the store's walk holds it, leaves it a possible
    /// root again once it returns, so the collection at the request's
    /// end frees the cycle the walk served:
    ///
    /// ```php
    /// class A { public $data = []; function __destruct() { gc_collect_cycles(); }
    ///     function b() { $this->data[] = new B($this); return $this->data[0]; } }
    /// class B { function __construct(public $a) {} function __destruct() {} }
    /// $a = new A; $b = $a->b(); unset($b, $a);
    /// echo "done\n";
    /// ```
    /// ```expect
    /// done
    /// ```
    ///
    /// A destructor that takes an owner of its object resurrects it:
    /// the release that served it finds a count above zero and frees
    /// nothing, and the object is never served again, because
    /// `IS_OBJ_DESTRUCTOR_CALLED` stands on it for the rest of the
    /// request.
    ///
    /// ```php
    /// class A {
    ///     public $n;
    ///     public $self;
    ///     function __construct($n) { $this->n = $n; }
    ///     function __destruct() { echo "d", $this->n, "\n"; $this->self = $this; }
    /// }
    /// $a = new A(1);
    /// $a = 0;
    /// echo "after\n";
    /// ```
    /// ```expect
    /// d1
    /// after
    /// ```
    pub(crate) fn destruct(&mut self, object: PhpObject) {
        let class = object.class();
        let Some(destructor) = self.destructor(class) else {
            return;
        };
        // `zend_object_is_lazy`: a lazy object's destructor is not
        // served, nor a forwarding proxy's, whose instance serves its own;
        // and none once the memory limit was met (`ZEND_HAS_OOM()`,
        // `Z/Zend/zend_objects.c:119` at 8.5.12).
        if object.heap().laziness() != php_heap::Laziness::Eager || self.has_oom() {
            return;
        }
        if !self.destructor_reachable(object, destructor) {
            return;
        }
        let saved = self.save_pending(object);
        if matches!(saved, Saved::TheObject) {
            return;
        }
        if let Err(thrown) = self.serve_method_made(destructor, Some(object), class, &[], &[]) {
            self.leave_pending(&thrown);
            if !self.frames().executing() {
                self.end_the_walk();
                return;
            }
        }
        self.restore_pending(saved);
        // `OBJ_RELEASE(object)` after php's `GC_ADDREF`: the owners as
        // they were, and the object a possible root again where any are
        // left, as a walk that served it leaves it. A collection the
        // destructor ran may have found it reachable and taken it out of
        // the buffer, and the shutdown collection finds it here.
        self.memory_mut().gc_possible_root(&object.as_value());
    }

    /// php's `dtor_obj != zend_objects_destroy_object || ce->destructor`:
    /// the object's table serves a destructor of its own
    /// ([`DtorObj::Own`]), or its class declares or inherits `__destruct`.
    /// What `zend_objects_store_del`, the cycle collector and a lazy
    /// object's reset ask before they serve `dtor_obj`.
    pub(crate) fn has_destructor(&self, object: PhpObject) -> bool {
        matches!(
            self.object_handlers(object.heap()).dtor_obj(),
            DtorObj::Own(_)
        ) || self.destructor(object.class()).is_some()
    }

    /// `zend_call_function`'s tail (`Z/Zend/zend_execute_API.c:1069-1076`)
    /// where a destructor left a throwable pending and nothing is
    /// executing: php calls `zend_throw_exception_internal(NULL)`, which
    /// renders the uncaught fatal there and `zend_bailout`s
    /// (`Z/Zend/zend_exceptions.c:203-222`), so the shutdown is unclean
    /// and a debug build's leak sensor is silent; `shutdown_destructors`
    /// catches that `zend_bailout` and calls
    /// `zend_objects_store_mark_destructed`, so neither walk serves
    /// another destructor. The fatal is rendered here rather than left
    /// for the end of the walk, which is what keeps it one throwable
    /// with no `Next` (design §5.10).
    ///
    /// The rule is `zend_call_function`'s and not the walk's: a raise
    /// inside a serving has a frame to land in and is the pending
    /// throwable of that frame, which is the branch above this one.
    ///
    /// ```php
    /// class A {
    ///     public $n;
    ///     public $self;
    ///     function __construct($n) { $this->n = $n; }
    ///     function __destruct() { echo "d", $this->n, "\n"; nope(); }
    /// }
    /// $a = new A(1);
    /// $a->self = $a;
    /// $b = new A(2);
    /// $b->self = $b;
    /// echo "end\n";
    /// ```
    /// ```expect exit=255
    /// end
    /// d1
    ///
    /// Fatal error: Uncaught Error: Call to undefined function nope() in %s:6
    /// Stack trace:
    /// #0 [internal function]: A->__destruct()
    /// #1 {main}
    ///   thrown in %s on line 6
    /// ```
    ///
    /// The symbol table's walk ends the same way, and it runs first, so
    /// a raise there leaves the store's walk nothing to serve: `$b` is
    /// deleted before `$a`, as `zend_hash_reverse_apply` takes them, and
    /// `$a`'s destructor is never served.
    ///
    /// ```php
    /// class A {
    ///     public $n;
    ///     function __construct($n) { $this->n = $n; }
    ///     function __destruct() { echo "d", $this->n, "\n"; nope(); }
    /// }
    /// $a = new A(1);
    /// $b = new A(2);
    /// echo "end\n";
    /// ```
    /// ```expect exit=255
    /// end
    /// d2
    ///
    /// Fatal error: Uncaught Error: Call to undefined function nope() in %s:5
    /// Stack trace:
    /// #0 [internal function]: A->__destruct()
    /// #1 {main}
    ///   thrown in %s on line 5
    /// ```
    fn end_the_walk(&mut self) {
        self.objects.shutdown_walk_ended = true;
        self.heap.mark_destructed();
        if self.uncaught().is_err() {
            self.bail_out();
        }
    }

    /// `zend_objects_destroy_object`'s visibility branch: a public
    /// `__destruct` is served from anywhere, and a private or a
    /// protected one is held to `zend_check_method_accessible` from the
    /// executed scope while a frame stands, php's `Error` naming the
    /// visibility, the object's class and the scope the release came
    /// from. With no frame standing, which is the store's shutdown walk
    /// ([`Request::call_destructors`]) and php's `EG(current_execute_data)`
    /// being null, there is no scope to check and the destructor is
    /// skipped with a warning instead.
    ///
    /// ```php
    /// class A { private function __destruct() { echo "never\n"; } }
    /// $a = new A();
    /// $a = 1;
    /// echo "after\n";
    /// ```
    /// ```expect exit=255
    /// Fatal error: Uncaught Error: Call to private A::__destruct() from global scope in %s:4
    /// Stack trace:
    /// #0 {main}
    ///   thrown in %s on line 4
    /// ```
    ///
    /// ```php
    /// class A { public $self; private function __destruct() { echo "never\n"; } }
    /// $a = new A();
    /// $a->self = $a;
    /// echo "end\n";
    /// ```
    /// ```expect
    /// end
    ///
    /// Warning: Call to private A::__destruct() from global scope during shutdown ignored in Unknown on line 0
    /// ```
    fn destructor_reachable(&mut self, object: PhpObject, destructor: Function) -> bool {
        let visibility = self.method_visibility(destructor);
        if visibility == Visibility::Public {
            return true;
        }
        let shown = visibility_string(visibility);
        let class = self.symbols().class_name(object.class());
        if !self.frames().executing() {
            crate::api::raising::warning(
                self,
                &format!(
                    "Call to {shown} {class}::__destruct() from global scope during shutdown ignored"
                ),
            );
            return false;
        }
        if self.method_accessible(destructor) {
            return true;
        }
        let from = self.scope_phrase(self.executed_scope());
        let thrown = crate::api::raising::error(
            self,
            &format!("Call to {shown} {class}::__destruct() from {from}"),
        );
        self.leave_pending(&thrown);
        false
    }

    /// `shutdown_destructors` (`Z/Zend/zend_execute_API.c:251`), the
    /// first half of `zend_call_destructors` and so the first
    /// destructor php serves at shutdown (design §5.10): the symbol
    /// table walked in reverse, `zend_hash_reverse_apply` over
    /// `zval_call_destructor`, each symbol that holds an object nothing
    /// else owns deleted, which serves that object's destructor; the
    /// walk repeated while the count of symbols moves, since one
    /// destructor's work can drop another symbol's last other owner.
    /// [`Request::call_destructors`], the store's walk, runs after it
    /// and takes what is left.
    ///
    /// So two top-level variables holding an object each are served in
    /// reverse declaration order, the order the CVs were numbered read
    /// backwards, and not in the order the store made them.
    ///
    /// ```php
    /// class A {
    ///     public $n;
    ///     function __construct($n) { $this->n = $n; }
    ///     function __destruct() { echo "d", $this->n, "\n"; }
    /// }
    /// $a = new A(1);
    /// $b = new A(2);
    /// echo "end\n";
    /// ```
    /// ```expect
    /// end
    /// d2
    /// d1
    /// ```
    ///
    /// A global whose object has an owner beside the symbol is kept by
    /// the walk and served by the store's after it, in store order, so
    /// the two orders are visible in one program: `$b`, owned by its
    /// symbol alone, is served first and backwards; `$a` and `$c`, each
    /// holding itself, are served after it and forwards.
    ///
    /// ```php
    /// class A {
    ///     public $n;
    ///     public $self;
    ///     function __construct($n) { $this->n = $n; }
    ///     function __destruct() { echo "d", $this->n, "\n"; }
    /// }
    /// $a = new A(1);
    /// $a->self = $a;
    /// $b = new A(2);
    /// $c = new A(3);
    /// $c->self = $c;
    /// echo "end\n";
    /// ```
    /// ```expect
    /// end
    /// d2
    /// d1
    /// d3
    /// ```
    pub(crate) fn shutdown_destructors(&mut self) {
        let Some(arr) = self.symbol_arr(crate::exec::TableId::GLOBAL) else {
            return;
        };
        loop {
            let symbols = arr.len();
            let mut at = arr.num_used();
            while at > 0 && !self.objects.shutdown_walk_ended {
                at -= 1;
                // `zval_call_destructor`: an `IS_INDIRECT` row followed
                // to its CV first.
                let alone = arr.entry_at(at).is_some_and(|(_, z)| {
                    let value = Value::of_zval(z.as_indirect().map_or(z, php_heap::ZvalRef::read));
                    value.as_object().is_some() && value.refcount() == 1
                });
                if alone && let Some(z) = self.heap.array_take_at(arr, at) {
                    self.release_row(z);
                }
            }
            if symbols == arr.len() || self.objects.shutdown_walk_ended {
                break;
            }
        }
    }

    /// `zend_objects_store_call_destructors`, the second half of
    /// `zend_call_destructors` (design §5.10): every live object's
    /// destructor not yet served, in store order, until one leaves a
    /// throwable pending, which [`Request::end_the_walk`] renders where
    /// php renders it and ends both walks with. What the symbol table
    /// owned alone was served before this by
    /// [`Request::shutdown_destructors`]. The store's walk still marks
    /// every object it passes, as php's `zend_objects_store_mark_destructed`
    /// marks them behind the `zend_bailout`, so nothing is served after
    /// it either.
    ///
    /// An object a variable no longer names alone outlives that walk,
    /// so the store's is what serves it: each of these holds itself,
    /// so their destructors are served after the last statement, and in
    /// the order the store made them, which is not the order the
    /// variables were written.
    ///
    /// ```php
    /// class A {
    ///     public $n;
    ///     public $self;
    ///     function __construct($n) { $this->n = $n; }
    ///     function __destruct() { echo "d", $this->n, "\n"; }
    /// }
    /// $a = new A(1);
    /// $a->self = $a;
    /// $b = new A(2);
    /// $b->self = $b;
    /// echo "end\n";
    /// ```
    /// ```expect
    /// end
    /// d1
    /// d2
    /// ```
    pub(crate) fn call_destructors(&mut self) {
        Heap::call_destructors(&mut Hooks(self));
        self.uncaught().ok();
    }

    /// `zend_objects_store_free_object_storage`, in `shutdown_executor`
    /// after the extensions' `rshutdown`: every live object's members
    /// and payload freed once, the bytes left for the mass release.
    pub(crate) fn free_object_storage(&mut self) {
        Heap::free_object_storage(&mut Hooks(self));
    }

    /// Whether no extension registered a handler table for `class` or a
    /// class it extends, so its dimensions are `zend_std_read_dimension`'s,
    /// whose failure answers `NULL`, where a table of an extension's
    /// answers what its own C handler does (`spl_array_read_dimension_ex`
    /// answers `&EG(uninitialized_zval)`).
    pub(crate) fn std_dimensions(&self, class: ClassId) -> bool {
        !self
            .symbols()
            .lineage(class)
            .any(|class| self.handler_row(class).is_some())
    }

    /// The handler table `class` itself declares, none where it declares
    /// none of its own.
    fn handler_row(&self, class: ClassId) -> Option<&'static dyn ObjectHandlers> {
        let at = self.native_class_at(class)?;
        self.shared.handlers.get(at).copied().flatten()
    }

    /// The handler table an object of `class` is reached through
    /// (design §5.4): the class's own where it declared one, else the
    /// nearest ancestor's, as php inherits `create_object` and the table
    /// it installs, and `Std`'s at a root and for every user class. The
    /// walk is cut at a depth no declaration can make.
    pub(crate) fn handlers_of(&self, class: ClassId) -> &'static dyn ObjectHandlers {
        self.own_handlers(class).unwrap_or(&Std)
    }

    /// `serve` served as the extension that declared the table `class`
    /// is reached through, where an extension did: what a slot that
    /// answers a function (`dtor_obj`, `clone_obj`, `current_place`,
    /// `unserialize`) serves when the function is called, which a debug
    /// build tags as that extension's as [`crate::served`] tags the slots
    /// that serve code (#2113). A release build serves it as it stands.
    pub(crate) fn serve_as_table<T>(
        &mut self,
        class: ClassId,
        serve: impl FnOnce(&mut Request) -> T,
    ) -> T {
        #[cfg(debug_assertions)]
        {
            let row = self
                .symbols()
                .lineage(class)
                .find(|at| self.handler_row(*at).is_some());
            let extension = row.and_then(|row| self.extensions().class_extension(row));
            if let Some(extension) = extension.filter(|by| *by != super::ExtensionId::CORE) {
                return self.serve_for(extension, serve);
            }
        }
        #[cfg(not(debug_assertions))]
        let _ = class;
        serve(self)
    }

    /// [`PhpObject::set_class`]: `object` takes `class` where neither
    /// lays down a slot or a native payload, both are served by [`Std`],
    /// the object is not lazy and `class` is one `new` could make.
    pub(crate) fn set_object_class(&self, object: PhpObject, class: ClassId) -> bool {
        let plain = |at: ClassId| {
            self.class_slots(at) == 0
                && self.own_handlers(at).is_none()
                && self.native_new_of(at).is_none()
        };
        let makeable = self
            .symbols()
            .class_decl(class)
            .is_some_and(|decl| decl.kind == ClassKind::Class && !decl.flags.is_abstract);
        let o = object.heap();
        makeable
            && o.laziness() == php_heap::Laziness::Eager
            && plain(object.class())
            && plain(class)
            && o.set_class(class.0, 0)
    }

    /// The handler table of `class` where it is not
    /// `std_object_handlers`: its own or the nearest parent's, or an
    /// enum's; `None` where its objects are served by [`Std`].
    pub(crate) fn own_handlers(&self, class: ClassId) -> Option<&'static dyn ObjectHandlers> {
        if let Some(table) = self
            .symbols()
            .lineage(class)
            .find_map(|at| self.handler_row(at))
        {
            return Some(table);
        }
        self.class_is(class, ClassKind::Enum)
            .then_some(&crate::enums::ENUM as &'static dyn ObjectHandlers)
    }

    /// The `create_object` of the class or of the nearest parent that
    /// declares one (design §5.4), php's inherited `ce->create_object`;
    /// `None` for a class whose objects carry no native payload, which
    /// is `zend_objects_new`.
    pub(crate) fn native_new_of(&self, class: ClassId) -> Option<NativeNew> {
        self.symbols().lineage(class).find_map(|class| {
            let at = self.native_class_at(class)?;
            self.shared.natives.get(at).copied().flatten()
        })
    }

    /// Whether objects of `class` are made by `zend_default_exception_new`,
    /// which every class under `Throwable` reaches: the throwables' table
    /// its own or an ancestor's, under any table an extension gives a
    /// subclass, whose `create_object` wraps it as php's
    /// `ce->create_object` wraps `zend_ce_exception->create_object`.
    pub(crate) fn records_throw_site(&self, class: ClassId) -> bool {
        self.symbols()
            .lineage(class)
            .filter_map(|class| self.handler_row(class))
            .any(crate::classes::records_throw_site)
    }

    /// [`Request::handlers_of`] for the object's class, php's
    /// `zobj->handlers`.
    pub(crate) fn object_handlers(&self, o: Obj) -> &'static dyn ObjectHandlers {
        let class = ClassId(o.class());
        self.made_ref(class)
            .map_or_else(|| self.handlers_of(class), |made| made.handlers)
    }

    /// `zend_class_implements_interface(ce, zend_ce_arrayaccess)`: the
    /// object's class where it is an `ArrayAccess`, else
    /// `zend_bad_array_access`, php's `Error` for an object used as an
    /// array.
    fn array_access(&mut self, object: PhpObject) -> Result<(), Thrown> {
        let implements = self
            .class_named(Bare::resolved(b"ArrayAccess"))
            .is_some_and(|interface| self.symbols().instance_of(object.class(), interface));
        if implements {
            return Ok(());
        }
        let class = self.symbols().class_name(object.class());
        Err(crate::api::raising::error(
            self,
            &format!("Cannot use object of type {class} as array"),
        ))
    }

    /// What the four `zend_std_*_dimension` handlers do around the
    /// `ArrayAccess` methods they call: the offset copied, dereferenced
    /// (`ZVAL_COPY_DEREF(&tmp_offset, offset)`), and the object held
    /// (`GC_ADDREF(object)`) for the span of `calls`, both given back
    /// after it (`OBJ_RELEASE`, `zval_ptr_dtor(&tmp_offset)`), so a
    /// method that gives up every other owner of either does not free
    /// what the next call is served on or handed.
    fn with_array_access<T>(
        &mut self,
        object: PhpObject,
        offset: Value,
        calls: impl FnOnce(&mut Request, Value) -> Result<T, Thrown>,
    ) -> Result<T, Thrown> {
        let offset = Owner::take(self, offset.deref());
        let held = Owner::take(self, object.as_value());
        let called = calls(self, *offset);
        held.give_up(self);
        offset.give_up(self);
        called
    }

    /// `zend_std_read_dimension`: `offsetGet` on an `ArrayAccess`, the
    /// offset `null` for `$o[]`; under `BP_VAR_IS` `offsetExists` first,
    /// `null` where it says no, and `offsetGet` after it where it says
    /// yes, so a chain under an `isset` calls the two in that order and
    /// calls neither on a class that is no `ArrayAccess`. The object and
    /// the offset are held across the two ([`Request::with_array_access`]).
    ///
    /// ```php
    /// class Test implements ArrayAccess {
    ///     public function offsetExists($x): bool { $GLOBALS["obj"] = 24; return true; }
    ///     public function offsetGet($x): mixed { var_dump($x); return 42; }
    ///     public function offsetSet($x, $y): void { }
    ///     public function offsetUnset($x): void { }
    /// }
    /// $obj = new Test;
    /// $name = "foo";
    /// var_dump($obj[$name] ?? 12);
    /// var_dump($obj);
    /// ```
    /// ```expect
    /// string(3) "foo"
    /// int(42)
    /// int(24)
    /// ```
    pub(crate) fn read_dimension(
        &mut self,
        object: PhpObject,
        offset: Option<&Value>,
        read: DimensionRead,
    ) -> Result<Owner<Value>, Thrown> {
        self.array_access(object)?;
        let offset = offset.copied().unwrap_or_else(Value::null);
        let read = self.with_array_access(object, offset, |req, offset| {
            if read == DimensionRead::Silent
                && !truth_of_call(req, object, b"offsetExists", offset)?
            {
                return Ok(Value::null());
            }
            crate::api::call::call_method_made(req, &object, b"offsetGet", &[offset])
        });
        adopted(read)
    }

    /// `zend_std_write_dimension`: `offsetSet` on an `ArrayAccess`, the
    /// offset `null` for `$o[] = v`; `value`'s owner is given up either
    /// way, the method having taken its own.
    pub(crate) fn write_dimension(
        &mut self,
        object: PhpObject,
        offset: Option<&Value>,
        value: Owner<Value>,
    ) -> Result<(), Thrown> {
        if let Err(thrown) = self.array_access(object) {
            value.give_up(self);
            return Err(thrown);
        }
        let offset = offset.copied().unwrap_or_else(Value::null);
        let served = self.with_array_access(object, offset, |req, offset| {
            crate::api::call::call_method_made(req, &object, b"offsetSet", &[offset, *value])
        });
        value.give_up(self);
        self.release(served?);
        Ok(())
    }

    /// `zend_std_has_dimension`: `offsetExists` on an `ArrayAccess`,
    /// and under `not_empty` the truth of `offsetGet` after it where
    /// nothing was thrown, the object and the offset held across the two.
    ///
    /// ```php
    /// class Test implements ArrayAccess {
    ///     public function offsetExists($x): bool { $GLOBALS["obj"] = 24; return true; }
    ///     public function offsetGet($x): mixed { var_dump($x); return 42; }
    ///     public function offsetSet($x, $y): void { }
    ///     public function offsetUnset($x): void { }
    /// }
    /// $obj = new Test;
    /// $name = "foo";
    /// var_dump(empty($obj[$name]));
    /// var_dump($obj);
    /// ```
    /// ```expect
    /// string(3) "foo"
    /// bool(false)
    /// int(24)
    /// ```
    pub(crate) fn has_dimension(
        &mut self,
        object: PhpObject,
        offset: &Value,
        not_empty: bool,
    ) -> Result<bool, Thrown> {
        self.array_access(object)?;
        self.with_array_access(object, *offset, |req, offset| {
            let answer = truth_of_call(req, object, b"offsetExists", offset)?;
            if !answer || !not_empty {
                return Ok(answer);
            }
            truth_of_call(req, object, b"offsetGet", offset)
        })
    }

    /// `zend_std_unset_dimension`: `offsetUnset` on an `ArrayAccess`,
    /// the object and the offset held across it.
    pub(crate) fn unset_dimension(
        &mut self,
        object: PhpObject,
        offset: &Value,
    ) -> Result<(), Thrown> {
        self.array_access(object)?;
        let served = self.with_array_access(object, *offset, |req, offset| {
            crate::api::call::call_method_made(
                req,
                &object,
                b"offsetUnset",
                std::slice::from_ref(&offset),
            )
        })?;
        self.release(served);
        Ok(())
    }

    /// `zend_std_get_constructor`: the class's `__construct` where the
    /// fake or the executed scope may reach it, `zend_bad_constructor_call`'s
    /// `Error` where it may not, the object then marked as one whose
    /// constructor failed so no destructor is served for it
    /// (`zend_object_store_ctor_failed`), and none for a class with no
    /// constructor.
    ///
    /// ```php
    /// class P { private function __construct() {} function __destruct() { echo "destructed\n"; } }
    /// try { new P; } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// Call to private P::__construct() from global scope
    /// ```
    pub(crate) fn constructor_of(&mut self, object: PhpObject) -> Result<Option<Function>, Thrown> {
        let Some(constructor) = self.constructor(object.class()) else {
            return Ok(None);
        };
        // `!(constructor->op_array.fn_flags & ZEND_ACC_PUBLIC)` is what
        // asks the scope.
        if self.method_visibility(constructor) == Visibility::Public {
            return Ok(Some(constructor));
        }
        let scope = self.property_scope();
        if self.method_accessible_from(constructor, scope) {
            return Ok(Some(constructor));
        }
        // `zend_bad_constructor_call`: the sentence names no `method`.
        let shown = visibility_string(self.method_visibility(constructor));
        let class = self.method_class(constructor).map_or_else(
            || String::from("object"),
            |class| self.symbols().class_name(class),
        );
        let from = self.scope_phrase(scope);
        object.heap().set_destructor_called(true);
        Err(crate::api::raising::error(
            self,
            &format!("Call to {shown} {class}::__construct() from {from}"),
        ))
    }

    /// `zend_std_cast_object_tostring`: `__toString` for a string, its
    /// value where it is one and php's `Error` where it is not; `true`
    /// for a bool; and none for every other type and for a class with no
    /// `__toString`. php throws the `Error` only where nothing is pending
    /// (`if (!EG(exception))`): a call refused while a throwable stands,
    /// as a native's conversion past a throw meets it, answers that one,
    /// which ext/intl's `umsg_format_helper` is the witness of.
    ///
    /// The object is held for the call, `GC_ADDREF(readobj)`, and given up
    /// after it with `zend_object_release`, so a cast or an interpolation
    /// leaves an object another owner holds a possible root:
    ///
    /// ```php
    /// class A { function __toString(): string { return 'a'; } }
    /// function t() {
    ///     $o = new A;
    ///     gc_collect_cycles(); $s = (string) $o; $one = gc_status()['roots'];
    ///     gc_collect_cycles(); $s = "<$o>"; $two = gc_status()['roots'];
    ///     var_dump($s, $one, $two);
    /// }
    /// t();
    /// ```
    /// ```expect
    /// string(3) "<a>"
    /// int(1)
    /// int(1)
    /// ```
    pub(crate) fn cast_object(
        &mut self,
        object: PhpObject,
        to: TypeTag,
    ) -> Result<Option<Owner<Value>>, Thrown> {
        match to {
            TypeTag::String => {
                if self
                    .symbols()
                    .method_named(object.class(), b"__tostring")
                    .is_none()
                {
                    return Ok(None);
                }
                let held = Owner::take(self, object);
                let value = crate::api::call::call_method_made(self, &object, b"__toString", &[]);
                held.give_up(self);
                let value = self.unwrap_reference(value?);
                if value.as_php_string().is_some() {
                    return Ok(Some(Owner::adopt(value)));
                }
                self.release(value);
                crate::api::raising::pending(self)?;
                let class = self.symbols().class_name(object.class());
                Err(crate::api::raising::error(
                    self,
                    &format!("Method {class}::__toString() must return a string value"),
                ))
            }
            TypeTag::Bool => Ok(Some(Owner::bool(true))),
            _ => Ok(None),
        }
    }

    /// `zend_std_get_debug_info` (`Z/Zend/zend_object_handlers.c:2148`):
    /// what `__debugInfo` returns, an array's rows, the empty set for
    /// `null` and `E_ERROR` for anything else. `None` for a class that
    /// declares none, which is php's `get_properties(object)` there: the
    /// members themselves, left to the reader that asked. The rows the
    /// method returned are the user's own and carry no mangling, so a
    /// row named as a private property is shown under the name the
    /// method gave it.
    ///
    /// ```php
    /// class D { public $a = 1; public function __debugInfo(): array { return ['shown' => 2]; } }
    /// print_r(new D);
    /// var_dump(new D);
    /// ```
    /// ```expect
    /// D Object
    /// (
    ///     [shown] => 2
    /// )
    /// object(D)#1 (1) {
    ///   ["shown"]=>
    ///   int(2)
    /// }
    /// ```
    ///
    /// ```php
    /// class D { private $a = 1; public function __debugInfo(): array { return ['a' => 2, 5]; } }
    /// print_r(new D);
    /// var_dump(new D);
    /// ```
    /// ```expect
    /// D Object
    /// (
    ///     [a] => 2
    ///     [0] => 5
    /// )
    /// object(D)#1 (2) {
    ///   ["a"]=>
    ///   int(2)
    ///   [0]=>
    ///   int(5)
    /// }
    /// ```
    ///
    /// A `null` is an empty table, with php's deprecation since 8.5:
    ///
    /// ```php
    /// class N { public $a = 1; public function __debugInfo() { return null; } }
    /// var_dump(new N);
    /// ```
    /// ```expect
    /// Deprecated: Returning null from N::__debugInfo() is deprecated, return an empty array instead in %s on line 3
    /// object(N)#1 (0) {
    /// }
    /// ```
    ///
    /// ```php
    /// class E { public function __debugInfo() { return 1; } }
    /// var_dump(new E);
    /// ```
    /// ```expect exit=255
    /// Fatal error: __debuginfo() must return an array in %s on line 3
    /// ```
    pub(crate) fn debug_info(
        &mut self,
        object: PhpObject,
    ) -> Result<Option<Owner<PhpArray>>, Thrown> {
        if self
            .symbols()
            .method_named(object.class(), b"__debuginfo")
            .is_none()
        {
            // `zend_lazy_object_debug_info`: a proxy that forwards shows
            // its instance, and a lazy object that waits shows its
            // members as they stand, neither initialised for it.
            let info = self.lazy_debug_info(object);
            return adopted_some(info);
        }
        let value = crate::api::call::call_method_made(self, &object, b"__debugInfo", &[])?;
        let value = self.unwrap_reference(value);
        match value.as_array() {
            // The array the method returned, its one owner the caller's,
            // which is php's `*is_temp = 1` over the same table.
            Some(array) => Ok(Some(Owner::adopt(array))),
            None if value.tag() == TypeTag::Null => {
                self.release(value);
                let class = self.symbols().class_name(object.class());
                crate::api::raising::deprecated(
                    self,
                    &format!(
                        "Returning null from {class}::__debugInfo() is deprecated, return an empty array instead"
                    ),
                );
                let empty = self.array();
                adopted(empty).map(Some)
            }
            None => {
                self.release(value);
                Err(crate::api::raising::fatal(
                    self,
                    "__debuginfo() must return an array",
                ))
            }
        }
    }

    /// `zend_std_compare_objects` (`Z/Zend/zend_object_handlers.c:2227`),
    /// the `Std` `compare` slot. An object against a non-object is the
    /// cast path: `cast_object` to the other side's type, a refusal for
    /// an int or a float php's notice and `1` or `1.0` in the object's
    /// place, a refusal for anything else the object as the greater side,
    /// and the cast compared by [`crate::ops::compare`] in the object's
    /// position. Two objects are equal when one; uncomparable, `None`,
    /// across classes; and within one class, while neither carries a
    /// dynamic table, each declared slot in turn, both uninitialised
    /// passing, one uninitialised `1`, both held compared as `zend_compare`
    /// compares them, the left object protected for the walk and the
    /// `Error` php throws when the walk reaches it again. Where either
    /// carries a dynamic table it is `zend_compare_symbol_tables` over
    /// the two rebuilt tables: the counts first, then the left one's keys
    /// looked up in the right one, an uninitialised slot `-1` against a
    /// held one and `1` the other way.
    ///
    /// php opens the handler with `zend_objects_check_stack_limit`
    /// (`Z/Zend/zend_object_handlers.c:2231`) and throws
    /// `Maximum call stack size reached during object comparison` where
    /// the thread has come down to `zend.max_allowed_stack_size`, which
    /// is [`php_compile::StackLimit::overflowed`] here and is asked in
    /// the same place, before either side is even read as an object. It
    /// ends a walk no recursion guard can: a chain of objects deep
    /// enough to exhaust the C stack without ever reaching one twice.
    ///
    /// ```php
    /// $o = $l = new stdClass; $p = $r = new stdClass;
    /// for ($i = 0; $i < 25000; $i++) { $o->n = new stdClass; $o = $o->n; $p->n = new stdClass; $p = $p->n; }
    /// try { var_dump($l == $r); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// foreach ([$l, $r] as $h) { while ($next = $h->n ?? null) { $h->n = $next->n ?? null; } }
    /// ```
    /// ```expect regime=small-stack
    /// Maximum call stack size reached during object comparison
    /// ```
    pub(crate) fn compare_objects(
        &mut self,
        left: &Value,
        right: &Value,
    ) -> Result<Option<std::cmp::Ordering>, Thrown> {
        use std::cmp::Ordering;
        if self.limits().stack_overflowed() {
            return Err(crate::api::raising::error(
                self,
                "Maximum call stack size reached during object comparison",
            ));
        }
        let (a, b) = match (left.as_object(), right.as_object()) {
            (Some(a), Some(b)) => (a, b),
            (Some(object), None) => return self.compare_cast(object, right, true).map(Some),
            (None, Some(object)) => return self.compare_cast(object, left, false).map(Some),
            (None, None) => return Ok(None),
        };
        if a == b {
            return Ok(Some(Ordering::Equal));
        }
        if a.class() != b.class() {
            return Ok(None);
        }
        let (x, y) = (a.heap(), b.heap());
        // `GC_ADDREF` on both for the walk, given up at `done`.
        self.addref(left);
        self.addref(right);
        let eager = |o: Obj| o.laziness() == php_heap::Laziness::Eager;
        let order = if x.properties().is_none() && y.properties().is_none() && eager(x) && eager(y)
        {
            self.compare_slots(x, y)
        } else {
            self.compare_properties(a, b)
        };
        self.release(*left);
        self.release(*right);
        order.map(Some)
    }

    /// [`Request::compare_objects`] where either side carries a dynamic
    /// table or is lazy: `zend_std_get_properties_ex` on each side, which
    /// initialises a lazy one first, the left before the right, and the
    /// two tables compared.
    fn compare_properties(
        &mut self,
        a: PhpObject,
        b: PhpObject,
    ) -> Result<std::cmp::Ordering, Thrown> {
        let a = self.lazy_init(a)?;
        let b = self.lazy_init(b)?;
        self.compare_tables(a.heap(), b.heap())
    }

    /// The two objects' property tables as `rebuild_object_properties`
    /// lays them down, the left one's first; the fatal where the heap
    /// cannot. A call of its own, so a fatal's handling is off the stack
    /// of a nest of objects compared level by level (#3159).
    #[inline(never)]
    fn property_tables(
        &mut self,
        x: Obj,
        y: Obj,
    ) -> Result<(php_heap::PropertyTable, php_heap::PropertyTable), Thrown> {
        let left = self.heap.object_property_table(x);
        let left = self.fatal_of(left)?;
        let right = self.heap.object_property_table(y);
        let right = self.fatal_of(right)?;
        Ok((left, right))
    }

    /// The declared-slot walk of [`Request::compare_objects`], under the
    /// recursion guard on the left object; it is enough to protect one,
    /// as php notes, since the other may be reached from it.
    fn compare_slots(&mut self, x: Obj, y: Obj) -> Result<std::cmp::Ordering, Thrown> {
        use std::cmp::Ordering;
        if x.slots() == 0 {
            return Ok(Ordering::Equal);
        }
        if x.is_recursive() {
            return Err(crate::ops::nesting_too_deep(self));
        }
        x.protect();
        let mut order = Ordering::Equal;
        for i in 0..x.slots() {
            let (p, q) = (x.slot(i), y.slot(i));
            order = match (p.is_undef(), q.is_undef()) {
                (true, true) => continue,
                (true, false) | (false, true) => Ordering::Greater,
                (false, false) => {
                    match crate::ops::compare(self, &Value::of_slot(p), &Value::of_slot(q)) {
                        Ok(Ordering::Equal) => continue,
                        Ok(order) => order,
                        Err(thrown) => {
                            x.unprotect();
                            return Err(thrown);
                        }
                    }
                }
            };
            break;
        }
        x.unprotect();
        Ok(order)
    }

    /// `zend_compare_symbol_tables` over the two objects' tables as
    /// `zend_std_get_properties_ex` lays them down
    /// (`rebuild_object_properties_internal`), each in php's positions:
    /// one table compares equal to itself; else the counts first, every
    /// declared slot a row while the slots stand for them and every copy
    /// a row once the table holds copies of them
    /// ([`php_heap::DeclaredRows::Copied`]), then `zend_hash_compare`
    /// with `ordered` off, the left table guarded for the walk: each of
    /// its rows looked up by key in the right one, a key the right one
    /// lacks `1`, an uninitialised slot `-1` against a held row and `1`
    /// the other way, two held ones as `zend_compare` compares them.
    ///
    /// ```php
    /// #[AllowDynamicProperties]
    /// class C { public int $b = 1; function __construct() { $this->a = 1; $this->b = 3; } }
    /// $o = (new ReflectionClass(C::class))->newLazyGhost(function ($o) { $o->__construct(); });
    /// var_dump($o); $o->a += 1;
    /// $q = new C; $q->a = 2; $q->b = 1;
    /// var_dump($o == $q, $q == $o, $o == new C, $o < $q, $o->b);
    /// #[AllowDynamicProperties]
    /// class R { public $r; }
    /// $x = new R; $x->r = $x; $x->d = 1; $y = new R; $y->r = $y; $y->d = 1;
    /// try { var_dump($x == $y); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// lazy ghost object(C)#3 (0) {
    ///   ["b"]=>
    ///   uninitialized(int)
    /// }
    /// bool(true)
    /// bool(true)
    /// bool(false)
    /// bool(false)
    /// int(3)
    /// Nesting level too deep - recursive dependency?
    /// ```
    ///
    /// The two tables are held for the walk and given up after it, as
    /// `zend_compare_symbol_tables` holds them ([`crate::ops::HeldTables`]),
    /// so each is a possible root after it, beside the two objects:
    ///
    /// ```php
    /// function t() {
    ///     $a = new stdClass; $a->x = 1; $b = new stdClass; $b->x = 1;
    ///     gc_collect_cycles();
    ///     var_dump($a == $b, gc_status()['roots']);
    /// }
    /// t();
    /// ```
    /// ```expect
    /// bool(true)
    /// int(4)
    /// ```
    fn compare_tables(&mut self, x: Obj, y: Obj) -> Result<std::cmp::Ordering, Thrown> {
        let (left, right) = self.property_tables(x, y)?;
        let table = left.table();
        if table.is_same(right.table()) {
            return Ok(std::cmp::Ordering::Equal);
        }
        let held = crate::ops::HeldTables::hold(self, table, right.table());
        // `zend_hash_compare`'s stack check, before its recursion guard
        // (`Z/Zend/zend_hash.c:3432-3437`), as [`crate::ops`]' walk of two
        // arrays asks it.
        let order = if self.limits().stack_overflowed() {
            Err(crate::api::raising::error(
                self,
                "Maximum call stack size reached during comparison",
            ))
        } else if left.len() != right.len() {
            Ok(left.len().cmp(&right.len()))
        } else if table.is_recursive() {
            Err(crate::ops::nesting_too_deep(self))
        } else {
            table.protect();
            let order = self.compare_rows(left, right, ClassId(x.class()));
            table.unprotect();
            order
        };
        held.give_up(self);
        order
    }

    /// The walk of [`Request::compare_tables`]: each row of `left` in
    /// its order against the row of the same key in `right`, both
    /// objects of `class`, whose slots' mangled names key their
    /// declared rows.
    fn compare_rows(
        &mut self,
        left: php_heap::PropertyTable,
        right: php_heap::PropertyTable,
        class: ClassId,
    ) -> Result<std::cmp::Ordering, Thrown> {
        use std::cmp::Ordering;
        let names = self.slot_names(class)?;
        let key_of = |row: php_heap::TableRow| match row {
            php_heap::TableRow::Entry(key) => Some(key),
            php_heap::TableRow::Slot(i) => usize::try_from(i)
                .ok()
                .and_then(|i| names.get(i))
                .map(|name| php_heap::Key::Str(*name)),
        };
        let mut pos = 0;
        while pos < left.used() {
            let at = left.row_at(pos);
            pos += 1;
            let Some((row, p)) = at else {
                continue;
            };
            let Some(key) = key_of(row) else {
                continue;
            };
            // `zend_hash_find` over the right table: a declared slot by
            // its mangled name while the slots stand for the declared
            // rows, the table's own row otherwise.
            let mut found = None;
            let mut at = 0;
            while let Some((php_heap::TableRow::Slot(i), z)) = right.row_at(at) {
                if key_of(php_heap::TableRow::Slot(i)) == Some(key) {
                    found = Some(z);
                    break;
                }
                at += 1;
            }
            let q = match found {
                Some(z) => z,
                None => match right.table().get(key) {
                    Some(z) => z,
                    None => return Ok(Ordering::Greater),
                },
            };
            let order = match (p.is_undef(), q.is_undef()) {
                (true, true) => Ordering::Equal,
                (true, false) => Ordering::Less,
                (false, true) => Ordering::Greater,
                (false, false) => {
                    crate::ops::compare(self, &Value::of_slot(p), &Value::of_slot(q))?
                }
            };
            if order != Ordering::Equal {
                return Ok(order);
            }
        }
        Ok(Ordering::Equal)
    }

    /// The cast path of [`Request::compare_objects`]: the object's
    /// `cast_object` to the other side's type, `object_lhs` saying which
    /// side the object is on, as `zend_std_compare_objects` reads it.
    fn compare_cast(
        &mut self,
        object: PhpObject,
        other: &Value,
        object_lhs: bool,
    ) -> Result<std::cmp::Ordering, Thrown> {
        use std::cmp::Ordering;
        let target = other.tag();
        let casted = match self
            .object_handlers(object.heap())
            .cast_object(self, object, target)?
        {
            Some(casted) => casted,
            None => match target {
                TypeTag::Int | TypeTag::Float => {
                    let class = self.symbols().class_name(object.class());
                    let (to, one) = if target == TypeTag::Int {
                        ("int", Value::int(1))
                    } else {
                        ("float", Value::float(1.0))
                    };
                    crate::api::raising::notice(
                        self,
                        &format!("Object of class {class} could not be converted to {to}"),
                    );
                    Owner::take(self, one)
                }
                _ => {
                    return Ok(if object_lhs {
                        Ordering::Greater
                    } else {
                        Ordering::Less
                    });
                }
            },
        };
        let order = if object_lhs {
            crate::ops::compare(self, &casted, other)
        } else {
            crate::ops::compare(self, other, &casted)
        };
        casted.give_up(self);
        order
    }

    /// `zend_objects_clone_obj`, the `Std` `clone_obj` slot: a fresh
    /// entry of the same class holding what this one holds, one more
    /// owner each, and the class's `__clone` served on it, the copy held
    /// at one more owner meanwhile and given up with what `__clone`
    /// threw. An object with a native payload is its class's own
    /// `clone_obj` to copy, since the heap cannot copy a payload it only
    /// knows how to drop; `Std` reaching one is the internal-error fatal
    /// naming the class.
    pub(crate) fn clone_members(&mut self, object: PhpObject) -> Result<Owner<PhpObject>, Thrown> {
        if object.heap().laziness() != php_heap::Laziness::Eager {
            return self.lazy_clone(object);
        }
        let custom = self.object_custom(object.class());
        let cloned = self.heap.object_clone(object.heap(), custom);
        let Some(new) = self.fatal_of(cloned)? else {
            let class = self.symbols().class_name(object.class());
            return self.fatal_of(Err(Fatal::error(format!(
                "InternalError: Std::clone_obj over an object of class {class}, whose native payload its own table must copy"
            ))));
        };
        self.cloned(PhpObject::of(new)).map(Owner::adopt)
    }

    /// `zend_objects_clone_members`' `ZEND_REF_ADD_TYPE_SOURCE`: a slot
    /// of the copy holding a cell a typed property is a source of shares
    /// the cell with the original, and the copy's own property is one
    /// more source of it, so the cell stays held to the type when the
    /// original is gone.
    ///
    /// ```php
    /// class Test { public int $x = 42; }
    /// $test = new Test; $x =& $test->x;
    /// $test2 = clone $test;
    /// unset($test);
    /// try { $x = "foo"; } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// var_dump($test2->x);
    /// ```
    /// ```expect
    /// Cannot assign string to reference held by property Test::$x of type int
    /// int(42)
    /// ```
    fn clone_slot_sources(&mut self, copy: PhpObject) {
        let o = copy.heap();
        for slot in 0..o.slots() {
            if let Some(cell) = o.slot(slot).as_reference()
                && cell.sources() != 0
            {
                self.slot_source_added(copy, slot, cell);
            }
        }
    }

    /// The tail of `zend_objects_clone_members`: the class's `__clone`
    /// served on the copy, with every declared slot of it
    /// `IS_PROP_REINITABLE` for the call, so the method may write a
    /// readonly property once more, and none after it, whatever the method
    /// threw. A class with no `__clone` is nothing to serve.
    pub(crate) fn serve_clone_method(&mut self, copy: PhpObject) -> Result<(), Thrown> {
        if self
            .symbols()
            .method_named(copy.class(), b"__clone")
            .is_none()
        {
            return Ok(());
        }
        Self::mark_reinitable(copy, true);
        let served = crate::api::call::call_method_made(self, &copy, b"__clone", &[]);
        Self::mark_reinitable(copy, false);
        served.map(|answered| self.release(answered))
    }

    /// `IS_PROP_REINITABLE` set or cleared on every declared slot of
    /// `object`, as `zend_objects_clone_members` and
    /// `zend_objects_clone_obj_with` do around `__clone` and a `clone
    /// with`'s writes.
    fn mark_reinitable(object: PhpObject, on: bool) {
        let o = object.heap();
        for slot in 0..o.slots() {
            o.set_slot_bit(slot, php_heap::PropBit::Reinitable, on);
        }
    }

    /// `zend_objects_clone_members(new, old)` over an object a class's
    /// `clone_obj` made itself: a dynamic table `new` has given up, the
    /// members of `old` copied onto it, what its slots held given up,
    /// and `__clone` served on it.
    pub(crate) fn clone_members_into(
        &mut self,
        new: PhpObject,
        old: PhpObject,
    ) -> Result<(), Thrown> {
        if let Some(table) = self.heap.object_take_properties(new.heap()) {
            self.release(Value::of_zval(Zval::array(table)));
        }
        let displaced = self.heap.object_clone_members_into(new.heap(), old.heap());
        for held in self.fatal_of(displaced)? {
            self.release(Value::of_slot(held));
        }
        self.clone_slot_sources(new);
        self.serve_clone_method(new)
    }

    /// [`PhpObject::clone_with`]: the members copied beside `payload`,
    /// and `__clone` served on the copy.
    pub(crate) fn clone_members_payload<T: Any + Send>(
        &mut self,
        object: PhpObject,
        payload: T,
    ) -> Result<PhpObject, Thrown> {
        let new = self.clone_with_payload(object, payload)?;
        self.cloned(new)
    }

    /// The end of a clone over `new`, the copy: its typed references
    /// counted ([`Request::clone_slot_sources`]) and `__clone` served
    /// on it, the copy released where it throws.
    fn cloned(&mut self, new: PhpObject) -> Result<PhpObject, Thrown> {
        self.clone_slot_sources(new);
        if let Err(thrown) = self.serve_clone_method(new) {
            self.release(new.as_value());
            return Err(thrown);
        }
        Ok(new)
    }

    /// `zend_objects_clone_obj_with`, the `Std` `clone_obj_with` slot:
    /// the class's `clone_obj`, then each of `properties` written on the
    /// copy as `zend_std_write_property` writes it, a reference with one
    /// owner followed and one with more php's `Error`, under
    /// `EG(fake_scope)` the scope `clone()` was called from, so a
    /// property that scope may not reach is php's `Error` and a parent's
    /// private one a dynamic property; the copy is given up with what a
    /// write threw. Every readonly property is writable once more for
    /// the writes, and each is locked again by the write that lands on
    /// it, so a hook that initialised one leaves it closed to the next.
    ///
    /// ```php
    /// class P { public $a = 'a'; protected $b = 'b'; private $c = 'c';
    ///     public function __construct(public readonly int $r = 1) {}
    ///     function with() { return clone($this, ['b' => 'B', 'c' => 'C', 'r' => 2]); }
    ///     function again() { return clone($this, ['r' => 3]); } }
    /// class C extends P { function mine() { return clone($this, ['c' => 'dynamic']); } }
    /// var_dump((new P)->with());
    /// try { clone(new P, ['b' => 'x']); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// try { clone(new P, ['r' => 3]); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// var_dump((new P)->again()->r);
    /// #[AllowDynamicProperties] class D extends C {}
    /// var_dump((new D)->mine());
    /// ```
    /// ```expect
    /// object(P)#2 (4) {
    ///   ["a"]=>
    ///   string(1) "a"
    ///   ["b":protected]=>
    ///   string(1) "B"
    ///   ["c":"P":private]=>
    ///   string(1) "C"
    ///   ["r"]=>
    ///   int(2)
    /// }
    /// Cannot access protected property P::$b
    /// Cannot modify protected(set) readonly property P::$r from global scope
    /// int(3)
    /// object(D)#3 (5) {
    ///   ["a"]=>
    ///   string(1) "a"
    ///   ["b":protected]=>
    ///   string(1) "b"
    ///   ["c":"P":private]=>
    ///   string(1) "c"
    ///   ["r"]=>
    ///   int(1)
    ///   ["c"]=>
    ///   string(7) "dynamic"
    /// }
    /// ```
    pub(crate) fn clone_members_with(
        &mut self,
        object: PhpObject,
        scope: Option<ClassId>,
        properties: PhpArray,
    ) -> Result<Owner<PhpObject>, Thrown> {
        let table = self.object_handlers(object.heap());
        let Some(clone_obj) = table.clone_obj() else {
            return Err(self.uncloneable(object));
        };
        let new = self.serve_as_table(object.class(), |req| clone_obj(req, object))?;
        // "Unlock readonly properties once more" for the writes, and lock
        // them again after, whatever a write threw; `EG(fake_scope) =
        // scope` for the writes and put back after them.
        Self::mark_reinitable(*new, true);
        let written =
            self.in_fake_scope(scope, |req| req.clone_with_writes(*new, table, properties));
        Self::mark_reinitable(*new, false);
        if let Err(thrown) = written {
            new.give_up(self);
            return Err(thrown);
        }
        Ok(new)
    }

    /// The writes of [`Request::clone_members_with`] over the copy, each
    /// through the class's `write_property`.
    fn clone_with_writes(
        &mut self,
        new: PhpObject,
        table: &'static dyn crate::api::abi::ObjectHandlers,
        properties: PhpArray,
    ) -> Result<(), Thrown> {
        use crate::api::values::walk::{self, Hold};
        // `ZEND_HASH_FOREACH_KEY_VAL(properties, …)`
        // (`Z/Zend/zend_objects.c:301`), no owner of the table taken: the
        // call's argument holds it while `write_property` runs a hook or a
        // `__set`.
        let walked = walk::foreach(self, properties, Hold::None, |req, row| -> walk::Step {
            let value = match row.value().as_php_ref() {
                Some(_) if row.value().refcount() > 1 => {
                    return Err(crate::api::raising::error(
                        req,
                        "Cannot assign by reference when cloning with updated properties",
                    ));
                }
                Some(_) => row.value().deref(),
                None => *row.value(),
            };
            let name = row.key().to_prop_name(req)?;
            let value = Owner::take(req, value);
            req.write_cloned_property(new, table, name, value)?;
            Ok(std::ops::ControlFlow::Continue(()))
        });
        walked.map(|_| ())
    }

    /// `zend_std_write_property` over one property of a `clone with`:
    /// where the name is the standard place, it is placed first
    /// ([`Request::declared_slot_for_write`]), which raises what the
    /// scope may not reach, a readonly property already written and a
    /// set visibility the scope lacks, and a declared slot the write
    /// lands on is locked again (`Z_PROP_FLAG_P(variable_ptr) &=
    /// ~IS_PROP_REINITABLE`); the write itself is the class's
    /// `write_property`, which serves a hook, a `__set` and a dynamic
    /// property.
    fn write_cloned_property(
        &mut self,
        object: PhpObject,
        table: &'static dyn crate::api::abi::ObjectHandlers,
        name: Owner<PhpString>,
        value: Owner<Value>,
    ) -> Result<(), Thrown> {
        let bytes = name.as_bytes().to_vec();
        let place = match table.get_property_ptr_ptr(self, object, &bytes) {
            Ok(place) => place,
            Err(thrown) => return self.refused_write(name, value, thrown),
        };
        if place == crate::api::abi::PropertyPlace::Handlers {
            return table.write_property(self, object, name, value);
        }
        let silent = self.serves_accessor(object.class(), super::Accessor::Set);
        let slot = match self.declared_slot_for_write(object, &bytes, "modify", silent) {
            Ok(placed) => placed.map(|(slot, _)| slot),
            Err(thrown) => return self.refused_write(name, value, thrown),
        };
        table.write_property(self, object, name, value)?;
        if let Some(slot) = slot {
            object
                .heap()
                .set_slot_bit(slot, php_heap::PropBit::Reinitable, false);
        }
        Ok(())
    }

    /// php's `Error` for a class whose `clone_obj` slot is null.
    pub(super) fn uncloneable(&mut self, object: PhpObject) -> Thrown {
        let class = self.symbols().class_name(object.class());
        crate::api::raising::error(
            self,
            &format!("Trying to clone an uncloneable object of class {class}"),
        )
    }

    /// The body `ZEND_CLONE` and the `clone()` function share
    /// (`Z/Zend/zend_vm_def.h:6011`, `Z/Zend/zend_builtin_functions.c:72`):
    /// a null `clone_obj` is php's `Error` for an uncloneable object,
    /// read before anything else; a `__clone` that is not public is held
    /// to `zend_check_method_accessible` from the executed scope, the
    /// `Error` `zend_bad_method_call`'s; then the class's `clone_obj`,
    /// or its `clone_obj_with` where `with` names properties, php's
    /// `Error` where the class has none. The copy is the caller's, one
    /// owner.
    pub(crate) fn clone_object(
        &mut self,
        object: PhpObject,
        with: Option<PhpArray>,
    ) -> Result<Owner<PhpObject>, Thrown> {
        let table = self.object_handlers(object.heap());
        let Some(clone_obj) = table.clone_obj() else {
            return Err(self.uncloneable(object));
        };
        if let Some(clone) = self.symbols().method_named(object.class(), b"__clone") {
            self.check_method_access(clone, b"__clone")?;
        }
        match with.filter(|with| !with.is_empty()) {
            Some(with) => {
                let Some(clone_obj_with) = table.clone_obj_with() else {
                    let class = self.symbols().class_name(object.class());
                    return Err(crate::api::raising::error(
                        self,
                        &format!(
                            "Cloning objects of class {class} with updated properties is not supported"
                        ),
                    ));
                };
                let scope = self.executed_scope();
                self.serve_as_table(object.class(), |req| {
                    clone_obj_with(req, object, scope, with)
                })
            }
            None => self.serve_as_table(object.class(), |req| clone_obj(req, object)),
        }
    }

    /// A class's `clone_obj` over an object with a native payload: the
    /// members copied as `clone_members` copies them and `payload`, the
    /// class's copy of the old one, in the new object's slot. What the
    /// copy names of the heap is the caller's to have taken an owner of.
    pub(crate) fn clone_with_payload<T: std::any::Any + Send>(
        &mut self,
        object: PhpObject,
        payload: T,
    ) -> Result<PhpObject, Thrown> {
        let custom = self.object_custom(object.class());
        let cloned = self.heap.object_clone_with(object.heap(), custom, payload);
        let new = self.fatal_of(cloned).map(PhpObject::of)?;
        self.lay_beside(new)?;
        Ok(new)
    }
}

/// The truth of what `object`'s `method` answers for `offset`, the
/// answer given up once read.
fn truth_of_call(
    req: &mut Request,
    object: PhpObject,
    method: &[u8],
    offset: Value,
) -> Result<bool, Thrown> {
    let answered = crate::api::call::call_method_made(req, &object, method, &[offset])?;
    req.truth_of(answered)
}
