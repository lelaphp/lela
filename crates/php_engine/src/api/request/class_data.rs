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

use php_unit::decl::ClassKind;

use super::Symbols;
use super::{PropertyInfo, Request};
use crate::api::abi::{NativeNew, ObjectHandlers};
use crate::api::raising::Thrown;
use crate::api::values::{ClassId, PhpObject, Value};

/// One class's row of [`Request`]'s `class_data`.
#[derive(Default)]
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
    /// The constants of the class's own declaration marked
    /// `IS_CONSTANT_VISITED` (`MARK_CONSTANT_VISITED`) while
    /// `zend_get_class_constant_ex` evaluates them, by name.
    visiting: Vec<Box<[u8]>>,
    /// What `new` reads of the class ([`Made`]), resolved on its first
    /// object in the request.
    made: Option<Made>,
}

/// What `object_init_ex` reads of a class to make an object of it, which
/// php keeps on the class entry, set where the class is linked and
/// inherited by `zend_do_inheritance`: `ce->create_object`, the handler
/// table it installs, `ce->default_properties_count`, and the size
/// `zend_object_alloc` asks for. Each is the linked class's and
/// immutable, read off its lineage once per request and kept on its row,
/// so making an object walks no lineage and hashes nothing.
#[derive(Clone, Copy)]
pub(crate) struct Made {
    /// `ce->default_properties_count` ([`Request::class_slots`]).
    pub(crate) slots: u32,
    /// The struct's bytes past the header and the declared slots
    /// ([`Request::object_custom`]).
    pub(crate) custom: usize,
    /// The class whose declaration names the struct and what is
    /// allocated beside it ([`Request::lay_beside`]).
    declared_by: Option<ClassId>,
    /// `ce->create_object` where it lays a native payload
    /// ([`Request::native_new_of`]).
    pub(crate) create: Option<NativeNew>,
    /// The handler table objects of the class are reached through
    /// ([`Request::handlers_of`]).
    pub(crate) handlers: &'static dyn ObjectHandlers,
    /// How the class's objects serve a property's name
    /// ([`PropertyNames`]), which a property opcode's inline path reads
    /// before it places a name the program computed.
    pub(crate) names: PropertyNames,
    /// Whether `create_object` is `zend_default_exception_new`'s
    /// ([`Request::records_throw_site`]).
    throw_site: bool,
}

/// How the objects of a class serve the name of a property
/// ([`Request::property_names`]), as
/// `zend_get_property_offset` starts: by the handler table and whether
/// `ce->properties_info` holds a row.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum PropertyNames {
    /// `std_object_handlers` and `ce->properties_info` empty, no class of
    /// the chain declaring a property, instance or `static`
    /// (`zend_hash_num_elements(&ce->properties_info) == 0`): every name
    /// but one led by NUL is a dynamic property whatever the scope.
    Dynamic,
    /// `std_object_handlers` over a class that declares a property: a
    /// name is placed by one find of `ce->properties_info`.
    Declared,
    /// A class of the chain declares a handler table of its own
    /// ([`Request::own_handlers`]), which serves every name.
    Own,
}

/// Each class's row by the class's number, as php reaches a class's
/// `mutable_data` through its map pointer (design §5.14): the classes
/// every request shares at their published number, the request's own at
/// theirs, one word a class until its row is made. Reading a row is one
/// index.
#[derive(Default)]
struct Rows {
    published: Vec<Option<Box<ClassData>>>,
    own: Vec<Option<Box<ClassData>>>,
}

impl Rows {
    fn place(class: ClassId) -> (bool, usize) {
        match class.local_at() {
            Some(at) => (false, at),
            None => (true, class.0 as usize),
        }
    }

    fn table(&self, published: bool) -> &Vec<Option<Box<ClassData>>> {
        if published {
            &self.published
        } else {
            &self.own
        }
    }

    #[inline]
    fn get(&self, class: ClassId) -> Option<&ClassData> {
        let (published, at) = Rows::place(class);
        self.table(published).get(at)?.as_deref()
    }

