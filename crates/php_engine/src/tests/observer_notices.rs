//! Seam 9's notices and the handlers added while a request is served,
//! as `TestExt` records them (design §4.3 row 9, revision 79): the error
//! observer told of every diagnostic before the user's handler and
//! `zend_error_cb`, declared functions and linked classes told as php
//! tells them with and without opcache and again when a unit is served
//! from the cache, and handlers added to and removed from one function
//! while a request is served, which the next request does not see. The
//! sequences are php's, as `zend_test`'s observer prints them in
//! `Z/ext/zend_test/tests/observer_declarations_01.phpt` and
//! `observer_error_*.phpt`, and as `Z/Zend/zend_observer.c` lays out
//! the slot.

use php_unit::decl::Bare;
use std::path::{Path, PathBuf};

use php_macros::php_function;

use super::test_ext::{Event, Events, Phase, Recording, TestExt, observed_name, record};
use crate::api::abi::{
    Admission, ExtensionBuilder, Lookup, Observed, ObserverHandlers, Removed, Validation,
};
use crate::api::raising::{Level, Thrown, fatal_while_pending, throw};
use crate::api::request::Request;
use crate::engine::Engine;
use crate::test::{Capture, Served};

use Event::Ext;
use Phase::{Begin, Declared, End, Error, ErrorObserved, Linked, ObserverInit};

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "php_engine_observer_notices_{name}_{}",
        std::process::id()
    ));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).ok();
    dir
}

fn serve_main(engine: &Engine, dir: &Path, source: &str) -> Served {
    let main = dir.join("main.php");
    std::fs::write(&main, source).ok();
    engine.serve_capture_in(
        dir,
        main.to_string_lossy().as_bytes(),
        source,
        Capture::default(),
    )
}

/// The events of `kinds` alone, in the order recorded.
fn only(events: &Events, kinds: &[Phase]) -> Vec<Event> {
    events.only(kinds).0
}

fn declared(name: &str) -> Event {
    Ext(0, Declared(name.to_owned()))
}

fn linked(name: &str) -> Event {
    Ext(0, Linked(name.to_owned()))
}

fn init(name: &str) -> Event {
    Ext(0, ObserverInit(name.to_owned()))
}

fn begin(name: &str) -> Event {
    Ext(0, Begin(name.to_owned()))
}

/// The notices and the frames' order: what `observer_declarations_01.phpt`
/// prints, less its `time()`, whose native call is #1810's.
fn declaring() -> [Phase; 5] {
    [
        Declared(String::new()),
        Linked(String::new()),
        ObserverInit(String::new()),
        Begin(String::new()),
        End(String::new(), None),
    ]
}

/// `observer_declarations_01.phpt`: a file's top-level function and the
/// classes bound early are told before its frame is begun, in source
/// order as php's compiler tells them, and a function and classes
/// written inside a block are told as their `DECLARE_FUNCTION` and
/// `DECLARE_CLASS` are served.
#[test]
fn declarations_are_told_as_the_file_loads_and_as_their_oplines_run() {
    let rec = Recording::start();
    let engine = Engine::cli_builder()
        .extensions(&[TestExt::<0>::observing()])
        .build();
    let dir = scratch("declarations");
    rec.mark("built");
    let served = serve_main(
        &engine,
        &dir,
        "<?php\nfunction foo()\n{\n    echo \"foo\\n\";\n}\n\nclass A {\n}\n\nclass B extends A {\n}\n\n\
         $go = true;\nif ($go) {\n    function nested()\n    {\n    }\n\n    class C\n    {\n    }\n\n\
         \x20   class D extends A\n    {\n    }\n}\n\nfoo();\n",
    );
    rec.mark("served");
    assert_eq!(String::from_utf8_lossy(&served.stdout), "foo\n");
    let file = "file 'main.php'";
    assert_eq!(
        only(&rec.events().between("built", "served"), &declaring()),
        [
            declared("foo"),
            linked("a"),
            linked("b"),
            init(file),
            begin(file),
            declared("nested"),
            linked("c"),
            linked("d"),
            init("foo"),
            begin("foo"),
            Ext(0, End("foo".to_owned(), Some("null".to_owned()))),
            Ext(0, End(file.to_owned(), Some("1".to_owned()))),
        ]
    );
}

