//! Methods as a request finds and checks them: a class's methods by
//! name and by number, natives' and enums' own, the class a method
//! belongs to and its declaration, `instanceof`, a class's parent, the
//! visibility checks `zend_check_method_accessible` and
//! `zend_std_get_static_method` make with php's wording, and the names
//! php prints a method by. The area holds no state of its own: it reads
//! the engine's linked classes and the classes the request declared.

use php_unit::decl::{ClassKind, MethodDecl, Visibility};
use php_unit::link::{LinkedClass, Row};

use crate::api::call::Function;
use crate::api::raising::Thrown;
use crate::api::values::{ClassId, PhpString};
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

impl Request {
    /// The class a method belongs to, `func->common.scope`: a native
    /// method's class, or the request's `ClassId` for the class of a
    /// user method; none for a function.
    pub(crate) fn method_class(&self, function: Function) -> Option<ClassId> {
        if let Some(method) = self.native_method(function) {
            // A parent hook's trampoline has the property's class as its
            // scope, which its call carries and no row of the table does.
            return method.at.class();
        }
        if let Some((class, _)) = self.enum_method_of(function) {
            return Some(class);
        }
        if let Some((trampoline, _)) = self.trampoline_of(function) {
            return match trampoline.via {
                crate::trampoline::Via::Magic(magic) => self.method_class(magic),
                crate::trampoline::Via::Invoke(_) => self.class_named(b"Closure"),
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
        let MethodRow { class, row } = self.method_row(class, name)?;
        self.method_function(row.holder_in(class), row.at)
    }

    /// [`Request::method_named`] for a method `class` declares itself,
    /// none where the entry under the name is one it inherited.
    fn own_method_named(&self, class: ClassId, name: &[u8]) -> Option<Function> {
        let MethodRow { row, .. } = self.method_row(class, name)?;
        self.method_function(class, row.at)
            .filter(|_| row.holder.is_none())
    }

    /// `zend_hash_find_ptr(&ce->function_table, lcname)` as a
    /// [`MethodRow`]: the row of `class`'s function table under the
    /// name, whatever its case, which [`Request::row_method`] reads;
    /// none where the table has no entry of that name. One lookup in the class's linked
    /// record, the name compared through the row, so nothing is lowered
    /// or allocated.
    pub fn method_row(&self, class: ClassId, name: &[u8]) -> Option<MethodRow> {
        let record = self.linked_class(class)?;
        let row = record
            .methods
            .find(name, |row| self.row_method_name(class, *row))
            .copied()?;
        Some(MethodRow { class, row })
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
            .map(move |&row| MethodRow { class, row })
    }

    /// The declaration of the method a row of a function table is, which
    /// [`MethodRow::scope`] declared.
    pub fn row_method(&self, row: MethodRow) -> Option<&MethodDecl> {
        self.row_method_at(row.class, row.row)
            .map(|(_, method)| method)
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
    fn method_function(&self, class: ClassId, at: u32) -> Option<Function> {
        let at = usize::try_from(at).ok()?;
        let functions = match self.native_class_at(class) {
            Some(native) => self.shared.method_functions.get(native)?,
            None => &self.declared_row(class)?.bound.functions,
        };
        functions.get(at).copied().flatten()
    }

    /// The enum method an enum's function number names, the request's
    /// own or the published arena's ([`Function::ENUM`]), and its enum.
    pub(super) fn enum_method_of(
        &self,
        function: Function,
    ) -> Option<(ClassId, crate::enums::EnumMethod)> {
        match function.local_enum_at() {
            Some(at) => self.enum_functions.get(at).copied(),
            None => self.arena.enum_method(function),
        }
    }

    /// The place of a native among the natives, in one count from zero:
    /// a native function or method at its own number, which is dense
    /// from zero, and an enum's method after them at its place among
    /// the published enums' methods and the request's; none for any
    /// other function, a user one, a call's trampoline or
    /// `zend_pass_function`. Where php's
    /// internal runtime cache gives each native its own extension slot
    /// (`zend_observer_fcall_internal_function_extension`).
    pub(super) fn native_place(&self, function: Function) -> Option<usize> {
        let natives = self
            .shared
            .functions
            .len()
            .checked_add(self.shared.methods.len())?;
        let at = usize::try_from(function.0).ok()?;
        if at < natives {
            return Some(at);
        }
        self.enum_method_of(function)?;
        // The published enums' methods and the request's own take turns,
        // so neither's count moves the other's places.
        let enum_at = match function.local_enum_at() {
            Some(at) => at.checked_mul(2)?.checked_add(1)?,
            None => usize::try_from(function.published_enum_at()?)
                .ok()?
                .checked_mul(2)?,
        };
        natives.checked_add(enum_at)
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
        if let Some((class, method)) = self.enum_method_of(function) {
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
    pub(crate) fn instance_of(&self, class: ClassId, of: ClassId) -> bool {
        if class == of {
            return true;
        }
        if self
            .class_decl(of)
            .is_some_and(|decl| decl.kind == ClassKind::Interface)
        {
            return self.interfaces(class).contains(&of);
        }
        let mut at = class;
        for _ in 0..64 {
            match self.linked_parent(at) {
                Some(parent) if parent == of => return true,
                Some(parent) => at = parent,
                None => return false,
            }
        }
        false
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
    /// that loops cannot hold the request.
    pub(crate) fn lineage(&self, class: ClassId) -> impl Iterator<Item = ClassId> + '_ {
        std::iter::successors(Some(class), move |&at| self.class_parent(at)).take(64)
    }

    /// The parents of a class up from its own, `ce->parent` and on, held
    /// to 64 as [`Request::lineage`] is.
    pub(crate) fn ancestors(&self, class: ClassId) -> impl Iterator<Item = ClassId> + '_ {
        std::iter::successors(self.class_parent(class), move |&at| self.class_parent(at)).take(64)
    }

    /// `ce->parent`: the linked parent of a class, none at a root; an
    /// engine's class's as startup linked it, a declared class's as it
    /// bound ([`super::Bound`]), and by its name where it bound first.
    pub(crate) fn class_parent(&self, class: ClassId) -> Option<ClassId> {
        let at = usize::try_from(class.0).ok()?;
        if let Some(parent) = self.shared.parents.get(at) {
            return *parent;
        }
        if let Some(parent) = self
            .declared_row(class)
            .and_then(|declared| declared.bound.parent)
        {
            return Some(parent);
        }
        self.class_decl(class)?
            .parent
            .as_ref()
            .and_then(|name| self.class_named(&name.0))
    }

    /// `ce->parent` as the link left it, read by number: an engine's
    /// class's as startup linked it, a declared class's as it bound.
    fn linked_parent(&self, class: ClassId) -> Option<ClassId> {
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
    /// method is a trait's, which is the callee php never caches.
    pub(crate) fn static_method_found(&mut self, function: Function) -> Result<bool, Thrown> {
        let Some(decl) = self.method_decl(function) else {
            return Ok(false);
        };
        let (is_abstract, name) = (decl.is_abstract, decl.function.name.clone());
        let Some(class) = self.method_class(function) else {
            return Ok(false);
        };
        let is_trait = self
            .class_decl(class)
            .is_some_and(|decl| decl.kind == ClassKind::Trait);
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
            if self.is_pending() {
                return Err(Thrown::of(None));
            }
        }
        Ok(is_trait)
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
    /// otherwise, which is what a protected method is reached through. A
    /// method overriding one a parent declares public or protected, or
    /// abstract, has the parent's for its prototype, or the one that
    /// method has (`do_inheritance_check_on_method`); a constructor has
    /// one only where the prototype it would take is abstract.
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
        let lowered = self.declared_name(function).to_ascii_lowercase();
        let mut root = class;
        let mut root_abstract = self.method_decl(function).is_some_and(|m| m.is_abstract);
        for _ in 0..64 {
            let Some(overridden) = self
                .class_parent(root)
                .and_then(|parent| self.method_named(parent, lowered.as_bytes()))
            else {
                break;
            };
            let Some(decl) = self.method_decl(overridden) else {
                break;
            };
            // "The parent method is private and not an abstract": no
            // prototype.
            if decl.visibility == Visibility::Private && !decl.is_abstract {
                break;
            }
            let Some(declaring) = self.method_class(overridden) else {
                break;
            };
            root = declaring;
            root_abstract = decl.is_abstract;
        }
        if lowered == "__construct" && !root_abstract {
            return Some(class);
        }
        Some(root)
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
    pub fn instantiable(&mut self, class: ClassId) -> Result<(), Thrown> {
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

    /// The name a call frame carries for a native method, the interned
    /// string of its declared name, and the empty string for the pass
    /// function, which has none.
    pub(crate) fn method_name_string(&mut self, function: Function) -> Result<PhpString, Thrown> {
        let name = if function == Function::PASS {
            String::new()
        } else {
            self.declared_name(function)
        };
        self.interned(name.as_bytes())
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
    pub(crate) fn std_method(&self, class: ClassId, lowered: &[u8]) -> Option<Function> {
        let found = self.method_named(class, lowered)?;
        // `zend_std_get_method` reads the scope only for a method its
        // flags mark `ZEND_ACC_CHANGED`, private or protected: a public
        // one no class above its own declares privately is the answer
        // whatever the scope, since the scope's private method can
        // stand in for it only from above it.
        if self.method_visibility(found) == Visibility::Public && !self.changed(found, lowered) {
            return Some(found);
        }
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

    /// `ZEND_ACC_CHANGED` on `function`, which
    /// `do_inheritance_check_on_method` sets where a class above the
    /// method's own declares a private method of its name, found here by
    /// walking up from its class: a call from that class's scope reaches
    /// its private method, whatever the child made public. The walk is
    /// cut at a depth no declaration can make.
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
    fn changed(&self, function: Function, lowered: &[u8]) -> bool {
        let Some(class) = self.method_class(function) else {
            return false;
        };
        self.ancestors(class).any(|class| {
            self.own_method_named(class, lowered)
                .is_some_and(|own| self.method_visibility(own) == Visibility::Private)
        })
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
