//! The ABI group (design §4.4, §7.1): the [`Extension`] value and its
//! builder, the marshalling vocabulary a native's signature is written
//! in, and the two state declarations of ADR-0005 with [`Cache`], the
//! one thing that changes across requests. The marshalling traits are
//! markers here; the functions the macros expand to land with the macros
//! and their lines in `api.lock`.

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::hash::Hash;
use std::sync::{Arc, Mutex};

use php_unit::decl::{ClassDecl, ConstDecl, FunctionDecl, IniDecl};

use crate::api::raising::Thrown;
use crate::api::request::Request;
use crate::api::values::{PhpObject, PhpString, Value};

/// The startup ini view an extension's engine state is built from: what
/// the host set before the first request, immutable afterwards.
#[derive(Debug, Clone, Default)]
pub struct StartupIni {
    pub entries: Vec<(PhpString, PhpString)>,
}

/// What an ini entry's `on_modify` function says of a new value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IniVerdict {
    Accepted,
    Rejected,
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

/// A native function's entry: the concrete request, the arguments as
/// passed, and the value or the pending throwable (ADR-0002).
pub(crate) type Entry = fn(&mut Request, &[Value]) -> Result<Value, Thrown>;
/// Builds one extension's request state at request start.
pub(crate) type StateBuilder = (TypeId, fn() -> Box<dyn Any + Send>);
/// Builds one extension's engine state from the startup ini, once.
pub(crate) type EngineStateBuilder =
    Arc<dyn Fn(&StartupIni) -> Box<dyn Any + Send + Sync> + Send + Sync>;
/// The `RINIT` function.
pub(crate) type Rinit = fn(&mut Request) -> Result<(), Thrown>;

/// One extension (design §7.1): declarations paired with entries,
/// constants, ini entries, the lifecycle functions, and the two state
/// declarations. Built by [`ExtensionBuilder`], linked by the engine.
#[derive(Clone)]
pub struct Extension {
    pub(crate) name: String,
    pub(crate) functions: Vec<(FunctionDecl, Entry)>,
    pub(crate) classes: Vec<ClassDecl>,
    pub(crate) constants: Vec<ConstDecl>,
    pub(crate) ini: Vec<IniDecl>,
    pub(crate) request_state: Option<StateBuilder>,
    pub(crate) engine_state: Option<EngineStateBuilder>,
    pub(crate) rinit: Option<Rinit>,
    pub(crate) rshutdown: Option<fn(&mut Request)>,
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
    classes: Vec<ClassDecl>,
    constants: Vec<ConstDecl>,
    ini: Vec<IniDecl>,
    request_state: Option<StateBuilder>,
    engine_state: Option<EngineStateBuilder>,
    rinit: Option<Rinit>,
    rshutdown: Option<fn(&mut Request)>,
}

impl ExtensionBuilder {
    /// The natives: each declaration with its entry.
    #[must_use]
    pub fn functions(mut self, functions: Vec<(FunctionDecl, Entry)>) -> ExtensionBuilder {
        self.functions.extend(functions);
        self
    }

    /// The native classes, declared.
    #[must_use]
    pub fn classes(mut self, classes: Vec<ClassDecl>) -> ExtensionBuilder {
        self.classes.extend(classes);
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
            request_state: self.request_state,
            engine_state: self.engine_state,
            rinit: self.rinit,
            rshutdown: self.rshutdown,
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

/// Seam 2 (design §4.3): Zend's 25 handler slots, `Std` being every
/// slot's default. The slots land with the linker in week two and grow
/// `api.lock` by their names.
pub trait ObjectHandlers: Send + Sync {}