    fn get_mut(&mut self, class: ClassId) -> Option<&mut ClassData> {
        let (published, at) = Rows::place(class);
        let table = if published {
            &mut self.published
        } else {
            &mut self.own
        };
        table.get_mut(at)?.as_deref_mut()
    }

    /// The row of `class`, made empty where the request had none.
    fn row(&mut self, class: ClassId) -> &mut ClassData {
        let (published, at) = Rows::place(class);
        let table = if published {
            &mut self.published
        } else {
            &mut self.own
        };
        if table.len() <= at {
            table.resize_with(at + 1, || None);
        }
        let slot = &mut table[at];
        slot.get_or_insert_with(Box::default)
    }

    /// Every row made, the table left empty.
    #[cfg(debug_assertions)]
    fn take(&mut self) -> impl Iterator<Item = ClassData> + use<> {
        let Rows { published, own } = std::mem::take(self);
        published.into_iter().chain(own).flatten().map(|row| *row)
    }
}

/// Each class's mutable data and the blocks made beside each object, whose fields no file but this one
/// reads.
#[derive(Default)]
pub(super) struct ClassDataTables {
    /// `zend_class_mutable_data` of each class the request updated or
    /// read a constant of, or made an object of.
    rows: Rows,
    /// The `emalloc`s php's `create_object` made beside each object
    /// that has them, by the object's id ([`Request::lay_beside`]).
    beside: super::WordMap<u32, Vec<php_heap::Block>>,
}

impl ClassDataTables {
    /// The value the request evaluated the constant `name` of `declaring`
    /// to and keeps in its row, a view; none where it evaluated none.
    pub(super) fn evaluated_constant(&self, declaring: ClassId, name: &[u8]) -> Option<Value> {
        let data = self.rows.get(declaring)?;
        let (_, value) = data.constants.iter().find(|(held, _)| **held == *name)?;
        Some(*value)
    }
}

impl Request {
    /// Each class's mutable data given up, a debug build's teardown, as
    /// `zend_cleanup_mutable_class_data` and the walk of
    /// `zend_shutdown_executor_values` release a class's evaluated
    /// constants and its default properties before the object store,
    /// since either may hold an object (`Z/Zend/zend_execute_API.c:334-358`).
    /// A release build leaves them to the heap's mass release.
    #[cfg(debug_assertions)]
    pub(super) fn free_class_data(&mut self) {
        for data in self.class_data.rows.take() {
            for (_, value) in data.constants {
                self.release(value);
            }
            for value in data.defaults.into_iter().flatten() {
                self.release(value);
            }
        }
    }

    /// What `zend_object_alloc(sizeof(custom), ce)` counts of an object
    /// of `class` past the header and the declared slots (design §5.13):
    /// the fields of the struct the nearest internal class above it
    /// declares ([`php_unit::decl::ClassDecl::object_size`]), and the
    /// guard slot `zend_object_properties_size` adds for a class that
    /// uses guards.
    ///
    /// ```php
    /// class G { public $a; function __get($n) {} }
    /// class P { public $a; }
    /// foreach (['G', 'P'] as $c) { $keep = []; $m = memory_get_usage(); for ($i = 0; $i < 100; $i++) $keep[] = new $c; echo $c, ' ', (memory_get_usage() - $m) / 100, "\n"; }
    /// ```
    /// ```expect
    /// G 106.48
    /// P 82.16
    /// ```
    pub(crate) fn object_custom(&mut self, class: ClassId) -> usize {
        self.made(class).custom
    }

    /// What `new` read of `class` ([`Made`]) where the request has made
    /// an object of it, through a request held shared.
    #[inline]
    pub(crate) fn made_ref(&self, class: ClassId) -> Option<Made> {
        self.class_data.rows.get(class)?.made
    }

    /// What `new` reads of `class` ([`Made`]), read off its lineage once
    /// per request and kept on its row.
    #[inline]
    pub(crate) fn made(&mut self, class: ClassId) -> Made {
        match self.made_ref(class) {
            Some(made) => made,
            None => self.resolve_made(class),
        }
    }

