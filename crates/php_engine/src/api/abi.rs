//! The ABI group (design §4.4, §7.1): the [`Extension`] value and its
//! builder, the marshalling vocabulary a native's signature is written
//! in, and the two state declarations of ADR-0005 with [`Cache`], the
//! one thing that changes across requests. The marshalling traits are
//! markers here; the functions the macros expand to land with the macros
//! and their lines in `api.lock`.

use std::any::{Any, TypeId};
use std::cmp::Ordering;
use std::collections::HashMap;
use std::hash::Hash;
use std::sync::{Arc, Mutex};

use php_heap::{Fatal, Heap, Obj};
use php_unit::decl::{
    ClassConstDecl, ClassDecl, ConstDecl, FunctionDecl, IniDecl, MethodDecl, Name,
};

use crate::api::call::{Function, Target};
use crate::api::coerce::BinaryOp;
use crate::api::raising::Thrown;
use crate::api::request::Request;
use crate::api::values::{ClassId, ClosureInfo, PhpArray, PhpObject, PhpString, TypeTag, Value};

/// The startup ini view an extension's engine state is built from: what
/// the host set before the first request, immutable afterwards.
#[derive(Debug, Clone, Default)]
pub struct StartupIni {
    pub entries: Vec<(Vec<u8>, Vec<u8>)>,
}

/// What an ini entry's `on_modify` function says of a new value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IniVerdict {
    Accepted,
    Rejected,
}

/// One row of `zend_module_entry.deps`, a `zend_module_dep`
/// (`Z/Zend/zend_modules.h`): the other extension's name, how this one
/// stands to it, and the `rel` and `version` a `ZEND_MOD_REQUIRED_EX`
/// row carries, which php prints after the kind and
/// `ReflectionExtension::getDependencies` reads. The startup-order
/// check php makes over the table is `api-check`'s bundle order here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dependency {
    pub name: String,
    pub kind: DependencyKind,
    pub rel: Option<String>,
    pub version: Option<String>,
}

/// `MODULE_DEP_REQUIRED`, `MODULE_DEP_CONFLICTS` and
/// `MODULE_DEP_OPTIONAL`, the three kinds a `zend_module_dep` row has.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DependencyKind {
    Required,
    Conflicts,
    Optional,
}

/// The engine-owned, bounded, get-or-build cache of immutable values an
/// extension holds as a field of its engine state (ADR-0005): the pcre
/// pattern cache and the unit cache are its two users. The `Mutex`, the
/// bound and the eviction are here and nowhere an extension can see.
pub struct Cache<K, V> {
    capacity: usize,
    entries: Mutex<HashMap<K, Arc<V>>>,
}

impl<K: Hash + Eq + Clone, V> Cache<K, V> {
    /// A cache holding at most `capacity` values; at the bound half are
    /// dropped, php's partial clean.
    #[must_use]
    pub fn new(capacity: usize) -> Cache<K, V> {
        Cache {
            capacity: capacity.max(1),
            entries: Mutex::new(HashMap::new()),
        }
    }

    /// The value for `key`, built by `build` once and shared afterwards.
    pub fn get_or_build(&self, key: K, build: impl FnOnce(&K) -> V) -> Arc<V> {
        let mut entries = match self.entries.lock() {
            Ok(entries) => entries,
            Err(poisoned) => poisoned.into_inner(),
        };
        if let Some(found) = entries.get(&key) {
            return Arc::clone(found);
        }
        if entries.len() >= self.capacity {
            let keep = self.capacity / 2;
            let keys: Vec<K> = entries.keys().skip(keep).cloned().collect();
            for k in keys {
                entries.remove(&k);
            }
        }
        let value = Arc::new(build(&key));
        entries.insert(key, Arc::clone(&value));
        value
    }
}

/// A native function's entry: the concrete request, off which it reads
/// its arguments with [`Request::argument_count`] and [`Request::argument`]
/// as `ZEND_FUNCTION` reads them off `execute_data`, and the value or the
/// pending throwable (ADR-0002). The call frame holding the arguments
/// stays on the VM stack while the entry runs, which is what lets a
/// backtrace taken inside it print them.
pub(crate) type Entry = fn(&mut Request) -> Result<Value, Thrown>;
/// Builds one extension's request state at request start.
pub(crate) type StateBuilder = (TypeId, fn() -> Box<dyn Any + Send>);
/// Builds one extension's engine state from the startup ini, once.
pub(crate) type EngineStateBuilder =
    Arc<dyn Fn(&StartupIni) -> Box<dyn Any + Send + Sync> + Send + Sync>;
/// The `RINIT` function.
pub(crate) type Rinit = fn(&mut Request) -> Result<(), Thrown>;

