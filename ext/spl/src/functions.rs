//! The natives of `php_spl.c` and the three `iterator_*` natives of
//! `spl_iterators.c`, in `php_spl.stub.php`'s order: the class
//! introspection of `class_implements`, `class_parents` and
//! `class_uses`, the autoload stack over the engine's
//! (`Request::autoload_register` and `call::autoload`), `spl_classes`,
//! the two object identities, and the walks of a `Traversable`
//! `spl_iterator_apply` makes.

use php_ext::abi::Callable;
use php_ext::call::{autoload, call, call_array, lookup_class, lookup_function};
use php_ext::coerce::{callable_checked, truthy, value_name};
use php_ext::decl::FunctionDecl;
use php_ext::raising::{Level, Thrown, arg_type_error, arg_value_error, docref};
use php_ext::request::Request;
use php_ext::values::{ClassId, Key, PhpArray, PhpString, Value, ValueRef};

use crate::{Entry, State, stub};

/// The natives, in the stub's order.
///
/// ```php
/// var_dump(get_extension_funcs("spl"));
/// ```
/// ```expect
/// array(15) {
///   [0]=>
///   string(16) "class_implements"
///   [1]=>
///   string(13) "class_parents"
///   [2]=>
///   string(10) "class_uses"
///   [3]=>
///   string(12) "spl_autoload"
///   [4]=>
///   string(17) "spl_autoload_call"
///   [5]=>
///   string(23) "spl_autoload_extensions"
///   [6]=>
///   string(22) "spl_autoload_functions"
///   [7]=>
///   string(21) "spl_autoload_register"
///   [8]=>
///   string(23) "spl_autoload_unregister"
///   [9]=>
///   string(11) "spl_classes"
///   [10]=>
///   string(15) "spl_object_hash"
///   [11]=>
///   string(13) "spl_object_id"
///   [12]=>
///   string(14) "iterator_apply"
///   [13]=>
///   string(14) "iterator_count"
///   [14]=>
///   string(17) "iterator_to_array"
/// }
/// ```
pub(crate) fn natives() -> Vec<(FunctionDecl, Entry)> {
    [
        (
            "class_implements($object_or_class, bool $autoload = true): array|false",
            class_implements as Entry,
        ),
        (
            "class_parents($object_or_class, bool $autoload = true): array|false",
            class_parents,
        ),
        (
            "class_uses($object_or_class, bool $autoload = true): array|false",
            class_uses,
        ),
        (
            "spl_autoload(string $class, ?string $file_extensions = null): void",
            spl_autoload,
        ),
        ("spl_autoload_call(string $class): void", spl_autoload_call),
        (
            "spl_autoload_extensions(?string $file_extensions = null): string",
            spl_autoload_extensions,
        ),
        ("spl_autoload_functions(): array", spl_autoload_functions),
        (
            "spl_autoload_register(?callable $callback = null, bool $throw = true, bool $prepend = false): bool",
            spl_autoload_register,
        ),
        (
            "spl_autoload_unregister(callable $callback): bool",
            spl_autoload_unregister,
        ),
        ("spl_classes(): array", spl_classes),
        ("spl_object_hash(object $object): string", spl_object_hash),
        ("spl_object_id(object $object): int", spl_object_id),
        (
            "iterator_apply(Traversable $iterator, callable $callback, ?array $args = null): int",
            iterator_apply,
        ),
        ("iterator_count(iterable $iterator): int", iterator_count),
        (
            "iterator_to_array(iterable $iterator, bool $preserve_keys = true): array",
            iterator_to_array,
        ),
    ]
    .into_iter()
    .map(|(line, entry)| (stub::function(line), entry))
    .collect()
}