    /// [`Request::made`] read off the lineage, the class's first `new`
    /// in the request.
    #[cold]
    fn resolve_made(&mut self, class: ClassId) -> Made {
        const ZEND_OBJECT: usize = 56;
        let (size, declared_by) = self
            .symbols()
            .lineage(class)
            .map_while(|at| {
                self.symbols()
                    .class_decl(at)
                    .map(|decl| (at, decl.object_size))
            })
            .find(|&(_, size)| size != 0)
            .map_or((0, None), |(at, size)| (size as usize, Some(at)));
        let guards = if self.uses_guards(class) { 16 } else { 0 };
        let made = Made {
            slots: self.class_slots(class),
            custom: size.saturating_sub(ZEND_OBJECT) + guards,
            declared_by,
            create: self.native_new_of(class),
            handlers: self.handlers_of(class),
            names: if self.own_handlers(class).is_some() {
                PropertyNames::Own
            } else if self.declares_properties(class) {
                PropertyNames::Declared
            } else {
                PropertyNames::Dynamic
            },
            throw_site: self.records_throw_site(class),
        };
        self.class_data.rows.row(class).made = Some(made);
        made
    }

    /// The `emalloc`s `create_object` makes beside a new `object`, each
    /// at the size php asks for
    /// ([`php_unit::decl::ClassDecl::object_beside`]), held until the
    /// object is freed ([`Request::free_beside`]), so what
    /// `memory_get_usage()` counts of it is php's (design §5.13); the
    /// witnesses stand beside the classes that declare them, `SplHeap`'s
    /// and the random engines' among them.
    pub(crate) fn lay_beside(&mut self, object: PhpObject) -> Result<(), Thrown> {
        let Some(declared_by) = self.made(object.class()).declared_by else {
            return Ok(());
        };
        let count = self
            .symbols()
            .class_decl(declared_by)
            .map_or(0, |decl| decl.object_beside.len());
        let mut blocks = Vec::with_capacity(count);
        for at in 0..count {
            let size = self
                .symbols()
                .class_decl(declared_by)
                .and_then(|decl| decl.object_beside.get(at).copied())
                .unwrap_or(0);
            match self.heap.block(size as usize) {
                Ok(block) => blocks.push(block),
                Err(fatal) => {
                    for block in blocks {
                        self.heap.free_block(block);
                    }
                    self.release(object.as_value());
                    return self.fatal_of(Err(fatal));
                }
            }
        }
        if !blocks.is_empty() {
            self.class_data.beside.insert(object.heap().id().0, blocks);
        }
        Ok(())
    }

    /// `size` bytes allocated beside `object` after it was made, given
    /// back with it as [`Request::lay_beside`]'s are: what a native
    /// class's `get_iterator` `emalloc`s for the `InternalIterator`
    /// `zend_create_internal_iterator_zval` makes over one of its objects
    /// walked by index ([`crate::interfaces::internal_iterator`]).
    pub(crate) fn lay_beside_sized(
        &mut self,
        object: PhpObject,
        size: usize,
    ) -> Result<(), Thrown> {
        let block = self.heap.block(size);
        let block = self.fatal_of(block)?;
        self.class_data
            .beside
            .entry(object.heap().id().0)
            .or_default()
            .push(block);
        Ok(())
    }

    /// The rest of `create_object` over a new `object`, its properties
    /// laid down: what is allocated beside it ([`Request::lay_beside`]),
    /// a throwable's `file`, `line` and `trace`, the site of `new`
    /// written by `zend_default_exception_new` before any constructor
    /// runs ([`Request::records_throw_site`]), then what its class lays
    /// down with it ([`crate::api::abi::ObjectHandlers::create_object`]);
    /// the object released where any fails.
    pub(crate) fn object_created(&mut self, object: PhpObject) -> Result<(), Thrown> {
        let made = self.made(object.class());
        self.object_made(object, made)
    }

    /// [`Request::object_created`] with what `new` read of the class.
    pub(super) fn object_made(&mut self, object: PhpObject, made: Made) -> Result<(), Thrown> {
        if made.declared_by.is_some() {
            self.lay_beside(object)?;
        }
        let handlers = made.handlers;
        let thrown = if made.throw_site {
            self.note_throw_site(object).err()
        } else {
            None
        };
        if let Some(thrown) = thrown.or_else(|| handlers.create_object(self, object).err()) {
            self.release(object.as_value());
            return Err(thrown);
        }
        Ok(())
    }