/// An ini entry's `on_modify`, php's `PHP_INI_MH` at
/// `ZEND_INI_STAGE_RUNTIME`: served by `ini_set()` with the new value
/// before the entry takes it, raising what the entry's handler raises,
/// and answering whether the value is taken, `false` being php's
/// `FAILURE`, which leaves the entry as it was and makes `ini_set()`
/// answer `false`.
pub type OnModify = fn(&mut Request, &[u8]) -> Result<bool, Thrown>;
/// The `MINFO` function: writes the extension's `phpinfo()` table
/// through [`Request::info_table`].
pub(crate) type Info = fn(&mut Request);
/// A native class as an extension declares it (design §7.1): the
/// declaration, beside it each method's declaration with its entry,
/// the shape `functions` has, and the class's handler table (design
/// §5.4), none where the class takes its parent's, as php's
/// `create_object` is inherited, and [`Std`]'s at a root. The methods
/// given here are the class's; the engine places them on the
/// declaration as it links it, so the declaration's own `methods` is
/// left empty by an extension. A method's entry is served as a
/// function's is, with the receiver on the frame as `Request::this`
/// (ADR-0002). The fourth is the class's `create_object` where its
/// objects carry a native payload, [`NativeNew`].
pub(crate) type NativeClassDecl = (
    ClassDecl,
    Vec<(MethodDecl, Entry)>,
    Option<&'static dyn ObjectHandlers>,
    Option<NativeNew>,
);

/// `ce->create_object` for a class whose objects carry a native payload
/// (design §5.4), php's `zend_object_alloc(sizeof(custom), ce)`: the
/// allocation sized for the payload with the payload's `Default`
/// already written, so the slot stands before the constructor runs and
/// a method reaches it through [`PhpObject::native_mut`]. `None` is
/// php's null slot, a class whose objects are `zend_objects_new` and
/// carry nothing; the engine takes the nearest parent's, as php
/// inherits `create_object`.
pub(crate) type NativeNew = fn(&mut Heap, u32, u32) -> Result<Obj, Fatal>;

/// One [`NativeNew`], the payload's `Default` into a fresh object of
/// `class` with `slots` declared property slots.
/// The `create_object` of a native class of the engine's own whose
/// objects carry a `T`, as [`ExtensionBuilder::class_payload`] gives an
/// extension's.
pub(crate) fn new_payload<T: Any + Send + Default>() -> NativeNew {
    new_with_payload::<T> as NativeNew
}

fn new_with_payload<T: Any + Send + Default>(
    heap: &mut Heap,
    class: u32,
    slots: u32,
) -> Result<Obj, Fatal> {
    heap.object_with(class, slots, T::default())
}

/// One extension (design §7.1): declarations paired with entries,
/// constants, ini entries, the lifecycle functions, and the two state
/// declarations. Built by [`ExtensionBuilder`], linked by the engine.
#[derive(Clone)]
pub struct Extension {
    pub(crate) name: String,
    pub(crate) functions: Vec<(FunctionDecl, Entry)>,
    pub(crate) classes: Vec<NativeClassDecl>,
    pub(crate) constants: Vec<ConstDecl>,
    pub(crate) ini: Vec<IniDecl>,
    pub(crate) on_modify: Vec<(Vec<u8>, OnModify)>,
    pub(crate) dependencies: Vec<Dependency>,
    pub(crate) request_state: Option<StateBuilder>,
    pub(crate) engine_state: Option<EngineStateBuilder>,
    pub(crate) rinit: Option<Rinit>,
    pub(crate) rshutdown: Option<fn(&mut Request)>,
    pub(crate) info: Option<Info>,
    /// What [`ExtensionBuilder::register`] was handed, in its order.
    pub(crate) registered: Vec<Arc<dyn Any + Send + Sync>>,
    /// What [`ExtensionBuilder::class_constants`] was handed: each
    /// class another extension declares, with the constants this one
    /// adds to it.
    pub(crate) class_constants: Vec<(Name, Vec<ClassConstDecl>)>,
    /// What [`ExtensionBuilder::trampolines`] was handed: each class,
    /// with the methods its `get_method` answers off its table.
    pub(crate) trampolines: Vec<(Name, MethodDecl, Entry)>,
}

impl std::fmt::Debug for Extension {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Extension")
            .field("name", &self.name)
            .field("functions", &self.functions.len())
            .field("classes", &self.classes.len())
            .finish_non_exhaustive()
    }
}

/// The builder of an [`Extension`], what a crate's `extension()` calls.
#[derive(Default)]
pub struct ExtensionBuilder {
    functions: Vec<(FunctionDecl, Entry)>,
    classes: Vec<NativeClassDecl>,
    constants: Vec<ConstDecl>,
    ini: Vec<IniDecl>,
    on_modify: Vec<(Vec<u8>, OnModify)>,
    dependencies: Vec<Dependency>,
    request_state: Option<StateBuilder>,
    engine_state: Option<EngineStateBuilder>,
    rinit: Option<Rinit>,
    rshutdown: Option<fn(&mut Request)>,
    info: Option<Info>,
    registered: Vec<Arc<dyn Any + Send + Sync>>,
    class_constants: Vec<(Name, Vec<ClassConstDecl>)>,
    trampolines: Vec<(Name, MethodDecl, Entry)>,
}

