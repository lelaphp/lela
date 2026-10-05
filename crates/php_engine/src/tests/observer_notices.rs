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

use std::path::{Path, PathBuf};

use super::test_ext::{Event, Events, Phase, Recording, TestExt, observed_name, record};
use crate::api::abi::{
    Admission, ExtensionBuilder, Lookup, Observed, ObserverHandlers, Removed, Validation,
};
use crate::api::raising::Level;
use crate::engine::Engine;
use crate::test::{Capture, Served};

use Event::Ext;
use Phase::{Begin, Declared, End, Error, ErrorObserved, Linked, ObserverInit};

fn scratch(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "php_engine_observer_notices_{name}_{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::create_dir_all(&dir);
    dir
}

fn serve_main(engine: &Engine, dir: &Path, source: &str) -> Served {
    let main = dir.join("main.php");
    let _ = std::fs::write(&main, source);
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
    let _ = std::fs::write(dir.join("inc.php"), INCLUDED);
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
        .function_named(b"target")
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
    let untouched = observed.function_named(b"untouched");
    let added = observed.add_begin_handler(function, extra_begin);
    let untouched = untouched.is_some_and(|f| observed.add_begin_handler(f, extra_begin));
    acted(format!("add {added} untouched {untouched}"));
}

fn add_end(observed: &mut Observed<'_>) {
    let function = target(observed);
    let added = observed.add_end_handler(function, extra_end);
    acted(format!("add_end {added}"));
}

fn remove(observed: &mut Observed<'_>) {
    let function = target(observed);
    let removed = observed.remove_begin_handler(function, extra_begin);
    acted(format!("remove {}", removed.is_some()));
}

fn remove_end(observed: &mut Observed<'_>) {
    let function = target(observed);
    let removed = observed.remove_end_handler(function, extra_end);
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
    if let Some(Removed { next: Some(next) }) = observed.remove_begin_handler(function, first_begin)
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