/// The class `$object_or_class` names, php's `spl_find_ce_by_name` for
/// a string: the table, then the autoloaders where `$autoload` asks,
/// and the warning and `false` where neither has it.
fn subject(req: &mut Request) -> Result<Option<ClassId>, Thrown> {
    crate::arity(req, 1, 2)?;
    let given = crate::value(req, 1);
    let autoloading = crate::bool_or(req, 2, true)?;
    match given.as_ref() {
        ValueRef::Object(object) => Ok(Some(object.class())),
        ValueRef::Str(name) => {
            let name = name.as_bytes().to_vec();
            let mut found = lookup_class(req, &name)?;
            if found.is_none() && autoloading {
                found = autoload(req, &name)?;
            }
            if found.is_none() {
                let tail = if autoloading {
                    " and could not be loaded"
                } else {
                    ""
                };
                docref(
                    req,
                    Level::Warning,
                    &format!(
                        "Class {} does not exist{tail}",
                        String::from_utf8_lossy(&name)
                    ),
                );
            }
            Ok(found)
        }
        _ => {
            let name = value_name(req, &given);
            Err(arg_type_error(
                req,
                1,
                &format!("be of type object|string, {name} given"),
            ))
        }
    }
}

/// `spl_add_class_name` over a list of classes: each keyed by its name.
fn class_names(req: &mut Request, classes: &[ClassId]) -> Result<Value, Thrown> {
    let mut out = PhpArray::new(req)?;
    for class in classes {
        let name = crate::class_name(req, *class);
        let key = PhpString::new(req, &name)?;
        if out.contains(&Key::from_string(key)) {
            continue;
        }
        let value = crate::string_value(req, &name)?;
        out.set(req, Key::from_string(key), value)?;
    }
    Ok(Value::array(out))
}

/// `class_implements($object_or_class, bool $autoload = true):
/// array|false`: every interface of the class, in `ce->interfaces`'
/// order.
///
/// ```php
/// class Box implements IteratorAggregate, Countable {
///     function getIterator(): Iterator { return new EmptyIterator; }
///     function count(): int { return 0; }
/// }
/// var_dump(class_implements(new Box), class_implements('Missing', false));
/// ```
/// ```expect
///
/// Warning: class_implements(): Class Missing does not exist in %s on line 6
/// array(3) {
///   ["IteratorAggregate"]=>
///   string(17) "IteratorAggregate"
///   ["Countable"]=>
///   string(9) "Countable"
///   ["Traversable"]=>
///   string(11) "Traversable"
/// }
/// bool(false)
/// ```
fn class_implements(req: &mut Request) -> Result<Value, Thrown> {
    let Some(class) = subject(req)? else {
        return Ok(Value::bool(false));
    };
    let interfaces = crate::interfaces_of(req, class);
    class_names(req, &interfaces)
}

/// `class_parents($object_or_class, bool $autoload = true):
/// array|false`: the parents, nearest first.
///
/// ```php
/// var_dump(class_parents('OutOfBoundsException'));
/// class_parents(42);
/// ```
/// ```expect exit=255
/// array(2) {
///   ["RuntimeException"]=>
///   string(16) "RuntimeException"
///   ["Exception"]=>
///   string(9) "Exception"
/// }
///
/// Fatal error: Uncaught TypeError: class_parents(): Argument #1 ($object_or_class) must be of type object|string, int given in %s:3
/// Stack trace:
/// #0 %s(3): class_parents(42)
/// #1 {main}
///   thrown in %s on line 3
/// ```
fn class_parents(req: &mut Request) -> Result<Value, Thrown> {
    let Some(class) = subject(req)? else {
        return Ok(Value::bool(false));
    };
    let mut parents = Vec::new();
    let mut at = crate::parent_of(req, class);
    while let Some(parent) = at {
        if parents.contains(&parent) {
            break;
        }
        parents.push(parent);
        at = crate::parent_of(req, parent);
    }
    class_names(req, &parents)
}