/// An extension that holds every file the requests include, as opcache
/// holds one under `opcache.validate_timestamps=0`, beside the observing
/// one.
fn holding() -> crate::api::abi::Extension {
    ExtensionBuilder::default()
        .admission(|_, _| Admission::Held {
            validation: Validation::Never,
            lookup: Lookup::Resolved,
            protection: 0,
            file_cache: None,
        })
        .build("holding")
}

/// What `inc.php` declares, two requests running: a class bound early
/// below one that extends it, then a function, so the orders differ.
const INCLUDED: &str = "<?php\nclass B extends A {}\nclass A {}\nfunction f() {}\n";

/// The notices of including [`INCLUDED`], on two requests of one engine.
fn included_twice(engine: &Engine, dir: &Path, rec: &Recording) -> [Vec<Event>; 2] {
    std::fs::write(dir.join("inc.php"), INCLUDED).ok();
    let main = "<?php\ninclude __DIR__ . '/inc.php';\n";
    rec.mark("first");
    serve_main(engine, dir, main);
    rec.mark("second");
    serve_main(engine, dir, main);
    rec.mark("end");
    let kinds = declaring();
    let events = rec.events();
    let inc = |events: Events| -> Vec<Event> {
        only(&events, &kinds)
            .into_iter()
            .filter(|e| !matches!(e, Ext(_, ObserverInit(n) | Begin(n) | End(n, _)) if n.contains("main.php")))
            .collect()
    };
    [
        inc(events.between("first", "second")),
        inc(events.between("second", "end")),
    ]
}

/// Without opcache, php tells each declaration as its compile finishes
/// it, in source order, and a class whose parent is declared below it
/// as its `DECLARE_CLASS` links it while the file runs; a file compiled
/// again, or served again from the engine's units, is told again.
#[test]
fn without_opcache_declarations_are_told_in_source_order_on_every_load() {
    let rec = Recording::start();
    let engine = Engine::cli_builder()
        .extensions(&[TestExt::<0>::observing()])
        .build();
    let dir = scratch("compiled");
    let inc = "file 'inc.php'";
    let expected = [
        linked("a"),
        declared("f"),
        init(inc),
        begin(inc),
        linked("b"),
        Ext(0, End(inc.to_owned(), Some("1".to_owned()))),
    ];
    let [first, second] = included_twice(&engine, &dir, &rec);
    assert_eq!(first, expected);
    assert_eq!(second, expected);
}

/// With opcache, php's compile tells nothing
/// (`ZEND_COMPILE_IGNORE_OBSERVER`) and `zend_accel_load_script` tells
/// what it copies on the miss that compiled the file and on every hit
/// after: the functions, then the classes bound early, then the delayed
/// early bindings, before the file's frame is begun.
#[test]
fn a_cached_unit_replays_declared_and_linked_in_opcache_order() {
    let rec = Recording::start();
    let engine = Engine::cli_builder()
        .extensions(&[TestExt::<0>::observing(), holding()])
        .build();
    let dir = scratch("held");
    let inc = "file 'inc.php'";
    let expected = [
        declared("f"),
        linked("a"),
        linked("b"),
        init(inc),
        begin(inc),
        Ext(0, End(inc.to_owned(), Some("1".to_owned()))),
    ];
    let [first, second] = included_twice(&engine, &dir, &rec);
    assert_eq!(first, expected, "the miss that compiled and held the file");
    assert_eq!(second, expected, "the hit that served it from the cache");
}

/// `source` served as `name` in `dir`.
fn serve_file(engine: &Engine, dir: &Path, name: &str, source: &str) -> Served {
    let path = dir.join(name);
    std::fs::write(&path, source).ok();
    engine.serve_capture_in(
        dir,
        path.to_string_lossy().as_bytes(),
        source,
        Capture::default(),
    )
}

