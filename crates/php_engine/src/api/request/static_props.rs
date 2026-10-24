//! The static properties of a request: `CE_STATIC_MEMBERS(ce)`, one
//! slot per declared `static` property keyed by the property's row,
//! laid down on first use from its default as
//! `zend_class_init_statics` lays it, read, written under the declared
//! type, and bound by reference. The slots are [`StaticProps`]'s, whose
//! field no file but this one reads.

use php_unit::decl::ClassKind;

use crate::api::abi::PropertyRead;
use crate::api::raising::Thrown;
use crate::api::values::{ClassId, Emalloc, PhpRef, TypeTag, Value, ValueRef};

use super::{PropertyInfo, Request, WordMap};

/// The static properties' slots, whose field no file but this one
/// reads.
#[derive(Default)]
pub(super) struct StaticProps {
    /// `CE_STATIC_MEMBERS(ce)` (`Z/Zend/zend_object_handlers.c:2094`):
    /// one slot per declared `static` property, keyed by the property's
    /// row, `prop_info->ce` and the index php's `prop_info->offset`
    /// stands for. php gives every class a table of its own whose
    /// inherited rows are `IS_INDIRECT` into the parent's, so a child
    /// and its parent read and write one cell; the row names the
    /// declaring class, so that cell is the one slot here. A slot is
    /// laid down on first use from the default the declaration carries,
    /// `zend_class_init_statics`, and a typed property with no default
    /// is laid down `IS_UNDEF`. php's table hangs off a map pointer and
    /// is the request's; so is this one.
    slots: WordMap<PropertyInfo, Value>,
    /// The table itself, by the class it is laid for: the block
    /// `zend_class_init_statics` `emalloc`s, a zval for each of the
    /// class's `default_static_members_count` slots, an inherited one
    /// among them ([`Request::init_statics`]).
    /// A class whose chain has no slot is kept with none, so its next
    /// access walks nothing.
    tables: WordMap<ClassId, Option<Emalloc>>,
    /// The rows in the order their slots were laid down, which a debug
    /// build's teardown gives them up in.
    #[cfg(debug_assertions)]
    laid: Vec<PropertyInfo>,
}

impl Request {
    /// Every `static` property's slot given up, a debug build's teardown,
    /// as `zend_shutdown_executor_values` releases the static members
    /// before the final collection, since they may hold roots
    /// (`Z/Zend/zend_execute_API.c:311-332`). A release build leaves them
    /// to the heap's mass release.
    #[cfg(debug_assertions)]
    pub(super) fn free_static_members(&mut self) {
        let StaticProps {
            mut slots,
            tables,
            laid,
        } = std::mem::take(&mut self.static_props);
        for value in laid.iter().filter_map(|info| slots.remove(info)) {
            self.release(value);
        }
        for table in tables.into_values().flatten() {
            table.free(self);
        }
    }