impl ExtensionBuilder {
    /// The natives: each declaration with its entry.
    #[must_use]
    pub fn functions(mut self, functions: Vec<(FunctionDecl, Entry)>) -> ExtensionBuilder {
        self.functions.extend(functions);
        self
    }

    /// The native classes: each declaration with its methods' entries
    /// beside it, `(ClassDecl, Vec<(MethodDecl, Entry)>)`, the shape
    /// [`ExtensionBuilder::functions`] has. The engine links each into
    /// the linked table beside the classes Zend declares (design §6.2,
    /// §7.1); a method's entry reads its arguments as a function's does
    /// and its receiver as `Request::this`.
    #[must_use]
    pub fn classes(
        mut self,
        classes: Vec<(ClassDecl, Vec<(MethodDecl, Entry)>)>,
    ) -> ExtensionBuilder {
        self.classes.extend(
            classes
                .into_iter()
                .map(|(decl, methods)| (decl, methods, None, None)),
        );
        self
    }

    /// One native class whose objects carry a payload of `T` (design
    /// §5.4): the declaration and its methods as
    /// [`ExtensionBuilder::class_with`] takes them, the [`ObjectHandlers`]
    /// that release and walk what the payload holds, and `T` itself as
    /// the class's `create_object`. `new` lays the payload down as
    /// `T::default()` before the constructor runs, so a method reaches
    /// it through [`PhpObject::native_mut`] with no allocation of its
    /// own; a subclass takes it as it takes the table, since php
    /// inherits `create_object`.
    #[must_use]
    pub fn class_payload<T: Any + Send + Default>(
        mut self,
        class: ClassDecl,
        methods: Vec<(MethodDecl, Entry)>,
        handlers: &'static dyn ObjectHandlers,
    ) -> ExtensionBuilder {
        self.classes.push((
            class,
            methods,
            Some(handlers),
            Some(new_with_payload::<T> as NativeNew),
        ));
        self
    }

    /// One native class with its own handler table (design §5.4): the
    /// declaration and its methods' entries as [`ExtensionBuilder::classes`]
    /// takes them, and beside them the [`ObjectHandlers`] the kernels
    /// dispatch to for an object of the class, `Std` for every slot the
    /// table leaves to it. A class declared through `classes` takes its
    /// parent's table, and `Std` at a root, as php inherits
    /// `create_object`.
    #[must_use]
    pub fn class_with(
        mut self,
        class: ClassDecl,
        methods: Vec<(MethodDecl, Entry)>,
        handlers: &'static dyn ObjectHandlers,
    ) -> ExtensionBuilder {
        self.classes.push((class, methods, Some(handlers), None));
        self
    }

    /// The constants.
    #[must_use]
    pub fn constants(mut self, constants: Vec<ConstDecl>) -> ExtensionBuilder {
        self.constants.extend(constants);
        self
    }

    /// The ini entries with their defaults.
    #[must_use]
    pub fn ini(mut self, entries: Vec<IniDecl>) -> ExtensionBuilder {
        self.ini.extend(entries);
        self
    }

    /// The `on_modify` of the ini entry `name`, one of the extension's
    /// own, served whenever a program changes it with `ini_set()`.
    #[must_use]
    pub fn on_modify(mut self, name: &[u8], handler: OnModify) -> ExtensionBuilder {
        self.on_modify.push((name.to_vec(), handler));
        self
    }

    /// The `deps` table, `ZEND_MOD_REQUIRED("libxml")` and its kin, in
    /// the order the extension lists them; an extension that declares
    /// none has php's null pointer, the empty table.
    #[must_use]
    pub fn dependencies(mut self, dependencies: Vec<Dependency>) -> ExtensionBuilder {
        self.dependencies.extend(dependencies);
        self
    }

    /// The `MINFO` function, what `phpinfo()`, `php --ri` and
    /// `ReflectionExtension::info` print for the extension, written
    /// through [`Request::info_table`]; an extension without one gets
    /// php's `Version` row and its ini entries.
    #[must_use]
    pub fn info(mut self, hook: fn(&mut Request)) -> ExtensionBuilder {
        self.info = Some(hook);
        self
    }