/// A load that ends in a redeclaration tells what php declared before
/// it (#2012). Without opcache, php's compiler tells each declaration
/// as it finishes it and meets the redeclared function where it is
/// written, so the function and the class above it are told and those
/// below are not, as the oracle's zend-test variant prints with
/// `zend_test.observer.observe_declaring`. Where opcache serves a held
/// file, `zend_accel_load_script` copies and tells every function, then
/// every class, and meets the class the request declared already there
/// (`Z/ext/opcache/zend_accelerator_util_funcs.c:186-230`), so the
/// functions and the class before it are told.
#[test]
fn a_load_that_ends_in_a_redeclaration_tells_what_was_declared_before_it() {
    let rec = Recording::start();
    let kinds = [Declared(String::new()), Linked(String::new())];
    let dir = scratch("redeclared");

    let engine = Engine::cli_builder()
        .extensions(&[TestExt::<0>::observing()])
        .build();
    std::fs::write(
        dir.join("functions.php"),
        "<?php\nfunction f1() {}\nclass B {}\nfunction g() {}\nfunction f2() {}\nclass C {}\n",
    )
    .ok();
    rec.mark("function");
    serve_file(
        &engine,
        &dir,
        "function_main.php",
        "<?php\nfunction g() {}\ninclude __DIR__ . '/functions.php';\n",
    );
    rec.mark("function served");
    assert_eq!(
        only(&rec.events().between("function", "function served"), &kinds),
        [declared("g"), declared("f1"), linked("b")]
    );

    let engine = Engine::cli_builder()
        .extensions(&[TestExt::<0>::observing(), holding()])
        .build();
    std::fs::write(
        dir.join("classes.php"),
        "<?php\nclass B {}\nfunction f1() {}\nclass A {}\nfunction f2() {}\nclass C {}\n",
    )
    .ok();
    serve_file(
        &engine,
        &dir,
        "held_first.php",
        "<?php\ninclude __DIR__ . '/classes.php';\n",
    );
    rec.mark("class");
    serve_file(
        &engine,
        &dir,
        "held_second.php",
        "<?php\nclass A {}\ninclude __DIR__ . '/classes.php';\n",
    );
    rec.mark("class served");
    assert_eq!(
        only(&rec.events().between("class", "class served"), &kinds),
        [linked("a"), declared("f1"), declared("f2"), linked("b")]
    );
}

/// A function and a class written on one line are told in the order
/// they were written, as php's compiler finishes them, which no line
/// tells apart (#2012).
#[test]
fn declarations_on_one_line_are_told_in_the_order_written() {
    let rec = Recording::start();
    let engine = Engine::cli_builder()
        .extensions(&[TestExt::<0>::observing()])
        .build();
    let dir = scratch("one_line");
    let kinds = [Declared(String::new()), Linked(String::new())];
    rec.mark("class first");
    serve_file(
        &engine,
        &dir,
        "a.php",
        "<?php\nclass A {} function f() {}\n",
    );
    rec.mark("function first");
    serve_file(
        &engine,
        &dir,
        "b.php",
        "<?php\nfunction f() {} class A {}\n",
    );
    rec.mark("end");
    let events = rec.events();
    assert_eq!(
        only(&events.between("class first", "function first"), &kinds),
        [linked("a"), declared("f")]
    );
    assert_eq!(
        only(&events.between("function first", "end"), &kinds),
        [declared("f"), linked("a")]
    );
}

/// `fatal_while_thrown()`: an `Exception` left pending, then a fatal
/// raised over it, as a native that meets php's `E_ERROR` while
/// `EG(exception)` holds one raises it.
#[php_function]
fn fatal_while_thrown(req: &mut Request) -> Result<(), Thrown> {
    let thrown = throw(req, "Exception", "pending");
    Err(fatal_while_pending(req, thrown, "after"))
}

