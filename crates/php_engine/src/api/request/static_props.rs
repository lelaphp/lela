//! The static properties of a request: `CE_STATIC_MEMBERS(ce)`, one
//! slot per declared `static` property keyed by the class that declared
//! it, laid down on first use from its default as
//! `zend_class_init_statics` lays it, read, written under the declared
//! type, and bound by reference. The slots are [`StaticProps`]'s, whose
//! field no file but this one reads.

use php_unit::decl::{ClassKind, PropertyDecl, Visibility};

use crate::api::abi::PropertyRead;
use crate::api::raising::Thrown;
use crate::api::values::{ClassId, PhpRef, TypeTag, Value, ValueRef};

use super::Request;

/// The static properties' slots, whose field no file but this one
/// reads.
#[derive(Default)]
pub(super) struct StaticProps {
    /// `CE_STATIC_MEMBERS(ce)` (`Z/Zend/zend_object_handlers.c:2094`):
    /// one slot per declared `static` property, keyed by the class that
    /// declared it and by the property's name. php gives every class a
    /// table of its own whose inherited rows are `IS_INDIRECT` into the
    /// parent's, so a child and its parent read and write one cell; the
    /// declaring class is that cell's name here. A slot is laid down on
    /// first use from the default the declaration carries,
    /// `zend_class_init_statics`, and a typed property with no default
    /// is laid down `IS_UNDEF`. php's table hangs off a map pointer and
    /// is the request's; so is this one.
    slots: Vec<((ClassId, Vec<u8>), Value)>,
}