    /// What [`Request::lay_beside`] allocated for `object`, given back as
    /// its `free_obj` gives it back.
    pub(crate) fn free_beside(&mut self, object: PhpObject) {
        if self.class_data.beside.is_empty() {
            return;
        }
        for block in self
            .class_data
            .beside
            .remove(&object.heap().id().0)
            .unwrap_or_default()
        {
            self.heap.free_block(block);
        }
    }

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
    ///
    /// A class whose table holds no expression php declares updated
    /// already, so its parent's constants are not evaluated for it, one
    /// the class redeclares among them:
    ///
    /// ```php
    /// class B { const X = UNRESOLVED; }
    /// class C extends B { const X = 42; public $p = 1; }
    /// var_dump((new C)->p);
    /// try { new B; } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// int(1)
    /// Undefined constant "UNRESOLVED"
    /// ```
    pub(crate) fn update_class_constants(&mut self, class: ClassId) -> Result<(), Thrown> {
        if self
            .class_data
            .rows
            .get(class)
            .is_some_and(|data| data.updated)
        {
            return Ok(());
        }
        let Some(decl) = self.symbols().class_decl(class) else {
            return Ok(());
        };
        if decl.kind == ClassKind::Enum {
            return Ok(());
        }
        let native = self.native_class(class);
        let updated_as_declared = !native && self.holds_no_expression(class);
        if !updated_as_declared && let Some(parent) = self.symbols().class_parent(class) {
            self.update_class_constants(parent)?;
        }
        if !native && !updated_as_declared {
            let constants: Vec<(ClassId, Box<[u8]>)> = self
                .symbols()
                .constants_in_order(class)
                .into_iter()
                .map(|(declaring, constant)| (declaring, constant.name.0.clone()))
                .collect();
            for (declaring, name) in constants {
                let value = self.evaluated_class_constant(declaring, &name)?;
                self.release(value);
            }
        }
        let statics: Vec<PropertyInfo> =
            self.symbols()
                .class_decl(class)
                .map_or_else(Vec::new, |decl| {
                    (0u32..)
                        .zip(&decl.properties)
                        .filter(|(_, property)| !property.is_instance)
                        .map(|(at, _)| PropertyInfo {
                            declaring: class,
                            at,
                        })
                        .collect()
                });
        for info in statics {
            self.lay_down_static(info)?;
        }
        self.evaluate_defaults(class)?;
        self.class_data.rows.row(class).updated = true;
        Ok(())
    }

    /// Whether php sets `ZEND_ACC_CONSTANTS_UPDATED` on the class as it
    /// declares it and links it: no constant of its table, no default of
    /// its instance properties and no `static` property of its own or
    /// its parents' is an expression still to evaluate, a value written
    /// out being none (`zend_compile_class_decl`, and
    /// `zend_do_inheritance_ex` clearing the bit for what it inherits).
    fn holds_no_expression(&self, class: ClassId) -> bool {
        let written = super::linking::is_literal;
        let constants = self
            .symbols()
            .constants_in_order(class)
            .iter()
            .all(|(_, constant)| written(&constant.value));
        let defaults = self
            .placed(class)
            .all(|(_, _, property)| property.default.as_ref().is_none_or(written));
        let statics = self.symbols().lineage(class).all(|here| {
            self.symbols().class_decl(here).is_none_or(|decl| {
                decl.properties
                    .iter()
                    .filter(|property| !property.is_instance)
                    .all(|property| property.default.as_ref().is_none_or(written))
            })
        });
        constants && defaults && statics
    }