/// The throwable pending as a fatal is raised is reported first, as
/// `zend_exception_error(EG(exception), E_WARNING)` reports it
/// (`Z/Zend/zend.c:1514`) through `zend_error_va`
/// (`Z/Zend/zend_exceptions.c:921-930`): told to the observers with
/// `E_DONT_BAIL`, uncaught, and served by `zend_error_cb` alone, the
/// user's handler never asked (#2012).
#[test]
fn a_throwable_pending_at_a_fatal_is_told_uncaught_and_served_without_the_handler() {
    let rec = Recording::start();
    let engine = Engine::cli_builder()
        .extensions(&[
            TestExt::<0>::observing(),
            ExtensionBuilder::default()
                .functions(vec![fatal_while_thrown::declared()])
                .build("pending_fatal"),
        ])
        .build();
    let dir = scratch("pending_fatal");
    rec.mark("built");
    let served = serve_main(
        &engine,
        &dir,
        "<?php\nset_error_handler(function ($n, $s) { echo \"handler\\n\"; return true; });\n\
         fatal_while_thrown();\n",
    );
    rec.mark("served");
    assert!(
        !String::from_utf8_lossy(&served.stdout).contains("handler"),
        "{}",
        String::from_utf8_lossy(&served.stdout)
    );
    let events = only(
        &rec.events().between("built", "served"),
        &[
            ErrorObserved(Level::Error, String::new(), false),
            Error(String::new()),
        ],
    );
    assert!(
        matches!(
            events.as_slice(),
            [
                Ext(0, ErrorObserved(Level::Warning, told, true)),
                Ext(0, Error(served)),
                Ext(0, ErrorObserved(Level::Error, fatal, false)),
                Ext(0, Error(_)),
            ] if told.starts_with("Uncaught Exception: pending") && told == served && fatal == "after"
        ),
        "{events:?}"
    );
}

/// `class_alias()` links a class of the program again under the alias,
/// `zend_register_class_alias_ex`; an anonymous class's declaration and
/// an alias of an extension's class tell nothing.
#[test]
fn an_alias_of_a_program_class_is_linked_and_an_anonymous_class_is_not() {
    let rec = Recording::start();
    let engine = Engine::cli_builder()
        .extensions(&[TestExt::<0>::observing()])
        .build();
    let dir = scratch("alias");
    rec.mark("built");
    serve_main(
        &engine,
        &dir,
        "<?php\nclass P {}\nclass_alias('P', 'Q');\nclass_alias('stdClass', 'R');\n$o = new class {};\n",
    );
    rec.mark("served");
    assert_eq!(
        only(
            &rec.events().between("built", "served"),
            &[Declared(String::new()), Linked(String::new())]
        ),
        [linked("p"), linked("q")]
    );
}

/// `zend_observer_error_notify` beside `zend_error_cb`: the observer is
/// told of every diagnostic before the user's handler or the
/// `zend_error_cb` chain is handed it, of one the user's handler takes,
/// which no link is handed, and of one `@` hides, which the chain is
/// handed and `php_error_cb` masks; and of an uncaught throwable as the
/// `E_ERROR` `zend_exception_error` renders, marked uncaught as php's
/// `E_DONT_BAIL` marks it, where a fatal `zend_error` raises is not.
#[test]
fn the_error_observer_is_told_of_every_diagnostic_before_it_is_served() {
    let rec = Recording::start();
    let engine = Engine::cli_builder()
        .extensions(&[TestExt::<0>::observing()])
        .build();
    let dir = scratch("errors");
    rec.mark("built");
    serve_main(
        &engine,
        &dir,
        "<?php\nset_error_handler(function () { return true; });\necho $a;\n\
         restore_error_handler();\necho @$b;\nthrow new Exception('x');\n",
    );
    rec.mark("served");
    let events = only(
        &rec.events().between("built", "served"),
        &[
            ErrorObserved(Level::Error, String::new(), false),
            Error(String::new()),
        ],
    );
    let undefined = |name: &str| format!("Undefined variable ${name}");
    assert_eq!(
        events.get(..3).unwrap_or_default(),
        [
            Ext(0, ErrorObserved(Level::Warning, undefined("a"), false)),
            Ext(0, ErrorObserved(Level::Warning, undefined("b"), false)),
            Ext(0, Error(undefined("b"))),
        ]
    );
    assert!(
        matches!(
            events.get(3..),
            Some([Ext(0, ErrorObserved(Level::Error, told, true)), Ext(0, Error(served))])
                if told.starts_with("Uncaught Exception: x") && told == served
        ),
        "the uncaught throwable is told and then served: {events:?}"
    );
}