    /// Declare request state: built at request start in bundle order,
    /// dropped after the `rshutdown` hooks in reverse (ADR-0005).
    #[must_use]
    pub fn request_state<T: Default + Send + 'static>(mut self) -> ExtensionBuilder {
        self.request_state = Some((TypeId::of::<T>(), || Box::new(T::default())));
        self
    }

    /// Declare engine state: built once in `Engine::new` from the startup
    /// ini, immutable afterwards, shared by every request (ADR-0005).
    #[must_use]
    pub fn engine_state<E: Send + Sync + 'static>(
        mut self,
        build: fn(&StartupIni) -> E,
    ) -> ExtensionBuilder {
        self.engine_state = Some(Arc::new(move |ini: &StartupIni| {
            Box::new(build(ini)) as Box<dyn Any + Send + Sync>
        }));
        self
    }

    /// The request-start function, `RINIT`.
    #[must_use]
    pub fn rinit(mut self, hook: fn(&mut Request) -> Result<(), Thrown>) -> ExtensionBuilder {
        self.rinit = Some(hook);
        self
    }

    /// The request-end function, `RSHUTDOWN`, served with every extension's
    /// request state still live.
    #[must_use]
    pub fn rshutdown(mut self, hook: fn(&mut Request)) -> ExtensionBuilder {
        self.rshutdown = Some(hook);
        self
    }

    /// A value this extension hands at startup to the extension
    /// that reads every value of its type, php's `php_pdo_register_driver`
    /// into `pdo_driver_hash` from `pdo_sqlite`'s `MINIT`: immutable once the
    /// engine is built, and answered by [`Request::registered`] in bundle
    /// order, which is `MINIT` order (ADR-0005).
    #[must_use]
    pub fn register<T: Send + Sync + 'static>(mut self, value: T) -> ExtensionBuilder {
        self.registered.push(Arc::new(value));
        self
    }

    /// Constants this extension declares on a class another extension
    /// declares, php's `zend_declare_class_constant_ex` over a foreign
    /// `zend_class_entry` at `MINIT`, which is how `pdo_sqlite` puts its
    /// deprecated `PDO::SQLITE_*` aliases on `PDO`: appended to that
    /// class's own as the engine links it, after every constant the class
    /// declares, in bundle order. A class the bundle does not declare
    /// takes none.
    #[must_use]
    pub fn class_constants(
        mut self,
        class: &[u8],
        constants: Vec<ClassConstDecl>,
    ) -> ExtensionBuilder {
        self.class_constants.push((Name(class.into()), constants));
        self
    }

    /// Methods of a class another extension declares that the class's
    /// `get_method` answers and its table carries none of, `pdo`'s
    /// methods (`pdo_dbh_methods.get_driver_methods`, which
    /// `pdo_hash_methods` lays down with `func.scope = pdo_dbh_ce`): each
    /// is a trampoline of `class` ([`Request::trampoline`]), so no lookup
    /// by name and no rendering finds it, and its diagnostics name the
    /// class it is laid down on.
    #[must_use]
    pub fn trampolines(
        mut self,
        class: &[u8],
        methods: Vec<(MethodDecl, Entry)>,
    ) -> ExtensionBuilder {
        self.trampolines.extend(
            methods
                .into_iter()
                .map(|(method, entry)| (Name(class.into()), method, entry)),
        );
        self
    }

    /// The extension, named as php names it (`ctype`, `pcre`).
    #[must_use]
    pub fn build(self, name: &str) -> Extension {
        Extension {
            name: name.to_owned(),
            functions: self.functions,
            classes: self.classes,
            constants: self.constants,
            ini: self.ini,
            on_modify: self.on_modify,
            dependencies: self.dependencies,
            request_state: self.request_state,
            engine_state: self.engine_state,
            rinit: self.rinit,
            rshutdown: self.rshutdown,
            info: self.info,
            registered: self.registered,
            class_constants: self.class_constants,
            trampolines: self.trampolines,
        }
    }
}

/// A validated callable: a function name, a `[$object, 'method']` pair,
/// a `Class::method` string or a `Closure`, as `zend_is_callable` accepts
/// them, held with the value it came from and the callee it resolved to.
#[derive(Debug, Clone, PartialEq)]
pub struct Callable {
    value: Value,
    name: String,
    target: Target,
}

impl Callable {
    pub(crate) fn of(value: Value, name: &[u8], target: Target) -> Callable {
        Callable {
            value,
            name: String::from_utf8_lossy(name).into_owned(),
            target,
        }
    }

    pub(crate) fn name(&self) -> String {
        self.name.clone()
    }

    /// `zend_fcall_info_cache` as `zend_is_callable_ex` filled it: what
    /// the callable resolved to when it was made, which is what serving
    /// it enters. php holds the same three fields for the same span,
    /// `zend_fcc_dup` past the frame the callable was read off.
    pub(crate) const fn target(&self) -> Target {
        self.target
    }

    /// The value the callable came from, a view and no owner taken. A
    /// list that holds the callable past the call frame it was read off
    /// takes its owners with [`Callable::addref`], not of this alone.
    pub(crate) const fn value(&self) -> Value {
        self.value
    }

    pub(crate) fn into_value(self) -> Value {
        self.value
    }

    /// `zend_fcc_addref` (`Z/Zend/zend_API.h:773`): the owners a list
    /// that keeps the callable past the frame it was read off takes, its
    /// value's and, `GC_ADDREF(fcc->object)`, the receiver's it resolved
    /// to. A `Class::method` string taken with `$this` in scope names an
    /// object the string does not own, so the list owns it itself. A
    /// closure's owner is its value's, since `fcc->closure` is the value.
    /// Given up by [`Callable::release`].
    pub(crate) fn addref(&self, req: &Request) {
        req.addref(&self.value);
        if let Target::Method {
            this: Some(this), ..
        } = self.target
        {
            req.addref(&this.as_value());
        }
    }

