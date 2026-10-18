//! The class constants of a request: `zend_get_class_constant_ex`
//! and the `FETCH_CLASS_CONSTANT` opline's lookup, a constant's
//! visibility, its evaluation with `CONST_IS_RECURSIVE` against itself,
//! its deprecation, and an enum's cases, each made once and handed back
//! by identity, with the check `zend_enum_build_backed_enum_table` makes
//! of a backed enum's values. The enum cases, the backed enums checked
//! and the constants being worded are [`ClassConstants`]'s, whose fields
//! no file but this one reads.

use php_link::Row;
use php_unit::decl::{ClassConstDecl, ClassKind, Visibility};

use crate::api::call::write_prop;
use crate::api::raising::Thrown;
use crate::api::values::{ClassId, Owner, PhpObject, TypeTag, Value};

use super::Request;

/// The enum cases, the backed enums checked and the constants being
/// worded, whose fields no file but this one reads.
#[derive(Default)]
pub(super) struct ClassConstants {
    /// `zend_enum_get_case`'s slot (`Z/Zend/zend_enum.c:410`): one
    /// instance per case of each enum, made on the first read of the
    /// case and handed back by identity from then on, so that
    /// `E::A === E::A`. php keeps it in the class's constant table,
    /// which outlives the request; the linked tables here are shared
    /// and immutable and an object is neither, so the slot is the
    /// request's and the cases go with it.
    enum_cases: Vec<((ClassId, Vec<u8>), PhpObject)>,
    /// The backed enums whose case values `zend_enum_build_backed_enum_table`
    /// has held to their backing type and found no two alike, php's
    /// `ZEND_ACC_CONSTANTS_UPDATED` on each; a check that failed is made
    /// again on the next read, as php's is.
    enums_updated: Vec<ClassId>,
    /// `CONST_IS_RECURSIVE` over the constants a deprecation is being
    /// worded for, so an attribute naming the constant it marks reads it
    /// without raising again (GH-18463): the declaring class and the
    /// name of a class constant, a program's alone, or the name of a
    /// global one.
    deprecating: Vec<(Option<ClassId>, Box<[u8]>)>,
}

impl Request {
    /// The enum cases given up, a debug build's teardown: php's case
    /// objects stand in the class's constant table, which
    /// `zend_shutdown_executor_values` releases before the object store
    /// (`Z/Zend/zend_execute_API.c:334-346`). A release build leaves them
    /// to the heap's mass release.
    #[cfg(debug_assertions)]
    pub(super) fn free_enum_cases(&mut self) {
        for (_, case) in std::mem::take(&mut self.class_constants.enum_cases) {
            self.release(case.as_value());
        }
    }

    /// `zend_hash_find_ptr(CE_CONSTANTS_TABLE(ce), name)`: the constant
    /// under the name, byte for byte, in the class's linked record,
    /// which holds the class's own constants, then what it inherited
    /// from its parent, a parent's private one left out
    /// (`do_inherit_class_constant`), and from its interfaces. The
    /// answer is the class that declared it, which is the scope the
    /// expression is evaluated in and the scope its visibility is
    /// checked against, and the declaration itself. One lookup, the
    /// name compared through the row, so nothing is allocated or cloned.
    ///
    /// A constant read through three levels and through an interface's
    /// parent, and a parent's private one shadowed by the child's own:
    ///
    /// ```php
    /// interface I { const IC = "I::IC"; }
    /// interface J extends I { const JC = "J::JC"; }
    /// class G { const A = "G::A"; private const P = "G::P"; const OV = "G::OV"; function gp() { return static::P; } }
    /// class P extends G implements J { const OV = "P::OV"; }
    /// class C extends P { private const P = "C::P"; function cp() { return self::P; } }
    /// echo C::A, " ", C::OV, " ", C::IC, " ", C::JC, " ", (new C)->cp(), " ", (new G)->gp(), "\n";
    /// var_dump(defined('C::IC'), defined('C::P'), defined('P::P'));
    /// try { echo P::P; } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// try { echo (new C)->gp(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// G::A P::OV I::IC J::JC C::P G::P
    /// bool(true)
    /// bool(false)
    /// bool(false)
    /// Undefined constant P::P
    /// Cannot access private constant C::P
    /// ```
    pub fn class_constant_of(
        &self,
        class: ClassId,
        name: &[u8],
    ) -> Option<(ClassId, &ClassConstDecl)> {
        let row = self.linked_class(class)?.constants.find(name, |row| {
            self.row_constant(class, *row)
                .map_or(&[][..], |(_, constant)| &constant.name.0)
        })?;
        self.row_constant(class, *row)
    }