/// `observer_error_01.phpt`'s order: a fatal is told to the observer
/// before it is rendered, and every open frame is ended after.
#[test]
fn a_fatal_is_told_before_it_is_rendered_and_the_frames_ended() {
    let rec = Recording::start();
    let engine = Engine::cli_builder()
        .extensions(&[TestExt::<0>::observing()])
        .build();
    let dir = scratch("fatal");
    rec.mark("built");
    serve_main(
        &engine,
        &dir,
        "<?php\nfunction foo() { trigger_error('Foo error', E_USER_ERROR); }\nfoo();\n",
    );
    rec.mark("served");
    let events = only(
        &rec.events().between("built", "served"),
        &[
            ErrorObserved(Level::Error, String::new(), false),
            Error(String::new()),
            End(String::new(), None),
        ],
    );
    let fatal = events.iter().position(|e| {
        *e == Ext(
            0,
            ErrorObserved(Level::UserError, "Foo error".to_owned(), false),
        )
    });
    assert!(
        fatal.is_some_and(|at| {
            events.get(at + 1..)
                == Some(&[
                    Ext(0, Error("Foo error".to_owned())),
                    Ext(0, End("trigger_error".to_owned(), None)),
                    Ext(0, End("foo".to_owned(), None)),
                    Ext(0, End("file 'main.php'".to_owned(), None)),
                ])
        }),
        "{events:?}"
    );
}

/// The observer the tests of added handlers declare: it declines `target` and
/// every file, and gives each function named for an act a begin handler
/// that does it to `target`.
fn acting_init(observed: &mut Observed<'_>) -> ObserverHandlers {
    let name = observed_name(observed);
    record(0, ObserverInit(name.clone()));
    let act: fn(&mut Observed<'_>) = match name.as_str() {
        "add" => add,
        "add_end" => add_end,
        "remove" => remove,
        "remove_end" => remove_end,
        _ => return ObserverHandlers::default(),
    };
    ObserverHandlers {
        begin: Some(act),
        end: None,
    }
}

fn target(observed: &Observed<'_>) -> crate::api::call::Function {
    observed
        .function_named(Bare::resolved(b"target"))
        .unwrap_or(crate::api::call::Function::PASS)
}

fn extra_begin(observed: &mut Observed<'_>) {
    record(0, Begin(format!("extra {}", observed_name(observed))));
}

fn extra_end(observed: &mut Observed<'_>, returned: Option<&crate::api::values::Value>) {
    let returned = returned.map(super::test_ext::returned_name);
    record(
        0,
        End(format!("extra {}", observed_name(observed)), returned),
    );
}

fn acted(what: String) {
    record(0, Begin(what));
}

fn add(observed: &mut Observed<'_>) {
    let function = target(observed);
    let untouched = observed.function_named(Bare::resolved(b"untouched"));
    let added = observed.code_mut().add_begin_handler(function, extra_begin);
    let untouched =
        untouched.is_some_and(|f| observed.code_mut().add_begin_handler(f, extra_begin));
    acted(format!("add {added} untouched {untouched}"));
}

fn add_end(observed: &mut Observed<'_>) {
    let function = target(observed);
    let added = observed.code_mut().add_end_handler(function, extra_end);
    acted(format!("add_end {added}"));
}

fn remove(observed: &mut Observed<'_>) {
    let function = target(observed);
    let removed = observed
        .code_mut()
        .remove_begin_handler(function, extra_begin);
    acted(format!("remove {}", removed.is_some()));
}

fn remove_end(observed: &mut Observed<'_>) {
    let function = target(observed);
    let removed = observed.code_mut().remove_end_handler(function, extra_end);
    acted(format!("remove_end {}", removed.is_some()));
}