    /// `zend_fcc_dtor` (`Z/Zend/zend_API.h:798`): the owners
    /// [`Callable::addref`] took given up, the receiver's first,
    /// `OBJ_RELEASE(fcc->object)`, and then the value's.
    pub(crate) fn release(self, req: &mut Request) {
        if let Target::Method {
            this: Some(this), ..
        } = self.target
        {
            req.release(this.as_value());
        }
        req.release(self.value);
    }
}

/// A type a native parameter may have: the engine coerces the argument
/// through its own kernels and the caller's `strict_types`. The
/// conversion functions the macros expand to land with the macros.
pub trait FromArgument: Sized {}

impl FromArgument for Value {}
impl FromArgument for bool {}
impl FromArgument for i64 {}
impl FromArgument for f64 {}
impl FromArgument for PhpString {}
impl FromArgument for PhpObject {}
impl FromArgument for Callable {}
impl<T: FromArgument> FromArgument for Option<T> {}

/// A type a native may return: converted to a value by the engine.
pub trait IntoReturn {}

impl IntoReturn for () {}
impl IntoReturn for Value {}
impl IntoReturn for bool {}
impl IntoReturn for i64 {}
impl IntoReturn for f64 {}
impl IntoReturn for PhpString {}
impl IntoReturn for PhpObject {}
impl<T: IntoReturn> IntoReturn for Option<T> {}
impl<T: IntoReturn> IntoReturn for OrFalse<T> {}

/// The receiver of a native method: `$this`.
#[derive(Debug, Clone)]
pub struct This(pub PhpObject);

/// A by-reference parameter written by the native and never read.
#[derive(Debug, Clone, Default)]
pub struct Out<T> {
    value: Option<T>,
}

impl<T> Out<T> {
    /// Write the value the caller's variable will hold.
    pub fn set(&mut self, value: T) {
        self.value = Some(value);
    }
}

/// A by-reference parameter read and written by the native.
#[derive(Debug, Clone)]
pub struct InOut<T> {
    pub value: T,
}

/// A return of `T|false`.
#[derive(Debug, Clone, PartialEq)]
pub enum OrFalse<T> {
    Value(T),
    False,
}

/// A parameter of two types, `array|string` and its kin; nested for
/// wider unions.
#[derive(Debug, Clone, PartialEq)]
pub enum Union<A, B> {
    A(A),
    B(B),
}

/// The variadic tail of a native's parameters.
#[derive(Debug, Clone, Default)]
pub struct Variadic<T>(pub Vec<T>);

/// The native payload of an internal class (design §5.4): one typed slot
/// on the object, its release and its tracing derived from the type.
pub trait NativeClass: Any + Send + Sync {}

/// What `has_property` is asked of a property, php's
/// `ZEND_PROPERTY_ISSET`, `ZEND_PROPERTY_NOT_EMPTY` and
/// `ZEND_PROPERTY_EXISTS`: `isset($o->p)`, `empty($o->p)` and
/// `property_exists`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PropertyTest {
    Isset,
    NotEmpty,
    Exists,
}

/// Which read `read_property` is serving, php's `type` argument to the
/// slot: `BP_VAR_R`, the read that warns of a property nothing holds
/// and refuses one the scope may not reach; `BP_VAR_IS`, the read an
/// `isset`, an `empty` or a `??` makes, which raises none of the three
/// and answers `null`; and `BP_VAR_W`, the read a write fetch falls
/// back to where `get_property_ptr_ptr` answered no place, which reads
/// as `BP_VAR_R` does and refuses a hooked property's value that no
/// write through it could reach back into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PropertyRead {
    Warning,
    Silent,
    Write,
}

/// What `get_property_ptr_ptr` answers for one name, php's pointer or
/// its `NULL`: whether a write to the property addresses the standard
/// place, or goes through the class's `read_property` and
/// `write_property` because the class serves the name itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PropertyPlace {
    /// `zend_std_get_property_ptr_ptr`'s place: the declared slot or
    /// the dynamic entry, which the kernel lays down itself under the
    /// scope's access rule, with the `__get` arm and every diagnostic
    /// the standard slot raises.
    Std,
    /// php's `NULL`: the class serves the name, so a write reaches its
    /// `write_property`, and a compound write, an increment and a write
    /// fetch read through its `read_property` first.
    Handlers,
}

/// What `get_properties_for` is asked for, php's `zend_prop_purpose`:
/// the five readers of an object's properties that a class may answer
/// each in its own way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PropertyPurpose {
    Debug,
    ArrayCast,
    Serialize,
    VarExport,
    Json,
}

/// The `clone_obj` slot: a fresh object holding what this one holds,
/// one owner, the caller's.
pub type CloneObj = fn(&mut Request, PhpObject) -> Result<PhpObject, Thrown>;
/// The `clone_obj_with` slot: `clone($o, [...])`, the clone with each
/// named property written through `write_property` from `scope`.
pub type CloneObjWith =
    fn(&mut Request, PhpObject, Option<ClassId>, PhpArray) -> Result<PhpObject, Thrown>;

