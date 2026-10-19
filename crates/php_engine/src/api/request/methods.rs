//! Methods as a request finds and checks them: a class's methods by
//! name and by number, natives' and enums' own, the class a method
//! belongs to and its declaration, `instanceof`, a class's parent, the
//! visibility checks `zend_check_method_accessible` and
//! `zend_std_get_static_method` make with php's wording, and the names
//! php prints a method by. The area holds no state of its own: it reads
//! the engine's linked classes and the classes the request declared.

use php_link::{LinkedClass, Row};
use php_unit::decl::{Bare, ClassKind, MethodDecl, PropertyDecl, Visibility};

use crate::api::call::{ClassNative, Function};
use crate::api::raising::{Thrown, none_pending};
use crate::api::values::{ClassId, Owner, PhpObject, PhpString};
use crate::engine::{MethodAt, NativeMethod};

use super::{Member, MemberAt, Request};

/// One row of a linked class's function table, `ce->function_table`,
/// named by value: the class whose table it is and the row the linked
/// record holds there, read by [`Request::row_method`]. It names the
/// row, not a reference into the request's tables, so it is `Copy`, is
/// kept across calls into the engine, and stands as long as the
/// record, which the link wrote once and nothing writes after. What php
/// holds as a `zend_function *` out of the table, `intern->ptr` for a
/// `ReflectionMethod`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MethodRow {
    class: ClassId,
    row: Row<ClassId>,
    /// The row's position in the table, where the record keeps its
    /// prototype.
    at: u32,
}

impl MethodRow {
    /// The class whose function table holds the row: the class the
    /// lookup or the walk was asked about.
    pub fn class(self) -> ClassId {
        self.class
    }

    /// `common.scope`: the class that declared the method, the table's
    /// own class for one it declares, a trait's method among them.
    pub fn scope(self) -> ClassId {
        self.row.holder_in(self.class)
    }
}

/// One row of a linked class's property table, `ce->properties_info`,
/// named by value as a [`MethodRow`] is: the class whose table it is and
/// the row the linked record holds there, read by
/// [`Request::row_property_info`]. What php holds as a `zend_property_info *`
/// out of the table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PropertyInfoRow {
    class: ClassId,
    row: Row<ClassId>,
}

impl PropertyInfoRow {
    /// The class whose property table holds the row.
    pub fn class(self) -> ClassId {
        self.class
    }

    /// `prop_info->ce`: the class that declared the property, the
    /// table's own class for one it declares.
    pub fn declaring(self) -> ClassId {
        self.row.holder_in(self.class)
    }
}

