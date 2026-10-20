//! Property hooks as the standard handlers serve them (#427): the
//! `IS_HOOKED_PROPERTY_OFFSET` arms of `zend_std_read_property`,
//! `zend_std_write_property`, `zend_std_has_property`,
//! `zend_std_unset_property` and `zend_std_get_property_ptr_ptr`
//! (`Z/Zend/zend_object_handlers.c`). A hooked property is placed apart
//! from a slot ([`Hooked`]), since what a read or a write of it does is
//! its hook's to decide: from outside the hook the hook runs; inside
//! it, on the object the hook runs on, the backing value is read and
//! written directly (`zend_should_call_hook`), and a virtual property,
//! which has none, is refused.

use php_link::{ClassHooks, Held, Resolved, ResolvedHook, resolve_property};
use php_unit::decl::{Bare, ClassDecl, FunctionDecl, HookKind, MethodDecl, Visibility};

use super::{Link, Member, MemberAt, Request};
use crate::api::__private::{native_function, parameter};
use crate::api::abi::{
    Entry, PropertiesFor, PropertyPlace, PropertyPurpose, PropertyRead, PropertyTest,
};
use crate::api::call::Function;
use crate::api::raising::Thrown;
use crate::api::values::{
    ClassId, Key, Layout, Owner, PhpArray, PhpObject, PhpString, TypeTag, Value,
};
use crate::engine::MethodAt;
use crate::trampoline::public_method;

/// A hooked property as an object's class holds it, what
/// `zend_get_property_offset` answers `ZEND_HOOKED_PROPERTY_OFFSET` and
/// the `prop_info` for: the class whose declaration stands in the
/// table, which the visibilities are measured against, the hook of
/// each kind it carries, its own or one inherited, a hook still
/// abstract being none, whether it has no backing value
/// (`ZEND_ACC_VIRTUAL`), and the class of its prototype, which
/// `zend_is_in_hook` compares.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Hooked {
    pub(crate) declaring: ClassId,
    pub(crate) visibility: Visibility,
    pub(crate) set_visibility: Option<Visibility>,
    pub(crate) get: Option<Function>,
    pub(crate) set: Option<Function>,
    pub(crate) is_virtual: bool,
    root: ClassId,
}

impl Request {
    /// The linked class a declaration names, the one `class` is where
    /// the declaration is its own.
    fn class_of_decl(&self, class: ClassId, decl: &ClassDecl) -> Option<ClassId> {
        match self.class_decl(class) {
            Some(own) if std::ptr::eq(own, decl) => Some(class),
            _ => self.class_named((&decl.name).into()),
        }
    }

    /// The function a resolved hook is, none for an abstract one, which
    /// no linked class that can make an object holds.
    fn hook_function(
        &self,
        class: ClassId,
        hook: Option<ResolvedHook<'_>>,
        kind: HookKind,
    ) -> Option<Function> {
        let hook = hook?;
        if hook.method.is_abstract {
            return None;
        }
        let holder = self.class_of_decl(class, hook.class)?;
        self.member_function(Member {
            class: holder,
            at: MemberAt::Hook {
                property: hook.property,
                kind,
            },
        })
    }

    /// `ZEND_ACC_VIRTUAL` of the instance property `name` as `class`'s
    /// table holds it: every declaration of it up the chain virtual,
    /// whether it has hooks or, as an internal class's property may,
    /// none, its class's handlers serving it (`DatePeriod`'s seven).
    pub(crate) fn property_virtual(&self, class: ClassId, name: &[u8]) -> bool {
        self.resolved_property(class, name)
            .is_some_and(|resolved| resolved.property.is_instance && resolved.is_virtual)
    }