/// `class_uses($object_or_class, bool $autoload = true): array|false`:
/// the traits the class itself uses, `ce->trait_names` in order.
///
/// ```php
/// var_dump(class_uses('stdClass'), class_uses(new Exception), class_uses('Nowhere'));
/// ```
/// ```expect
///
/// Warning: class_uses(): Class Nowhere does not exist and could not be loaded in %s on line 2
/// array(0) {
/// }
/// array(0) {
/// }
/// bool(false)
/// ```
///
/// A trait another trait uses is that trait's to list, and a class
/// lists none of its parent's:
///
/// ```php
/// trait T {} trait U { use T; }
/// class C { use U, T; } class D extends C {}
/// var_dump(class_uses('C'), class_uses(new D), class_uses('U'));
/// ```
/// ```expect
/// array(2) {
///   ["U"]=>
///   string(1) "U"
///   ["T"]=>
///   string(1) "T"
/// }
/// array(0) {
/// }
/// array(1) {
///   ["T"]=>
///   string(1) "T"
/// }
/// ```
fn class_uses(req: &mut Request) -> Result<Value, Thrown> {
    let Some(class) = subject(req)? else {
        return Ok(Value::bool(false));
    };
    let names: Vec<Vec<u8>> = req
        .class_decl(class)
        .map(|decl| decl.traits.iter().map(|t| t.0.to_vec()).collect())
        .unwrap_or_default();
    let mut traits = Vec::new();
    for name in names {
        if let Some(found) = lookup_class(req, &name)? {
            traits.push(found);
        }
    }
    class_names(req, &traits)
}

/// `SPL_DEFAULT_FILE_EXTENSIONS`.
const DEFAULT_EXTENSIONS: &[u8] = b".inc,.php";

/// `spl_autoload(string $class, ?string $file_extensions = null):
/// void`: the lowered class name with each extension in turn, `\` made
/// `/`, looked for along the `include_path`. A class whose file no path
/// finds is nothing, raised or loaded; a file found is included by the
/// engine's `require`, which the face does not offer an extension yet
/// (#708), so that arm is php's `Error`.
///
/// ```php
/// spl_autoload('Nowhere\\Missing');
/// var_dump(class_exists('Nowhere\\Missing', false));
/// ```
/// ```expect
/// bool(false)
/// ```
fn spl_autoload(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 1, 2)?;
    let class = crate::string(req, 1)?;
    let given = crate::nullable_string(req, 2)?;
    let extensions = match given {
        Some(given) => given.as_bytes().to_vec(),
        None => req
            .state::<State>()
            .autoload_extensions
            .clone()
            .unwrap_or_else(|| DEFAULT_EXTENSIONS.to_vec()),
    };
    let bare = class.as_bytes();
    let lowered = bare
        .strip_prefix(b"\\")
        .unwrap_or(bare)
        .to_ascii_lowercase();
    for extension in extensions.split(|b| *b == b',') {
        let mut file: Vec<u8> = lowered
            .iter()
            .map(|b| if *b == b'\\' { b'/' } else { *b })
            .collect();
        file.extend_from_slice(extension);
        if req.realpath(&file).is_some() {
            return Err(php_ext::raising::throw(
                req,
                "Error",
                "spl_autoload(): including the class's file is not yet served by this engine",
            ));
        }
    }
    Ok(Value::null())
}

/// `spl_autoload_call(string $class): void`: every autoloader served
/// for the name until one declares it.
///
/// ```php
/// spl_autoload_register(function ($c) { echo "load $c\n"; });
/// spl_autoload_call('Anything');
/// ```
/// ```expect
/// load Anything
/// ```
fn spl_autoload_call(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 1, 1)?;
    let class = crate::string(req, 1)?;
    autoload(req, class.as_bytes())?;
    Ok(Value::null())
}

/// `spl_autoload_extensions(?string $file_extensions = null): string`:
/// the extensions `spl_autoload` tries, set when given.
///
/// ```php
/// var_dump(spl_autoload_extensions(), spl_autoload_extensions('.class.php'), spl_autoload_extensions());
/// ```
/// ```expect
/// string(9) ".inc,.php"
/// string(10) ".class.php"
/// string(10) ".class.php"
/// ```
fn spl_autoload_extensions(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 1)?;
    if let Some(given) = crate::nullable_string(req, 1)? {
        req.state::<State>().autoload_extensions = Some(given.as_bytes().to_vec());
    }
    let now = req
        .state::<State>()
        .autoload_extensions
        .clone()
        .unwrap_or_else(|| DEFAULT_EXTENSIONS.to_vec());
    crate::string_value(req, &now)
}