    /// The constant a row of `class`'s constants table is, beside the
    /// class that declared it.
    fn row_constant(
        &self,
        class: ClassId,
        row: Row<ClassId>,
    ) -> Option<(ClassId, &ClassConstDecl)> {
        let holder = row.holder_in(class);
        let decl = self.class_decl(holder)?;
        Some((holder, decl.constants.get(usize::try_from(row.at).ok()?)?))
    }

    /// `CE_CONSTANTS_TABLE(ce)` of a linked class in php's order, each
    /// constant beside the class that declared it.
    pub fn constants_in_order(&self, class: ClassId) -> Vec<(ClassId, &ClassConstDecl)> {
        let Some(record) = self.linked_class(class) else {
            return Vec::new();
        };
        record
            .constants
            .rows()
            .iter()
            .filter_map(|row| self.row_constant(class, *row))
            .collect()
    }

    /// `zend_fetch_class_constant`'s lookup (`Z/Zend/zend_constants.c:355`):
    /// the constant `name` of `class` from the declaration's table,
    /// `Undefined constant C::A` where it has none, `Cannot access
    /// private constant C::A` where `zend_verify_const_access` refuses
    /// the executing scope, and otherwise its value, the constant
    /// expression the declaration kept evaluated in the class's scope
    /// (`zval_update_constant_ex`), one owner the caller's. The class
    /// is named in the error as the opline names it, `ce->name`.
    pub(crate) fn class_constant(&mut self, class: ClassId, name: &[u8]) -> Result<Value, Thrown> {
        let class_name = self.class_name(class);
        let scope = self.executed_scope();
        self.class_constant_from(class, name, &class_name, scope, ConstantFetch::Opline)
    }

    /// [`Request::class_constant`] with the class named in the error as
    /// `class_name` spells it: `zend_get_class_constant_ex` words
    /// `Undefined constant a::Y` with the name it was handed, which is
    /// the string `constant()` or `defined()` was given, and not the
    /// linked class's own.
    pub(crate) fn class_constant_shown(
        &mut self,
        class: ClassId,
        name: &[u8],
        class_name: &str,
    ) -> Result<Value, Thrown> {
        let scope = self.executed_scope();
        self.class_constant_from(class, name, class_name, scope, ConstantFetch::Lookup)
    }

    /// `zend_get_class_constant_ex` with the `scope` a constant
    /// expression is evaluated in, `zend_ast_evaluate`'s: the access is
    /// checked against it and not against the scope executing, so an
    /// attribute's `parent::X` reads a protected constant its class may.
    ///
    /// ```php
    /// #[Attribute(parent::x)]
    /// class x extends y {}
    /// class y { protected const x = Attribute::TARGET_CLASS; }
    /// #[x]
    /// class z {}
    /// var_dump((new ReflectionClass(z::class))->getAttributes()[0]->newInstance());
    /// ```
    /// ```expect
    /// object(x)#1 (0) {
    /// }
    /// ```
    pub(crate) fn class_constant_from(
        &mut self,
        class: ClassId,
        name: &[u8],
        class_name: &str,
        scope: Option<ClassId>,
        fetch: ConstantFetch,
    ) -> Result<Value, Thrown> {
        let shown = String::from_utf8_lossy(name).into_owned();
        let Some((declaring, constant)) = self.class_constant_of(class, name) else {
            return Err(crate::api::raising::throw_error(
                self,
                "Error",
                &format!("Undefined constant {class_name}::{shown}"),
            ));
        };
        let (visibility, is_case) = (constant.visibility, constant.is_case);
        let deprecated = crate::attributes::Flags::of(&constant.attributes)
            .deprecated
            .then(|| constant.clone());
        let asked = class;
        let class = declaring;
        if !self.may_access_from(class, visibility, scope) {
            let word = match visibility {
                Visibility::Public => "public",
                Visibility::Protected => "protected",
                Visibility::Private => "private",
            };
            return Err(crate::api::raising::throw_error(
                self,
                "Error",
                &format!("Cannot access {word} constant {class_name}::{shown}"),
            ));
        }
        // A constant a trait declares is the composing class's to hand
        // out, never the trait's own.
        if self
            .class_decl(asked)
            .is_some_and(|decl| decl.kind == ClassKind::Trait)
        {
            return Err(crate::api::raising::throw_error(
                self,
                "Error",
                &format!("Cannot access trait constant {class_name}::{shown} directly"),
            ));
        }
        if let Some(constant) = deprecated {
            self.deprecated_class_constant(class, name, &constant)?;
        }
        // `ZEND_FETCH_CLASS_CONSTANT` updates a backed enum's constants
        // whole before it reads one (`zend_update_class_constants`), and
        // `zend_fetch_class_constant`'s `ZEND_CLASS_CONST_IS_CASE` arm
        // hands out a case as the enum's instance for it and not as the
        // expression it was written with.
        self.update_enum_constants(class)?;
        if is_case {
            return self.enum_case(class, name).map(|case| case.as_value());
        }
        match fetch {
            ConstantFetch::Opline => self.evaluated_class_constant(class, name),
            ConstantFetch::Lookup => self.visited_class_constant(class, name, class_name),
        }
    }