/// `zend_observer_add_begin_handler` and its kin over one function's
/// slot: a declined function takes a handler added and is begun by it
/// from its next call; a slot holds one begin and one end handler per
/// observer declared, so a second addition is refused, as is one to a
/// function not yet called, whose slot is unresolved; an end handler
/// added observes the calls after it; removing each leaves the
/// function declined again, and removing one it does not hold finds
/// nothing. What one request adds lives in its runtime cache, so the
/// next request on the engine, sharing the unit, sees none of it.
#[test]
fn handlers_are_added_to_and_removed_from_one_function_for_one_request() {
    let rec = Recording::start();
    let engine = Engine::cli_builder()
        .extensions(&[TestExt::<0>::builder()
            .observer(acting_init)
            .build("test_ext_0")])
        .build();
    let dir = scratch("added");
    let source = "<?php\nfunction target() { return 1; }\nfunction untouched() {}\n\
                  function add() {}\nfunction add_end() {}\nfunction remove() {}\nfunction remove_end() {}\n\
                  target(); add(); add(); target(); add_end(); target();\n\
                  remove(); target(); remove_end(); target(); remove();\n";
    rec.mark("first");
    serve_main(&engine, &dir, source);
    rec.mark("second");
    serve_main(
        &engine,
        &dir,
        "<?php\nfunction target() { return 1; }\ntarget();\n",
    );
    rec.mark("end");
    let kinds = [Begin(String::new()), End(String::new(), None)];
    let events = rec.events();
    let acted = |what: &str| Ext(0, Begin(what.to_owned()));
    let extra = || Ext(0, Begin("extra target".to_owned()));
    let extra_ended = || Ext(0, End("extra target".to_owned(), Some("1".to_owned())));
    assert_eq!(
        only(&events.between("first", "second"), &kinds),
        [
            acted("add true untouched false"),
            acted("add false untouched false"),
            extra(),
            acted("add_end true"),
            extra(),
            extra_ended(),
            acted("remove true"),
            extra_ended(),
            acted("remove_end true"),
            acted("remove false"),
        ]
    );
    assert_eq!(
        only(&events.between("second", "end"), &kinds),
        [],
        "the next request's slot is its own"
    );
}

/// The second observer's begin handler for `f`.
fn second_begin(observed: &mut Observed<'_>) {
    record(1, Begin(observed_name(observed)));
}

/// The first observer's begin handler for `f`, which removes itself the
/// first time it runs and, as php's contract asks, calls the handler
/// that moved into its place, which the walk passes over.
fn first_begin(observed: &mut Observed<'_>) {
    record(0, Begin(observed_name(observed)));
    let Some(function) = observed.function() else {
        return;
    };
    if let Some(Removed { next: Some(next) }) = observed
        .code_mut()
        .remove_begin_handler(function, first_begin)
    {
        next(observed);
    }
}