/// Seam 2 (design §4.3, §5.4): Zend's 25 handler slots in 8.5 order
/// (`Z/Zend/zend_object_handlers.h:206-235`), one method each, the
/// default bodies being [`Std`]'s, the user-class semantics, so an
/// internal class overrides the slots it needs and inherits the rest.
/// The table is reached from an object through its class; the kernels
/// call the table, the methods here call the kernels, and nothing else
/// knows how an `ArrayObject` indexes.
///
/// A slot php marks required is a method with a default body. A slot
/// php marks optional answers `None` for php's null: `clone_obj` and
/// `clone_obj_with` answer the function the slot holds, since `clone`
/// reads the null before it looks at `__clone` and before it calls
/// anything; `count_elements`, `get_debug_info`, `get_closure`,
/// `do_operation` and `get_properties_for` answer the value or `None`
/// in one call, since their kernels fall back either way.
pub trait ObjectHandlers: Send + Sync {
    /// `free_obj` for the native payload: what the payload names of the
    /// heap given back before the payload is dropped, with the members
    /// already freed and the object's bucket already invalid. `Std`
    /// releases nothing, a class with no payload naming nothing.
    fn free_obj(&self, req: &mut Request, object: PhpObject) {
        let _ = (req, object);
    }

    /// `dtor_obj`, `zend_objects_destroy_object`: the class's
    /// `__destruct` served on the object, which the store holds at one
    /// owner meanwhile.
    fn dtor_obj(&self, req: &mut Request, object: PhpObject) {
        req.destruct(object);
    }

    /// `clone_obj`, `zend_objects_clone_obj` at `Std`: the members
    /// copied with one more owner each and `__clone` served on the copy.
    /// `None` is php's null slot, an uncloneable class.
    fn clone_obj(&self) -> Option<CloneObj> {
        Some(Request::clone_members)
    }

    /// `clone_obj_with`, `zend_objects_clone_obj_with` at `Std`: the
    /// class's `clone_obj`, then each given property written through
    /// `write_property`. `None` is php's null slot, a class whose
    /// objects `clone($o, [...])` refuses.
    fn clone_obj_with(&self) -> Option<CloneObjWith> {
        Some(Request::clone_members_with)
    }

    /// `read_property`, `zend_std_read_property` at `Std`: one more
    /// owner of the value read, a reference followed. `read` is php's
    /// `type` argument, `BP_VAR_R` or the `BP_VAR_IS` an `isset`, an
    /// `empty` or a `??` reads through.
    fn read_property(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: &[u8],
        read: PropertyRead,
    ) -> Result<Value, Thrown> {
        req.read_property(object, name, read)
    }

    /// `write_property`, `zend_std_write_property` at `Std`: `value` is
    /// the object's from here and `name` is given up once the object
    /// has taken its own.
    fn write_property(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: PhpString,
        value: Value,
    ) -> Result<(), Thrown> {
        req.write_property(object, name, value)
    }

    /// `read_dimension`, `zend_std_read_dimension` at `Std`:
    /// `offsetGet` on an `ArrayAccess`, `null` for `$o[]`, and php's
    /// `Error` for an object of any other class.
    fn read_dimension(
        &self,
        req: &mut Request,
        object: PhpObject,
        offset: Option<&Value>,
    ) -> Result<Value, Thrown> {
        req.read_dimension(object, offset)
    }

    /// `write_dimension`, `zend_std_write_dimension` at `Std`:
    /// `offsetSet` on an `ArrayAccess`, and php's `Error` otherwise.
    /// `value` is given up either way.
    fn write_dimension(
        &self,
        req: &mut Request,
        object: PhpObject,
        offset: Option<&Value>,
        value: Value,
    ) -> Result<(), Thrown> {
        req.write_dimension(object, offset, value)
    }

    /// `get_property_ptr_ptr`, `zend_std_get_property_ptr_ptr` at
    /// `Std`: where a write to `name` goes, [`PropertyPlace::Std`] for
    /// the standard place the kernel lays down itself, and
    /// [`PropertyPlace::Handlers`], php's `NULL`, for a name the class
    /// serves. The answer is a verdict with no effect, so `ASSIGN_OBJ`
    /// asks it too: php's sends every name its cache slot has not placed
    /// to `write_property`, and the cache is filled only by the standard
    /// placement, so a name a class serves reaches its `write_property`
    /// and every other name the in-place write. A class that overrides
    /// `write_property` for a name answers `Handlers` for it, whether or
    /// not the class declares it.
    ///
    /// A declared property the class serves is its handlers' on a
    /// write, as `DatePeriod`'s are, whose handlers refuse the write;
    /// and a compound write over a name `DateInterval` serves reads it
    /// through `read_property` and writes the sum back through
    /// `write_property`, with no dynamic property laid down.
    ///
    /// ```php
    /// $p = new DatePeriod(new DateTime('2020-01-01'), new DateInterval('P1D'), 2);
    /// try { $p->recurrences = 5; } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// var_dump($p->recurrences);
    /// $i = new DateInterval('P0Y');
    /// $i->f = 0.5;
    /// $i->f += 0.25;
    /// var_dump($i->f, $i->f++, ++$i->f);
    /// ```
    /// ```expect
    /// Cannot modify readonly property DatePeriod::$recurrences
    /// int(3)
    /// float(0.75)
    /// float(0.75)
    /// float(2.75)
    /// ```
    fn get_property_ptr_ptr(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: &[u8],
    ) -> PropertyPlace {
        req.property_place(object, name)
    }