    /// `name` as `class`'s chain declares it, read off the declarations
    /// with every class taken as linked (`zend_get_property_info`'s
    /// row before access is read).
    fn resolved_property(&self, class: ClassId, name: &[u8]) -> Option<Resolved<'_>> {
        let lookup = |wanted: &[u8]| {
            self.class_named(Bare::resolved(wanted))
                .and_then(|id| self.class_decl(id))
                .map(|decl| Held {
                    decl,
                    bodies: None,
                    linked: true,
                    record: None,
                })
        };
        resolve_property(self.class_decl(class)?, name, &lookup)
    }

    /// `zend_get_property_offset`'s `property_info->hooks` arm: the
    /// hooked instance property `name` of `class` as the executed scope
    /// finds it ([`Request::scoped_property`]), none for a property
    /// with no hooks, one declared `static`, and a name no class of the
    /// chain declares.
    pub(crate) fn hooked(&self, class: ClassId, name: &[u8]) -> Option<Hooked> {
        if self.class_hooks(class) == ClassHooks::Absent {
            return None;
        }
        let (class, resolved) = self.scoped_property(class, name)?;
        self.hooked_of(class, &resolved)
    }

    /// `prop_info->hooks` of the entry `class`'s own table holds for
    /// `name`, whatever scope is executing: what a walk over the table's
    /// rows reads each row by, as php holds the row's `prop_info` in
    /// hand, and what `parent::$p::get()` names on the parent.
    pub(crate) fn row_hooked(&self, class: ClassId, name: &[u8]) -> Option<Hooked> {
        let resolved = self.resolved_property(class, name)?;
        self.hooked_of(class, &resolved)
    }

    /// The [`Hooked`] a resolution in `class`'s table makes.
    fn hooked_of(&self, class: ClassId, resolved: &Resolved<'_>) -> Option<Hooked> {
        if !resolved.is_hooked() || !resolved.property.is_instance {
            return None;
        }
        Some(Hooked {
            declaring: self.class_of_decl(class, resolved.class)?,
            visibility: resolved.property.visibility,
            set_visibility: resolved.property.set_visibility,
            get: self.hook_function(class, resolved.get, HookKind::Get),
            set: self.hook_function(class, resolved.set, HookKind::Set),
            is_virtual: resolved.is_virtual,
            root: self.class_of_decl(class, resolved.root)?,
        })
    }

    /// `ce->properties_info[name]` as `zend_get_property_offset` finds it
    /// under the executed scope, beside the class it is resolved in: the
    /// class's own table, or, where the entry redeclares a private
    /// property of the scope's and the scope is a parent of `class`
    /// (`ZEND_ACC_CHANGED`, `zend_get_parent_private_property`), the
    /// scope's own declaration; and none where the entry is a parent's
    /// private property the scope is not, which php reads as a dynamic
    /// property, or where no class of the chain declares `name` as an
    /// instance property. [`Request::declared_slot`] places a slot by
    /// the same rule.
    fn scoped_property(&self, class: ClassId, name: &[u8]) -> Option<(ClassId, Resolved<'_>)> {
        let resolved = self.resolved_property(class, name)?;
        let declaring = self.class_of_decl(class, resolved.class)?;
        if self.property_scope() != Some(declaring) {
            if let Some(own) = self.parent_private(class, name) {
                let resolved = self.resolved_property(own, name)?;
                return resolved.property.is_instance.then_some((own, resolved));
            }
            if resolved.property.visibility == Visibility::Private && declaring != class {
                return None;
            }
        }
        resolved.property.is_instance.then_some((class, resolved))
    }

    /// `zend_check_property_access`, what a `foreach` over an object and
    /// `get_object_vars()` hold each row of the table to: a declared row,
    /// of the property `name` that `declaring` declares, is walked where
    /// the executed scope finds that declaration and reaches it; a
    /// dynamic row, `declaring` none, where the scope finds no
    /// declaration of the name or a public one.
    ///
    /// ```php
    /// class A { private $changed { get => 'A'; } protected $promoted { get => 'A'; } private $shadowed = 'A';
    ///     function test() { foreach ($this as $k => $v) { echo "$k => $v\n"; } } }
    /// #[AllowDynamicProperties]
    /// class B extends A { public $changed { get => 'B'; } public $promoted { get => 'B'; } }
    /// $b = new B;
    /// $b->shadowed = 'Global';
    /// $b->test();
    /// foreach ($b as $k => $v) { echo "$k => $v\n"; }
    /// ```
    /// ```expect
    /// changed => A
    /// promoted => B
    /// shadowed => A
    /// promoted => B
    /// changed => B
    /// shadowed => Global
    /// ```
    ///
    /// From a parent's scope, the parent's private row is seen over a
    /// child's protected redeclaration, and a dynamic row shadowing a
    /// parent's private property is not:
    ///
    /// ```php
    /// class A { private $prop = "A"; function vars() { return get_object_vars($this); } }
    /// #[AllowDynamicProperties]
    /// class B extends A { protected $prop = "B"; }
    /// #[AllowDynamicProperties]
    /// class C extends A {}
    /// $c = new C; $c->prop = "dynamic";
    /// var_dump((new B)->vars(), $c->vars(), get_object_vars($c));
    /// ```
    /// ```expect
    /// array(1) {
    ///   ["prop"]=>
    ///   string(1) "A"
    /// }
    /// array(1) {
    ///   ["prop"]=>
    ///   string(1) "A"
    /// }
    /// array(1) {
    ///   ["prop"]=>
    ///   string(7) "dynamic"
    /// }
    /// ```
    pub(crate) fn property_row_visible(
        &self,
        class: ClassId,
        name: &[u8],
        declaring: Option<ClassId>,
    ) -> bool {
        let Some((at, resolved)) = self.scoped_property(class, name) else {
            return declaring.is_none();
        };
        let Some(found) = self.class_of_decl(at, resolved.class) else {
            return false;
        };
        let visibility = resolved.property.visibility;
        match declaring {
            None => visibility == Visibility::Public,
            Some(declaring) if declaring != found => false,
            Some(_) => {
                let scope = self.property_scope();
                match visibility {
                    Visibility::Public => true,
                    _ if scope == Some(found) => true,
                    Visibility::Private => false,
                    Visibility::Protected => self
                        .class_of_decl(at, resolved.root)
                        .is_some_and(|root| self.check_protected(root, scope)),
                }
            }
        }
    }

    /// `zend_get_property_offset`'s access rule over a hooked property:
    /// public, the scope its declaring class, or, for a protected one, a
    /// scope related to its prototype's class
    /// (`is_protected_compatible_scope(property_info->prototype->ce, scope)`),
    /// so two classes that each redeclare it reach each other's.
    ///
    /// ```php
    /// abstract class P { abstract public mixed $foo { get; } }
    /// class C1 extends P { public protected(set) mixed $foo { get => 1; set {} } }
    /// class G extends C1 { public protected(set) mixed $foo { get => 2; set {} } }
    /// class C2 extends C1 { static function foo($c) { return $c->foo += 1; } }
    /// var_dump(C2::foo(new G));
    /// ```
    /// ```expect
    /// int(3)
    /// ```
    pub(crate) fn hooked_accessible(&self, hooked: &Hooked) -> bool {
        let scope = self.property_scope();
        match hooked.visibility {
            Visibility::Public => true,
            _ if scope == Some(hooked.declaring) => true,
            Visibility::Private => false,
            Visibility::Protected => self.check_protected(hooked.root, scope),
        }
    }

    /// `EG(current_execute_data)`'s function and `$this`.
    fn current_call(&self) -> Option<(Option<Function>, Option<PhpObject>)> {
        Some(match self.chain().next()? {
            Link::Frame(_, state) => (state.function(), state.this()),
            Link::Call(call) => (call.function(), call.this()),
            Link::Site(_) | Link::Fake(_) => (None, None),
        })
    }

    /// Whether `function` is one of `zend_get_property_hook_trampoline`'s.
    pub(crate) fn is_parent_hook_trampoline(&self, function: Function) -> bool {
        self.native_method(function)
            .is_some_and(|method| matches!(method.at, MethodAt::ParentHook { .. }))
    }

    /// The parent hook trampoline of `kind`, which the engine numbers
    /// once among its native methods.
    pub(crate) fn parent_hook_trampoline(&self, kind: HookKind) -> Function {
        self.native_method_id(
            |method| matches!(method.at, MethodAt::ParentHook { kind: k, .. } if k == kind),
        )
        .unwrap_or(Function::PASS)
    }

    /// The property a parent hook trampoline being served names, php's
    /// `internal_function.reserved[0]`: the call's name `$name::get`
    /// less its `$` and its hook, beside the property's class, the
    /// call's scope.
    fn trampoline_property(&self) -> Option<(Vec<u8>, Option<ClassId>)> {
        let call = self.serving_call()?;
        let written = call.annex()?.name?;
        let bytes = written.as_bytes();
        let name = bytes.strip_prefix(b"$")?;
        let end = name.iter().rposition(|b| *b == b':')?.checked_sub(1)?;
        Some((name.get(..end)?.to_vec(), call.called_scope()))
    }

    /// `zend_is_in_hook`: whether the function running is a hook of the
    /// property `name` of the same prototype as `hooked`, one of its
    /// own hooks or one of a property it overrides or is overridden by,
    /// or a parent hook's trampoline serving the same property.
    fn in_hook(&self, hooked: &Hooked, name: &[u8]) -> bool {
        let Some((Some(function), _)) = self.current_call() else {
            return false;
        };
        if self.is_parent_hook_trampoline(function) {
            return self.trampoline_property().is_some_and(|(property, scope)| {
                property == name
                    && scope
                        .and_then(|scope| self.row_hooked(scope, name))
                        .is_none_or(|parent| parent.root == hooked.root)
            });
        }
        let Some(Member {
            class,
            at: MemberAt::Hook { property, .. },
        }) = self.user_function(function).and_then(|user| user.member)
        else {
            return false;
        };
        let Some(decl) = self.class_decl(class) else {
            return false;
        };
        let Some(own) = decl.properties.get(property as usize) else {
            return false;
        };
        if &*own.name.0 != name {
            return false;
        }
        self.hooked(class, name)
            .is_some_and(|running| running.root == hooked.root)
    }

    /// `zend_should_call_hook`: the hook runs unless the function
    /// running is a hook of the same property on the same object, or on
    /// the lazy proxy whose instance the object is.
    pub(crate) fn should_call_hook(&self, hooked: &Hooked, name: &[u8], object: PhpObject) -> bool {
        if !self.in_hook(hooked, name) {
            return true;
        }
        let Some((_, Some(this))) = self.current_call() else {
            return true;
        };
        if this == object {
            return false;
        }
        self.lazy_instance(this.heap()) != Some(object)
    }

    /// `zend_call_known_instance_method` over a hook, the object one
    /// more owner while it runs.
    fn call_hook(
        &mut self,
        object: PhpObject,
        hook: Function,
        arguments: &[Value],
    ) -> Result<Value, Thrown> {
        let held = Owner::take(self, object);
        let served = self.serve_method_made(hook, Some(object), object.class(), arguments, &[]);
        held.give_up(self);
        served
    }

    /// php's `Error` over a hooked property, naming the object's class
    /// as `zobj->ce->name` names it.
    fn hooked_error(&mut self, object: PhpObject, name: &[u8], sentence: &str) -> Thrown {
        let class = self.class_name(object.class());
        let shown = String::from_utf8_lossy(name).into_owned();
        let message = sentence
            .replace("{class}", &class)
            .replace("{name}", &shown);
        crate::api::raising::throw_error(self, "Error", &message)
    }

    /// `ZEND_HOOKED_PROPERTY_OFFSET` at `zend_std_read_property`, placed
    /// after the scope's access to it is held as any declared
    /// property's is: the read of [`Request::read_hooked`] where the
    /// scope reaches it, `zend_bad_property_access`'s `Error` where it
    /// does not and no `__get` may serve it instead, and none for a
    /// name that is no hooked property, or one a `__get` may serve.
    pub(crate) fn read_hooked_arm(
        &mut self,
        object: PhpObject,
        name: &[u8],
        read: PropertyRead,
    ) -> Option<Result<Value, Thrown>> {
        let class = object.class();
        let hooked = self.hooked(class, name)?;
        if self.hooked_accessible(&hooked) {
            return Some(self.read_hooked(object, name, hooked, read));
        }
        if read != PropertyRead::Silent && !self.serves_accessor(class, b"__get") {
            return Some(Err(self.inaccessible_property(
                class,
                hooked.visibility,
                name,
            )));
        }
        None
    }

    /// `zend_std_read_property`'s hooked arm: the `get` hook called, or,
    /// where there is none, the backing value read, which a write fetch
    /// may reach into only where it is an object; inside the hook, the
    /// backing value.
    ///
    /// ```php
    /// class C {
    ///     public $w { set { echo "set\n"; } }
    ///     public array $a = [1] { get => $this->a; }
    ///     public $v { get => $this->missing ?? 'v'; }
    /// }
    /// $c = new C;
    /// try { var_dump($c->w); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// try { $c->a[] = 2; } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// var_dump($c->a, $c->v);
    /// ```
    /// ```expect
    /// Property C::$w is write-only
    /// Indirect modification of C::$a is not allowed
    /// array(1) {
    ///   [0]=>
    ///   int(1)
    /// }
    /// string(1) "v"
    /// ```
    pub(crate) fn read_hooked(
        &mut self,
        object: PhpObject,
        name: &[u8],
        hooked: Hooked,
        read: PropertyRead,
    ) -> Result<Value, Thrown> {
        let Some(get) = hooked.get else {
            if hooked.is_virtual {
                return Err(self.hooked_error(
                    object,
                    name,
                    "Property {class}::${name} is write-only",
                ));
            }
            let value = self.read_backing(object, name, read)?;
            return self.refuse_indirect(object, name, value, read);
        };
        if !self.should_call_hook(&hooked, name, object) {
            if hooked.is_virtual {
                return Err(self.hooked_error(
                    object,
                    name,
                    "Must not read from virtual property {class}::${name}",
                ));
            }
            return self.read_backing(object, name, read);
        }
        let value = self.call_hook(object, get, &[])?;
        self.refuse_indirect(object, name, value, read)
    }

    /// `zend_assign_to_property_reference` over a hooked property whose
    /// `get` hook the read under `BP_VAR_W` calls: the hook's answer is
    /// what the read hands back (`retval = rv`), so `zend_fetch_property_address`
    /// hands that back even where `Indirect modification` was raised
    /// after it, and the site's `Cannot assign by reference to
    /// overloaded object` is thrown over it, the first its previous. A
    /// hook that threw answered nothing, and its throwable is the one
    /// raised. `None` where no such hook serves the read.
    ///
    /// ```php
    /// class T { public $v { get { return $this->v; } set { $this->v = $value; } } }
    /// $t = new T; $t->v = [];
    /// try { $t->v = &$r; } catch (Error $e) { echo $e->getMessage(), " / ", $e->getPrevious()->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// Cannot assign by reference to overloaded object / Indirect modification of T::$v is not allowed
    /// ```
    pub(crate) fn hooked_reference_refusal(
        &mut self,
        object: PhpObject,
        name: &[u8],
    ) -> Option<Thrown> {
        let hooked = self
            .hooked(object.class(), name)
            .filter(|hooked| self.hooked_accessible(hooked))?;
        let get = hooked
            .get
            .filter(|_| self.should_call_hook(&hooked, name, object))?;
        let answered = match self.call_hook(object, get, &[]) {
            Ok(answered) => answered,
            Err(thrown) => return Some(thrown),
        };
        let refused = self.refuse_indirect(object, name, answered, PropertyRead::Write);
        let thrown = crate::api::raising::throw_error(
            self,
            "Error",
            "Cannot assign by reference to overloaded object",
        );
        match refused {
            Ok(value) => self.release(value),
            Err(previous) => {
                if let (Some(&throwable), Some(&previous)) =
                    (thrown.exception(), previous.exception())
                {
                    self.set_previous(throwable, previous);
                }
            }
        }
        Some(thrown)
    }

    /// `Indirect modification of %s::$%s is not allowed`: a write
    /// fetch's read of a hooked property that answered what no write
    /// through it can reach, anything but an object or a reference.
    fn refuse_indirect(
        &mut self,
        object: PhpObject,
        name: &[u8],
        value: Value,
        read: PropertyRead,
    ) -> Result<Value, Thrown> {
        if read != PropertyRead::Write
            || value.as_object().is_some()
            || value.as_php_ref().is_some()
        {
            return Ok(value);
        }
        self.release(value);
        Err(self.hooked_error(
            object,
            name,
            "Indirect modification of {class}::${name} is not allowed",
        ))
    }

    /// The backing value of a hooked property, its slot read as
    /// `zend_std_read_property`'s `try_again` reads one: the value, one
    /// more owner; the lazy object initialised where the slot waits; and
    /// a typed slot nothing wrote the uninitialised-access `Error`.
    fn read_backing(
        &mut self,
        object: PhpObject,
        name: &[u8],
        read: PropertyRead,
    ) -> Result<Value, Thrown> {
        let Some(placed) = self.declared_slot(object.class(), name) else {
            return Ok(Value::null());
        };
        let o = object.heap();
        let z = o.slot(placed.slot);
        if !z.is_undef() {
            let value = Value::of_slot(z).deref();
            self.addref(&value);
            return Ok(value);
        }
        if self.lazy_slot(o, placed.slot) {
            let instance = self.lazy_init(object)?;
            return self.read_backing(instance, name, read);
        }
        if read == PropertyRead::Silent {
            return Ok(Value::null());
        }
        if placed.typed {
            return Err(self.uninitialized_property(object.class(), name));
        }
        let class = self.class_name(object.class());
        let shown = String::from_utf8_lossy(name).into_owned();
        crate::api::raising::warning(self, &format!("Undefined property: {class}::${shown}"));
        Ok(Value::null())
    }

    /// `zend_std_write_property`'s hooked arm: the `set` hook called, held
    /// to the property's set visibility first; where there is none, the
    /// backing value written, which a virtual property has not; inside
    /// the hook, the backing value. `value` and `name` are given up
    /// whatever arm runs.
    ///
    /// ```php
    /// class C {
    ///     public $r { get => 1; }
    ///     public int $n = 0 { set => $value * 2; }
    ///     public private(set) int $p = 0 { set { $this->p = $value; } }
    /// }
    /// $c = new C;
    /// try { $c->r = 2; } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// try { $c->p = 2; } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// $c->n = 21;
    /// $c->n++;
    /// var_dump($c->n);
    /// ```
    /// ```expect
    /// Property C::$r is read-only
    /// Cannot modify private(set) property C::$p from global scope
    /// int(86)
    /// ```
    pub(crate) fn write_hooked(
        &mut self,
        object: PhpObject,
        name: Owner<PhpString>,
        value: Owner<Value>,
        hooked: Hooked,
    ) -> Result<(), Thrown> {
        let bytes = name.as_bytes().to_vec();
        let refused = |req: &mut Request, name: Owner<PhpString>, value: Owner<Value>, sentence| {
            value.give_up(req);
            name.give_up(req);
            Err(req.hooked_error(object, &bytes, sentence))
        };
        let Some(set) = hooked.set else {
            if hooked.is_virtual {
                return refused(self, name, value, "Property {class}::${name} is read-only");
            }
            return self.write_backing(object, name, value, hooked);
        };
        if !self.should_call_hook(&hooked, &bytes, object) {
            if hooked.is_virtual {
                let sentence = "Must not write to virtual property {class}::${name}";
                return refused(self, name, value, sentence);
            }
            return self.write_backing(object, name, value, hooked);
        }
        if let Some(set_visibility) = hooked.set_visibility
            && !self.has_set_access(hooked.declaring, hooked.root, set_visibility)
        {
            let thrown = self.set_inaccessible_property(
                object.class(),
                set_visibility,
                false,
                &bytes,
                "modify",
            );
            value.give_up(self);
            name.give_up(self);
            return Err(thrown);
        }
        let served = self.call_hook(object, set, &[*value]);
        value.give_up(self);
        name.give_up(self);
        let answered = served?;
        self.release(answered);
        Ok(())
    }

    /// The backing value written, as `zend_std_write_property`'s
    /// `try_again` writes a slot: held to the set visibility, the lazy
    /// object initialised where the slot waits, then the slot written
    /// under its type.
    fn write_backing(
        &mut self,
        object: PhpObject,
        name: Owner<PhpString>,
        value: Owner<Value>,
        hooked: Hooked,
    ) -> Result<(), Thrown> {
        if let Some(set_visibility) = hooked.set_visibility
            && !self.has_set_access(hooked.declaring, hooked.root, set_visibility)
        {
            let thrown = self.set_inaccessible_property(
                object.class(),
                set_visibility,
                false,
                name.as_bytes(),
                "modify",
            );
            value.give_up(self);
            name.give_up(self);
            return Err(thrown);
        }
        if let Some(placed) = self.declared_slot(object.class(), name.as_bytes())
            && object.heap().slot(placed.slot).is_undef()
            && self.lazy_slot(object.heap(), placed.slot)
        {
            return self.lazy_write(object, name, value, false);
        }
        self.write_std_property(object, name, value)
    }

    /// `zend_std_has_property`'s hooked arm: `property_exists` finds a
    /// virtual property and a backed one's slot; `isset` and `empty` call
    /// the `get` hook and read what it answered, or read the backing
    /// value where there is no hook to call.
    ///
    /// ```php
    /// class C { public $v { get => null; } public $b = 1 { set => $value; } }
    /// $c = new C;
    /// var_dump(isset($c->v), empty($c->v), isset($c->b), property_exists($c, 'v'));
    /// ```
    /// ```expect
    /// bool(false)
    /// bool(true)
    /// bool(true)
    /// bool(true)
    /// ```
    pub(crate) fn has_hooked(
        &mut self,
        object: PhpObject,
        name: &[u8],
        test: PropertyTest,
        hooked: Hooked,
    ) -> Result<bool, Thrown> {
        if test == PropertyTest::Exists {
            if hooked.is_virtual {
                return Ok(true);
            }
            return self.has_backing(object, name, test);
        }
        let Some(get) = hooked.get else {
            if hooked.is_virtual {
                return Err(self.hooked_error(
                    object,
                    name,
                    "Property {class}::${name} is write-only",
                ));
            }
            return self.has_backing(object, name, test);
        };
        if !self.should_call_hook(&hooked, name, object) {
            if hooked.is_virtual {
                return Err(self.hooked_error(
                    object,
                    name,
                    "Must not read from virtual property {class}::${name}",
                ));
            }
            return self.has_backing(object, name, test);
        }
        let value = self.call_hook(object, get, &[])?;
        let answer = match test {
            PropertyTest::NotEmpty => crate::api::coerce::truthy(self, &value),
            PropertyTest::Isset | PropertyTest::Exists => Ok(value.deref().tag() != TypeTag::Null),
        };
        self.release(value);
        answer
    }

    /// The backing value tested as `zend_std_has_property`'s slot arm
    /// tests one, the lazy object initialised where the slot waits.
    fn has_backing(
        &mut self,
        object: PhpObject,
        name: &[u8],
        test: PropertyTest,
    ) -> Result<bool, Thrown> {
        let Some(placed) = self.declared_slot(object.class(), name) else {
            return Ok(false);
        };
        let o = object.heap();
        let z = o.slot(placed.slot);
        if z.is_undef() {
            if self.lazy_slot(o, placed.slot) {
                let instance = self.lazy_init(object)?;
                return self.has_backing(instance, name, test);
            }
            return Ok(false);
        }
        let value = Value::of_slot(z).deref();
        Ok(match test {
            PropertyTest::Exists => true,
            PropertyTest::Isset => value.tag() != TypeTag::Null,
            PropertyTest::NotEmpty => crate::api::coerce::truthy(self, &value)?,
        })
    }

    /// `zend_std_unset_property`'s hooked arm, php's `Error` whether or
    /// not a hook is running.
    ///
    /// ```php
    /// class C { public $p = 1 { get => $this->p; } }
    /// $c = new C;
    /// try { unset($c->p); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// Cannot unset hooked property C::$p
    /// ```
    pub(crate) fn unset_hooked(&mut self, object: PhpObject, name: &[u8]) -> Thrown {
        self.hooked_error(
            object,
            name,
            "Cannot unset hooked property {class}::${name}",
        )
    }

    /// `zend_std_get_property_ptr_ptr`: a hooked property's backing slot
    /// in place where the hook reads it, and no place otherwise, so a
    /// write goes to `write_property` and a write fetch reads through
    /// `read_property`; every other name the standard place.
    ///
    /// ```php
    /// class C {
    ///     public array $log = [] { get => $this->log; set { $this->log = $value; $this->log[] = 'set'; } }
    /// }
    /// $c = new C;
    /// $c->log = ['a'];
    /// var_dump($c->log);
    /// ```
    /// ```expect
    /// array(2) {
    ///   [0]=>
    ///   string(1) "a"
    ///   [1]=>
    ///   string(3) "set"
    /// }
    /// ```
    pub(crate) fn property_place(&self, object: PhpObject, name: &[u8]) -> PropertyPlace {
        match self.hooked(object.class(), name) {
            Some(hooked)
                if self.hooked_accessible(&hooked)
                    && (hooked.is_virtual || self.should_call_hook(&hooked, name, object)) =>
            {
                PropertyPlace::Handlers
            }
            _ => PropertyPlace::Std,
        }
    }
}