fn first_init(observed: &mut Observed<'_>) -> ObserverHandlers {
    ObserverHandlers {
        begin: (observed_name(observed) == "f").then_some(first_begin as fn(&mut Observed<'_>)),
        end: None,
    }
}

fn second_init(observed: &mut Observed<'_>) -> ObserverHandlers {
    ObserverHandlers {
        begin: (observed_name(observed) == "f").then_some(second_begin as fn(&mut Observed<'_>)),
        end: None,
    }
}

/// `zend_observer_remove_handler`'s `next`: a begin handler that removes
/// itself while the slot is walked moves the next one into its place,
/// which the walk then passes over, and is handed that one to call; so
/// each observer's handler runs once on the call that removed the
/// first, and the second's alone on the call after.
#[test]
fn a_handler_that_removes_itself_is_handed_the_next_to_call() {
    let rec = Recording::start();
    let engine = Engine::cli_builder()
        .extensions(&[
            TestExt::<0>::builder()
                .observer(first_init)
                .build("test_ext_0"),
            TestExt::<1>::builder()
                .observer(second_init)
                .build("test_ext_1"),
        ])
        .build();
    let dir = scratch("self");
    rec.mark("built");
    serve_main(&engine, &dir, "<?php\nfunction f() {}\nf();\nf();\n");
    rec.mark("served");
    assert_eq!(
        only(
            &rec.events().between("built", "served"),
            &[Begin(String::new())]
        ),
        [
            Ext(0, Begin("f".to_owned())),
            Ext(1, Begin("f".to_owned())),
            Ext(1, Begin("f".to_owned())),
        ]
    );
}

fn native_init(observed: &mut Observed<'_>) -> ObserverHandlers {
    let act: fn(&mut Observed<'_>) = match observed_name(observed).as_str() {
        "add" => add_to_native,
        "remove" => remove_from_native,
        _ => return ObserverHandlers::default(),
    };
    ObserverHandlers {
        begin: Some(act),
        end: None,
    }
}

fn add_to_native(observed: &mut Observed<'_>) {
    let native = observed.function_named(Bare::resolved(b"strcmp"));
    let strncmp = observed.function_named(Bare::resolved(b"strncmp"));
    let added = native.is_some_and(|f| observed.code_mut().add_begin_handler(f, extra_begin));
    let unresolved = strncmp.is_some_and(|f| observed.code_mut().add_begin_handler(f, extra_begin));
    acted(format!("add {added} unresolved {unresolved}"));
}

fn remove_from_native(observed: &mut Observed<'_>) {
    let native = observed.function_named(Bare::resolved(b"strcmp"));
    let removed = native.and_then(|f| observed.code_mut().remove_begin_handler(f, extra_begin));
    acted(format!("remove {}", removed.is_some()));
}

/// A native's slot takes a handler added and gives it up as a
/// function's does: php's `zend_observer_add_begin_handler` and its kin take any
/// `zend_function`, and a native's slot is the request's internal
/// runtime cache (`zend_observer_fcall_internal_function_extension`);
/// one no call in the request has resolved takes nothing.
#[test]
fn a_native_takes_a_handler_added_and_removed_for_one_request() {
    let rec = Recording::start();
    let engine = Engine::cli_builder()
        .extensions(&[TestExt::<0>::builder()
            .observer(native_init)
            .build("test_ext_0")])
        .build();
    let dir = scratch("native");
    rec.mark("built");
    serve_main(
        &engine,
        &dir,
        "<?php\nfunction add() {}\nfunction remove() {}\n\
         strcmp('a', 'b'); add(); strcmp('b', 'c'); remove(); strcmp('c', 'd');\n",
    );
    rec.mark("served");
    assert_eq!(
        only(
            &rec.events().between("built", "served"),
            &[Begin(String::new())]
        ),
        [
            Ext(0, Begin("add true unresolved false".to_owned())),
            Ext(0, Begin("extra strcmp".to_owned())),
            Ext(0, Begin("remove true".to_owned())),
        ]
    );
}

fn declared_in_fiber(
    _: &mut crate::api::request::Request,
    _: crate::api::call::Function,
    name: &[u8],
) {
    record(0, Declared(String::from_utf8_lossy(name).into_owned()));
}

fn linked_in_fiber(
    _: &mut crate::api::request::Request,
    _: crate::api::values::ClassId,
    name: &[u8],
) {
    record(0, Linked(String::from_utf8_lossy(name).into_owned()));
}

/// A fiber runs on a stack of its own and its chain of open frames is
/// its own (#1811), but a function's slot is the function's: a begin
/// handler added inside a fiber begins the function's calls on the
/// request's own stack and in the fiber alike, and a file the fiber
/// includes is told as any load is.
#[test]
fn a_handler_added_in_a_fiber_and_a_load_in_one_are_the_requests() {
    let rec = Recording::start();
    let engine = Engine::cli_builder()
        .extensions(&[TestExt::<0>::builder()
            .observer(acting_init)
            .function_declared_observer(declared_in_fiber)
            .class_linked_observer(linked_in_fiber)
            .build("test_ext_0")])
        .build();
    let dir = scratch("fiber");
    std::fs::write(dir.join("inc.php"), "<?php\nfunction g() {}\nclass K {}\n").ok();
    rec.mark("built");
    serve_main(
        &engine,
        &dir,
        "<?php\nfunction target() { return 1; }\nfunction add() {}\ntarget();\n\
         $f = new Fiber(function () { add(); Fiber::suspend(); target(); include __DIR__ . '/inc.php'; });\n\
         $f->start();\ntarget();\n$f->resume();\n",
    );
    rec.mark("served");
    assert_eq!(
        only(
            &rec.events().between("built", "served"),
            &[
                Begin(String::new()),
                Declared(String::new()),
                Linked(String::new())
            ]
        ),
        [
            Ext(0, Declared("target".to_owned())),
            Ext(0, Declared("add".to_owned())),
            Ext(0, Begin("add true untouched false".to_owned())),
            Ext(0, Begin("extra target".to_owned())),
            Ext(0, Begin("extra target".to_owned())),
            Ext(0, Declared("g".to_owned())),
            Ext(0, Linked("k".to_owned())),
        ]
    );
}