    /// `has_property`, `zend_std_has_property` at `Std`: whether the
    /// property is there, is set, or is not empty, as `test` asks. A
    /// class with a `__isset` serves it here, and one with a `__get`
    /// beside it is asked that too under `empty`, so the answer is what
    /// a call left pending as much as what a slot holds.
    fn has_property(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: &[u8],
        test: PropertyTest,
    ) -> Result<bool, Thrown> {
        req.has_property(object, name, test)
    }

    /// `unset_property`, `zend_std_unset_property` at `Std`: the
    /// declared slot left uninitialised or the dynamic entry removed,
    /// what it held released.
    fn unset_property(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: &[u8],
    ) -> Result<(), Thrown> {
        req.unset_property(object, name)
    }

    /// `has_dimension`, `zend_std_has_dimension` at `Std`:
    /// `offsetExists` on an `ArrayAccess`, and `offsetGet`'s truth after
    /// it under `not_empty`, `empty($o[$k])`.
    fn has_dimension(
        &self,
        req: &mut Request,
        object: PhpObject,
        offset: &Value,
        not_empty: bool,
    ) -> Result<bool, Thrown> {
        req.has_dimension(object, offset, not_empty)
    }

    /// `unset_dimension`, `zend_std_unset_dimension` at `Std`:
    /// `offsetUnset` on an `ArrayAccess`.
    fn unset_dimension(
        &self,
        req: &mut Request,
        object: PhpObject,
        offset: &Value,
    ) -> Result<(), Thrown> {
        req.unset_dimension(object, offset)
    }

    /// `get_properties`, `zend_std_get_properties` at `Std`: the
    /// declared slots that hold a value then the dynamic table, in
    /// `rebuild_object_properties`' order; views, no owner taken. A lazy
    /// object is initialised first and the rows are those of what that
    /// answers, `zend_lazy_object_get_properties`.
    fn get_properties(
        &self,
        req: &mut Request,
        object: PhpObject,
    ) -> Result<Vec<(PhpString, Value)>, Thrown> {
        let object = req.lazy_init(object)?;
        req.object_entries(object.heap())
    }

    /// `get_method`, `zend_std_get_method` at `Std`: the method under
    /// the lowered name, by identity, and none where the class has no
    /// entry of that name. The access rule is the caller's, as php's
    /// `INIT_METHOD_CALL` applies it after the lookup.
    fn get_method(&self, req: &mut Request, object: PhpObject, lowered: &[u8]) -> Option<Function> {
        req.method_named(object.class(), lowered)
    }

    /// The object a method `get_method` did not find is looked up on
    /// and called on in its place, the `*object` php's `get_method`
    /// may replace: `spl_dual_it_get_method` hands an outer iterator's
    /// unknown method to its inner iterator, `spl_recursive_it_get_method`
    /// to the iterator of the level it stands on. A view, no owner
    /// taken; `None` at `Std`.
    ///
    /// ```php
    /// class Inner extends ArrayIterator { function hello() { return get_class($this) . " says hello"; } }
    /// $outer = new IteratorIterator(new Inner([1]));
    /// echo $outer->hello(), "\n";
    /// try { $outer->missing(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// Inner says hello
    /// Call to undefined method IteratorIterator::missing()
    /// ```
    fn method_receiver(&self, req: &mut Request, object: PhpObject) -> Option<PhpObject> {
        let _ = (req, object);
        None
    }

    /// `get_constructor`, `zend_std_get_constructor` at `Std`: the
    /// class's `__construct` where the calling scope may reach it, php's
    /// `Error` where it may not, and none for a class with no
    /// constructor.
    fn get_constructor(
        &self,
        req: &mut Request,
        object: PhpObject,
    ) -> Result<Option<Function>, Thrown> {
        req.constructor_of(object)
    }

    /// `get_class_name`, `zend_std_get_class_name` at `Std`: `ce->name`.
    fn get_class_name(&self, req: &mut Request, object: PhpObject) -> String {
        req.class_name(object.class())
    }

    /// `cast_object`, `zend_std_cast_object_tostring` at `Std`: the
    /// `__toString` of the class for a string, `true` for a bool, and
    /// `None`, php's `FAILURE`, for every other type and for a class
    /// with no `__toString`.
    fn cast_object(
        &self,
        req: &mut Request,
        object: PhpObject,
        to: TypeTag,
    ) -> Result<Option<Value>, Thrown> {
        req.cast_object(object, to)
    }