/// `spl_autoload_functions(): array`: the stack, in the order it is
/// served, a method given as `'Class::method'` handed back as the pair
/// php's `autoload_func_info` keeps, the class named as declared.
///
/// ```php
/// function first($c) {}
/// class Loader { static function load($c) {} function __invoke($c) {} }
/// spl_autoload_register('first');
/// spl_autoload_register('loader::load');
/// spl_autoload_register($f = fn ($c) => null, true, true);
/// var_dump(count(spl_autoload_functions()), spl_autoload_functions()[1], spl_autoload_functions()[2]);
/// ```
/// ```expect
/// int(3)
/// string(5) "first"
/// array(2) {
///   [0]=>
///   string(6) "Loader"
///   [1]=>
///   string(4) "load"
/// }
/// ```
fn spl_autoload_functions(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let loaders = req.autoload_functions();
    let mut out = PhpArray::new(req)?;
    for loader in loaders {
        let loader = match static_pair(req, &loader)? {
            Some(pair) => pair,
            None => crate::copied(req, loader),
        };
        out.append(req, loader)?;
    }
    Ok(Value::array(out))
}

/// A loader written `'Class::method'` as the pair `[Class, method]`,
/// the class by its declared name and the method by the name its class
/// declares it with; `None` for any other loader.
fn static_pair(req: &mut Request, loader: &Value) -> Result<Option<Value>, Thrown> {
    let held = loader.deref();
    let ValueRef::Str(text) = held.as_ref() else {
        return Ok(None);
    };
    let text = text.as_bytes().to_vec();
    let Some(at) = text.windows(2).position(|w| w == b"::") else {
        return Ok(None);
    };
    let (class, method) = (&text[..at], &text[at + 2..]);
    let class = class.strip_prefix(b"\\").unwrap_or(class);
    let Some(id) = lookup_class(req, class).ok().flatten() else {
        return Ok(None);
    };
    let declared = crate::class_name(req, id);
    let method = req
        .class_decl(id)
        .and_then(|decl| {
            decl.methods
                .iter()
                .find(|m| m.function.name.0.eq_ignore_ascii_case(method))
                .map(|m| m.function.name.0.to_vec())
        })
        .unwrap_or_else(|| method.to_vec());
    let mut pair = PhpArray::new(req)?;
    let class = crate::string_value(req, &declared)?;
    pair.append(req, class)?;
    let method = crate::string_value(req, &method)?;
    pair.append(req, method)?;
    Ok(Some(Value::array(pair)))
}

/// The callable at `position`, php's `Z_PARAM_FUNC` and its refusal,
/// `coerce::callable_argument`'s over the declared `callable`.
fn callable(req: &mut Request, position: u32) -> Result<Callable, Thrown> {
    match php_ext::coerce::callable_argument(req, position)? {
        Some(callable) => Ok(callable),
        None => Err(php_ext::raising::fatal(
            req,
            "InternalError: a callable parameter read no callable",
        )),
    }
}

/// `spl_autoload_register(?callable $callback = null, bool $throw =
/// true, bool $prepend = false): bool`: the callable onto the stack,
/// at its head under `$prepend`, `spl_autoload` where none is given,
/// and nothing where it is there already.
///
/// ```php
/// class Made { const HOW = "by the loader"; }
/// spl_autoload_register(function ($class) {
///     echo "asked for $class\n";
///     if ($class === 'Later') { class_alias('Made', $class); }
/// });
/// echo Later::HOW, "\n";
/// var_dump(class_exists('Unknown'), new Later instanceof Made);
/// spl_autoload_register(null, false);
/// ```
/// ```expect
/// asked for Later
/// by the loader
/// asked for Unknown
/// bool(false)
/// bool(true)
///
/// Notice: spl_autoload_register(): Argument #2 ($do_throw) has been ignored, spl_autoload_register() will always throw in %s on line 9
/// ```
fn spl_autoload_register(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 3)?;
    let given = crate::given(req, 1).map(|v| v.deref());
    let loader = match given {
        Some(v) if !matches!(v.as_ref(), ValueRef::Null) => Some(callable(req, 1)?),
        _ => None,
    };
    let throw = crate::bool_or(req, 2, true)?;
    let prepend = crate::bool_or(req, 3, false)?;
    if !throw {
        docref(
            req,
            Level::Notice,
            "Argument #2 ($do_throw) has been ignored, spl_autoload_register() will always throw",
        );
    }
    if loader.is_some() && given.is_some_and(|g| names_native(req, &g, b"spl_autoload_call")) {
        return Err(arg_value_error(
            req,
            1,
            "not be the spl_autoload_call() function",
        ));
    }
    let loader = if let Some(loader) = loader {
        loader
    } else {
        let name = crate::string_value(req, b"spl_autoload")?;
        let Ok(loader) = callable_checked(req, &name) else {
            return Ok(Value::bool(false));
        };
        loader
    };
    req.autoload_register(loader, prepend)?;
    Ok(Value::bool(true))
}

