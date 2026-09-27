//! What a request evaluates of a class's declaration, php's
//! `zend_class_mutable_data` (`Z/Zend/zend.h`): the linked class is
//! shared and immutable (principle 3), so the values its constant
//! expressions come to are the request's, one [`ClassData`] per class
//! (ADR-0005: request-scoped). `zend_update_class_constants`
//! (`Z/Zend/zend_API.c:1535`) fills it before an object of the class is
//! made and before a `static` property of it is reached: the parent's
//! first, then every constant the class holds, its own `static`
//! properties and its default properties, each held to its type under
//! the strict rule, and the class marked `ZEND_ACC_CONSTANTS_UPDATED`
//! once all of it stood. A failure leaves what was evaluated before it
//! and marks nothing, so the next `new` or the next read tries again
//! and fails again where it failed.

use php_unit::decl::{ClassConstDecl, ClassKind};

use super::Request;
use crate::api::raising::Thrown;
use crate::api::values::{ClassId, Value};

/// One class's row of [`Request`]'s `class_data`.
#[derive(Debug, Default)]
pub(super) struct ClassData {
    /// `ZEND_ACC_CONSTANTS_UPDATED` in `mutable_data->ce_flags`.
    updated: bool,
    /// The constants of the class's own declaration that were
    /// evaluated, each held to its type, by name: `constants_table`'s
    /// rows once `zend_update_class_constant` replaced their expression. Each
    /// value's one owner is the row.
    constants: Vec<(Box<[u8]>, Value)>,
    /// `default_properties_table`, slot by slot in
    /// [`Request::placed_properties`]' order, as far as it has been
    /// evaluated: `None` for a typed slot with no default, which
    /// `object_properties_init` leaves `IS_UNDEF`. Each value's one
    /// owner is the row.
    defaults: Vec<Option<Value>>,
}

impl Request {
    /// `zend_update_class_constants` (`Z/Zend/zend_API.c:1535`), which
    /// `object_init_ex` and `zend_std_get_static_property_with_info` call
    /// before anything of the class is read: the parent's first, then
    /// each constant the class holds in the scope that declared
    /// it, the class's own `static` properties and its default
    /// properties, a typed one held to its type under the strict rule
    /// whatever the file's regime (`update_property`). An enum's
    /// constants are its cases, which [`Request::update_enum_constants`]
    /// and [`Request::enum_case`] serve.
    ///
    /// A constant nothing reads still fails the `new`:
    ///
    /// ```php
    /// class A { public const self CONST1 = C; }
    /// try { define("C", new A()); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// class B { public static $a = X; public static $b = 1; }
    /// try { echo B::$b; } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// Undefined constant "C"
    /// Undefined constant "X"
    /// ```
    ///
    /// A default is held to its property's type strictly, and a class
    /// whose update failed fails again at the next `new`:
    ///
    /// ```php
    /// define("FOO", 5);
    /// class B { public string $foo = FOO; public float $f = FOO; }
    /// for ($i = 0; $i < 2; $i++) {
    ///     try { new B; } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// }
    /// class F { public float $f = FOO; }
    /// var_dump((new F)->f);
    /// ```
    /// ```expect
    /// Cannot assign int to property B::$foo of type string
    /// Cannot assign int to property B::$foo of type string
    /// float(5)
    /// ```
    ///
    /// Each expression is evaluated once in the request, so what it
    /// raises is raised once:
    ///
    /// ```php
    /// class A { const X = E_STRICT; public $p = E_STRICT; } new A; new A;
    /// var_dump(A::X);
    /// ```
    /// ```expect
    ///
    /// Deprecated: Constant E_STRICT is deprecated since 8.4, the error level was removed in %s on line 2
    ///
    /// Deprecated: Constant E_STRICT is deprecated since 8.4, the error level was removed in %s on line 2
    /// int(2048)
    /// ```
    pub(crate) fn update_class_constants(&mut self, class: ClassId) -> Result<(), Thrown> {
        if self.class_data.get(&class).is_some_and(|data| data.updated) {
            return Ok(());
        }
        let Some(decl) = self.class_decl(class) else {
            return Ok(());
        };
        if decl.kind == ClassKind::Enum {
            return Ok(());
        }
        let native = self.native_class(class);
        if let Some(parent) = self.class_parent(class) {
            self.update_class_constants(parent)?;
        }
        if !native {
            for name in self.constant_names(class) {
                let Some((declaring, constant)) = self.class_constant_of(class, &name, 0) else {
                    continue;
                };
                let value = self.evaluated_class_constant(declaring, &name, &constant)?;
                self.release(value);
            }
        }
        let statics: Vec<Box<[u8]>> = self.class_decl(class).map_or_else(Vec::new, |decl| {
            decl.properties
                .iter()
                .filter(|property| !property.is_instance)
                .map(|property| property.name.0.clone())
                .collect()
        });
        for name in statics {
            self.lay_down_static(class, &name)?;
        }
        self.evaluate_defaults(class)?;
        self.class_data.entry(class).or_default().updated = true;
        Ok(())
    }