    /// `zend_update_class_constant` over the constant `name` that
    /// `declaring` declares: its value from the row where the request
    /// evaluated it before, and otherwise the expression it was written
    /// as evaluated in `declaring`'s scope, held to its type, and kept.
    /// One more owner is the caller's. A value its type refuses is given
    /// up with the `TypeError`, as `zend_update_class_constant` gives up
    /// `tmp` (`Z/Zend/zend_API.c:1504`), and nothing is kept.
    pub(crate) fn evaluated_class_constant(
        &mut self,
        declaring: ClassId,
        name: &[u8],
    ) -> Result<Value, Thrown> {
        if let Some(value) = self.class_data.evaluated_constant(declaring, name) {
            self.addref(&value);
            return Ok(value);
        }
        let Some((expr, ty)) = self
            .symbols()
            .class_constant_of(declaring, name)
            .map(|(_, constant)| (constant.value.clone(), constant.ty.clone()))
        else {
            return Err(crate::api::raising::fatal(
                self,
                "InternalError: a class constant evaluated in a class whose table holds none of its name",
            ));
        };
        let evaluated = crate::api::call::eval_declared(self, &expr, Some(declaring))?;
        let value = match &ty {
            Some(ty) if ty.mask() != php_unit::decl::Type::MAY_BE_ANY => {
                match crate::ops::verify_class_constant_type(self, *evaluated, ty, declaring, name)
                {
                    // The owner goes on to the row that keeps the value,
                    // and the caller's is taken below; the float an int
                    // converts to holds nothing either way.
                    Ok(value) => {
                        evaluated.hand_on();
                        value
                    }
                    Err(thrown) => {
                        evaluated.give_up(self);
                        return Err(thrown);
                    }
                }
            }
            _ => evaluated.hand_on(),
        };
        self.addref(&value);
        self.class_data
            .rows
            .row(declaring)
            .constants
            .push((name.into(), value));
        // `zend_update_class_constant` destroys the expression once its
        // value stands in its place, and keeps it where the evaluation
        // or the type refused.
        self.constant_evaluated(declaring, name);
        Ok(value)
    }

    /// `zend_update_class_constant` over the constant `name` of `class`
    /// (`Z/Zend/zend_API.c:1504`), found as the constants table has it:
    /// the case's instance for an enum's case, and otherwise the value
    /// [`Request::evaluated_class_constant`] keeps, with no access
    /// checked. `Undefined constant` where the class has no such
    /// constant.
    pub(crate) fn updated_class_constant(
        &mut self,
        class: ClassId,
        name: &[u8],
    ) -> Result<Value, Thrown> {
        let Some((declaring, is_case)) = self
            .symbols()
            .class_constant_of(class, name)
            .map(|(declaring, constant)| (declaring, constant.is_case))
        else {
            let class_name = self.symbols().class_name(class);
            return Err(crate::api::raising::throw_error(
                self,
                "Error",
                &format!(
                    "Undefined constant {class_name}::{}",
                    String::from_utf8_lossy(name)
                ),
            ));
        };
        self.update_enum_constants(declaring)?;
        if is_case {
            return self.enum_case(declaring, name).map(|case| case.as_value());
        }
        self.evaluated_class_constant(declaring, name)
    }

    /// `zend_get_class_constant_ex`'s update of the constant `name` that
    /// `declaring` declares (`Z/Zend/zend_constants.c:400`): one not
    /// yet evaluated is marked while its expression is, and one found
    /// marked is php's `Cannot declare self-referencing constant`, the
    /// class named as the fetch was handed it; then
    /// [`Request::evaluated_class_constant`]. The opline's fetch marks
    /// nothing, so a cycle entered through it is refused one constant
    /// further in.
    ///
    /// ```php
    /// class A { const FOO = [self::BAR]; const BAR = [self::FOO]; }
    /// class Foo { const A = Foo::B; const B = Foo::A; }
    /// try { var_dump(A::FOO); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// try { echo Foo::A; } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// try { echo constant('Foo::B'); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// Cannot declare self-referencing constant self::BAR
    /// Cannot declare self-referencing constant Foo::B
    /// Cannot declare self-referencing constant Foo::B
    /// ```
    pub(crate) fn visited_class_constant(
        &mut self,
        declaring: ClassId,
        name: &[u8],
        class_name: &str,
    ) -> Result<Value, Thrown> {
        let data = self.class_data.rows.row(declaring);
        if data.constants.iter().any(|(held, _)| **held == *name) {
            return self.evaluated_class_constant(declaring, name);
        }
        if data.visiting.iter().any(|held| **held == *name) {
            return Err(crate::api::raising::throw_error(
                self,
                "Error",
                &format!(
                    "Cannot declare self-referencing constant {class_name}::{}",
                    String::from_utf8_lossy(name)
                ),
            ));
        }
        data.visiting.push(name.into());
        let evaluated = self.evaluated_class_constant(declaring, name);
        if let Some(data) = self.class_data.rows.get_mut(declaring) {
            data.visiting.retain(|held| **held != *name);
        }
        evaluated
    }