/// `zend_get_property_hook_trampoline`'s two functions, each with the
/// declaration the engine numbers it under: named `$::get` and `$::set`
/// where php names the one it makes after the property, since the
/// call carries the property's own name ([`Request::trampoline_property`]).
/// Their parameters are php's `arg_info`: none for `get`, and `$value`
/// for `set`, so a named argument is placed by that name and any other
/// is php's `Error` for an unknown one; each entry holds the count it
/// was sent to its own, as php's does with its `zpp`.
///
/// ```php
/// class A { public mixed $prop1; public mixed $prop2 { set(mixed $custom) => $custom; } }
/// class B extends A {
///     public mixed $prop1 { set { parent::$prop1::set(value: 42); parent::$prop1::set(unknown: 43); } }
///     public mixed $prop2 { set { parent::$prop2::set(custom: 42); parent::$prop2::set(value: 43); } }
/// }
/// $b = new B();
/// try { $b->prop1 = 0; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// try { $b->prop2 = 0; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// var_dump($b->prop1, $b->prop2);
/// ```
/// ```expect
/// Unknown named parameter $unknown
/// Unknown named parameter $value
/// int(42)
/// int(42)
/// ```
pub(crate) fn parent_hook_trampolines() -> Vec<(HookKind, MethodDecl, Entry)> {
    HookKind::ALL
        .into_iter()
        .map(|kind| {
            let entry: Entry = match kind {
                HookKind::Get => parent_hook_get,
                HookKind::Set => parent_hook_set,
            };
            let parameters = match kind {
                HookKind::Get => Vec::new(),
                HookKind::Set => vec![parameter(b"value", None)],
            };
            let name = format!("$::{}", kind.name());
            let function = FunctionDecl {
                parameters,
                ..native_function(name.as_bytes())
            };
            let method = public_method(function, true);
            (kind, method, entry)
        })
        .collect()
}