    /// The names of the class's constants table in php's order: its
    /// own declaration's, then what inheritance appended, a parent's
    /// that is not private and then an interface's, each name once.
    fn constant_names(&self, class: ClassId) -> Vec<Box<[u8]>> {
        let mut names: Vec<Box<[u8]>> = Vec::new();
        let mut sources = vec![(class, 0)];
        while let Some((at, depth)) = sources.pop() {
            let Some(decl) = self.class_decl(at) else {
                continue;
            };
            for constant in &decl.constants {
                if (depth == 0 || constant.visibility != php_unit::decl::Visibility::Private)
                    && !names.contains(&constant.name.0)
                {
                    names.push(constant.name.0.clone());
                }
            }
            if depth >= 64 {
                continue;
            }
            // Popped last-in first: the interfaces go on first so the
            // parent's table is walked before them.
            for interface in decl.interfaces.iter().rev() {
                if let Some(found) = self.class_named(&interface.0) {
                    sources.push((found, depth + 1));
                }
            }
            if let Some(parent) = self.class_parent(at) {
                sources.push((parent, depth + 1));
            }
        }
        names
    }

    /// `zend_update_class_constant` over the constant `name` that
    /// `declaring` declares: its value from the row where the request
    /// evaluated it before, and otherwise the expression it was written
    /// as evaluated in `declaring`'s scope, held to its type, and kept.
    /// One more owner is the caller's.
    pub(crate) fn evaluated_class_constant(
        &mut self,
        declaring: ClassId,
        name: &[u8],
        constant: &ClassConstDecl,
    ) -> Result<Value, Thrown> {
        if let Some((_, value)) = self
            .class_data
            .get(&declaring)
            .and_then(|data| data.constants.iter().find(|(held, _)| **held == *name))
        {
            let value = *value;
            self.addref(&value);
            return Ok(value);
        }
        let evaluated = crate::api::call::eval_const_expr(self, &constant.value, Some(declaring))?;
        let value = match &constant.ty {
            Some(ty) if ty.mask() != php_unit::decl::Type::MAY_BE_ANY => {
                crate::ops::verify_class_constant_type(self, evaluated, ty, declaring, name)?
            }
            _ => evaluated,
        };
        self.addref(&value);
        self.class_data
            .entry(declaring)
            .or_default()
            .constants
            .push((name.into(), value));
        Ok(value)
    }

    /// The default properties table of `class` once the class is
    /// updated, each slot's value with one more owner the object's, as
    /// `_object_properties_init` copies the table (`ZVAL_COPY_PROP`).
    pub(super) fn default_properties(
        &mut self,
        class: ClassId,
    ) -> Result<Vec<Option<Value>>, Thrown> {
        self.update_class_constants(class)?;
        let defaults: Vec<Option<Value>> = self
            .class_data
            .get(&class)
            .map_or_else(Vec::new, |data| data.defaults.clone());
        for value in defaults.iter().flatten() {
            self.addref(value);
        }
        Ok(defaults)
    }

    /// The loop of `zend_update_class_constants` over
    /// `default_properties_table`: each slot not yet evaluated, in slot
    /// order, its expression evaluated in the scope of the class that
    /// declared it and, where the property is typed, held to the type
    /// under the strict rule (`update_property`), php's
    /// `property initializers must always be evaluated with strict
    /// types`.
    fn evaluate_defaults(&mut self, class: ClassId) -> Result<(), Thrown> {
        let rows = self.placed_properties(class);
        let done = self
            .class_data
            .get(&class)
            .map_or(0, |data| data.defaults.len());
        for (declaring, property) in rows.into_iter().skip(done) {
            let value = match &property.default {
                Some(expr) => {
                    let value = crate::api::call::eval_const_expr(self, expr, Some(declaring))?;
                    Some(self.strictly_held(value, &property, declaring)?)
                }
                None if property.ty.is_some() => None,
                None => Some(Value::null()),
            };
            self.class_data
                .entry(class)
                .or_default()
                .defaults
                .push(value);
        }
        Ok(())
    }

    /// `update_property`'s check: an evaluated default held to its
    /// property's type under the strict rule, converted where the rule
    /// converts, and refused with the type's `TypeError` where it does
    /// not.
    pub(super) fn strictly_held(
        &mut self,
        value: Value,
        property: &php_unit::decl::PropertyDecl,
        declaring: ClassId,
    ) -> Result<Value, Thrown> {
        let Some(ty) = &property.ty else {
            return Ok(value);
        };
        match crate::ops::verify_property_type(self, &value, ty, declaring, &property.name.0, true)
        {
            Ok(None) => Ok(value),
            Ok(Some(coerced)) => {
                self.release(value);
                Ok(coerced)
            }
            Err(thrown) => {
                self.release(value);
                Err(thrown)
            }
        }
    }
}