    /// `zend_class_init_statics` (`Z/Zend/zend_object_handlers.c:2094`): the table of
    /// `class`'s static members laid down on its first static access or
    /// the first update of a class with expressions still to evaluate,
    /// its parent's before it, one zval a slot of each class of the chain
    /// that has any, as `CE_STATIC_MEMBERS` is the request's. The slots'
    /// values are laid one by one ([`Request::lay_down_static`]). A
    /// table is 16 bytes a slot, the parent's slots among a child's; a
    /// class whose table stands lays none again, and one whose
    /// declaration holds no expression lays its own at its first static
    /// access, not at its first object:
    ///
    /// ```php
    /// function m($c) { $m = memory_get_usage(); eval($c); return memory_get_usage() - $m; }
    /// m('class W { function __toString() { return ""; } }');
    /// m('$q0 = 1;');
    /// foreach ([
    ///     'class C17 { const K = R17::B; public static $s = 1; } class R17 { const B = 2; }',
    ///     '$GLOBALS["g"] = C17::$s;',
    ///     '$GLOBALS["h"] = C17::K;',
    ///     'class Q10 { public static $p = R::B; } class R { const B = 1; }',
    ///     '$GLOBALS["m2"] = Q10::$p;',
    ///     'class S1 { public static $a = 1, $b = 2; } class S2 extends S1 { public static $c = 3; }',
    ///     '$GLOBALS["s2"] = S2::$c;',
    ///     '$GLOBALS["s1"] = S1::$a;',
    ///     'class S3 { public static $a = 1; public $x = 2; }',
    ///     '$GLOBALS["s3"] = new S3;',
    ///     '$GLOBALS["s3a"] = S3::$a;',
    /// ] as $c) echo m($c), "  ", $c, "\n";
    /// ```
    /// ```expect regime=jit-off
    /// 1312  class C17 { const K = R17::B; public static $s = 1; } class R17 { const B = 2; }
    /// 416  $GLOBALS["g"] = C17::$s;
    /// 0  $GLOBALS["h"] = C17::K;
    /// 896  class Q10 { public static $p = R::B; } class R { const B = 1; }
    /// 48  $GLOBALS["m2"] = Q10::$p;
    /// 944  class S1 { public static $a = 1, $b = 2; } class S2 extends S1 { public static $c = 3; }
    /// 112  $GLOBALS["s2"] = S2::$c;
    /// 32  $GLOBALS["s1"] = S1::$a;
    /// 512  class S3 { public static $a = 1; public $x = 2; }
    /// 88  $GLOBALS["s3"] = new S3;
    /// 48  $GLOBALS["s3a"] = S3::$a;
    /// ```
    pub(super) fn init_statics(&mut self, class: ClassId) -> Result<(), Thrown> {
        if self.static_props.tables.contains_key(&class) {
            return Ok(());
        }
        let lineage: Vec<ClassId> = self.symbols().lineage(class).collect();
        for class in lineage.into_iter().rev() {
            if self.static_props.tables.contains_key(&class) {
                continue;
            }
            let count = if self.native_class(class) {
                0
            } else {
                self.static_members(class)
            };
            let table = match count {
                0 => None,
                count => Some(Emalloc::new(
                    self,
                    count * std::mem::size_of::<php_heap::Zval>(),
                )?),
            };
            self.static_props.tables.insert(class, table);
        }
        Ok(())
    }

    /// `zend_std_get_static_property_with_info`
    /// (`Z/Zend/zend_object_handlers.c:2118`): the slot of the `static`
    /// property `name` on `class`, named by the class that declared it,
    /// with the declaration beside it. The access check is php's and
    /// comes first, so a private property of another class is `Cannot
    /// access private property C::$p` and not the undeclared text; a
    /// name no class of the chain declares, and one declared as an
    /// instance property, are `Access to undeclared static property
    /// C::$p`, both naming the class asked about. `scope` is
    /// `get_fake_or_executed_scope()`: the executed scope at an opline,
    /// the class itself where an extension reads through
    /// `zend_read_static_property_ex`, which sets the fake scope to it.
    /// The slot is laid down here on its first use,
    /// `zend_class_init_statics`, from the default the declaration
    /// carries, evaluated in the declaring scope; a typed property
    /// with no default is laid down `IS_UNDEF`, which is what php's
    /// read of one before initialization refuses.
    ///
    /// Under [`PropertyRead::Silent`], php's `BP_VAR_IS`, neither the
    /// access check nor the undeclared text raises: both are the lines
    /// php guards with `type != BP_VAR_IS`, and the answer is `None`,
    /// with no slot laid down, since php returns before
    /// `zend_class_init_statics`.
    ///
    /// `ZEND_ASSIGN_STATIC_PROP` addresses the slot here before it
    /// reads the value operand, which is why the handlers reach this.
    ///
    /// A property addressed on a trait itself, and not on a class that
    /// took it, is php's deprecation once the slot is found; each class
    /// that uses the trait has a slot of its own:
    ///
    /// ```php
    /// trait T { public static $s = 1; }
    /// class C { use T; }
    /// C::$s = 2;
    /// echo T::$s, C::$s, "\n";
    /// T::$s = 3;
    /// echo T::$s, C::$s, "\n";
    /// ```
    /// ```expect
    ///
    /// Deprecated: Accessing static trait property T::$s is deprecated, it should only be accessed on a class using the trait in %s on line 5
    /// 12
    ///
    /// Deprecated: Accessing static trait property T::$s is deprecated, it should only be accessed on a class using the trait in %s on line 6
    ///
    /// Deprecated: Accessing static trait property T::$s is deprecated, it should only be accessed on a class using the trait in %s on line 7
    /// 32
    /// ```
    pub(crate) fn static_slot(
        &mut self,
        class: ClassId,
        name: &[u8],
        scope: Option<ClassId>,
        read: PropertyRead,
    ) -> Result<Option<PropertyInfo>, Thrown> {
        let silent = read == PropertyRead::Silent;
        // The row is handed on past what writes the request (the
        // constants' update, the slot laid down), and the declaration
        // read again through it where it is needed.
        let found = self.property_info(class, name).and_then(|info| {
            let property = self.property_decl(info)?;
            Some((info, property.visibility, property.is_instance))
        });
        if let Some((info, visibility, _)) = found
            && !self.may_access_from(info.declaring, visibility, scope)
        {
            if silent {
                return Ok(None);
            }
            return Err(self.inaccessible_property(class, visibility, name));
        }
        let Some((info, _, _)) = found.filter(|(_, _, instance)| !instance) else {
            if silent {
                return Ok(None);
            }
            let class = self.symbols().class_name(class);
            return Err(crate::api::raising::error(
                self,
                &format!(
                    "Access to undeclared static property {class}::${}",
                    String::from_utf8_lossy(name)
                ),
            ));
        };
        self.update_class_constants(class)?;
        self.init_statics(class)?;
        self.lay_down_static(info)?;
        if self.class_is(class, ClassKind::Trait) {
            let shown = format!(
                "{}::${}",
                self.symbols().class_name(info.declaring),
                String::from_utf8_lossy(name)
            );
            crate::api::raising::deprecated(
                self,
                &format!(
                    "Accessing static trait property {shown} is deprecated, it should only be accessed on a class using the trait"
                ),
            );
        }
        Ok(Some(info))
    }