/// The receiver and the property of the trampoline being served, and
/// the sentence its `zpp` raises for `given` arguments where it takes
/// `wanted`, `%s() expects exactly %d argument%s, %d given`.
fn trampoline_call(
    req: &mut Request,
    kind: HookKind,
    wanted: usize,
) -> Result<(PhpObject, Vec<u8>), Thrown> {
    let Some((name, scope)) = req.trampoline_property() else {
        return Err(crate::api::raising::throw_error(
            req,
            "Error",
            "InternalError: a parent hook trampoline served with no property",
        ));
    };
    let given = req.argument_count();
    if given != wanted {
        let class = scope.map(|scope| req.class_name(scope)).unwrap_or_default();
        let shown = String::from_utf8_lossy(&name).into_owned();
        // The member string `get_active_function_or_method_name` makes
        // stands across the printed throw.
        let member = format!("{class}::${shown}::{}", kind.name());
        let message = format!(
            "{member}() expects exactly {wanted} argument{}, {given} given",
            if wanted == 1 { "" } else { "s" }
        );
        return Err(crate::api::raising::under_name(req, Some(&member), |req| {
            crate::api::raising::throw_error(req, "ArgumentCountError", &message)
        }));
    }
    let Some(this) = req.this() else {
        return Err(crate::api::raising::throw_error(
            req,
            "Error",
            "InternalError: a parent hook trampoline served with no receiver",
        ));
    };
    Ok((this, name))
}