    /// `zend_get_class_constant_ex` under `ZEND_FETCH_CLASS_SILENT`, as
    /// `defined()` asks it: whether `class` has a constant `name` the
    /// executed scope may reach, not through a trait, with nothing
    /// raised and nothing evaluated.
    pub(crate) fn class_constant_defined(&self, class: ClassId, name: &[u8]) -> bool {
        let trait_asked = self
            .class_decl(class)
            .is_some_and(|decl| decl.kind == ClassKind::Trait);
        self.class_constant_of(class, name)
            .is_some_and(|(declaring, constant)| {
                !trait_asked && self.may_access(declaring, constant.visibility)
            })
    }

    /// `ZEND_ACC_DEPRECATED` on a class constant, as
    /// `ZEND_FETCH_CLASS_CONSTANT` and `zend_get_class_constant_ex` test
    /// it after the access checks and before the value is evaluated:
    /// [`crate::attributes::deprecated_class_constant`], once per fetch,
    /// and not again while its own attribute's arguments are read for
    /// the message (`CONST_IS_RECURSIVE`, a program's class alone).
    fn deprecated_class_constant(
        &mut self,
        declaring: ClassId,
        name: &[u8],
        constant: &ClassConstDecl,
    ) -> Result<(), Thrown> {
        let user = !self.native_class(declaring);
        if user && !self.protect_recursion(Some(declaring), name) {
            return Ok(());
        }
        let raised =
            crate::attributes::deprecated_class_constant(self, declaring, name, constant, !user);
        if user {
            self.class_constants.deprecating.pop();
        }
        raised
    }

    /// `CONST_PROTECT_RECURSION` over a constant, `class` none for a
    /// global one: `false`, and nothing held, where it is held already,
    /// which is `CONST_IS_RECURSIVE`; the caller pops what it held.
    pub(crate) fn protect_recursion(&mut self, class: Option<ClassId>, name: &[u8]) -> bool {
        if self
            .class_constants
            .deprecating
            .iter()
            .any(|(held_class, held)| *held_class == class && **held == *name)
        {
            return false;
        }
        self.class_constants.deprecating.push((class, name.into()));
        true
    }

    /// `CONST_UNPROTECT_RECURSION`: the last constant
    /// [`Request::protect_recursion`] held let go.
    pub(crate) fn unprotect_recursion(&mut self) {
        self.class_constants.deprecating.pop();
    }

    /// `zend_enum_build_backed_enum_table` (`Z/Zend/zend_enum.c:197`),
    /// which `zend_update_class_constants` runs for a backed enum a
    /// program declared: each case's value evaluated and held to the
    /// backing type, php's `TypeError` for one of another type, and two
    /// cases of one value php's `Error`. A native enum's values are the
    /// stub's and are not checked, as php checks only a user class's.
    ///
    /// ```php
    /// enum Foo: int { case Bar = 'bar'; }
    /// try { var_dump(Foo::Bar); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// enum Dup: int { case A = 1; case B = 1; }
    /// try { Dup::from(1); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// Enum case type string does not match enum backing type int
    /// Duplicate value in enum Dup for cases A and B
    /// ```
    pub(crate) fn update_enum_constants(&mut self, class: ClassId) -> Result<(), Thrown> {
        if self.class_constants.enums_updated.contains(&class) || self.native_class(class) {
            return Ok(());
        }
        let Some(decl) = self.class_decl(class) else {
            return Ok(());
        };
        if decl.kind != ClassKind::Enum || decl.backing == php_unit::decl::EnumBacking::Pure {
            return Ok(());
        }
        let backing = decl.backing;
        let cases: Vec<(Vec<u8>, php_unit::decl::ConstExpr)> = decl
            .constants
            .iter()
            .filter(|constant| constant.is_case)
            .map(|constant| (constant.name.0.to_vec(), constant.value.clone()))
            .collect();
        let enum_name = self.class_name(class);
        let mut seen: Vec<(Value, Vec<u8>)> = Vec::new();
        let mut outcome = Ok(());
        for (name, expr) in cases {
            let value = match crate::api::call::eval_declared_made(self, &expr, Some(class)) {
                Ok(value) => value,
                Err(thrown) => {
                    outcome = Err(thrown);
                    break;
                }
            };
            let wanted = match backing {
                php_unit::decl::EnumBacking::Int => TypeTag::Int,
                _ => TypeTag::String,
            };
            if value.tag() != wanted {
                let given = value.type_name();
                let want = if wanted == TypeTag::Int {
                    "int"
                } else {
                    "string"
                };
                self.release(value);
                outcome = Err(crate::api::raising::throw_error(
                    self,
                    "TypeError",
                    &format!("Enum case type {given} does not match enum backing type {want}"),
                ));
                break;
            }
            // Each value is an int or a string by the test above, the
            // key php's `backed_enum_table` is built on, so a duplicate
            // is an equal value.
            if let Some((_, earlier)) = seen.iter().find(|(held, _)| *held == value) {
                let earlier = String::from_utf8_lossy(earlier).into_owned();
                self.release(value);
                outcome = Err(crate::api::raising::throw_error(
                    self,
                    "Error",
                    &format!(
                        "Duplicate value in enum {enum_name} for cases {earlier} and {}",
                        String::from_utf8_lossy(&name)
                    ),
                ));
                break;
            }
            seen.push((value, name));
        }
        for (value, _) in seen {
            self.release(value);
        }
        outcome?;
        self.class_constants.enums_updated.push(class);
        Ok(())
    }