    /// The default properties table of `class` once the class is
    /// updated, each slot's value with one more owner the object's, as
    /// `_object_properties_init` copies the table (`ZVAL_COPY_PROP`).
    /// A string the declaration wrote is the interned one
    /// `zend_declare_typed_property` made of it ([`eval_declared`]), so
    /// the slot takes no owner and costs the object nothing, and a
    /// `static` property, a class constant and the readers of the
    /// default table read the same string.
    ///
    /// [`eval_declared`]: crate::api::call::eval_declared
    ///
    /// ```php
    /// $a = 'mine'; debug_zval_dump($a);
    /// class C { public $own = 'mine'; } $c = new C; debug_zval_dump($c); debug_zval_dump($c->own);
    /// function f($x = 'dflt') { debug_zval_dump($x); } f();
    /// class M { public $a = 'a long default'; public static $s = 'stat'; const K = 'kconst'; } $m = memory_get_usage(); $o = new M; var_dump(memory_get_usage() - $m);
    /// debug_zval_dump(M::$s, M::K, get_class_vars('M')['a'], (new ReflectionProperty('M', 'a'))->getDefaultValue());
    /// ```
    /// ```expect
    /// string(4) "mine" interned
    /// object(C)#1 (1) refcount(2){
    ///   ["own"]=>
    ///   string(4) "mine" interned
    /// }
    /// string(4) "mine" interned
    /// string(4) "dflt" interned
    /// int(56)
    /// string(4) "stat" interned
    /// string(6) "kconst" interned
    /// string(14) "a long default" interned
    /// string(14) "a long default" interned
    /// ```
    ///
    /// An array the declaration wrote, every key and value folded, is
    /// the unit's immutable array ([`ConstExpr::ImmutableArray`]), so a
    /// slot, a `static` and a constant copied from it take no owner, as
    /// opcache leaves php's table of defaults:
    ///
    /// [`ConstExpr::ImmutableArray`]: php_unit::decl::ConstExpr::ImmutableArray
    ///
    /// ```php
    /// class C { public $arr = ['x']; public $n = [[1], 'a']; public $q = [PHP_INT_SIZE]; public static $s = ['y', 'k' => 1]; const K = ['z']; }
    /// debug_zval_dump(new C);
    /// debug_zval_dump(C::$s, C::K, get_class_vars('C')['arr'], (new ReflectionProperty('C', 'arr'))->getDefaultValue());
    /// ```
    /// ```expect regime=immutable-literals
    /// object(C)#1 (3) refcount(1){
    ///   ["arr"]=>
    ///   array(1) packed interned {
    ///     [0]=>
    ///     string(1) "x" interned
    ///   }
    ///   ["n"]=>
    ///   array(2) packed interned {
    ///     [0]=>
    ///     array(1) packed interned {
    ///       [0]=>
    ///       int(1)
    ///     }
    ///     [1]=>
    ///     string(1) "a" interned
    ///   }
    ///   ["q"]=>
    ///   array(1) packed interned {
    ///     [0]=>
    ///     int(8)
    ///   }
    /// }
    /// array(2) interned {
    ///   [0]=>
    ///   string(1) "y" interned
    ///   ["k"]=>
    ///   int(1)
    /// }
    /// array(1) packed interned {
    ///   [0]=>
    ///   string(1) "z" interned
    /// }
    /// array(1) packed interned {
    ///   [0]=>
    ///   string(1) "x" interned
    /// }
    /// array(1) packed interned {
    ///   [0]=>
    ///   string(1) "x" interned
    /// }
    /// ```
    pub(super) fn default_properties(
        &mut self,
        class: ClassId,
    ) -> Result<Vec<Option<Value>>, Thrown> {
        self.update_class_constants(class)?;
        let defaults: Vec<Option<Value>> = self
            .class_data
            .rows
            .get(class)
            .map_or_else(Vec::new, |data| data.defaults.clone());
        for value in defaults.iter().flatten() {
            self.addref(value);
        }
        Ok(defaults)
    }