/// `zend_parent_hook_get_trampoline`: the property read through the
/// object's `read_property`, which finds the trampoline running and so
/// reads what the parent's property holds.
fn parent_hook_get(req: &mut Request) -> Result<Owner<Value>, Thrown> {
    let (this, name) = trampoline_call(req, HookKind::Get, 0)?;
    let table = req.object_handlers(this.heap());
    table.read_property(req, this, &name, PropertyRead::Warning)
}

/// `zend_parent_hook_set_trampoline`: the value written through the
/// object's `write_property`, and returned.
fn parent_hook_set(req: &mut Request) -> Result<Owner<Value>, Thrown> {
    let (this, name) = trampoline_call(req, HookKind::Set, 1)?;
    let value = req.argument(1).unwrap_or_else(Value::null).deref();
    let key = req.string(&name)?;
    let key = Owner::adopt(key);
    // The return's owner, and the write's.
    let returned = Owner::take(req, value);
    let written = Owner::take(req, value);
    let table = req.object_handlers(this.heap());
    if let Err(thrown) = table.write_property(req, this, key, written) {
        returned.give_up(req);
        return Err(thrown);
    }
    Ok(returned)
}

/// The table [`Request::hooked_object_table`] builds, php's
/// `zend_hooked_object_build_properties`, for the one reader that asks:
/// php's table, its declared rows first and the dynamic ones after
/// them, each under the key php's table holds it by, a hooked row a
/// `null` its reader reads the hook in place of, as php reads an
/// `IS_PTR` row, the caller's to give up once it has read it
/// (`zend_release_properties`); what the reader reads those rows by
/// ([`HookedRows`]); and `zend_hash_num_elements` of php's table, which
/// also counts a row for each virtual property with no `get` hook and
/// each slot nothing wrote, rows every reader steps over and this table
/// leaves out. What [`crate::api::abi::PropertiesFor::Hooked`] hands a
/// reader of an object whose class has a hooked property.
#[derive(Debug)]
pub struct HookedTable {
    pub(crate) table: Owner<PhpArray>,
    pub(crate) rows: HookedRows,
    pub(crate) elements: u32,
}

/// What a reader of a [`HookedTable`] reads its `IS_PTR` rows by: the
/// object whose rows they are, a lazy object's instance; what php's
/// `IS_INDIRECT` and `IS_PTR` rows carry of each declared row, in the
/// table's order; and the build, which says what name a hook is read
/// under.
#[derive(Debug, Clone)]
pub(crate) struct HookedRows {
    pub(crate) object: PhpObject,
    pub(crate) declared: Vec<HookedDeclared>,
    pub(crate) build: HookedBuild,
}

/// One declared row of a [`HookedTable`], its `prop_info`: the name as
/// declared, the class that declared it and its visibility, and the
/// hooked property it is, none for a slot's row.
#[derive(Debug, Clone)]
pub(crate) struct HookedDeclared {
    pub(crate) name: Box<[u8]>,
    pub(crate) declaring: ClassId,
    pub(crate) visibility: Visibility,
    pub(crate) hooked: Option<Hooked>,
}

/// Which reader a [`HookedTable`] is built for, which decides its rows
/// and the name its hooks are read under: `get_object_vars` and
/// `var_export` read each under a `zend_string_init` copy of the
/// unmangled name, and `json_encode`, which reads none but a public
/// row's, under `prop_info->name` itself; `foreach`'s iterator
/// (`zho_build_properties_ex(zobj, true, true, false)`) lays only the
/// declared rows the scope may reach, every one an `IS_PTR`, and reads
/// a hook under a copy of the name where the name is mangled.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum HookedBuild {
    Properties,
    Json,
    Iterator,
}

/// `sizeof(zend_hooked_object_iterator)`, what
/// `zend_hooked_object_get_iterator` `emalloc`s for a `foreach`.
const HOOKED_ITERATOR: usize = 160;

/// A `foreach` over an object whose class has a hooked property, for the
/// length of the loop: the iterator `zend_hooked_object_get_iterator`
/// allocates and the table of declared rows it walks
/// ([`HookedBuild::Iterator`]), named by the loop's hash iterator.
pub(crate) struct HookedWalk {
    at: u32,
    iterator: php_heap::Block,
    table: PhpArray,
    rows: HookedRows,
}

/// Whose a key of `zho_build_properties_ex`'s table is: php keys the
/// table by the mangled name, so a public property, the protected one
/// every class of the line shares, and each class's private one are
/// three keys for one name.
#[derive(Clone, Copy, PartialEq, Eq)]
enum KeyOwner {
    Public,
    Protected,
    Private(ClassId),
}

impl Request {
    /// The declared rows of `zho_build_properties_ex`'s table in its
    /// order: the chain walked from its root, each class's own instance
    /// properties updating the row of their key in place or adding one,
    /// each row the name, the class whose declaration it is and its
    /// visibility.
    fn hooked_object_keys(&self, class: ClassId) -> Vec<(Box<[u8]>, ClassId, Visibility)> {
        let chain: Vec<ClassId> = self.lineage(class).collect();
        let mut keys: Vec<(Box<[u8]>, KeyOwner, ClassId, Visibility)> = Vec::new();
        for here in chain.into_iter().rev() {
            let Some(decl) = self.class_decl(here) else {
                continue;
            };
            for property in decl.properties.iter().filter(|p| p.is_instance) {
                let name = &property.name.0;
                // "When promoting properties from protected to public,
                // use the unmangled name to preserve order."
                let owner = match property.visibility {
                    Visibility::Public => KeyOwner::Public,
                    Visibility::Protected
                        if self
                            .property_info_of(class, name)
                            .is_some_and(|(_, p)| p.visibility == Visibility::Public) =>
                    {
                        KeyOwner::Public
                    }
                    Visibility::Protected => KeyOwner::Protected,
                    Visibility::Private => KeyOwner::Private(here),
                };
                let row = (name.clone(), owner, here, property.visibility);
                match keys.iter().position(|k| k.0 == row.0 && k.1 == owner) {
                    Some(i) => keys[i] = row,
                    None => keys.push(row),
                }
            }
        }
        keys.into_iter()
            .map(|(name, _, declaring, visibility)| (name, declaring, visibility))
            .collect()
    }