    /// `zend_class_init_statics` and `zend_update_class_constants` over
    /// the `static` property `info` names: its slot, on the class that
    /// declared it, laid down where it has none, from its default
    /// evaluated in the declaring scope and held to its type under the
    /// strict rule, and `IS_UNDEF` for a typed one with no default.
    pub(super) fn lay_down_static(&mut self, info: PropertyInfo) -> Result<(), Thrown> {
        if self.static_props.slots.contains_key(&info) {
            return Ok(());
        }
        let declaring = info.declaring;
        let Some(property) = self.property_decl(info) else {
            return Ok(());
        };
        // The default expression alone is handed on past what writes the
        // request, so it is what is cloned.
        let (default, typed) = (property.default.clone(), property.ty.is_some());
        let value = match default {
            Some(expr) => {
                let value = crate::api::call::eval_declared_made(self, &expr, Some(declaring))?;
                self.strictly_held(value, info)?
            }
            None if typed => Value::undef(),
            None => Value::null(),
        };
        self.static_props.slots.insert(info, value);
        #[cfg(debug_assertions)]
        self.static_props.laid.push(info);
        Ok(())
    }

    /// `CE_STATIC_MEMBERS(ce)[prop_info->offset]`: what the slot of the
    /// `static` property `info` names holds, nobody's to release, and
    /// `IS_UNDEF` where the slot stands unwritten or the table has none.
    pub(crate) fn held_static(&self, info: PropertyInfo) -> Value {
        self.static_props
            .slots
            .get(&info)
            .map_or_else(Value::undef, |value| *value)
    }

    /// `zend_read_static_property_ex` (`Z/Zend/zend_API.c`): the value
    /// of a `static` property, one more owner taken for the caller and
    /// the slot keeping its own (`RETURN_COPY_DEREF`), a reference
    /// followed. A typed slot nothing has written is php's `Error` for
    /// a `static` property read before initialization, which names the
    /// declaring class and not the one asked about.
    ///
    /// `read` is php's `type`, the `silent` argument of
    /// `zend_read_static_property_ex` under another name: under
    /// [`PropertyRead::Silent`] the property nothing declares, the one
    /// the scope may not reach and the typed slot nothing has written
    /// are each `null`, the last because php guards that throw with
    /// `type == BP_VAR_R || type == BP_VAR_RW`.
    pub(crate) fn static_property(
        &mut self,
        class: ClassId,
        name: &[u8],
        scope: Option<ClassId>,
        read: PropertyRead,
    ) -> Result<Value, Thrown> {
        match self.static_slot(class, name, scope, read)? {
            Some(info) => self.static_value(info, read),
            None => Ok(Value::null()),
        }
    }