/// Whether `given` names the native `name`, as a string callable does.
fn names_native(req: &Request, given: &Value, name: &[u8]) -> bool {
    given.as_str_bytes().is_some_and(|written| {
        written
            .strip_prefix(b"\\")
            .unwrap_or(written)
            .eq_ignore_ascii_case(name)
    }) && lookup_function(req, name).is_some()
}

/// `spl_autoload_unregister(callable $callback): bool`: the callable
/// off the stack, `false` where it was not on it; `spl_autoload_call`
/// itself empties the stack, with php's deprecation.
///
/// ```php
/// $loader = function ($c) { echo "never\n"; };
/// spl_autoload_register($loader);
/// var_dump(spl_autoload_unregister($loader), spl_autoload_unregister($loader), spl_autoload_functions());
/// spl_autoload_register($loader);
/// var_dump(spl_autoload_unregister('spl_autoload_call'), spl_autoload_functions());
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// array(0) {
/// }
///
/// Deprecated: spl_autoload_unregister(): Using spl_autoload_call() as a callback for spl_autoload_unregister() is deprecated, to remove all registered autoloaders, call spl_autoload_unregister() for all values returned from spl_autoload_functions() in %s on line 6
/// bool(true)
/// array(0) {
/// }
/// ```
fn spl_autoload_unregister(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 1, 1)?;
    let loader = callable(req, 1)?;
    let given = crate::value(req, 1);
    let named = match given.deref().as_ref() {
        ValueRef::Str(s) => {
            let name = s.as_bytes();
            name.strip_prefix(b"\\")
                .unwrap_or(name)
                .eq_ignore_ascii_case(b"spl_autoload_call")
        }
        _ => false,
    };
    if named {
        docref(
            req,
            Level::Deprecated,
            "Using spl_autoload_call() as a callback for spl_autoload_unregister() is deprecated, to remove all registered autoloaders, call spl_autoload_unregister() for all values returned from spl_autoload_functions()",
        );
        for held in req.autoload_functions() {
            if let Ok(held) = callable_checked(req, &held) {
                req.autoload_unregister(&held);
            }
        }
        return Ok(Value::bool(true));
    }
    Ok(Value::bool(req.autoload_unregister(&loader)))
}

/// `SPL_LIST_CLASSES`, in its alphabetical order.
const SPL_CLASSES: &[&str] = &[
    "AppendIterator",
    "ArrayIterator",
    "ArrayObject",
    "BadFunctionCallException",
    "BadMethodCallException",
    "CachingIterator",
    "CallbackFilterIterator",
    "DirectoryIterator",
    "DomainException",
    "EmptyIterator",
    "FilesystemIterator",
    "FilterIterator",
    "GlobIterator",
    "InfiniteIterator",
    "InvalidArgumentException",
    "IteratorIterator",
    "LengthException",
    "LimitIterator",
    "LogicException",
    "MultipleIterator",
    "NoRewindIterator",
    "OuterIterator",
    "OutOfBoundsException",
    "OutOfRangeException",
    "OverflowException",
    "ParentIterator",
    "RangeException",
    "RecursiveArrayIterator",
    "RecursiveCachingIterator",
    "RecursiveCallbackFilterIterator",
    "RecursiveDirectoryIterator",
    "RecursiveFilterIterator",
    "RecursiveIterator",
    "RecursiveIteratorIterator",
    "RecursiveRegexIterator",
    "RecursiveTreeIterator",
    "RegexIterator",
    "RuntimeException",
    "SeekableIterator",
    "SplDoublyLinkedList",
    "SplFileInfo",
    "SplFileObject",
    "SplFixedArray",
    "SplHeap",
    "SplMinHeap",
    "SplMaxHeap",
    "SplObjectStorage",
    "SplObserver",
    "SplPriorityQueue",
    "SplQueue",
    "SplStack",
    "SplSubject",
    "SplTempFileObject",
    "UnderflowException",
    "UnexpectedValueException",
];

