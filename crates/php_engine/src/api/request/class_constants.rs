//! The class constants of a request: `zend_get_class_constant_ex`
//! and the `FETCH_CLASS_CONSTANT` opline's lookup, a constant's
//! visibility, its evaluation with `CONST_IS_RECURSIVE` against itself,
//! its deprecation, and an enum's cases, each made once and handed back
//! by identity, with the check `zend_enum_build_backed_enum_table` makes
//! of a backed enum's values. The enum cases, the backed enums checked
//! and the constants being worded are [`ClassConstants`]'s, whose fields
//! no file but this one reads.

use php_link::Row;
use php_unit::decl::{ClassConstDecl, ClassKind, ConstExpr, Visibility};

use crate::api::raising::Thrown;
use crate::api::values::{ClassId, Key, Owner, PhpArray, PhpObject, TypeTag, Value};

use super::Request;
use super::Symbols;

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
    /// The backed enums a program declared whose constants
    /// `zend_update_class_constants` has updated, php's
    /// `ZEND_ACC_CONSTANTS_UPDATED` on each, beside the
    /// `backed_enum_table` `zend_enum_build_backed_enum_table` built of
    /// them on the request's heap, each case's name keyed by its value; a
    /// build that failed is made again on the next read, as php's is.
    enums_updated: Vec<(ClassId, PhpArray)>,
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
        for (_, table) in std::mem::take(&mut self.class_constants.enums_updated) {
            self.release(Value::array(table));
        }
        for (_, case) in std::mem::take(&mut self.class_constants.enum_cases) {
            self.release(case.as_value());
        }
    }

    /// The constant a row of `class`'s constants table is, beside the
    /// class that declared it.
    fn row_constant(
        &self,
        class: ClassId,
        row: Row<ClassId>,
    ) -> Option<(ClassId, &ClassConstDecl)> {
        self.class_lookup().row_constant(class, row)
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
        let class_name = self.symbols().class_name(class);
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
        let Some((declaring, constant)) = self.symbols().class_constant_of(class, name) else {
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
            .symbols()
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
        // whole before it reads one (`zend_update_class_constants`), where
        // `zend_get_class_constant_ex` updates the one it reads, so an
        // expression of the enum's that names one of its constants reads
        // it without updating the enum again; and
        // `zend_fetch_class_constant`'s `ZEND_CLASS_CONST_IS_CASE` arm
        // hands out a case as the enum's instance for it and not as the
        // expression it was written with.
        if fetch == ConstantFetch::Opline {
            self.update_enum_constants(class)?;
        }
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
            .symbols()
            .class_decl(class)
            .is_some_and(|decl| decl.kind == ClassKind::Trait);
        self.symbols()
            .class_constant_of(class, name)
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

    /// `zend_update_class_constants` over a backed enum a program
    /// declared, which the first read of one of its cases runs: every
    /// case made, then `zend_enum_build_backed_enum_table`
    /// (`Z/Zend/zend_enum.c:197`) over their values, each held to the
    /// backing type, php's `TypeError` for one of another type, and two
    /// cases of one value php's `Error`. The table keys each case by its
    /// value and holds an owner of a string one. A native enum's values
    /// are the stub's and are not checked, as php checks only a user
    /// class's.
    ///
    /// An `eval`'d enum's string value is the string php's scanner made,
    /// which its compile left in the case's constant expression, so the
    /// case and the table hold it as two owners; a cached file's is the
    /// one opcache's persist interned (the witness of
    /// [`Request::enum_case`]). Its first case read makes every case:
    ///
    /// ```php
    /// eval('enum E: string { case A = "abcdefgh"; case B = "x"; case C = "strlen"; }');
    /// debug_zval_dump(E::A->value, E::B->value, E::C->value);
    /// $v = E::A->value; debug_zval_dump($v);
    /// eval('enum I: int { case A = 1; case B = 2; }');
    /// $o = new stdClass; $b = I::B; var_dump(spl_object_id(I::A) - spl_object_id($o));
    /// ```
    /// ```expect regime=jit-off
    /// string(8) "abcdefgh" refcount(3)
    /// string(1) "x" interned
    /// string(6) "strlen" refcount(3)
    /// string(8) "abcdefgh" refcount(4)
    /// int(1)
    /// ```
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
    ///
    /// Its constants are updated with its cases, so a constant that
    /// refuses refuses the first opline's read of a case, `from()` and
    /// `tryFrom()`, each time until it evaluates, where `constant()`,
    /// `zend_get_class_constant_ex`, updates the one it names; a constant
    /// naming one of the enum's own is read without updating the enum
    /// again:
    ///
    /// ```php
    /// enum S: string { case A = "a"; const K = Undef::X; }
    /// foreach ([fn () => S::from("a"), fn () => S::A, fn () => S::tryFrom("b"), fn () => constant('S::A')] as $read) {
    ///     try { var_dump($read()); } catch (Error $e) { echo get_class($e), ': ', $e->getMessage(), "\n"; }
    /// }
    /// enum T: int { case A = 1; const K = self::A; const L = T::K; }
    /// var_dump(T::from(1), T::L);
    /// ```
    /// ```expect exit=0
    /// Error: Class "Undef" not found
    /// Error: Class "Undef" not found
    /// Error: Class "Undef" not found
    /// enum(S::A)
    /// enum(T::A)
    /// enum(T::A)
    /// ```
    pub(crate) fn update_enum_constants(&mut self, class: ClassId) -> Result<(), Thrown> {
        if self
            .class_constants
            .enums_updated
            .iter()
            .any(|(held, _)| *held == class)
            || self.native_class(class)
        {
            return Ok(());
        }
        let Some(decl) = self.symbols().class_decl(class) else {
            return Ok(());
        };
        if decl.kind != ClassKind::Enum || decl.backing == php_unit::decl::EnumBacking::Pure {
            return Ok(());
        }
        let backing = decl.backing;
        let names = crate::enums::case_names(self, class);
        // Every case's value is had before the table is built, as
        // `zend_update_class_constants` updates every constant first: a
        // case made already by its own read, or by `cases()`, which
        // updates its constants one at a time, keeps its value.
        let mut values: Vec<(Vec<u8>, bool, Value)> = Vec::with_capacity(names.len());
        let mut outcome = Ok(());
        for name in names {
            let made = self.held_case(class, &name);
            let value = match made {
                Some(case) => {
                    let value = case.prop(self, b"value").unwrap_or_default();
                    self.addref(&value);
                    Ok(value)
                }
                None => self.case_value(class, &name),
            };
            match value {
                Ok(value) => values.push((name, made.is_some(), value)),
                Err(thrown) => {
                    outcome = Err(thrown);
                    break;
                }
            }
        }
        if outcome.is_ok() {
            outcome = self.backed_values_held(class, backing, &values);
        }
        if let Err(thrown) = outcome {
            for (_, _, value) in values {
                self.release(value);
            }
            return Err(thrown);
        }
        for (name, made, value) in values {
            if made {
                self.release(value);
                continue;
            }
            let case = self.made_case(class, &name, Some(value))?;
            self.release(case.as_value());
        }
        // Its other constants with them, in the table's order, each
        // expression's copy given back as it is evaluated.
        let constants: Vec<(ClassId, Box<[u8]>)> = (self.symbols().constants_in_order(class))
            .into_iter()
            .filter(|(_, constant)| !constant.is_case)
            .map(|(declaring, constant)| (declaring, constant.name.0.clone()))
            .collect();
        for (declaring, name) in constants {
            let value = self.evaluated_class_constant(declaring, &name)?;
            self.release(value);
        }
        let table = self.backed_enum_table(class)?;
        self.class_constants.enums_updated.push((class, table));
        Ok(())
    }

    /// `zend_enum_build_backed_enum_table` (`Z/Zend/zend_enum.c:197`)
    /// over the cases of `class`, every one made: a table `emalloc`'d
    /// and `zend_hash_init`ed empty, then each case's name, in the order
    /// the constants table holds them, added at its value, an int as
    /// `zend_hash_index_add_new` adds it and a string as
    /// `zend_hash_add_new` does, its own owner of the key taken. The
    /// table is the request's, as php's is the class's until the request
    /// ends. An enum's first `from()` lays it down, beside each case
    /// it makes, a cached file's and an `eval`'s alike, and an int
    /// enum's values under eight lay a packed table where a string's
    /// and a larger int's lay a hash:
    ///
    /// ```php
    /// function m($c) { $m = memory_get_usage(); eval($c); return memory_get_usage() - $m; }
    /// m('$q = 1;');
    /// enum G: string { case A = "a"; case B = "b"; }
    /// enum H: int { case A = 1; case B = 2; }
    /// enum K: int { case A = 100; case B = 200; }
    /// echo m('$GLOBALS["w"] = 1;'), "\n";
    /// echo m('$GLOBALS["g"] = G::from("b");'), "\n";
    /// echo m('$GLOBALS["h"] = H::from(2);'), "\n";
    /// echo m('$GLOBALS["k"] = K::A;'), "\n";
    /// ```
    /// ```expect regime=jit-off
    /// 512
    /// 536
    /// 376
    /// 536
    /// ```
    fn backed_enum_table(&mut self, class: ClassId) -> Result<PhpArray, Thrown> {
        let mut table = PhpArray::new(self)?.hand_on();
        for name in crate::enums::case_names(self, class) {
            let row = self.held_case(class, &name).and_then(|case| {
                let (spelled, value) = (case.slot(CASE_NAME)?, case.slot(CASE_VALUE)?);
                let key = match (value.as_int(), value.as_php_string()) {
                    (Some(n), _) => Key::Int(n),
                    (None, Some(s)) => Key::Str(s),
                    (None, None) => return None,
                };
                Some((spelled, key))
            });
            let Some((spelled, key)) = row else {
                self.release(Value::array(table));
                return Err(crate::api::raising::fatal(
                    self,
                    "InternalError: a backed enum's table built over a case not made, or one whose value is no int or string",
                ));
            };
            let key = match key {
                Key::Int(n) => Owner::index(n),
                Key::Str(s) => {
                    self.addref(&Value::string(s));
                    Owner::adopt(s).into_key()
                }
            };
            self.addref(&spelled);
            if let Err(thrown) = table.add_new(self, key, Owner::adopt(spelled)) {
                self.release(Value::array(table));
                return Err(thrown);
            }
        }
        Ok(table)
    }

    /// The case of `class` whose value is `value`, read off the
    /// `backed_enum_table` its constants' update built, as
    /// `zend_enum_get_case_by_value` finds it.
    pub(crate) fn backed_case_named(&self, class: ClassId, value: &Value) -> ByValue {
        let Some((_, table)) =
            (self.class_constants.enums_updated.iter()).find(|(held, _)| *held == class)
        else {
            return ByValue::Unbuilt;
        };
        let key = match (value.as_int(), value.as_php_string()) {
            (Some(n), _) => Key::Int(n),
            (None, Some(s)) => Key::Str(s),
            (None, None) => return ByValue::Missing,
        };
        table.get(&key).map_or(ByValue::Missing, ByValue::Named)
    }

    /// `zend_enum_build_backed_enum_table`'s two refusals over the case
    /// values of `class`, by name: php's `TypeError` for a value not of
    /// the backing type, and its `Error` for two cases of one value.
    fn backed_values_held(
        &mut self,
        class: ClassId,
        backing: php_unit::decl::EnumBacking,
        values: &[(Vec<u8>, bool, Value)],
    ) -> Result<(), Thrown> {
        let wanted = match backing {
            php_unit::decl::EnumBacking::Int => TypeTag::Int,
            _ => TypeTag::String,
        };
        for (at, (name, _, value)) in values.iter().enumerate() {
            if value.tag() != wanted {
                let given = value.type_name();
                let want = if wanted == TypeTag::Int {
                    "int"
                } else {
                    "string"
                };
                return Err(crate::api::raising::throw_error(
                    self,
                    "TypeError",
                    &format!("Enum case type {given} does not match enum backing type {want}"),
                ));
            }
            // Each value is an int or a string by the test above, the
            // key php's `backed_enum_table` is built on, so a duplicate
            // is an equal value.
            let earlier = values.get(..at).unwrap_or_default();
            if let Some((earlier, _, _)) = earlier.iter().find(|(_, _, held)| held == value) {
                let enum_name = self.symbols().class_name(class);
                return Err(crate::api::raising::throw_error(
                    self,
                    "Error",
                    &format!(
                        "Duplicate value in enum {enum_name} for cases {} and {}",
                        String::from_utf8_lossy(earlier),
                        String::from_utf8_lossy(name)
                    ),
                ));
            }
        }
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
    /// the unit's and stays, as opcache's persisted one does (an `eval`'d
    /// enum's goes, [`Request::constant_evaluated`]), where php
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
        if let Some(case) = self.held_case(class, name) {
            self.addref(&case.as_value());
            return Ok(case);
        }
        let backing = self
            .symbols()
            .class_decl(class)
            .map_or(php_unit::decl::EnumBacking::Pure, |decl| decl.backing);
        let value = if backing == php_unit::decl::EnumBacking::Pure {
            if self.symbols().class_constant_of(class, name).is_none() {
                return Err(self.undefined_case(class, name));
            }
            None
        } else {
            Some(self.case_value(class, name)?)
        };
        self.made_case(class, name, value)
    }

    /// The instance the request keeps for the case `name` of `class`,
    /// where a read made it, as a view.
    fn held_case(&self, class: ClassId, name: &[u8]) -> Option<PhpObject> {
        self.class_constants
            .enum_cases
            .iter()
            .find(|((held, under), _)| *held == class && under == name)
            .map(|(_, case)| *case)
    }

    /// The backing value of the case `name` of `class`, with one owner:
    /// for an enum a unit compiled for this request alone declared, a
    /// string written as one is the string php's scanner made, which
    /// its compile left in the case's constant expression as it stood,
    /// where opcache's persist interns a cached file's.
    fn case_value(&mut self, class: ClassId, name: &[u8]) -> Result<Value, Thrown> {
        let Some(expr) = self
            .symbols()
            .class_constant_of(class, name)
            .map(|(_, constant)| constant.value.clone())
        else {
            return Err(self.undefined_case(class, name));
        };
        match &expr {
            ConstExpr::Str(bytes) if self.declared_for_the_request(class) => {
                Ok(Value::string(self.string(bytes)?))
            }
            expr => crate::api::call::eval_declared_made(self, expr, Some(class)),
        }
    }

    /// `zend_enum_new`: the instance standing for the case `name`, its
    /// `name` the constant's interned name and its `value`, for a backed
    /// enum, `value`, whose owner it takes; kept in the request's slot,
    /// with one more owner for the caller.
    fn made_case(
        &mut self,
        class: ClassId,
        name: &[u8],
        value: Option<Value>,
    ) -> Result<PhpObject, Thrown> {
        let case = match self.new_object(class) {
            Ok(case) => case,
            Err(thrown) => {
                if let Some(value) = value {
                    self.release(value);
                }
                return Err(thrown);
            }
        };
        // The case's name is the constant's, as `zend_enum_new` copies
        // `case_name` into `OBJ_PROP_NUM(zobj, 0)` and the value into
        // the slot after it, no property looked up by name.
        let spelled = match self.declared_string(name) {
            Ok(spelled) => spelled.into_value(),
            Err(thrown) => {
                if let Some(value) = value {
                    self.release(value);
                }
                self.release(case.as_value());
                return Err(thrown);
            }
        };
        // A slot takes the value it is handed, written or given back; an
        // error leaves the case, and a value not yet handed on, to go.
        if let Err(thrown) = case.set_slot(self, CASE_NAME, spelled) {
            if let Some(value) = value {
                self.release(value);
            }
            self.release(case.as_value());
            return Err(thrown);
        }
        if let Some(value) = value
            && let Err(thrown) = case.set_slot(self, CASE_VALUE, Owner::adopt(value))
        {
            self.release(case.as_value());
            return Err(thrown);
        }
        self.class_constants
            .enum_cases
            .push(((class, name.to_vec()), case));
        self.addref(&case.as_value());
        // `zval_update_constant_with_ctx` destroys the constant's
        // expression once the case stands in its place.
        self.constant_evaluated(class, name);
        Ok(case)
    }

    /// php's `Error` for a case `class` does not declare.
    fn undefined_case(&mut self, class: ClassId, name: &[u8]) -> Thrown {
        let class = self.symbols().class_name(class);
        crate::api::raising::throw_error(
            self,
            "Error",
            &format!(
                "Undefined constant {class}::{}",
                String::from_utf8_lossy(name)
            ),
        )
    }
}

impl<'r> Symbols<&'r Request> {
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
        self,
        class: ClassId,
        name: &[u8],
    ) -> Option<(ClassId, &'r ClassConstDecl)> {
        self.0.class_lookup().class_constant_of(class, name)
    }

    /// `CE_CONSTANTS_TABLE(ce)` of a linked class in php's order, each
    /// constant beside the class that declared it.
    pub fn constants_in_order(self, class: ClassId) -> Vec<(ClassId, &'r ClassConstDecl)> {
        let Some(record) = self.0.linked_class(class) else {
            return Vec::new();
        };
        record
            .constants
            .rows()
            .iter()
            .filter_map(|row| self.0.row_constant(class, *row))
            .collect()
    }
}

/// The slot of a case's object `zend_enum_new` writes its name into,
/// `OBJ_PROP_NUM(zobj, 0)`.
const CASE_NAME: u32 = 0;
/// The slot it writes a backed case's value into, the one after.
const CASE_VALUE: u32 = 1;

/// What a backed enum's `backed_enum_table` answers for a value
/// ([`Request::backed_case_named`]).
pub(crate) enum ByValue {
    /// The name of the case the value is, a view of the table's.
    Named(Value),
    /// No case has the value.
    Missing,
    /// The request built no table for the enum: a native one, whose
    /// table is the stub's.
    Unbuilt,
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
