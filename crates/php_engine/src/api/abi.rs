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
use php_unit::decl::{ClassDecl, ConstDecl, FunctionDecl, IniDecl, MethodDecl};

use crate::api::call::Function;
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
    pub(crate) dependencies: Vec<Dependency>,
    pub(crate) request_state: Option<StateBuilder>,
    pub(crate) engine_state: Option<EngineStateBuilder>,
    pub(crate) rinit: Option<Rinit>,
    pub(crate) rshutdown: Option<fn(&mut Request)>,
    pub(crate) info: Option<Info>,
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
    dependencies: Vec<Dependency>,
    request_state: Option<StateBuilder>,
    engine_state: Option<EngineStateBuilder>,
    rinit: Option<Rinit>,
    rshutdown: Option<fn(&mut Request)>,
    info: Option<Info>,
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

    /// The extension, named as php names it (`ctype`, `pcre`).
    #[must_use]
    pub fn build(self, name: &str) -> Extension {
        Extension {
            name: name.to_owned(),
            functions: self.functions,
            classes: self.classes,
            constants: self.constants,
            ini: self.ini,
            dependencies: self.dependencies,
            request_state: self.request_state,
            engine_state: self.engine_state,
            rinit: self.rinit,
            rshutdown: self.rshutdown,
            info: self.info,
        }
    }
}

/// A validated callable: a function name, a `[$object, 'method']` pair,
/// a `Class::method` string or a `Closure`, as `zend_is_callable` accepts
/// them, held with the value it came from.
#[derive(Debug, Clone, PartialEq)]
pub struct Callable {
    value: Value,
    name: String,
}

impl Callable {
    pub(crate) fn named(value: Value, name: &[u8]) -> Callable {
        Callable {
            value,
            name: String::from_utf8_lossy(name).into_owned(),
        }
    }

    pub(crate) fn name(&self) -> String {
        self.name.clone()
    }

    pub(crate) fn into_value(self) -> Value {
        self.value
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

    /// `read_property` under `BP_VAR_R`, `zend_std_read_property` at
    /// `Std`: one more owner of the value read, a reference followed.
    fn read_property(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: &[u8],
    ) -> Result<Value, Thrown> {
        req.read_property(object, name)
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
    /// `Std`: the declared slot a write fetch may address in place,
    /// `OBJ_PROP_NUM`'s index, and `None` where the fetch goes through
    /// `read_property` and `write_property` instead.
    fn get_property_ptr_ptr(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: &[u8],
    ) -> Option<u32> {
        req.property_slot(object, name)
    }

    /// `has_property`, `zend_std_has_property` at `Std`: whether the
    /// property is there, is set, or is not empty, as `test` asks. A
    /// class with `__isset` would serve it here; the answer is a `bool`
    /// until one is declared.
    fn has_property(
        &self,
        req: &mut Request,
        object: PhpObject,
        name: &[u8],
        test: PropertyTest,
    ) -> bool {
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
    /// `rebuild_object_properties`' order; views, no owner taken.
    fn get_properties(
        &self,
        req: &mut Request,
        object: PhpObject,
    ) -> Result<Vec<(PhpString, Value)>, Thrown> {
        req.object_entries(object.heap())
    }

    /// `get_method`, `zend_std_get_method` at `Std`: the method under
    /// the lowered name, by identity, and none where the class has no
    /// entry of that name. The access rule is the caller's, as php's
    /// `INIT_METHOD_CALL` applies it after the lookup.
    fn get_method(&self, req: &mut Request, object: PhpObject, lowered: &[u8]) -> Option<Function> {
        req.method_named(object.class(), lowered)
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

    /// `get_debug_info`: the rows `var_dump` and `print_r` show in place
    /// of the properties; `zend_std_get_debug_info` at `Std`, what
    /// `__debugInfo` returns, and the properties for a class with none.
    /// `None` is php's null slot.
    fn get_debug_info(
        &self,
        req: &mut Request,
        object: PhpObject,
    ) -> Result<Option<Vec<(PhpString, Value)>>, Thrown> {
        req.debug_info(object).map(Some)
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
}

/// `std_object_handlers`: every slot its default, the table a user
/// class and a native class that declares none are reached through.
#[derive(Debug, Clone, Copy, Default)]
pub struct Std;

impl ObjectHandlers for Std {}