    /// `object_properties_init` over a new `object` of a class
    /// [`Request::update_class_constants`] updated: each slot its
    /// class's default with one more owner, the object's
    /// (`ZVAL_COPY_PROP`), read off the class's row in place; a typed
    /// slot with none stays `IS_UNDEF` and `IS_PROP_UNINIT`.
    pub(super) fn init_properties(&mut self, object: PhpObject) {
        let Some(data) = self.class_data.rows.get(object.class()) else {
            return;
        };
        let o = object.heap();
        for (slot, value) in (0u32..).zip(&data.defaults) {
            match value {
                Some(value) => {
                    value.to_zval().addref();
                    if let Some(at) = o.slot_ref(slot) {
                        at.write(value.to_zval());
                    }
                }
                None => o.set_slot_bit(slot, php_heap::PropBit::Uninit, true),
            }
        }
    }

    fn evaluate_defaults(&mut self, class: ClassId) -> Result<(), Thrown> {
        let done = self
            .class_data
            .rows
            .get(class)
            .map_or(0, |data| data.defaults.len());
        // The rows not yet evaluated, which is none once the class's
        // defaults stood.
        let rows: Vec<PropertyInfo> = self
            .placed_properties(class)
            .iter()
            .skip(done)
            .copied()
            .collect();
        for info in rows {
            // The default expression alone is handed on past what writes
            // the request, so it is what is cloned.
            let (default, typed) = self
                .property_decl(info)
                .map_or((None, false), |p| (p.default.clone(), p.ty.is_some()));
            let value = match default {
                Some(expr) => {
                    let declaring = Some(info.declaring);
                    let value = crate::api::call::eval_declared_made(self, &expr, declaring)?;
                    Some(self.strictly_held(value, info)?)
                }
                None if typed => None,
                None => Some(Value::null()),
            };
            self.class_data.rows.row(class).defaults.push(value);
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
        info: PropertyInfo,
    ) -> Result<Value, Thrown> {
        let Some((ty, name)) = self
            .property_decl(info)
            .and_then(|p| Some((p.ty.clone()?, p.name.0.clone())))
        else {
            return Ok(value);
        };
        let verdict =
            crate::ops::verify_property_type(self, &value, &ty, info.declaring, &name, true);
        crate::ops::landed(self, value, verdict)
    }
}

impl Symbols<&Request> {
    /// The loop of `zend_update_class_constants` over
    /// `default_properties_table`: each slot not yet evaluated, in slot
    /// order, its expression evaluated in the scope of the class that
    /// declared it and, where the property is typed, held to the type
    /// under the strict rule (`update_property`), php's
    /// `property initializers must always be evaluated with strict
    /// types`.
    /// `default_properties_table[OBJ_PROP_TO_NUM(prop->offset)]` of
    /// `class` for its instance property `name`, what `_property_string`
    /// prints: the default `zend_update_class_constants` evaluated, a
    /// view, once the class stands updated in this request; `None`
    /// before, where php's table still holds the expression, and for a
    /// typed slot with no default.
    ///
    /// ```php
    /// class C { public stdClass $a = FOO; public $i = N; }
    /// $r = new ReflectionProperty('C', 'i');
    /// define('FOO', new stdClass); define('N', 5);
    /// echo $r; new C; echo $r, new ReflectionProperty('C', 'a');
    /// ```
    /// ```expect
    /// Property [ public $i = N ]
    /// Property [ public $i = 5 ]
    /// Property [ public stdClass $a = object(stdClass) ]
    /// ```
    #[must_use]
    pub fn evaluated_default(self, class: ClassId, name: &[u8]) -> Option<Value> {
        let data = self
            .0
            .class_data
            .rows
            .get(class)
            .filter(|data| data.updated)?;
        let (at, _, _) = self.0.placed_named(class, name)?;
        data.defaults.get(at).copied().flatten()
    }
}