    /// [`Request::static_property`] over a slot already addressed, as a
    /// handler that read the slot off its runtime cache reads it
    /// (`zend_fetch_static_property_address`, `Z/Zend/zend_execute.c:3793`):
    /// one more owner of the value the slot of `info` holds, a reference
    /// followed, and under [`PropertyRead::Warning`] php's `Error` for a
    /// typed slot nothing has written, `null` under the silent read.
    pub(crate) fn static_value(
        &mut self,
        info: PropertyInfo,
        read: PropertyRead,
    ) -> Result<Value, Thrown> {
        let held = self.held_static(info);
        if held.tag() == TypeTag::Undef {
            if read == PropertyRead::Warning
                && let Some(property) = self.property_decl(info).filter(|p| p.ty.is_some())
            {
                let message = format!(
                    "Typed static property {}::${} must not be accessed before initialization",
                    self.symbols().class_name(info.declaring),
                    String::from_utf8_lossy(&property.name.0)
                );
                return Err(crate::api::raising::error(self, &message));
            }
            return Ok(Value::null());
        }
        let held = held.deref();
        self.addref(&held);
        Ok(held)
    }

    /// `zend_std_get_static_property(ce, name, BP_VAR_IS)` as the
    /// pointer php hands back, which
    /// `ReflectionClass::getStaticPropertyValue` and
    /// `setStaticPropertyValue` read rather than a value:
    /// [`Request::static_slot`] under [`PropertyRead::Silent`] is the
    /// `NULL`, so nothing here is a name no class of the chain
    /// declares, one declared as an instance property or one the scope
    /// may not reach; an undefined value is the slot php hands back
    /// holding `IS_UNDEF`, a typed property nothing has written, which
    /// [`Request::static_property`] flattens to `null` because php's
    /// `zend_read_static_property_ex` reads the slot and never the
    /// pointer; and otherwise one more owner of the value, a reference
    /// followed. Nothing is raised on those paths; a default
    /// expression that throws as the slot is laid down is php's own
    /// `zend_update_class_constants` failure and is handed on.
    pub(crate) fn static_property_silent(
        &mut self,
        class: ClassId,
        name: &[u8],
        scope: Option<ClassId>,
    ) -> Result<Option<Value>, Thrown> {
        let Some(info) = self.static_slot(class, name, scope, PropertyRead::Silent)? else {
            return Ok(None);
        };
        let held = self.held_static(info);
        if held.tag() == TypeTag::Undef {
            return Ok(Some(if self.property_typed(info) {
                Value::undef()
            } else {
                Value::null()
            }));
        }
        let held = held.deref();
        self.addref(&held);
        Ok(Some(held))
    }

    /// `zend_update_static_property_ex` (`Z/Zend/zend_API.c`): the value
    /// is the slot's from here, and what the slot held is given up. A
    /// slot holding a reference is written through
    /// (`ZEND_TRY_ASSIGN_COPY_PROP`), so a name bound to it sees the
    /// write.
    ///
    /// A property with a declared type is held to it here
    /// ([`crate::ops::verify_property_type`]) under the regime the call
    /// site asks for, since php's two entries ask for different ones:
    /// `ZEND_ASSIGN_STATIC_PROP` verifies under the frame's
    /// `EX_USES_STRICT_TYPES()` and `zend_update_static_property_ex`,
    /// the entry Reflection reaches, under `strict = 0` whatever the
    /// file that called it declared (`Z/Zend/zend_API.c:1585`).
    pub(crate) fn set_static_property(
        &mut self,
        class: ClassId,
        name: &[u8],
        scope: Option<ClassId>,
        value: Value,
        strict: bool,
    ) -> Result<(), Thrown> {
        let info = match self.static_slot(class, name, scope, PropertyRead::Warning) {
            Ok(Some(info)) => info,
            // A write is never `BP_VAR_IS`, so the silent miss cannot
            // stand here; the value is given up as a raising one is.
            Ok(None) => {
                self.release(value);
                return Ok(());
            }
            Err(thrown) => {
                self.release(value);
                return Err(thrown);
            }
        };
        self.set_static_slot(info, name, value, strict)
            .map(|(_, garbage)| self.release(garbage))
    }

