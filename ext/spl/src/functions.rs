//! The natives of `php_spl.c` and the three `iterator_*` natives of
//! `spl_iterators.c`, in `php_spl.stub.php`'s order: the class
//! introspection of `class_implements`, `class_parents` and
//! `class_uses`, the autoload stack over the engine's
//! (`Symbols::autoload_register` and `call::autoload`), `spl_classes`,
//! the two object identities, and the walks of a `Traversable`
//! `spl_iterator_apply` makes.

use php_ext::abi::{Callable, Instance, Iterable, OrFalse, Raw, Untyped};
use php_ext::attr::php_function;
use php_ext::call::{autoload, call, call_array, lookup_class, lookup_function};
use php_ext::coerce::{callable_checked, truthy, value_name};
use php_ext::raising::{Level, Thrown, arg_type_error, arg_value_error, docref};
use php_ext::request::Request;
use php_ext::values::walk::{self};
use php_ext::values::{ClassId, Owner, PhpArray, PhpObject, PhpString, Value, ValueRef};

use crate::{State, Traversable};

/// The class `$object_or_class` names, php's `spl_find_ce_by_name` for
/// a string: the table, then the autoloaders where `$autoload` asks,
/// and the warning and `false` where neither has it.
fn subject(req: &mut Request, given: Value, autoloading: bool) -> Result<Option<ClassId>, Thrown> {
    match given.as_ref() {
        ValueRef::Object(object) => Ok(Some(object.class())),
        ValueRef::Str(name) => {
            let name = name.as_bytes().to_vec();
            let found = if autoloading {
                autoload(req, &name)?
            } else {
                lookup_class(req, &name)?
            };
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
                &[b"be of type object|string, ".as_slice(), &name, b" given"].concat(),
            ))
        }
    }
}