    /// `count_elements`: what `count()` answers for the object, and
    /// `None` at `Std`, php's null slot, so `count()` asks `Countable`
    /// and throws for a class that is not.
    fn count_elements(&self, req: &mut Request, object: PhpObject) -> Result<Option<i64>, Thrown> {
        let _ = (req, object);
        Ok(None)
    }

    /// `get_debug_info`: the table `var_dump` and `print_r` show in
    /// place of the properties, php's `*is_temp` one, with one owner,
    /// the caller's. `zend_std_get_debug_info` at `Std`, which is what
    /// `__debugInfo` returns. `None` is the members themselves, which
    /// php's slot answers as the properties table it is handed
    /// (`get_properties(object)`) and a reader here walks for itself, so
    /// a typed property nothing has written still shows as one.
    fn get_debug_info(
        &self,
        req: &mut Request,
        object: PhpObject,
    ) -> Result<Option<PhpArray>, Thrown> {
        req.debug_info(object)
    }

    /// `get_closure`: the function a call on the object serves, with
    /// its receiver and scope; `zend_std_get_closure` at `Std`,
    /// `__invoke` with the object as its receiver, and none for a class
    /// with no `__invoke`, which is not callable.
    fn get_closure(&self, req: &mut Request, object: PhpObject) -> Option<ClosureInfo> {
        req.invoke_closure(object)
    }

    /// `get_gc`: the values the object's payload owns, for the cycle
    /// collector's walk. The declared slots and the dynamic table are
    /// the heap's to walk; `Std` names nothing more.
    fn get_gc(&self, req: &mut Request, object: PhpObject, visit: &mut dyn FnMut(Value)) {
        let _ = (req, object, visit);
    }

    /// `do_operation`: the value an operator over the object answers,
    /// `right` none for a unary one; `None` at `Std`, php's null slot,
    /// so the operator is `zend_binop_error`'s.
    fn do_operation(
        &self,
        req: &mut Request,
        op: BinaryOp,
        left: &Value,
        right: Option<&Value>,
    ) -> Result<Option<Value>, Thrown> {
        let _ = (req, op, left, right);
        Ok(None)
    }

    /// `compare`, `zend_std_compare_objects` at `Std`: equal for one
    /// object, uncomparable (`None`, php's `ZEND_UNCOMPARABLE`) across
    /// classes, and the declared slots then the dynamic tables in order
    /// within one class.
    fn compare(
        &self,
        req: &mut Request,
        left: &Value,
        right: &Value,
    ) -> Result<Option<Ordering>, Thrown> {
        req.compare_objects(left, right)
    }

    /// The element a `zend_object_iterator` of the class stands at
    /// `index` steps past its rewind, its key and its value, one more
    /// owner each: what an `InternalIterator` over the object answers
    /// ([`crate::api::call::internal_iterator`]), for a class php walks
    /// with an iterator of its own in C rather than with methods.
    /// `None` at `Std` and past the last element.
    fn iterator_element(
        &self,
        req: &mut Request,
        object: PhpObject,
        index: usize,
    ) -> Result<Option<(Value, Value)>, Thrown> {
        let _ = (req, object, index);
        Ok(None)
    }

    /// `get_properties_for`: the rows one purpose reads; `None` at
    /// `Std`, php's null slot, so every purpose reads `get_properties`.
    fn get_properties_for(
        &self,
        req: &mut Request,
        object: PhpObject,
        purpose: PropertyPurpose,
    ) -> Result<Option<Vec<(PhpString, Value)>>, Thrown> {
        let _ = (req, object, purpose);
        Ok(None)
    }

    /// Whether the table's `get_properties` is not
    /// `zend_std_get_properties` or its `get_properties_for` is set: the
    /// class answers the reader of its properties itself, so its
    /// property table is not what a reader sees. What
    /// `spl_array_set_array` refuses an object as the backing of an
    /// `ArrayObject` for; `false` at `Std`, and `true` for every table
    /// that answers [`ObjectHandlers::get_properties_for`] for some
    /// purpose, and for every table php sets either slot on.
    ///
    /// ```php
    /// foreach ([new DateInterval('P1D'), new SplFixedArray(2), new stdClass] as $o) {
    ///     try { @new ArrayObject($o); echo get_class($o), " backs\n"; }
    ///     catch (InvalidArgumentException $e) { echo $e->getMessage(), "\n"; }
    /// }
    /// ```
    /// ```expect
    /// Overloaded object of type DateInterval is not compatible with ArrayObject
    /// Overloaded object of type SplFixedArray is not compatible with ArrayObject
    /// stdClass backs
    /// ```
    fn properties_overloaded(&self) -> bool {
        false
    }
}

/// `std_object_handlers`: every slot its default, the table a user
/// class and a native class that declares none are reached through.
#[derive(Debug, Clone, Copy, Default)]
pub struct Std;

impl ObjectHandlers for Std {}