impl Request {
    /// `ZEND_HASH_FOREACH_PTR(&ce->properties_info, prop_info)`: every
    /// row of `class`'s property table in php's order, the class's own
    /// declarations, instance and `static`, then each inherited one it
    /// does not declare again, an internal class's redeclaration at the
    /// end; each a [`PropertyInfoRow`] [`Request::row_property_info`] reads, read
    /// off the linked record in place; nothing for a class with no
    /// linked record.
    ///
    /// ```php
    /// class A { public $a = 1; static $s = 1; protected $p; } class B extends A { public $b; public $a = 2; }
    /// foreach ((new ReflectionClass('B'))->getProperties() as $p) echo $p->class, '::', $p->name, "\n";
    /// var_dump(get_class_vars('B'));
    /// ```
    /// ```expect
    /// B::b
    /// B::a
    /// A::s
    /// A::p
    /// array(3) {
    ///   ["b"]=>
    ///   NULL
    ///   ["a"]=>
    ///   int(2)
    ///   ["s"]=>
    ///   int(1)
    /// }
    /// ```
    pub fn property_info_rows(
        &self,
        class: ClassId,
    ) -> impl ExactSizeIterator<Item = PropertyInfoRow> + '_ {
        self.linked_class(class)
            .map_or(&[][..], |record| record.properties.rows())
            .iter()
            .map(move |&row| PropertyInfoRow { class, row })
    }

    /// `zend_hash_find_ptr(&ce->properties_info, name)` as a
    /// [`PropertyInfoRow`]: the row of `class`'s property table under the
    /// name, byte for byte; none where the table has no entry of that
    /// name. One lookup in the class's linked record.
    pub fn property_info_row(&self, class: ClassId, name: &[u8]) -> Option<PropertyInfoRow> {
        let record = self.linked_class(class)?;
        let (_, &row) = record.properties.find_at(name, |row| {
            self.row_property_info(PropertyInfoRow { class, row: *row })
                .map_or(&[][..], |property| &property.name.0)
        })?;
        Some(PropertyInfoRow { class, row })
    }

    /// The declaration of the property a row of a property table is,
    /// which [`PropertyInfoRow::declaring`] declared.
    pub fn row_property_info(&self, row: PropertyInfoRow) -> Option<&PropertyDecl> {
        let decl = self.class_decl(row.declaring())?;
        decl.properties.get(usize::try_from(row.row.at).ok()?)
    }

    /// `obj->handlers->get_method(&obj, name, key)` as a call and
    /// `zend_is_callable_ex` ask it: the class's `get_method`, then its
    /// `__call`, then the same on the object its class hands the call
    /// to ([`ObjectHandlers::method_receiver`]), until a class answers
    /// or hands it to none. Beside the method, none where no class
    /// answered, the last object the call was handed to, which php's
    /// `get_method` leaves in `*object` whether or not it answered;
    /// `None` where the object itself was asked last. The access rule
    /// is the caller's.
    ///
    /// [`ObjectHandlers::method_receiver`]: crate::api::abi::ObjectHandlers::method_receiver
    pub(crate) fn method_found(
        &mut self,
        object: PhpObject,
        lowered: &[u8],
        written: &[u8],
    ) -> Result<(Option<Function>, Option<PhpObject>), Thrown> {
        let mut on = object;
        let mut receiver = None;
        loop {
            let table = self.object_handlers(on.heap());
            if let Some(function) = table.get_method_as(self, on, lowered, written)? {
                return Ok((Some(function), receiver));
            }
            if let Some(trampoline) = self.call_trampoline(on.class(), written, false) {
                return Ok((Some(trampoline), receiver));
            }
            match table.method_receiver(self, on) {
                Some(inner) => {
                    on = inner;
                    receiver = Some(inner);
                }
                None => return Ok((None, receiver)),
            }
        }
    }

    /// The class a method belongs to, `func->common.scope`: a native
    /// method's class, or the request's `ClassId` for the class of a
    /// user method; none for a function.
    pub(crate) fn method_class(&self, function: Function) -> Option<ClassId> {
        if let Some(method) = self.native_method(function) {
            // A parent hook's trampoline has the property's class as its
            // scope, which its call carries and no row of the table does.
            return method.at.class();
        }
        if let Some((class, _)) = self.class_native_of(function) {
            return Some(class);
        }
        if let Some((trampoline, _)) = self.trampoline_of(function) {
            return match trampoline.via {
                crate::trampoline::Via::Magic(magic) => self.method_class(magic),
                crate::trampoline::Via::Invoke(_) => self.class_named(Bare::resolved(b"Closure")),
                crate::trampoline::Via::Native(_) => None,
            };
        }
        self.user_function(function)?
            .member
            .map(|member| member.class)
    }

    /// `zend_hash_find_ptr(&ce->function_table, lcname)`: the method
    /// under the name, whatever its case, in the class's linked record,
    /// which holds what the class declares and what it inherited from
    /// its parents and its interfaces, as `zend_do_inheritance` and
    /// `do_interface_implementation` copy them in; none where the table
    /// has no entry of that name. One lookup, the name compared through
    /// the row, so nothing is lowered or allocated.
    ///
    /// ```php
    /// interface I1 { function m(); }
    /// interface I2 extends I1 { function n(); }
    /// abstract class A implements I2 { function n() {} }
    /// abstract class B extends A {}
    /// var_dump(method_exists('I2', 'm'), method_exists('I2', 'N'), method_exists('A', 'm'), method_exists('B', 'M'), method_exists('B', 'x'));
    /// var_dump(get_class_methods('B'));
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(true)
    /// bool(true)
    /// bool(true)
    /// bool(false)
    /// array(2) {
    ///   [0]=>
    ///   string(1) "n"
    ///   [1]=>
    ///   string(1) "m"
    /// }
    /// ```
    ///
    /// A method inherited through three levels, one overridden in the
    /// middle, and a parent's private one, which the child's table holds
    /// and only the parent's scope may call, beside the child's own of
    /// that name:
    ///
    /// ```php
    /// class G { function inherited() { return "G::inherited"; } function over() { return "G::over"; } private function hidden() { return "G::hidden"; } function callHidden() { return $this->hidden(); } }
    /// class P extends G { function over() { return "P::over"; } }
    /// class C extends P { private function hidden() { return "C::hidden"; } function mine() { return $this->hidden(); } }
    /// $c = new C;
    /// echo $c->inherited(), " ", $c->over(), " ", $c->callHidden(), " ", $c->mine(), "\n";
    /// var_dump(method_exists('P', 'hidden'), method_exists(new P, 'hidden'));
    /// class D extends P { function probe() { return $this->hidden(); } }
    /// try { (new D)->probe(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// G::inherited P::over G::hidden C::hidden
    /// bool(false)
    /// bool(true)
    /// Call to private method G::hidden() from scope D
    /// ```
    pub fn method_named(&self, class: ClassId, name: &[u8]) -> Option<Function> {
        let MethodRow { class, row, .. } = self.method_row(class, name)?;
        self.method_function(row.holder_in(class), row.at)
    }

    /// `ce->constructor`: `__construct` as `class`'s linked record
    /// resolved it once ([`LinkedClass::constructor`]), its row read by
    /// position; none for a class with none.
    ///
    /// [`LinkedClass::constructor`]: php_link::LinkedClass::constructor
    pub(crate) fn constructor(&self, class: ClassId) -> Option<Function> {
        self.resolved_method(class, |record| record.constructor)
    }

    /// `ce->destructor`, as [`Request::constructor`] reads it.
    pub(crate) fn destructor(&self, class: ClassId) -> Option<Function> {
        self.resolved_method(class, |record| record.destructor)
    }

    /// The method at the position of `class`'s function table `at`
    /// picks out of its linked record.
    fn resolved_method(
        &self,
        class: ClassId,
        at: impl FnOnce(&LinkedClass<ClassId>) -> Option<u32>,
    ) -> Option<Function> {
        let record = self.linked_class(class)?;
        let row = record
            .methods
            .rows()
            .get(usize::try_from(at(record)?).ok()?)?;
        self.method_function(row.holder_in(class), row.at)
    }

    /// `zend_hash_find_ptr(&ce->function_table, lcname)` as a
    /// [`MethodRow`]: the row of `class`'s function table under the
    /// name, whatever its case, which [`Request::row_method`] reads;
    /// none where the table has no entry of that name. One lookup in the class's linked
    /// record, the name compared through the row, so nothing is lowered
    /// or allocated.
    pub fn method_row(&self, class: ClassId, name: &[u8]) -> Option<MethodRow> {
        let record = self.linked_class(class)?;
        let (at, &row) = record
            .methods
            .find_at(name, |row| self.row_method_name(class, *row))?;
        let at = u32::try_from(at).ok()?;
        Some(MethodRow { class, row, at })
    }

    /// `ZEND_HASH_FOREACH_PTR(&ce->function_table, mptr)`: every row of
    /// `class`'s function table in php's order, each a [`MethodRow`]
    /// [`Request::row_method`] reads; nothing for a class with no linked
    /// record. The rows are read off the record in place, so the walk
    /// allocates nothing.
    pub fn method_rows(&self, class: ClassId) -> impl ExactSizeIterator<Item = MethodRow> + '_ {
        self.linked_class(class)
            .map_or(&[][..], |record| record.methods.rows())
            .iter()
            .enumerate()
            .map(move |(at, &row)| MethodRow {
                class,
                row,
                at: u32::try_from(at).unwrap_or(u32::MAX),
            })
    }

    /// `common.prototype`'s scope for the method a row of a function
    /// table is, which the link set once
    /// ([`php_link::LinkedClass::prototypes`]): the class whose method the
    /// row's prototype is, none where it has none.
    pub fn row_prototype(&self, row: MethodRow) -> Option<ClassId> {
        let record = self.linked_class(row.class)?;
        record
            .prototypes
            .get(usize::try_from(row.at).ok()?)
            .copied()
            .flatten()
    }

    /// The declaration of the method a row of a function table is, which
    /// [`MethodRow::scope`] declared.
    pub fn row_method(&self, row: MethodRow) -> Option<&MethodDecl> {
        self.row_method_at(row.class, row.row)
            .map(|(_, method)| method)
    }

    /// The function a row of a function table is, the `zend_function *`
    /// the bucket holds: the declaring class's method at the row's place,
    /// a native trait's method copied into a class among them, which is
    /// the class's own function ([`Request::function_extension`] reads
    /// the trait's extension off it).
    ///
    /// ```php
    /// class E extends Exception { function own() {} }
    /// foreach (['getMessage', 'own'] as $m) {
    ///     var_dump((new ReflectionMethod('E', $m))->getExtensionName());
    /// }
    /// ```
    /// ```expect
    /// string(4) "Core"
    /// bool(false)
    /// ```
    pub fn row_function(&self, row: MethodRow) -> Option<Function> {
        self.method_function(row.scope(), row.row.at)
    }

    /// The name of the method a row of `class`'s function table is.
    fn row_method_name(&self, class: ClassId, row: Row<ClassId>) -> &[u8] {
        self.row_method_at(class, row)
            .map_or(&[][..], |(_, method)| &method.function.name.0)
    }

    /// The method a row of `class`'s function table is, beside the class
    /// that declared it.
    fn row_method_at(&self, class: ClassId, row: Row<ClassId>) -> Option<(ClassId, &MethodDecl)> {
        let holder = row.holder_in(class);
        let decl = self.class_decl(holder)?;
        Some((holder, decl.methods.get(usize::try_from(row.at).ok()?)?))
    }

    /// `ce->function_table` of a linked class in php's order, each method
    /// beside the class that declared it, `common.scope`.
    pub(crate) fn methods_in_order(&self, class: ClassId) -> Vec<(ClassId, &MethodDecl)> {
        let Some(record) = self.linked_class(class) else {
            return Vec::new();
        };
        record
            .methods
            .rows()
            .iter()
            .filter_map(|row| self.row_method_at(class, *row))
            .collect()
    }

    /// The linked record of a class: a native class's as startup linked
    /// it, a declared class's as it bound ([`Request::bind_members`]).
    pub(crate) fn linked_class(&self, class: ClassId) -> Option<&LinkedClass<ClassId>> {
        match self.native_class_at(class) {
            Some(native) => self.shared.records.get(native),
            None => self.declared_row(class)?.bound.record.as_ref(),
        }
    }

    /// The function the method at `at` among `class`'s own declarations
    /// is.
    pub(super) fn method_function(&self, class: ClassId, at: u32) -> Option<Function> {
        let at = usize::try_from(at).ok()?;
        let functions = match self.native_class_at(class) {
            Some(native) => self.shared.method_functions.get(native)?,
            None => &self.declared_row(class)?.bound.functions,
        };
        functions.get(at).copied().flatten()
    }

    /// The class native a function number names, the request's own or
    /// the published arena's ([`Function::CLASS_NATIVE`]), and its class.
    pub(super) fn class_native_of(&self, function: Function) -> Option<(ClassId, ClassNative)> {
        match function.local_class_native_at() {
            Some(at) => self.class_natives.get(at).copied(),
            None => self.arena.class_native(function),
        }
    }

    /// The native a function number serves by: the trait's own for a
    /// native trait's method copied into a class, the function itself
    /// for any other. What php's copy shares with the trait's, its
    /// handler, its flags, its extension and its runtime cache, is read
    /// through this alone (`entry_of`, `function_flags`,
    /// `function_extension`, `native_place`); its scope is the class's
    /// (`method_class`), and its declaration, the name and the
    /// visibility an alias gave it, the class's own row
    /// ([`Request::method_decl`]).
    pub(super) fn served_native(&self, function: Function) -> Function {
        match self.class_native_of(function) {
            Some((_, ClassNative::Trait { native, .. })) => native,
            _ => function,
        }
    }

    /// The place of a native among the natives, in one count from zero:
    /// a native function or method at its own number, which is dense
    /// from zero, and a class native after them at its place among
    /// the published class natives and the request's; none for any
    /// other function, a user one, a call's trampoline or
    /// `zend_pass_function`. Where php's
    /// internal runtime cache gives each native its own extension slot
    /// (`zend_observer_fcall_internal_function_extension`). A native
    /// trait's method copied into a class has the trait's place, since
    /// `zend_add_trait_method`'s copy shares the trait's runtime cache
    /// pointer: an observer is asked of the method once, under the
    /// scope of the first class to call it.
    pub(super) fn native_place(&self, function: Function) -> Option<usize> {
        let function = self.served_native(function);
        let natives = self
            .shared
            .functions
            .len()
            .checked_add(self.shared.methods.len())?;
        let at = usize::try_from(function.0).ok()?;
        if at < natives {
            return Some(at);
        }
        self.class_native_of(function)?;
        // The published class natives and the request's own take turns,
        // so neither's count moves the other's places.
        let class_native_at = match function.local_class_native_at() {
            Some(at) => at.checked_mul(2)?.checked_add(1)?,
            None => usize::try_from(function.published_class_native_at()?)
                .ok()?
                .checked_mul(2)?,
        };
        natives.checked_add(class_native_at)
    }

    /// The number of the first native method the predicate takes, which
    /// is its position among `methods` past the natives.
    pub(super) fn native_method_id(
        &self,
        matching: impl FnMut(NativeMethod) -> bool,
    ) -> Option<Function> {
        let base = self.shared.functions.len();
        self.shared
            .methods
            .iter()
            .copied()
            .position(matching)
            .and_then(|i| u32::try_from(base.saturating_add(i)).ok())
            .map(Function)
    }

    /// The declaration of a method, native or user, if `function` is one.
    pub(crate) fn method_decl(&self, function: Function) -> Option<&MethodDecl> {
        // `zend_add_trait_method`'s copy of a native trait's method: the
        // using class's row, under the alias's name and visibility.
        if let Some((class, ClassNative::Trait { at, .. })) = self.class_native_of(function) {
            return self
                .class_decl(class)?
                .methods
                .get(usize::try_from(at).ok()?);
        }
        if let Some((class, ClassNative::Enum(method))) = self.class_native_of(function) {
            return self
                .class_decl(class)?
                .methods
                .iter()
                .find(|decl| decl.function.name.0.eq_ignore_ascii_case(method.name()));
        }
        if let Some((trampoline, _)) = self.trampoline_of(function) {
            return Some(&trampoline.decl);
        }
        if let Some(method) = self.native_method(function) {
            return match method.at {
                MethodAt::Declared { class, at } => self
                    .class_decl(class)?
                    .methods
                    .get(usize::try_from(at).ok()?),
                MethodAt::Trampoline { at: index, .. }
                | MethodAt::ParentHook { at: index, .. }
                | MethodAt::Unlisted { at: index } => {
                    self.shared.trampolines.get(usize::try_from(index).ok()?)
                }
            };
        }
        let member = self.user_function(function)?.member?;
        let decl = self.class_decl(member.class)?;
        match member.at {
            MemberAt::Method(at) => decl.methods.get(usize::try_from(at).ok()?),
            MemberAt::Hook { property, kind } => decl
                .properties
                .get(usize::try_from(property).ok()?)?
                .hooks
                .as_ref()?
                .of(kind),
        }
    }

    /// The number of the user function a class's declaration holds at
    /// `member`, a method or a hook, among the functions bound with it:
    /// one of the numbers the class took as it bound, in its own space.
    pub(crate) fn member_function(&self, member: Member) -> Option<Function> {
        let members = self.declared_row(member.class)?.bound.members.clone();
        members.map(Function).find(|function| {
            self.user_function(*function)
                .is_some_and(|user| user.member == Some(member))
        })
    }

    /// `ZSTR_VAL(ce->name)` as a message's `%s` prints it: the name a
    /// class was declared under up to its NUL, so an anonymous class is
    /// `class@anonymous` ([`php_unit::decl::shown`]), and `object` for
    /// one the tables have no declaration for. A value holding the name,
    /// `get_class()`'s, is [`Request::class_name_bytes`].
    pub fn class_name(&self, class: ClassId) -> String {
        self.class_decl(class).map_or_else(
            || String::from("object"),
            |decl| php_unit::decl::shown(&decl.name.0).into_owned(),
        )
    }

    /// `ce->name` whole, the NUL and what follows it included.
    pub(crate) fn class_name_bytes(&self, class: ClassId) -> Vec<u8> {
        self.class_decl(class)
            .map_or_else(|| b"object".to_vec(), |decl| decl.name.0.to_vec())
    }

    /// `instanceof_function`: whether `class` is `of`, extends it, or
    /// implements it. An interface is one scan of the class's linked
    /// `ce->interfaces`, which holds every interface it implements
    /// directly, through its parents and through their parents; a class
    /// is a walk up the linked parents. Nothing is looked up by name. A
    /// chain longer than any declaration can make is cut.
    ///
    /// ```php
    /// interface I0 {} interface I1 extends I0 {} interface J {} interface K extends J {}
    /// class G implements I1 {} class P extends G {} class C extends P implements K {}
    /// $c = new C;
    /// var_dump($c instanceof I0, $c instanceof I1, $c instanceof J, $c instanceof G, $c instanceof Countable, new P instanceof K);
    /// echo implode(",", class_implements($c)), "\n";
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(true)
    /// bool(true)
    /// bool(true)
    /// bool(false)
    /// bool(false)
    /// I0,I1,K,J
    /// ```
    pub fn instance_of(&self, class: ClassId, of: ClassId) -> bool {
        if class == of {
            return true;
        }
        if self
            .class_decl(of)
            .is_some_and(|decl| decl.kind == ClassKind::Interface)
        {
            return self.interfaces(class).contains(&of);
        }
        self.ancestors(class).any(|parent| parent == of)
    }

    /// `ce->interfaces` of a linked class: every interface it implements,
    /// each once, in php's order, which `class_implements` lists;
    /// nothing for a class with no linked record.
    pub fn interfaces(&self, class: ClassId) -> &[ClassId] {
        self.linked_class(class)
            .map_or(&[][..], |record| &record.interfaces)
    }

    /// The class and its parents up from it, `ce` then `ce->parent` and
    /// on, as php's walks up a chain go, held to 64 classes so a chain
    /// that loops cannot hold the request. Each step is
    /// [`Request::class_parent`], read by number and never by name, so a
    /// parent an `extends` spelled in another case is the class as it
    /// was declared:
    ///
    /// ```php
    /// class A {} class B extends a {} class C extends b {}
    /// var_dump(class_parents('C'), get_parent_class('C'));
    /// ```
    /// ```expect
    /// array(2) {
    ///   ["B"]=>
    ///   string(1) "B"
    ///   ["A"]=>
    ///   string(1) "A"
    /// }
    /// string(1) "B"
    /// ```
    pub fn lineage(&self, class: ClassId) -> impl Iterator<Item = ClassId> + '_ {
        std::iter::successors(Some(class), move |&at| self.class_parent(at)).take(64)
    }

    /// The parents of a class up from its own, `ce->parent` and on, held
    /// to 64 as [`Request::lineage`] is.
    pub(crate) fn ancestors(&self, class: ClassId) -> impl Iterator<Item = ClassId> + '_ {
        std::iter::successors(self.class_parent(class), move |&at| self.class_parent(at)).take(64)
    }

    /// `ce->parent`: the linked parent of a class, none at a root, read
    /// by number and never by name: an engine's class's as startup
    /// linked it, a declared or a published class's as it bound
    /// ([`super::Bound`]).
    ///
    /// ```php
    /// class P {} class C extends P {} class E extends RuntimeException {}
    /// var_dump(get_parent_class(new C), get_parent_class("E"), get_parent_class("RuntimeException"), get_parent_class("P"), new E instanceof Exception);
    /// ```
    /// ```expect
    /// string(1) "P"
    /// string(16) "RuntimeException"
    /// string(9) "Exception"
    /// bool(false)
    /// bool(true)
    /// ```
    #[must_use]
    pub fn class_parent(&self, class: ClassId) -> Option<ClassId> {
        match self.native_class_at(class) {
            Some(at) => self.shared.parents.get(at).copied().flatten(),
            None => self.declared_row(class)?.bound.parent,
        }
    }

    /// `zend_check_protected`: whether `scope` is `class` or one of its
    /// parents, or `class` is `scope` or one of its parents; false with
    /// no scope. Each walk is cut at a depth no declaration can make.
    pub(crate) fn check_protected(&self, class: ClassId, scope: Option<ClassId>) -> bool {
        let Some(scope) = scope else {
            return false;
        };
        let up = |from: ClassId, to: ClassId| self.lineage(from).any(|here| here == to);
        up(class, scope) || up(scope, class)
    }

    /// `zend_std_get_method`'s access check, which
    /// `zend_std_get_static_method` and `zend_std_get_constructor` share:
    /// a public method is anyone's; a private or a protected one is
    /// called from its own class's scope, a protected one from a scope
    /// `zend_check_protected` relates to it, and otherwise php's `Error`,
    /// `zend_bad_method_call`, naming the visibility, the method's class
    /// and the name as `written`, and the calling scope or `global
    /// scope`. A class with `__call` would take the call instead; none
    /// declared here has one.
    pub(crate) fn check_method_access(
        &mut self,
        function: Function,
        written: &[u8],
    ) -> Result<(), Thrown> {
        // A public method is anyone's, so the scope is not read for it,
        // as `zend_std_get_method` reads it only under
        // `ZEND_ACC_PRIVATE|ZEND_ACC_PROTECTED`.
        if self.method_visibility(function) == Visibility::Public {
            return Ok(());
        }
        let scope = self.executed_scope();
        self.check_method_access_from(function, written, scope)
    }

    /// [`Request::check_method_access`] measured against `scope` rather
    /// than the executed one, as `zend_check_method_accessible(fptr,
    /// scope)` measures a callable a constant expression names from the
    /// scope that evaluates it.
    pub(crate) fn check_method_access_from(
        &mut self,
        function: Function,
        written: &[u8],
        scope: Option<ClassId>,
    ) -> Result<(), Thrown> {
        if self.method_accessible_from(function, scope) {
            return Ok(());
        }
        let shown = visibility_string(self.method_visibility(function));
        let class = self
            .method_class(function)
            .map_or_else(|| String::from("object"), |class| self.class_name(class));
        let from = match scope {
            Some(scope) => format!("scope {}", self.class_name(scope)),
            None => String::from("global scope"),
        };
        Err(crate::api::raising::throw_error(
            self,
            "Error",
            &format!(
                "Call to {shown} method {class}::{}() from {from}",
                String::from_utf8_lossy(written)
            ),
        ))
    }

    /// The end of `zend_std_get_static_method`
    /// (`Z/Zend/zend_object_handlers.c:2066`), once the method is found
    /// and the scope may reach it: an abstract method is
    /// `zend_abstract_method_call`'s `Error`, and one whose scope is a
    /// trait, which is a method named on the trait itself and never one
    /// a class took from it, is php's deprecation, after which a
    /// handler that threw stops the call. The answer is whether the
    /// method is a trait's, which is the callee php never caches, and
    /// whether it takes `$this`, read off the one declaration
    /// ([`StaticFound`]).
    pub(crate) fn static_method_found(
        &mut self,
        function: Function,
    ) -> Result<StaticFound, Thrown> {
        let Some((is_abstract, takes_this)) = self
            .method_decl(function)
            .map(|decl| (decl.is_abstract, decl.is_instance))
        else {
            return Ok(StaticFound::default());
        };
        let plain = StaticFound {
            of_trait: false,
            takes_this,
        };
        let Some(class) = self.method_class(function) else {
            return Ok(plain);
        };
        let is_trait = self
            .class_decl(class)
            .is_some_and(|decl| decl.kind == ClassKind::Trait);
        // The name is spelled only for the `Error` or the deprecation:
        // nearly every call is neither, and makes no string.
        if !is_abstract && !is_trait {
            return Ok(plain);
        }
        let name = self
            .method_decl(function)
            .map(|decl| decl.function.name.clone())
            .unwrap_or_default();
        let shown = format!(
            "{}::{}",
            self.class_name(class),
            String::from_utf8_lossy(&name.0)
        );
        if is_abstract {
            return Err(crate::api::raising::throw_error(
                self,
                "Error",
                &format!("Cannot call abstract method {shown}()"),
            ));
        }
        if is_trait {
            crate::api::raising::deprecated(
                self,
                &format!(
                    "Calling static trait method {shown} is deprecated, it should only be called on a class using the trait"
                ),
            );
            none_pending(self)?;
        }
        Ok(StaticFound {
            of_trait: is_trait,
            takes_this,
        })
    }

    /// `zend_check_method_accessible` (`Z/Zend/zend_execute.h:399`): a
    /// public method is anyone's, a private one is its own class's
    /// scope alone, and a protected one is a scope
    /// `zend_check_protected` relates to it. A function that is no
    /// method, and one the tables have no declaration for, is anyone's.
    pub(crate) fn method_accessible(&self, function: Function) -> bool {
        // As [`Request::check_method_access`]: no scope for a public one.
        self.method_visibility(function) == Visibility::Public
            || self.method_accessible_from(function, self.executed_scope())
    }

    /// `zend_get_function_root_class`: the class of the method's
    /// `common.prototype` where it has one, the method's own class
    /// otherwise, which is what a protected method is reached through.
    /// The prototype is the one the link set on the row of the
    /// declaring class's function table ([`Request::row_prototype`]): a
    /// parent's method's, an abstract constructor's, or an interface's
    /// the class names.
    ///
    /// ```php
    /// abstract class Base { abstract protected function __clone(); abstract protected function m(); }
    /// class Boy extends Base { protected function __clone() { echo "Boy::__clone\n"; } protected function m() { echo "Boy::m\n"; } }
    /// class Kid extends Base {
    ///     public function __construct() { $b = new Boy; $b = clone $b; $b->m(); }
    ///     public function __clone() {} protected function m() {}
    /// }
    /// new Kid;
    /// ```
    /// ```expect
    /// Boy::__clone
    /// Boy::m
    /// ```
    fn function_root_class(&self, function: Function) -> Option<ClassId> {
        let class = self.method_class(function)?;
        let prototype = self
            .function_decl(function)
            .and_then(|decl| self.method_row(class, &decl.name.0))
            .and_then(|row| self.row_prototype(row));
        Some(prototype.unwrap_or(class))
    }

    /// `fn_flags & (ZEND_ACC_PUBLIC|ZEND_ACC_PROTECTED|ZEND_ACC_PRIVATE)`:
    /// how a method was declared, public for a function that is none.
    pub(super) fn method_visibility(&self, function: Function) -> Visibility {
        self.method_decl(function)
            .map_or(Visibility::Public, |decl| decl.visibility)
    }

    /// `scope ? "scope " : "global scope"` with the scope's name after
    /// it, the tail php's bad-call texts end in.
    pub(super) fn scope_phrase(&self) -> String {
        match self.executed_scope() {
            Some(scope) => format!("scope {}", self.class_name(scope)),
            None => String::from("global scope"),
        }
    }

    /// `object_init_ex`'s refusals: an interface, a trait, an enum or an
    /// abstract class is php's `Error` for what cannot be instantiated.
    ///
    /// ```php
    /// abstract class A { static function make() { return new static; } }
    /// class B extends A {}
    /// var_dump(B::make());
    /// try { A::make(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// new A;
    /// ```
    /// ```expect exit=255
    /// object(B)#1 (0) {
    /// }
    /// Cannot instantiate abstract class A
    ///
    /// Fatal error: Uncaught Error: Cannot instantiate abstract class A in %s:6
    /// Stack trace:
    /// #0 {main}
    ///   thrown in %s on line 6
    /// ```
    pub(crate) fn instantiable(&mut self, class: ClassId) -> Result<(), Thrown> {
        let Some(decl) = self.class_decl(class) else {
            return Ok(());
        };
        let what = match decl.kind {
            ClassKind::Interface => "interface",
            ClassKind::Trait => "trait",
            ClassKind::Enum => "enum",
            ClassKind::Class if decl.flags.is_abstract => "abstract class",
            ClassKind::Class => return Ok(()),
        };
        let name = self.class_name(class);
        Err(crate::api::raising::throw_error(
            self,
            "Error",
            &format!("Cannot instantiate {what} {name}"),
        ))
    }

    /// The name a call frame carries for a native method, its declared
    /// name with one owner the caller's ([`Request::declared_string`]),
    /// and the empty string for the pass function, which has none.
    pub(crate) fn method_name_string(&mut self, function: Function) -> Result<PhpString, Thrown> {
        let name = if function == Function::PASS {
            String::new()
        } else {
            self.declared_name(function)
        };
        self.declared_string(name.as_bytes()).map(Owner::hand_on)
    }

    /// [`Request::qualified_name`] for a raise about a method called on
    /// its class.
    pub(crate) fn qualified_name_of(&self, function: Function) -> String {
        self.qualified_name(function, None)
    }

    /// `zend_std_get_method`'s lookup (`Z/Zend/zend_object_handlers.c:1945`):
    /// the method `class` has under the lowered name, but where the
    /// executed scope is a class `class` descends from that declares a
    /// private method of that name itself, that one, which the one found
    /// overrides (`ZEND_ACC_CHANGED`, `zend_get_parent_private_method`):
    /// a private method is its class's, whatever a child redeclares.
    ///
    /// ```php
    /// class M { private function f() { return "M"; } function g() { return $this->f(); } }
    /// class P extends M { private function f() { return "P"; } }
    /// echo (new P)->g(), "\n";
    /// ```
    /// ```expect
    /// M
    /// ```
    ///
    /// php reads the scope only for a method its flags mark
    /// `ZEND_ACC_CHANGED`, private or protected. The scope's private
    /// method stands in only from a class above `class` that declares
    /// it, and such a method above a public one is what
    /// `ZEND_ACC_CHANGED` records, since no class below a public method
    /// can declare it private: so the scope's tests answer what the
    /// bit would, with no walk of the ancestors and no declaration read
    /// where the scope is none or `class` itself. A call from the
    /// private method's class reaches it, whatever a child made public:
    ///
    /// ```php
    /// class M { private function f() { return "M"; } function g() { return $this->f(); } }
    /// class P extends M { public function f() { return "P"; } }
    /// $p = new P;
    /// echo $p->g(), $p->f(), "\n";
    /// ```
    /// ```expect
    /// MP
    /// ```
    pub(crate) fn std_method(&self, class: ClassId, lowered: &[u8]) -> Option<Function> {
        let found = self.method_named(class, lowered)?;
        let Some(scope) = self.executed_scope() else {
            return Some(found);
        };
        if self.method_class(found) == Some(scope)
            || scope == class
            || !self.instance_of(class, scope)
        {
            return Some(found);
        }
        match self.method_named(scope, lowered) {
            Some(own)
                if self.method_class(own) == Some(scope)
                    && self.method_visibility(own) == Visibility::Private =>
            {
                Some(own)
            }
            _ => Some(found),
        }
    }

    /// `zend_check_method_accessible` (`Z/Zend/zend_execute.h:399`) with
    /// the scope given rather than read: a private method is its own
    /// class's, a protected one a scope `zend_check_protected` relates
    /// to its class.
    pub(crate) fn method_accessible_from(
        &self,
        function: Function,
        scope: Option<ClassId>,
    ) -> bool {
        let Some(class) = self.method_class(function) else {
            return true;
        };
        match self.method_visibility(function) {
            Visibility::Public => true,
            _ if scope == Some(class) => true,
            Visibility::Private => false,
            Visibility::Protected => {
                self.check_protected(self.function_root_class(function).unwrap_or(class), scope)
            }
        }
    }
}

/// `zend_visibility_string` (`Z/Zend/zend_compile.c:1063`): the word
/// php's bad-call texts name a method's visibility by.
pub(super) fn visibility_string(visibility: Visibility) -> &'static str {
    match visibility {
        Visibility::Public => "public",
        Visibility::Protected => "protected",
        Visibility::Private => "private",
    }
}

/// What [`Request::static_method_found`] read off a static call's
/// callee, the one declaration asked once: whether it is a trait's own,
/// which php never caches, and whether it takes `$this`, which php's
/// `INIT_STATIC_METHOD_CALL` reads off `fbc->common.fn_flags`.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct StaticFound {
    pub(crate) of_trait: bool,
    pub(crate) takes_this: bool,
}