    /// `zend_hooked_object_get_iterator` for a `foreach` over `object`,
    /// whose class has a hooked property, registered at the loop's hash
    /// iterator `at`: the iterator allocated, then its table of the
    /// declared rows the scope may reach, both held until the loop ends
    /// ([`Request::hooked_walk_end`]), so `memory_get_peak_usage()`
    /// counts them while it runs:
    ///
    /// ```php
    /// function pk($f) { $k = $f(); unset($k); $s = memory_get_usage(); memory_reset_peak_usage(); $k = $f(); $p = memory_get_peak_usage(); $u = memory_get_usage(); echo $p - $s, ' ', $u - $s, "\n"; }
    /// class H { public $a=1; public $b=1; public $c=1; public $d=1; public $e=1; public $f=1; public $g=1; public $h=1; public $i { get => 5; } public $j = 2 { get => $this->j; } }
    /// $h = new H;
    /// pk(function () use ($h) { $n = 0; foreach ($h as $v) $n++; return $n; });
    /// class P { public $a = 1; private $pp = 3; protected $qq = 4; public $i { get => 5; } function z() { foreach ($this as $k => $v) {} return 1; } }
    /// $p = new P;
    /// pk(fn () => $p->z());
    /// ```
    /// ```expect
    /// 872 0
    /// 600 0
    /// ```
    pub(crate) fn hooked_walk_start(&mut self, object: PhpObject, at: u32) -> Result<(), Thrown> {
        let iterator = self.heap.block(HOOKED_ITERATOR);
        let iterator = self.fatal_of(iterator)?;
        let built = match self.hooked_object_table(object, HookedBuild::Iterator) {
            Ok(Some(built)) => built,
            built => {
                self.heap.free_block(iterator);
                return built.map(|_| ());
            }
        };
        let (table, rows) = (built.table.hand_on(), built.rows);
        let walk = HookedWalk {
            at,
            iterator,
            table,
            rows,
        };
        self.properties.hooked_walks.push(walk);
        Ok(())
    }

    /// The end of the loop [`Request::hooked_walk_start`] began at hash
    /// iterator `at`: its table and its iterator given back
    /// (`zho_it_dtor`, then the iterator's `efree`).
    pub(crate) fn hooked_walk_end(&mut self, at: u32) {
        let walks = &mut self.properties.hooked_walks;
        if let Some(i) = walks.iter().position(|walk| walk.at == at) {
            let walk = walks.swap_remove(i);
            Owner::adopt(walk.table).give_up(self);
            self.heap.free_block(walk.iterator);
        }
    }

    /// `zho_declared_it_fetch_current`'s walk over the iterator's table,
    /// what a `foreach` over an object whose class has a hooked property
    /// reads: from the row `lead` stands at, the next whose value there
    /// is, a hooked one's read through its `get` hook under its class's
    /// scope, one owner the caller's ([`super::Walked::Hooked`]), and a
    /// virtual one with no `get` hook stepped over; none past the last,
    /// where the dynamic table's walk takes over. `lead` is left one past
    /// the row answered.
    ///
    /// ```php
    /// #[AllowDynamicProperties]
    /// class C { public $a = 1; public $v { get => 'v'; } public $b = 2 { get => $this->b * 10; } public $w { set {} } }
    /// $c = new C;
    /// $c->d = 3;
    /// foreach ($c as $k => $v) echo "$k=$v;";
    /// try { foreach ($c as $k => &$v) {} } catch (Error $e) { echo "\n", $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// a=1;v=v;b=20;d=3;
    /// Cannot create reference to property C::$v
    /// ```
    pub(crate) fn hooked_object_walk(
        &mut self,
        at: u32,
        lead: &mut usize,
        by_ref: bool,
    ) -> Result<Option<(Owner<PhpString>, super::Walked)>, Thrown> {
        loop {
            let walks = &self.properties.hooked_walks;
            let Some(walk) = walks.iter().find(|walk| walk.at == at) else {
                return Ok(None);
            };
            let Some(row) = walk.rows.declared.get(*lead).cloned() else {
                return Ok(None);
            };
            let (object, class) = (walk.rows.object, walk.rows.object.class());
            *lead += 1;
            if let Some(hooked) = row.hooked {
                if hooked.is_virtual && hooked.get.is_none() {
                    continue;
                }
                // A loop by reference binds only what a `get` hook
                // returns by reference.
                if by_ref
                    && !hooked
                        .get
                        .and_then(|get| self.function_decl(get))
                        .is_some_and(|decl| decl.returns_by_ref)
                {
                    return Err(self.hooked_error(
                        object,
                        &row.name,
                        "Cannot create reference to property {class}::${name}",
                    ));
                }
                // `value == &EG(uninitialized_zval)`: a backed property
                // with no `get` hook whose slot nothing wrote is no row.
                if hooked.get.is_none()
                    && self
                        .declared_slot(class, &row.name)
                        .is_none_or(|placed| object.heap().slot(placed.slot).is_undef())
                {
                    continue;
                }
                let value = self.read_deferred(object, &row, HookedBuild::Iterator)?;
                let walked = super::Walked::Hooked(value.hand_on());
                return self.iterator_key(&row).map(|key| Some((key, walked)));
            }
            let Some(slot) = self.slot_declared_by(class, row.declaring, &row.name) else {
                continue;
            };
            if object.heap().slot(slot).is_undef() {
                continue;
            }
            let walked = super::Walked::Slot(slot);
            return self.iterator_key(&row).map(|key| Some((key, walked)));
        }
    }

    /// `zho_declared_it_fetch_current`'s `current_key`: the declared
    /// name, or a string of its own for a protected or a private row,
    /// whose name the table holds mangled (`zend_string_init` of the
    /// unmangled part).
    fn iterator_key(&mut self, row: &HookedDeclared) -> Result<Owner<PhpString>, Thrown> {
        match row.visibility {
            Visibility::Public => self.declared_string(&row.name),
            _ => self.string_init(&row.name).map(Owner::adopt),
        }
    }

    /// php's `ce->num_hooked_props != 0`: whether a row of `class`'s
    /// properties table has hooks, which `get_object_vars`, the encoders
    /// and `foreach` read the object through the hooks' table and
    /// iterator for. A parent's private hooked property the class
    /// redeclares is no row, though the parent's scope still reads it
    /// through its hooks ([`ClassHooks::ParentPrivate`]):
    ///
    /// ```php
    /// function pk($f) { $k = $f(); unset($k); $s = memory_get_usage(); memory_reset_peak_usage(); $k = $f(); $p = memory_get_peak_usage(); $u = memory_get_usage(); echo $p - $s, ' ', $u - $s, "\n"; }
    /// class P { private $x { get => 5; } protected $q = 3; public $r = 4; function f() { return $this->x; } }
    /// class C extends P {}
    /// class R extends P { public $q = 5; private $x = 6; public $s1=1; public $s2=1; public $s3=1; public $s4=1; public $s5=1; public $s6=1; }
    /// var_dump((new C)->f(), (new R)->f());
    /// $r = new R;
    /// pk(fn () => get_object_vars($r));
    /// ```
    /// ```expect
    /// int(5)
    /// int(5)
    /// 728 696
    /// ```
    pub(crate) fn class_has_hooks(&self, class: ClassId) -> bool {
        self.class_hooks(class) == ClassHooks::Rows
    }

    /// Where `class`'s property hooks are, its linked record's
    /// [`php_link::LinkedClass::hooks`], or for a class with no record
    /// read off its chain's declarations by the same rule.
    fn class_hooks(&self, class: ClassId) -> ClassHooks {
        if let Some(record) = self.linked_class(class) {
            return record.hooks;
        }
        let mut hooks = ClassHooks::Absent;
        for here in self.lineage(class) {
            let Some(decl) = self.class_decl(here) else {
                continue;
            };
            for property in decl.properties.iter().filter(|p| p.hooks.is_some()) {
                if self.row_hooked(class, &property.name.0).is_some() {
                    return ClassHooks::Rows;
                }
                hooks = ClassHooks::ParentPrivate;
            }
        }
        hooks
    }