impl Request {
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
    ) -> Result<Option<(ClassId, PropertyDecl)>, Thrown> {
        let silent = read == PropertyRead::Silent;
        let found = self.property_info_of(class, name);
        if let Some((declaring, property)) = found.as_ref()
            && property.visibility != Visibility::Public
            && Some(*declaring) != scope
            && (property.visibility == Visibility::Private
                || !self.check_protected(*declaring, scope))
        {
            if silent {
                return Ok(None);
            }
            let visibility = property.visibility;
            return Err(self.inaccessible_property(class, visibility, name));
        }
        let Some((declaring, property)) = found.filter(|(_, p)| !p.is_instance) else {
            if silent {
                return Ok(None);
            }
            let class = self.class_name(class);
            return Err(crate::api::raising::throw_error(
                self,
                "Error",
                &format!(
                    "Access to undeclared static property {class}::${}",
                    String::from_utf8_lossy(name)
                ),
            ));
        };
        self.update_class_constants(class)?;
        self.lay_down_static(declaring, name)?;
        if self
            .class_decl(class)
            .is_some_and(|decl| decl.kind == ClassKind::Trait)
        {
            let shown = format!(
                "{}::${}",
                self.class_name(declaring),
                String::from_utf8_lossy(name)
            );
            crate::api::raising::deprecated(
                self,
                &format!(
                    "Accessing static trait property {shown} is deprecated, it should only be accessed on a class using the trait"
                ),
            );
        }
        Ok(Some((declaring, property)))
    }

    /// `zend_class_init_statics` and `zend_update_class_constants` over
    /// the `static` property `name` that `declaring` declares: its slot
    /// laid down where it has none, from its default evaluated in the
    /// declaring scope and held to its type under the strict rule, and
    /// `IS_UNDEF` for a typed one with no default.
    pub(super) fn lay_down_static(
        &mut self,
        declaring: ClassId,
        name: &[u8],
    ) -> Result<(), Thrown> {
        if self
            .static_props
            .slots
            .iter()
            .any(|((held, under), _)| *held == declaring && under == name)
        {
            return Ok(());
        }
        let Some(property) = self
            .class_decl(declaring)
            .and_then(|decl| decl.properties.iter().find(|p| *p.name.0 == *name).cloned())
        else {
            return Ok(());
        };
        let value = match property.default.as_ref() {
            Some(expr) => {
                let value = crate::api::call::eval_declared_made(self, expr, Some(declaring))?;
                self.strictly_held(value, &property, declaring)?
            }
            None if property.ty.is_some() => Value::undef(),
            None => Value::null(),
        };
        self.static_props
            .slots
            .push(((declaring, name.to_vec()), value));
        Ok(())
    }

    /// `CE_STATIC_MEMBERS(ce)[prop_info->offset]`: what the slot of the
    /// `static` property `name` on the class that declared it holds,
    /// nobody's to release, and `IS_UNDEF` where the slot stands
    /// unwritten or the table has none.
    pub(crate) fn held_static(&self, declaring: ClassId, name: &[u8]) -> Value {
        self.static_props
            .slots
            .iter()
            .find(|((at, under), _)| *at == declaring && under == name)
            .map_or_else(Value::undef, |(_, value)| *value)
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
        let Some((declaring, property)) = self.static_slot(class, name, scope, read)? else {
            return Ok(Value::null());
        };
        let held = self.held_static(declaring, name);
        if held.tag() == TypeTag::Undef {
            if property.ty.is_some() && read == PropertyRead::Warning {
                let declaring = self.class_name(declaring);
                return Err(crate::api::raising::throw_error(
                    self,
                    "Error",
                    &format!(
                        "Typed static property {declaring}::${} must not be accessed before initialization",
                        String::from_utf8_lossy(name)
                    ),
                ));
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
        let Some((declaring, property)) =
            self.static_slot(class, name, scope, PropertyRead::Silent)?
        else {
            return Ok(None);
        };
        let held = self.held_static(declaring, name);
        if held.tag() == TypeTag::Undef {
            return Ok(Some(if property.ty.is_some() {
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
        let (declaring, property) =
            match self.static_slot(class, name, scope, PropertyRead::Warning) {
                Ok(Some(slot)) => slot,
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
        self.set_static_slot(declaring, &property, name, value, strict)
            .map(|(_, garbage)| self.release(garbage))
    }

    /// [`Request::set_static_property`] over a slot already addressed,
    /// as `ZEND_ASSIGN_STATIC_PROP` addresses it before it reads its
    /// value and writes it after: `declaring` is the class the slot is
    /// in and `property` its declaration. What the slot holds after the
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
        declaring: ClassId,
        property: &PropertyDecl,
        name: &[u8],
        mut value: Value,
        strict: bool,
    ) -> Result<(Value, Value), Thrown> {
        // `zend_verify_property_type`: the value held to the declared
        // type and coerced by the weak rule where the writer's regime is
        // not strict, and php's `TypeError` where it neither satisfies
        // nor coerces, the value given up as php gives it up.
        if let Some(ty) = property.ty.clone() {
            match crate::ops::verify_property_type(self, &value, &ty, declaring, name, strict) {
                Ok(None) => {}
                Ok(Some(coerced)) => {
                    self.release(value);
                    value = coerced;
                }
                Err(thrown) => {
                    self.release(value);
                    return Err(thrown);
                }
            }
        }
        // `zend_assign_to_variable` over a slot bound by reference: the
        // cell is held to every property it is typed by, which may be
        // another `static` than this one (`Test::$x =& Test::$y`).
        let held = |req: &Request| {
            req.static_props
                .slots
                .iter()
                .find(|((at, under), _)| *at == declaring && under == name)
                .map_or(Value::null(), |(_, held)| held.deref())
        };
        let garbage = held(self);
        self.addref(&garbage);
        if let Some(cell) = self.static_slot_cell(declaring, name)
            && cell.heap().sources() != 0
        {
            if let Err(thrown) = self.assign_to_reference(cell.heap(), value, strict) {
                self.release(garbage);
                return Err(thrown);
            }
        } else {
            self.store_static_property(declaring, name, value);
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
    pub(crate) fn store_static_property(&mut self, declaring: ClassId, name: &[u8], value: Value) {
        let held = self
            .static_props
            .slots
            .iter()
            .position(|((at, under), _)| *at == declaring && under == name);
        let Some(at) = held else {
            self.release(value);
            return;
        };
        if let Some(ValueRef::Reference(cell)) =
            self.static_props.slots.get(at).map(|(_, v)| v.as_ref())
        {
            // What the cell held is given up after the write.
            let cell = cell.heap();
            self.reference_set(cell, value);
            return;
        }
        let previous = self
            .static_props
            .slots
            .get_mut(at)
            .map(|(_, slot)| std::mem::replace(slot, value))
            .unwrap_or_default();
        self.release(previous);
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
        declaring: ClassId,
        name: &[u8],
    ) -> Result<php_heap::ZvalRef, Thrown> {
        if let Some(cell) = self.static_slot_cell(declaring, name) {
            return Ok(cell.heap().val_ref());
        }
        let held = self.held_static(declaring, name);
        let held = if held.tag() == TypeTag::Undef {
            Value::null()
        } else {
            held
        };
        // The row owns what it holds and the cell takes that owner, so
        // the row's value is replaced and never released.
        let cell = self.reference(held)?;
        let at = self
            .static_props
            .slots
            .iter()
            .position(|((held, under), _)| *held == declaring && under == name);
        let Some(at) = at else {
            return Ok(cell.heap().val_ref());
        };
        if let Some((_, slot)) = self.static_props.slots.get_mut(at) {
            *slot = Value::reference(cell);
        }
        Ok(cell.heap().val_ref())
    }

    /// The cell the `static` property's slot holds, for the handler that
    /// must know whether it is writing through a reference php would
    /// have typed (`ZEND_REF_HAS_TYPE_SOURCES` at the top of
    /// `ZEND_ASSIGN_STATIC_PROP_OP`); none where the slot holds a value.
    pub(crate) fn static_slot_cell(&self, declaring: ClassId, name: &[u8]) -> Option<PhpRef> {
        self.static_props
            .slots
            .iter()
            .find(|((at, under), _)| *at == declaring && under == name)
            .and_then(|(_, value)| value.as_php_ref().copied())
    }

    /// `zend_assign_to_variable_reference` over a `static` property's
    /// slot, which `ZEND_ASSIGN_STATIC_PROP_REF` runs: the cell itself
    /// becomes what the slot holds, so the property and every name
    /// bound to it are one value, and what the slot held is given up
    /// after the write. This is not [`Request::set_static_property`],
    /// which writes *through* a reference standing there; a bind
    /// replaces it.
    pub(crate) fn bind_static_property(
        &mut self,
        class: ClassId,
        name: &[u8],
        scope: Option<ClassId>,
        bound: Value,
    ) -> Result<(), Thrown> {
        let (declaring, _) = match self.static_slot(class, name, scope, PropertyRead::Warning) {
            Ok(Some(slot)) => slot,
            Ok(None) => {
                self.release(bound);
                return Ok(());
            }
            Err(thrown) => {
                self.release(bound);
                return Err(thrown);
            }
        };
        let held = self
            .static_props
            .slots
            .iter()
            .position(|((at, under), _)| *at == declaring && under == name);
        let Some(at) = held else {
            self.release(bound);
            return Ok(());
        };
        let previous = self
            .static_props
            .slots
            .get_mut(at)
            .map(|(_, slot)| std::mem::replace(slot, bound))
            .unwrap_or_default();
        self.release(previous);
        Ok(())
    }
}