    /// [`Request::set_static_property`] over a slot already addressed,
    /// as `ZEND_ASSIGN_STATIC_PROP` addresses it before it reads its
    /// value and writes it after: `info` is the property, whose declaring
    /// class is the one the slot is in. What the slot holds after the
    /// write comes back, a view: the value `zend_assign_to_typed_prop`
    /// answers, coerced where the weak rule coerced it, which is what
    /// `RETURN_VALUE_USED` copies. Beside it comes what the slot held,
    /// one owner, for the caller to give up after that copy, php's
    /// `garbage`: a destructor it serves that writes the property again
    /// does not change what the assignment answers.
    ///
    /// ```php
    /// class Foo { public static int $i; public static string $s = "x"; }
    /// var_dump(Foo::$i = "1", Foo::$s = Foo::$i++, Foo::$s = 3);
    /// ```
    /// ```expect
    /// int(1)
    /// string(1) "1"
    /// string(1) "3"
    /// ```
    pub(crate) fn set_static_slot(
        &mut self,
        info: PropertyInfo,
        name: &[u8],
        mut value: Value,
        strict: bool,
    ) -> Result<(Value, Value), Thrown> {
        let declaring = info.declaring;
        // `zend_verify_property_type`: the value held to the declared
        // type and coerced by the weak rule where the writer's regime is
        // not strict, and php's `TypeError` where it neither satisfies
        // nor coerces, the value given up as php gives it up.
        // The declaration's type is copied only for a value it may
        // coerce or refuse (`ZEND_TYPE_CONTAINS_CODE` first,
        // [`crate::exec::SlotType::admits`]).
        let held_to = self.property_decl(info).map_or(
            crate::exec::SlotType::UNTYPED,
            crate::exec::SlotType::set_reached,
        );
        let ty = (!held_to.admits(value.to_zval()))
            .then(|| self.property_decl(info).and_then(|p| p.ty.clone()))
            .flatten();
        if let Some(ty) = ty {
            let verdict =
                crate::ops::verify_property_type(self, &value, &ty, declaring, name, strict);
            value = crate::ops::landed(self, value, verdict)?;
        }
        // `zend_assign_to_variable` over a slot bound by reference: the
        // cell is held to every property it is typed by, which may be
        // another `static` than this one (`Test::$x =& Test::$y`).
        let held = |req: &Request| {
            req.static_props
                .slots
                .get(&info)
                .map_or(Value::null(), Value::deref)
        };
        let garbage = held(self);
        self.addref(&garbage);
        if let Some(cell) = self.static_slot_cell(info)
            && cell.heap().sources() != 0
        {
            if let Err(thrown) = self.assign_to_reference(cell.heap(), value, strict) {
                self.release(garbage);
                return Err(thrown);
            }
        } else {
            self.store_static_property(info, value);
        }
        Ok((held(self), garbage))
    }

    /// The write half of [`Request::set_static_property`], php's
    /// `ZEND_TRY_ASSIGN_COPY_PROP` over the slot the declaring class
    /// owns: the value is the slot's from here and what it held is
    /// given up after the write, and a slot holding a reference is
    /// written *through*, so a name bound to it sees the write. A
    /// handler that has already held the value to the declared type
    /// reaches this one, since the verify raises a deprecation of its
    /// own and php runs it once (`zend_binary_assign_op_typed_prop`).
    ///
    /// The write through a reference gives what the cell held up once,
    /// the cell's own release after the write, so each value written
    /// over is freed and nothing is freed twice:
    ///
    /// ```php
    /// class C { public static $p; }
    /// $v = str_repeat("x", 32);
    /// C::$p = &$v;
    /// C::$p = str_repeat("y", 32);
    /// C::$p = str_repeat("z", 32);
    /// var_dump($v, C::$p);
    /// ```
    /// ```expect
    /// string(32) "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz"
    /// string(32) "zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz"
    /// ```
    pub(crate) fn store_static_property(&mut self, info: PropertyInfo, value: Value) {
        let previous = self.replace_static_property(info, value);
        self.release(previous);
    }