/// `spl_classes(): array`: SPL's classes and interfaces, each keyed by
/// its name.
///
/// ```php
/// $classes = spl_classes();
/// foreach ($classes as $first => $name) break;
/// var_dump(count($classes), $classes['ArrayObject'], $first);
/// ```
/// ```expect
/// int(55)
/// string(11) "ArrayObject"
/// string(14) "AppendIterator"
/// ```
fn spl_classes(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 0, 0)?;
    let mut out = PhpArray::new(req)?;
    for name in SPL_CLASSES {
        let key = PhpString::new(req, name.as_bytes())?;
        let value = crate::string_value(req, name.as_bytes())?;
        out.set(req, Key::from_string(key), value)?;
    }
    Ok(Value::array(out))
}

/// `spl_object_hash(object $object): string`, php's
/// `%016zx0000000000000000` over the object's id.
///
/// ```php
/// $a = new stdClass; $b = new stdClass;
/// var_dump(spl_object_hash($a), spl_object_hash($b) === spl_object_hash($b), spl_object_hash($a) === spl_object_hash($b));
/// ```
/// ```expect
/// string(32) "00000000000000010000000000000000"
/// bool(true)
/// bool(false)
/// ```
fn spl_object_hash(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 1, 1)?;
    let object = crate::object(req, 1, None)?;
    let hash = format!("{:016x}0000000000000000", object.index());
    crate::string_value(req, hash.as_bytes())
}

/// `spl_object_id(object $object): int`, the index the object store gave the object.
///
/// ```php
/// $a = new stdClass; $b = new stdClass;
/// var_dump(spl_object_id($a), spl_object_id($b));
/// spl_object_id('x');
/// ```
/// ```expect exit=255
/// int(1)
/// int(2)
///
/// Fatal error: Uncaught TypeError: spl_object_id(): Argument #1 ($object) must be of type object, string given in %s:4
/// Stack trace:
/// #0 %s(4): spl_object_id('x')
/// #1 {main}
///   thrown in %s on line 4
/// ```
fn spl_object_id(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 1, 1)?;
    let object = crate::object(req, 1, None)?;
    Ok(Value::int(i64::from(object.index())))
}

/// `Z_PARAM_ITERABLE`: an array or a `Traversable`, `coerce_param`'s.
fn iterable(req: &mut Request, position: u32) -> Result<Value, Thrown> {
    let given = crate::value(req, position);
    php_ext::coerce::coerce_param(req, &given, &php_ext::decl::Type::Iterable, position)
}

/// `iterator_apply(Traversable $iterator, callable $callback, ?array
/// $args = null): int`: the callback served once per element with
/// `$args`, until it answers something false; the count of calls.
///
/// ```php
/// class Letters implements Iterator {
///     private $i = 0;
///     function current(): mixed { return chr(97 + $this->i); }
///     function key(): mixed { return $this->i; }
///     function next(): void { $this->i = $this->i + 1; }
///     function rewind(): void { $this->i = 0; }
///     function valid(): bool { return $this->i < 3; }
/// }
/// $it = new Letters;
/// var_dump(iterator_apply($it, function ($it) { echo $it->current(), "\n"; return $it->key() < 1; }, [$it]));
/// ```
/// ```expect
/// a
/// b
/// int(2)
/// ```
fn iterator_apply(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 2, 3)?;
    let iterator = crate::object(req, 1, Some("Traversable"))?;
    let callback = callable(req, 2)?;
    let arguments = match crate::given(req, 3).map(|v| v.deref()) {
        Some(v) if !matches!(v.as_ref(), ValueRef::Null) => Some(crate::array(req, 3)?),
        _ => None,
    };
    let mut count = 0i64;
    crate::apply(req, iterator, |req, _| {
        count += 1;
        let answered = match arguments {
            Some(arguments) => call_array(req, &callback, arguments)?,
            None => call(req, &callback, &[])?,
        };
        let keep = truthy(req, &answered);
        req.release(answered);
        Ok(keep)
    })?;
    Ok(Value::int(count))
}