    /// `zend_hooked_object_build_properties`, the table
    /// `get_object_vars`, `json_encode` and `var_export` read an object
    /// whose class has a hooked property through, for the reader `build`
    /// names: the chain walked from its root, each class's own
    /// properties updating the row of their key in place or adding one,
    /// a hooked property's row one its reader reads through the `get`
    /// hook as it reaches it ([`Request::read_deferred`]), a virtual one
    /// with no `get` hook no row, a slot nothing wrote no row, then the
    /// dynamic table. The table is a request's array laid
    /// down at the size php's has once it holds its rows, so the
    /// readers' `memory_get_peak_usage()` counts it beside what they lay
    /// down while they read it ([`HookedTable`]). None for a class with
    /// no hooked property, whose readers read the object's own table.
    ///
    /// ```php
    /// #[AllowDynamicProperties]
    /// class T {
    ///     public $a = 'a';
    ///     public $v { get => 'V'; }
    ///     public $b = 'b' { get => strtoupper($this->b); }
    ///     public $w { set {} }
    ///     private $p = 'p' { get => $this->p . 'P'; }
    /// }
    /// $t = new T;
    /// $t->d = 'd';
    /// var_dump(get_object_vars($t));
    /// echo json_encode($t), "\n";
    /// var_export($t);
    /// ```
    /// ```expect
    /// array(4) {
    ///   ["a"]=>
    ///   string(1) "a"
    ///   ["v"]=>
    ///   string(1) "V"
    ///   ["b"]=>
    ///   string(1) "B"
    ///   ["d"]=>
    ///   string(1) "d"
    /// }
    /// {"a":"a","v":"V","b":"B","d":"d"}
    /// \T::__set_state(array(
    ///    'a' => 'a',
    ///    'v' => 'V',
    ///    'b' => 'B',
    ///    'p' => 'pP',
    ///    'd' => 'd',
    /// ))
    /// ```
    ///
    /// The declared rows are read off the slots whatever the object's
    /// table holds, and the dynamic ones are every row the table holds
    /// that is no `IS_INDIRECT` (`zho_build_properties_ex`), so a table
    /// that holds copies of the declared rows
    /// ([`php_heap::DeclaredRows::Copied`]) hands each copy on as a row
    /// of its own, under the name the slot's row has, as php's
    /// `zend_hash_add_new` lays both down; the `foreach` walk reads the
    /// same rows.
    ///
    /// ```php
    /// #[AllowDynamicProperties]
    /// class H { public int $b = 1; public $h = 5 { get => $this->h * 10; } function __construct() { $this->a = 1; $this->b = 3; } }
    /// $o = (new ReflectionClass(H::class))->newLazyGhost(function ($o) { $o->__construct(); });
    /// var_dump($o); $o->a += 1;
    /// echo json_encode($o), "\n";
    /// var_export($o); echo "\n";
    /// foreach ($o as $k => $v) echo "$k => $v\n";
    /// ```
    /// ```expect
    /// lazy ghost object(H)#3 (0) {
    ///   ["b"]=>
    ///   uninitialized(int)
    /// }
    /// {"b":3,"h":50,"b":1,"h":5,"a":2}
    /// \H::__set_state(array(
    ///    'b' => 3,
    ///    'h' => 50,
    ///    'b' => 1,
    ///    'h' => 5,
    ///    'a' => 2,
    /// ))
    /// b => 3
    /// h => 50
    /// b => 1
    /// h => 5
    /// a => 2
    /// ```
    pub(crate) fn hooked_object_table(
        &mut self,
        object: PhpObject,
        build: HookedBuild,
    ) -> Result<Option<HookedTable>, Thrown> {
        let class = object.class();
        if !self.class_has_hooks(class) {
            return Ok(None);
        }
        let object = self.lazy_init(object)?;
        let keys = self.hooked_object_keys(class);
        let o = object.heap();
        // `zend_new_array` of the object's table's elements where it has
        // one, else of `default_properties_count`, then
        // `zend_hash_real_init_mixed`; laid here at the size php's ends
        // at once it holds its rows, the rows this table leaves out
        // counted.
        let (initial, held) = match o.properties() {
            Some(table) => (o.slot_table().len(), table.len()),
            None => (o.slots(), 0),
        };
        let elements = u32::try_from(keys.len().saturating_add(held)).unwrap_or(u32::MAX);
        let mut table = PhpArray::sized(self, initial.max(elements), Layout::Hash)?;
        let mut declared = Vec::with_capacity(keys.len());
        let iterator = build == HookedBuild::Iterator;
        // `parents`, the chain walked from its root, an `emalloc` freed
        // once the declared rows are laid.
        let parents = self.heap.block(8 * self.lineage(class).count());
        let parents = self.fatal_of(parents)?;
        let laid = 'laid: {
            for (name, declaring, visibility) in keys {
                let hooked = self.row_hooked(declaring, &name);
                // `check_access`: `zend_check_property_access` under the scope.
                if iterator {
                    if let Err(thrown) = self.access_name_copy(&name, visibility) {
                        break 'laid Err(thrown);
                    }
                    if !self.property_row_visible(class, &name, Some(declaring))
                        || hooked.is_some_and(|hooked| !self.hooked_accessible(&hooked))
                    {
                        continue;
                    }
                }
                let row = HookedDeclared {
                    name,
                    declaring,
                    visibility,
                    hooked,
                };
                match self.hooked_declared_row(object, &mut table, &row, iterator) {
                    Ok(true) => declared.push(row),
                    Ok(false) => {}
                    Err(thrown) => break 'laid Err(thrown),
                }
            }
            Ok(())
        };
        self.heap.free_block(parents);
        if let Err(thrown) = laid {
            table.give_up(self);
            return Err(thrown);
        }
        let dynamic = object.heap().properties().filter(|_| !iterator);
        let rows: Vec<(Key, Value)> = dynamic.map_or_else(Vec::new, |dynamic| {
            dynamic
                .iter()
                .map(|(key, z)| (Key::of(key), Value::of_slot(z)))
                .collect()
        });
        for (key, value) in rows {
            let key = Owner::take(self, key);
            let value = Owner::take(self, value);
            if let Err(thrown) = table.add_new(self, key, value) {
                table.give_up(self);
                return Err(thrown);
            }
        }
        let rows = HookedRows {
            object,
            declared,
            build,
        };
        Ok(Some(HookedTable {
            table,
            rows,
            elements,
        }))
    }

    /// One declared row of [`Request::hooked_object_table`] laid into
    /// `table` under php's key, the name mangled for a protected or a
    /// private property: for a hooked property, and for every row of
    /// `foreach`'s iterator (`force_ptr`), a `null` its reader reads in
    /// place of, and for a slot the
    /// value its `IS_INDIRECT` reads through to, a reference only the
    /// slot holds read as its value (the `Z_REFCOUNT_P(value) == 1` of
    /// `get_object_vars`, the `ZVAL_DEREF` of the encoders). False where
    /// php's row is one every reader steps over: a virtual property with
    /// no `get` hook, a slot nothing wrote.
    fn hooked_declared_row(
        &mut self,
        object: PhpObject,
        table: &mut Owner<PhpArray>,
        row: &HookedDeclared,
        force_ptr: bool,
    ) -> Result<bool, Thrown> {
        let value = match row.hooked {
            _ if force_ptr => Value::null(),
            Some(hooked) if hooked.is_virtual && hooked.get.is_none() => return Ok(false),
            Some(_) => Value::null(),
            None => {
                let class = object.class();
                let Some(slot) = self.slot_declared_by(class, row.declaring, &row.name) else {
                    return Ok(false);
                };
                let z = object.heap().slot(slot);
                if z.is_undef() {
                    return Ok(false);
                }
                self.dup_value(Value::of_slot(z), None)
            }
        };
        let value = Owner::adopt(value);
        let key = match self.property_prefix(row.declaring, row.visibility) {
            Some(prefix) => self.declared_string(&[prefix.as_slice(), &row.name].concat()),
            None => self.declared_string(&row.name),
        };
        let key = match key {
            Ok(key) => key.into_key(),
            Err(thrown) => {
                value.give_up(self);
                return Err(thrown);
            }
        };
        table.add_new(self, key, value)?;
        Ok(true)
    }
}