    /// [`Request::store_static_property`]'s write without its give-up:
    /// the value is the slot's from here, written through a reference
    /// the slot holds, and what the slot or its cell held is handed back
    /// as the caller's owner to give up, as a compound assignment gives
    /// up the value it wrote over; `null` where the slot is gone, the
    /// value given up.
    pub(crate) fn replace_static_property(&mut self, info: PropertyInfo, value: Value) -> Value {
        let Some(slot) = self.static_props.slots.get_mut(&info) else {
            self.release(value);
            return Value::null();
        };
        if let ValueRef::Reference(cell) = slot.as_ref() {
            let at = cell.heap().val_ref();
            let previous = Value::of_slot(at.read());
            at.write(value.into_zval());
            return previous;
        }
        std::mem::replace(slot, value)
    }

    /// `zend_fetch_static_property_address` under `BP_VAR_W`
    /// (`Z/Zend/zend_execute.c:3084`): the `static` property's slot as a
    /// place, which is what `FETCH_STATIC_PROP_W` hands its `VAR` as an
    /// `IS_INDIRECT` and the `ASSIGN_DIM` over it writes through.
    /// The slot is one [`Request::static_slot`] has already found and
    /// filled, which is where access is refused and a default laid down,
    /// so a handler can read the slot as it stands before this binding
    /// lays `null` in an unwritten one.
    ///
    /// php's slot is one zval of `CE_STATIC_MEMBERS(ce)`, a table whose
    /// address is stable for the life of the request. The slots here
    /// are values of a `Vec` the request owns, and no address of one
    /// outlives a write to that table, so a slot a place is asked for
    /// is bound to a reference cell first and the cell is the place.
    /// Every read and every write of a `static` property follows a
    /// reference already ([`Request::static_property`] derefs what it
    /// reads and [`Request::store_static_property`] writes through what
    /// the slot holds), so the binding is a place to write through and
    /// nothing a program can see.
    pub(crate) fn bind_static_place(
        &mut self,
        info: PropertyInfo,
    ) -> Result<php_heap::ZvalRef, Thrown> {
        if let Some(cell) = self.static_slot_cell(info) {
            return Ok(cell.heap().val_ref());
        }
        let held = self.held_static(info);
        let held = if held.tag() == TypeTag::Undef {
            Value::null()
        } else {
            held
        };
        // The row owns what it holds and the cell takes that owner, so
        // the row's value is replaced and never released.
        let cell = self.reference(held)?;
        if let Some(slot) = self.static_props.slots.get_mut(&info) {
            *slot = Value::reference(cell);
        }
        Ok(cell.heap().val_ref())
    }

    /// The cell the `static` property's slot holds, for the handler that
    /// must know whether it is writing through a reference php would
    /// have typed (`ZEND_REF_HAS_TYPE_SOURCES` at the top of
    /// `ZEND_ASSIGN_STATIC_PROP_OP`); none where the slot holds a value.
    pub(crate) fn static_slot_cell(&self, info: PropertyInfo) -> Option<PhpRef> {
        self.static_props
            .slots
            .get(&info)
            .and_then(super::super::values::Value::as_php_ref)
    }

    /// `zend_assign_to_variable_reference` over the slot of the `static`
    /// property `info` names, one the opline has already addressed,
    /// which `ZEND_ASSIGN_STATIC_PROP_REF` runs: the cell itself
    /// becomes what the slot holds, so the property and every name
    /// bound to it are one value, and what the slot held is given up
    /// after the write. This is not [`Request::set_static_property`],
    /// which writes *through* a reference standing there; a bind
    /// replaces it.
    pub(crate) fn bind_static_property(&mut self, info: PropertyInfo, bound: Value) {
        let Some(slot) = self.static_props.slots.get_mut(&info) else {
            self.release(bound);
            return;
        };
        let previous = std::mem::replace(slot, bound);
        self.release(previous);
    }
}