/// `iterator_count(iterable $iterator): int`: the elements an array
/// holds or a `Traversable` walks to, `valid()` asked and never
/// `current()`.
///
/// ```php
/// class Three implements Iterator {
///     private $i = 0;
///     function current(): mixed { echo "current "; return $this->i; }
///     function key(): mixed { return $this->i; }
///     function next(): void { $this->i = $this->i + 1; }
///     function rewind(): void { $this->i = 0; }
///     function valid(): bool { return $this->i < 3; }
/// }
/// var_dump(iterator_count(new Three), iterator_count(['x' => 1]));
/// ```
/// ```expect
/// int(3)
/// int(1)
/// ```
fn iterator_count(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 1, 1)?;
    let given = iterable(req, 1)?;
    if let Some(array) = given.as_array() {
        return Ok(Value::int(i64::try_from(array.len()).unwrap_or(i64::MAX)));
    }
    let mut count = 0i64;
    if let Some(object) = given.as_object().copied() {
        crate::apply(req, object, |_, _| {
            count += 1;
            Ok(count < i64::MAX)
        })?;
    }
    Ok(Value::int(count))
}

/// `iterator_to_array(iterable $iterator, bool $preserve_keys = true):
/// array`: `current()` then `key()` per element, a later key
/// overwriting an earlier one; the values alone in order without the
/// keys.
///
/// ```php
/// class Pairs implements Iterator {
///     private $i = 0;
///     function current(): mixed { echo "current "; return $this->i * 2; }
///     function key(): mixed { echo "key "; return $this->i % 2 ? 'odd' : 'even'; }
///     function next(): void { $this->i = $this->i + 1; }
///     function rewind(): void { echo "rewind "; $this->i = 0; }
///     function valid(): bool { echo "valid "; return $this->i < 3; }
/// }
/// var_dump(iterator_to_array(new Pairs));
/// var_dump(iterator_to_array(new Pairs, false), iterator_to_array(['a' => 1], false));
/// ```
/// ```expect
/// rewind valid current key valid current key valid current key valid array(2) {
///   ["even"]=>
///   int(4)
///   ["odd"]=>
///   int(2)
/// }
/// rewind valid current valid current valid current valid array(3) {
///   [0]=>
///   int(0)
///   [1]=>
///   int(2)
///   [2]=>
///   int(4)
/// }
/// array(1) {
///   [0]=>
///   int(1)
/// }
/// ```
fn iterator_to_array(req: &mut Request) -> Result<Value, Thrown> {
    crate::arity(req, 1, 2)?;
    let given = iterable(req, 1)?;
    let keys = crate::bool_or(req, 2, true)?;
    let mut out = PhpArray::new(req)?;
    if let Some(array) = given.as_array() {
        if keys {
            return Ok(crate::copied(req, given));
        }
        for (_, value) in array.snapshot() {
            let value = crate::copied(req, value);
            out.append(req, value)?;
        }
        return Ok(Value::array(out));
    }
    let Some(object) = given.as_object().copied() else {
        return Ok(Value::array(out));
    };
    let walked = crate::apply(req, object, |req, iterator| {
        let current = crate::method(req, iterator, b"current")?;
        if keys {
            let key = match crate::method(req, iterator, b"key") {
                Ok(key) => key,
                Err(thrown) => {
                    req.release(current);
                    return Err(thrown);
                }
            };
            let set = crate::set_key(req, &mut out, &key, current);
            req.release(key);
            set?;
        } else {
            let current = crate::owned(req, current);
            out.append(req, current)?;
        }
        Ok(true)
    });
    if let Err(thrown) = walked {
        req.release(Value::array(out));
        return Err(thrown);
    }
    Ok(Value::array(out))
}