impl Request {
    /// [`Request::hooked_object_table`] as `zend_get_properties_for`
    /// hands it to `json_encode` and `var_export`
    /// (`zend_hooked_object_build_properties`), a table built for the
    /// reader, php's `*is_temp`, each declared name mangled as php keys
    /// it, `\0*\0p` for a protected property and `\0C\0p` for a private
    /// one, each row laid down without the lookup, so a name the hooks
    /// answer and the table holds as a copy is two rows; each hooked row
    /// read by the reader as it reaches it ([`crate::api::values::walk::Row::read`]),
    /// so a hook of a row the reader steps over is never called, as
    /// `json_encode` steps over a protected or private one:
    ///
    /// ```php
    /// class S { private $p { get { echo "p read\n"; return 1; } } public $q { get { echo "q read\n"; return 2; } } }
    /// echo json_encode(new S), "\n";
    /// var_export(new S); echo "\n";
    /// ```
    /// ```expect
    /// q read
    /// {"q":2}
    /// p read
    /// q read
    /// \S::__set_state(array(
    ///    'p' => 1,
    ///    'q' => 2,
    /// ))
    /// ```
    ///
    /// None for a class with no hooked property. A lazy object whose
    /// initializer throws answers the empty table and leaves the
    /// throwable pending for the reader to hand on once it has written
    /// what it read, as `zend_hooked_object_build_properties` answers
    /// `zend_empty_array` under `EG(exception)`
    /// (`Z/Zend/zend_property_hooks.c:137`).
    ///
    /// ```php
    /// class H { public $a { get => 1; } public $b = 2; }
    /// $o = (new ReflectionClass(H::class))->newLazyGhost(function ($o) { throw new Exception('init'); });
    /// try { var_export($o); } catch (Exception $e) { echo "\n", $e->getMessage(), "\n"; }
    /// try { json_encode($o); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
    /// var_dump(json_last_error(), $o);
    /// ```
    /// ```expect
    /// \H::__set_state(array(
    /// ))
    /// init
    /// init
    /// int(0)
    /// lazy ghost object(H)#3 (0) {
    /// }
    /// ```
    pub(crate) fn hooked_properties_for(
        &mut self,
        object: PhpObject,
        purpose: PropertyPurpose,
    ) -> Result<Option<PropertiesFor>, Thrown> {
        if !self.class_has_hooks(object.class()) {
            return Ok(None);
        }
        if let Err(thrown) = self.lazy_init(object) {
            if thrown.exception().is_none() {
                return Err(thrown);
            }
            self.leave_pending(&thrown);
            let empty = Owner::take(self, PhpArray::empty());
            return Ok(Some(PropertiesFor::Temporary(empty)));
        }
        let build = match purpose {
            PropertyPurpose::Json => HookedBuild::Json,
            _ => HookedBuild::Properties,
        };
        Ok(self
            .hooked_object_table(object, build)?
            .map(PropertiesFor::Hooked))
    }

    /// `zend_check_property_access` over a declared row php's table keys
    /// by its mangled name, a protected or a private one: the copy of the
    /// unmangled name it looks the property up by (`zend_string_init`),
    /// released before it answers, so `memory_get_peak_usage()` counts it
    /// above the table being filled.
    ///
    /// ```php
    /// function pk($f) { $k = $f(); unset($k); $s = memory_get_usage(); memory_reset_peak_usage(); $k = $f(); $p = memory_get_peak_usage(); $u = memory_get_usage(); echo $p - $s, ' ', $u - $s, "\n"; }
    /// class Q { public $q = 5; private $p = 6; public $s1=1; public $s2=1; public $s3=1; public $s4=1; public $s5=1; public $s6=1; public $r = 4; }
    /// class H { private $p = 6; public $s1=1; public $s2=1; public $s3=1; public $s4=1; public $s5=1; public $s6=1; public $r { get => 4; } }
    /// $q = new Q; $h = new H;
    /// pk(fn () => get_object_vars($q));
    /// pk(fn () => get_object_vars($h));
    /// ```
    /// ```expect
    /// 728 696
    /// 784 376
    /// ```
    pub(crate) fn access_name_copy(
        &mut self,
        name: &[u8],
        visibility: Visibility,
    ) -> Result<(), Thrown> {
        if visibility != Visibility::Public {
            let copy = Owner::adopt(self.string_init(name)?);
            copy.give_up(self);
        }
        Ok(())
    }

    /// An `IS_PTR` row of a [`HookedTable`] read through its `get` hook,
    /// `zend_read_property_ex(prop_info->ce, zobj, name, silent)`, under
    /// the name its reader passes ([`HookedBuild`]): a copy of the
    /// unmangled name (`zend_string_init`, one byte or not) held while
    /// the hook is called, or `prop_info->name` itself.
    pub(crate) fn read_deferred(
        &mut self,
        object: PhpObject,
        row: &HookedDeclared,
        build: HookedBuild,
    ) -> Result<Owner<Value>, Thrown> {
        let Some(hooked) = row.hooked else {
            return Ok(Owner::null());
        };
        let copied = match build {
            HookedBuild::Properties => true,
            HookedBuild::Json => false,
            HookedBuild::Iterator => row.visibility != Visibility::Public,
        };
        let copy = copied.then(|| self.string_init(&row.name)).transpose()?;
        let read = self.read_hooked(object, &row.name, hooked, PropertyRead::Silent);
        if let Some(copy) = copy {
            Owner::adopt(copy).give_up(self);
        }
        read.map(Owner::adopt)
    }
}

impl Request {
    /// [`crate::api::call::property_hook`]: the hook of `kind` the
    /// property `name` of `class` carries, with the class that wrote it.
    pub(crate) fn property_hook_decl(
        &self,
        class: ClassId,
        name: &[u8],
        kind: HookKind,
    ) -> Option<(ClassId, MethodDecl)> {
        let resolved = self.resolved_property(class, name)?;
        let hook = resolved.hook(kind)?;
        Some((self.class_of_decl(class, hook.class)?, hook.method.clone()))
    }

    /// The function of the hook of `kind` the property `name` of `class`
    /// carries, none for an abstract one or none at all.
    pub(crate) fn property_hook_function(
        &self,
        class: ClassId,
        name: &[u8],
        kind: HookKind,
    ) -> Option<Function> {
        let hooked = self.row_hooked(class, name)?;
        match kind {
            HookKind::Get => hooked.get,
            HookKind::Set => hooked.set,
        }
    }

    /// [`crate::api::call::read_raw_property`].
    pub(crate) fn read_raw(&mut self, object: PhpObject, name: &[u8]) -> Result<Value, Thrown> {
        match self.row_hooked(object.class(), name) {
            Some(hooked) if hooked.is_virtual => Err(self.hooked_error(
                object,
                name,
                "Must not read from virtual property {class}::${name}",
            )),
            _ => self.read_backing(object, name, PropertyRead::Warning),
        }
    }

    /// [`crate::api::call::write_raw_property`].
    pub(crate) fn write_raw(
        &mut self,
        object: PhpObject,
        name: Owner<PhpString>,
        value: Owner<Value>,
    ) -> Result<(), Thrown> {
        let Some(hooked) = self.row_hooked(object.class(), name.as_bytes()) else {
            return self.write_property(object, name, value);
        };
        if hooked.is_virtual {
            let bytes = name.as_bytes().to_vec();
            value.give_up(self);
            name.give_up(self);
            return Err(self.hooked_error(
                object,
                &bytes,
                "Must not write to virtual property {class}::${name}",
            ));
        }
        if let Some(placed) = self.declared_slot(object.class(), name.as_bytes())
            && object.heap().slot(placed.slot).is_undef()
            && self.lazy_slot(object.heap(), placed.slot)
        {
            return self.lazy_write(object, name, value, false);
        }
        self.write_std_property(object, name, value)
    }
}