    /// Whether `class` is one the engine was built with, a native's,
    /// rather than one a program declared.
    pub(super) fn native_class(&self, class: ClassId) -> bool {
        self.native_class_at(class).is_some()
    }

    /// `zend_enum_get_case` (`Z/Zend/zend_enum.c:410`): the one instance
    /// of `class` standing for the case `name`, made on the first read
    /// and handed back by identity from then on, its `name` the case's
    /// own and, for a backed enum, its `value` the backing value the
    /// declaration carries. One more owner is taken for the caller, the
    /// slot keeping its own, as php's constant table keeps one. A string
    /// value is the unit's interned string, as opcache persists the
    /// case's constant (design §5.2):
    ///
    /// ```php
    /// enum E: string { case X = 'ex'; } debug_zval_dump(E::X->value);
    /// ```
    /// ```expect regime=immutable-literals
    /// string(2) "ex" interned
    /// ```
    ///
    /// The first read costs the case's object, `zend_objects_new` at the
    /// enum's size, and nothing else: the case's constant expression is
    /// the unit's and stays, as opcache's persisted one does, where php
    /// -n frees the request's copy of it as it evaluates the case, 96
    /// bytes for a pure case, and so reads 40 bytes less than nothing
    /// (`trace:immutable-literal-arrays`):
    ///
    /// ```php
    /// enum E { case A; case B; }
    /// $m = memory_get_usage(); $x = E::A; echo memory_get_usage() - $m, "\n";
    /// $m = memory_get_usage(); $y = E::B; echo memory_get_usage() - $m, "\n";
    /// $m = memory_get_usage(); $z = E::A; echo memory_get_usage() - $m, "\n";
    /// ```
    /// ```expect regime=immutable-literals
    /// 56
    /// 56
    /// 0
    /// ```
    pub(crate) fn enum_case(&mut self, class: ClassId, name: &[u8]) -> Result<PhpObject, Thrown> {
        if let Some((_, case)) = self
            .class_constants
            .enum_cases
            .iter()
            .find(|((held, under), _)| *held == class && under == name)
        {
            let case = *case;
            self.addref(&case.as_value());
            return Ok(case);
        }
        let Some(value) = self
            .class_constant_of(class, name)
            .map(|(_, constant)| constant.value.clone())
        else {
            let class = self.class_name(class);
            return Err(crate::api::raising::throw_error(
                self,
                "Error",
                &format!(
                    "Undefined constant {class}::{}",
                    String::from_utf8_lossy(name)
                ),
            ));
        };
        let backing = self
            .class_decl(class)
            .map_or(php_unit::decl::EnumBacking::Pure, |decl| decl.backing);
        let case = self.new_object(class)?;
        // The case's name is the constant's, an interned string, as
        // `zend_enum_new` copies `case_name`.
        let spelled = self.declared_string(name)?;
        let spelled = Owner::take(self, Value::string(spelled));
        write_prop(self, &case, b"name", spelled)?;
        if backing != php_unit::decl::EnumBacking::Pure {
            let value = crate::api::call::eval_declared_made(self, &value, Some(class))?;
            let value = Owner::adopt(value);
            write_prop(self, &case, b"value", value)?;
        }
        self.class_constants
            .enum_cases
            .push(((class, name.to_vec()), case));
        self.addref(&case.as_value());
        Ok(case)
    }
}

/// Which of php's two reads of a class constant a fetch is, which
/// differ in one thing: whether the constant is marked while its
/// expression is evaluated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ConstantFetch {
    /// `ZEND_FETCH_CLASS_CONSTANT`'s handler, which updates the constant
    /// with no mark (`zend_update_class_constant`).
    Opline,
    /// `zend_get_class_constant_ex`, a constant expression's and
    /// `constant()`'s, which marks the constant `IS_CONSTANT_VISITED`
    /// while it is evaluated and refuses one it finds marked
    /// ([`Request::visited_class_constant`]).
    Lookup,
}