/// `spl_add_class_name` over a list of classes: each keyed by its name.
fn class_names(req: &mut Request, classes: &[ClassId]) -> Result<OrFalse<Owner<PhpArray>>, Thrown> {
    let mut out = PhpArray::new(req)?;
    for class in classes {
        let name = crate::class_name(req, *class);
        if out.find(&name).is_some() {
            continue;
        }
        let value = crate::string_value(req, &name)?;
        out.set_str(req, &name, value)?;
    }
    Ok(OrFalse::Value(out))
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
#[php_function]
fn class_implements(
    req: &mut Request,
    object_or_class: Untyped<Value>,
    #[php(default = true)] autoload: bool,
) -> Result<OrFalse<Owner<PhpArray>>, Thrown> {
    let Some(class) = subject(req, object_or_class.0, autoload)? else {
        return Ok(OrFalse::False);
    };
    let interfaces = req.symbols().interfaces(class);
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
#[php_function]
fn class_parents(
    req: &mut Request,
    object_or_class: Untyped<Value>,
    #[php(default = true)] autoload: bool,
) -> Result<OrFalse<Owner<PhpArray>>, Thrown> {
    let Some(class) = subject(req, object_or_class.0, autoload)? else {
        return Ok(OrFalse::False);
    };
    let parents: Vec<ClassId> = req.symbols().lineage(class).skip(1).collect();
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
#[php_function]
fn class_uses(
    req: &mut Request,
    object_or_class: Untyped<Value>,
    #[php(default = true)] autoload: bool,
) -> Result<OrFalse<Owner<PhpArray>>, Thrown> {
    let Some(class) = subject(req, object_or_class.0, autoload)? else {
        return Ok(OrFalse::False);
    };
    let names: Vec<Vec<u8>> = req
        .symbols()
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
/// void`: for each extension in turn, the lowered class name with it,
/// `\` made `/`, executed as php's default loader executes it
/// ([`php_ext::call::load_file`]) along the `include_path`, until the
/// class is declared. A name whose file no path finds is nothing,
/// raised or loaded.
///
/// ```php
/// $dir = sys_get_temp_dir() . '/witness_spl_autoload_' . getmypid();
/// @mkdir("$dir/ns", 0777, true);
/// file_put_contents("$dir/ns/thing.inc", '<?php namespace Ns; class Thing {} echo "inc\n";');
/// set_include_path($dir);
/// spl_autoload('Nowhere\\Missing');
/// spl_autoload('\\Ns\\Thing');
/// var_dump(class_exists('Nowhere\\Missing', false), class_exists('Ns\\Thing', false));
/// spl_autoload_register();
/// var_dump(spl_autoload_functions());
/// unlink("$dir/ns/thing.inc"); rmdir("$dir/ns"); rmdir($dir);
/// ```
/// ```expect
/// inc
/// bool(false)
/// bool(true)
/// array(1) {
///   [0]=>
///   string(12) "spl_autoload"
/// }
/// ```
#[php_function]
fn spl_autoload(
    req: &mut Request,
    class: PhpString,
    #[php(default = null)] file_extensions: Option<PhpString>,
) -> Result<(), Thrown> {
    let extensions = match file_extensions {
        Some(given) => given.as_bytes().to_vec(),
        None => req
            .state::<State>()
            .autoload_extensions
            .clone()
            .unwrap_or_else(|| DEFAULT_EXTENSIONS.to_vec()),
    };
    // `PHP_FUNCTION(spl_autoload)` drops one leading `\` before it
    // lowers the name (`Z/ext/spl/php_spl.c:304`), where
    // `spl_autoload_call` hands its loaders the name as given.
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
        if php_ext::call::load_file(req, &file, true)? && lookup_class(req, &lowered)?.is_some() {
            break;
        }
    }
    Ok(())
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
#[php_function]
fn spl_autoload_call(req: &mut Request, class: PhpString) -> Result<(), Thrown> {
    php_ext::call::perform_autoload(req, class)?;
    Ok(())
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
#[php_function]
fn spl_autoload_extensions(
    req: &mut Request,
    #[php(default = null)] file_extensions: Option<PhpString>,
) -> Result<Owner<PhpString>, Thrown> {
    if let Some(given) = file_extensions {
        req.state::<State>().autoload_extensions = Some(given.as_bytes().to_vec());
    }
    let now = req
        .state::<State>()
        .autoload_extensions
        .clone()
        .unwrap_or_else(|| DEFAULT_EXTENSIONS.to_vec());
    PhpString::new(req, &now)
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
#[php_function]
fn spl_autoload_functions(req: &mut Request) -> Result<Owner<PhpArray>, Thrown> {
    let loaders = req.symbols_mut().autoload_functions()?;
    let mut out = PhpArray::new(req)?;
    for loader in loaders {
        out.append(req, loader)?;
    }
    Ok(out)
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
///
/// The stack keeps what the callable resolved to and an owner of its
/// receiver, never the array it was registered as
/// (`autoload_func_info_from_fci`), so the program's `unset` frees the
/// array, which gives the receiver up and offers it to the collector,
/// and the receiver's own `unset` adds no root of its own:
///
/// ```php
/// class L { public function load($c) { } }
/// $l = new L;
/// $cb = [$l, 'load'];
/// spl_autoload_register($cb);
/// unset($cb);
/// echo gc_status()['roots'], "\n";
/// unset($l);
/// echo gc_status()['roots'], "\n";
/// ```
/// ```expect
/// 1
/// 1
/// ```
///
/// The declaration is php's, its defaults shown:
///
/// ```php
/// $f = new ReflectionFunction("spl_autoload_register");
/// foreach ($f->getParameters() as $p) echo $p, "\n";
/// echo $f->getReturnType(), " ", (new ReflectionFunction("iterator_to_array"))->getNumberOfRequiredParameters(), "\n";
/// ```
/// ```expect
/// Parameter #0 [ <optional> ?callable $callback = null ]
/// Parameter #1 [ <optional> bool $throw = true ]
/// Parameter #2 [ <optional> bool $prepend = false ]
/// bool 1
/// ```
#[php_function]
fn spl_autoload_register(
    req: &mut Request,
    #[php(default = null)] callback: Raw<Option<Callable>>,
    #[php(default = true)] throw: Raw<bool>,
    #[php(default = false)] prepend: Raw<bool>,
) -> Result<bool, Thrown> {
    let given = callback.value();
    let loader = callback.hold(req)?;
    let throw = throw.hold(req)?;
    let prepend = prepend.hold(req)?;
    if !throw {
        docref(
            req,
            Level::Notice,
            "Argument #2 ($do_throw) has been ignored, spl_autoload_register() will always throw",
        );
    }
    if loader.is_some() && names_native(req, &given, b"spl_autoload_call") {
        return Err(arg_value_error(
            req,
            1,
            "not be the spl_autoload_call() function",
        ));
    }
    if let Some(loader) = loader {
        req.symbols_mut().autoload_register(&loader, prepend)?;
        return Ok(true);
    }
    // The name is lent to the callable, which the stack takes its own of.
    let name = crate::string_value(req, b"spl_autoload")?;
    let Ok(loader) = callable_checked(req, &name) else {
        name.give_up(req);
        return Ok(false);
    };
    let registered = req.symbols_mut().autoload_register(&loader, prepend);
    name.give_up(req);
    registered?;
    Ok(true)
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
#[php_function]
fn spl_autoload_unregister(req: &mut Request, callback: Raw<Callable>) -> Result<bool, Thrown> {
    let given = callback.value();
    let loader = callback.hold(req)?;
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
        for listed in req.symbols_mut().autoload_functions()? {
            if let Ok(held) = callable_checked(req, &listed) {
                req.symbols_mut().autoload_unregister(&held);
            }
            listed.give_up(req);
        }
        return Ok(true);
    }
    Ok(req.symbols_mut().autoload_unregister(&loader))
}

/// `SPL_LIST_CLASSES`, in its order: alphabetical but for `SplMinHeap`
/// before `SplMaxHeap`.
pub(crate) const SPL_CLASSES: &[&str] = &[
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
#[php_function]
fn spl_classes(req: &mut Request) -> Result<Owner<PhpArray>, Thrown> {
    let mut out = PhpArray::new(req)?;
    for name in SPL_CLASSES {
        let value = crate::string_value(req, name.as_bytes())?;
        out.set_str(req, name.as_bytes(), value)?;
    }
    Ok(out)
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
#[php_function]
fn spl_object_hash(req: &mut Request, object: PhpObject) -> Result<Owner<PhpString>, Thrown> {
    let hash = format!("{:016x}0000000000000000", object.index());
    PhpString::new(req, hash.as_bytes())
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
#[php_function]
fn spl_object_id(object: PhpObject) -> i64 {
    i64::from(object.index())
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
#[php_function]
fn iterator_apply(
    req: &mut Request,
    iterator: Instance<Traversable>,
    callback: Callable,
    #[php(default = null)] args: Option<PhpArray>,
) -> Result<i64, Thrown> {
    let arguments = args;
    let mut count = 0i64;
    crate::apply(req, iterator.object(), |req, _| {
        count += 1;
        let answered = match arguments {
            Some(arguments) => call_array(req, &callback, arguments)?,
            None => call(req, &callback, &[])?,
        };
        let keep = truthy(req, &answered);
        answered.give_up(req);
        let keep = keep?;
        Ok(keep)
    })?;
    Ok(count)
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
#[php_function]
fn iterator_count(req: &mut Request, iterator: Iterable) -> Result<i64, Thrown> {
    let object = match iterator {
        Iterable::Array(array) => return Ok(i64::try_from(array.len()).unwrap_or(i64::MAX)),
        Iterable::Traversable(object) => object,
    };
    let mut count = 0i64;
    crate::apply(req, object, |_, _| {
        count += 1;
        Ok(count < i64::MAX)
    })?;
    Ok(count)
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
#[php_function]
fn iterator_to_array(
    req: &mut Request,
    iterator: Iterable,
    #[php(default = true)] preserve_keys: bool,
) -> Result<Owner<PhpArray>, Thrown> {
    let keys = preserve_keys;
    let object = match iterator {
        Iterable::Array(array) => {
            if keys {
                return Ok(Owner::take(req, array));
            }
            // `zend_array_to_list`, which no code of the program reaches: the
            // rows read under the borrowed request, each taken as
            // `zval_add_ref` takes it, then appended.
            let mut out = PhpArray::new(req)?;
            let values: Vec<Owner<Value>> = walk::rows(req, array)
                .map(|(_, value)| Owner::take(req, value.deref()))
                .collect();
            let mut values = values.into_iter();
            while let Some(value) = values.next() {
                if let Err(thrown) = out.append(req, value) {
                    for value in values {
                        value.give_up(req);
                    }
                    out.give_up(req);
                    return Err(thrown);
                }
            }
            return Ok(out);
        }
        Iterable::Traversable(object) => object,
    };
    let mut out = PhpArray::new(req)?;
    let walked = crate::apply(req, object, |req, iterator| {
        let current = iterator.value(req)?;
        if keys {
            let key = match iterator.key(req) {
                Ok(key) => key,
                Err(thrown) => {
                    current.give_up(req);
                    return Err(thrown);
                }
            };
            let set = crate::set_key(req, &mut out, &key, current);
            key.give_up(req);
            set?;
        } else {
            let current = crate::dereferenced(req, current);
            out.append(req, current)?;
        }
        Ok(true)
    });
    if let Err(thrown) = walked {
        out.give_up(req);
        return Err(thrown);
    }
    Ok(out)
}
