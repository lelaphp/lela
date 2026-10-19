//! The tier's reads of what a request's epoch holds (#2950;
//! `jit-design.md` §3.2, §4.1 *The owned view*, §5.3): the safepoint
//! words of a held unit's op arrays, zero and shared by every request of
//! the epoch, the sealed word of an op array no epoch holds, and
//! [`UnitView`], which accepts what the epoch keeps, refuses what the
//! request alone has, and outlives the request that built it. A program
//! reaches them through natives of the test's own, each reading the
//! frame of the user function that called it, as the tier reads the
//! frame at a site.

use std::cell::RefCell;
use std::path::PathBuf;
use std::sync::atomic::Ordering;

use php_macros::php_function;
use php_unit::CacheSlot;
use php_unit::decl::Bare;

use crate::api::abi::{Admission, Extension, ExtensionBuilder, Lookup, Validation};
use crate::api::request::Request;
use crate::api::values::PhpString;
use crate::engine::Engine;
use crate::test::Capture;
use crate::tier::{self, FuncRef, SafepointIdx, UnitView};

thread_local! {
    /// The views the natives built, with the op array each was asked to
    /// hold, kept past the request for the test to read.
    static VIEWS: RefCell<Vec<(UnitView, FuncRef)>> = const { RefCell::new(Vec::new()) };
}

/// The op array the calling user function serves.
fn caller(req: &Request) -> Option<FuncRef> {
    tier::func(req, req.innermost_user_frame()?)
}

/// `hold_caller(): bool`: a view of the request's epoch asked to hold the
/// calling function's op array, kept for the test.
#[php_function]
fn hold_caller(req: &mut Request) -> bool {
    let Some(f) = caller(req) else {
        return false;
    };
    let mut view = UnitView::new(req);
    let held = view.hold(req, f);
    VIEWS.with(|views| views.borrow_mut().push((view, f)));
    held
}

/// `hold_class_named(string $name): bool`: whether a view accepts the
/// class the request knows by `$name`.
#[php_function]
fn hold_class_named(req: &mut Request, name: PhpString) -> bool {
    let Some(class) = req.class_named(Bare::resolved(name.as_bytes())) else {
        return false;
    };
    let mut view = UnitView::new(req);
    view.hold_class(req, class) && view.class(class).is_some()
}

/// `bump_entry(): int`: the calling function's entry word, its first half
/// bumped as the tier bumps a cold word, and what it holds after; -1 for
/// a sealed word, which nothing writes.
#[php_function]
fn bump_entry(req: &mut Request) -> i64 {
    let Some(frame) = req.innermost_user_frame() else {
        return -2;
    };
    let word = tier::word(req, frame, SafepointIdx::ENTRY);
    let [heat, state] = word.halves();
    if state.load(Ordering::Relaxed) == u32::MAX {
        return -1;
    }
    i64::from(heat.fetch_add(1, Ordering::Relaxed).saturating_add(1))
}

/// What an accelerator answers for every file: held, never revalidated.
fn admitted(_: &mut Request, _: &[u8]) -> Admission {
    Admission::Held {
        validation: Validation::Never,
        lookup: Lookup::Resolved,
        protection: 0,
        file_cache: None,
    }
}

/// The natives, with every file the cache is asked for held.
fn viewing() -> Extension {
    ExtensionBuilder::default()
        .admission(admitted)
        .functions(vec![
            hold_caller::declared(),
            hold_class_named::declared(),
            bump_entry::declared(),
        ])
        .build("viewing")
}

/// A directory of the test's own holding `inc.php`, the file the cache
/// holds, written with `source`.
fn scratch(name: &str, source: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "php_engine_unit_view_{name}_{}",
        std::process::id()
    ));
    std::fs::remove_dir_all(&dir).ok();
    std::fs::create_dir_all(&dir).ok();
    std::fs::write(dir.join("inc.php"), source).ok();
    dir
}

/// What `program` wrote, served in `dir`.
fn serve(engine: &Engine, dir: &std::path::Path, program: &str) -> String {
    let main = dir.join("main.php");
    let served = engine.serve_capture_in(
        dir,
        main.to_string_lossy().as_bytes(),
        program,
        Capture::default(),
    );
    String::from_utf8_lossy(&served.stdout).into_owned()
}

/// A function of a file the cache holds is held by the view; one an
/// `eval` declared, whose unit is the request's alone, is refused. The
/// view of the held one reads its op array after the request ended and
/// the engine was dropped, the epoch it holds keeping the unit; the
/// refused one reads nothing.
#[test]
fn a_view_holds_a_held_units_function_refuses_an_evals_and_outlives_its_request() {
    VIEWS.with(|views| views.borrow_mut().clear());
    let dir = scratch(
        "hold",
        "<?php function held() { for ($i = 0; $i < 2; $i++) {} return hold_caller(); }",
    );
    let engine = Engine::cli_builder().extensions(&[viewing()]).build();
    let out = serve(
        &engine,
        &dir,
        "<?php include __DIR__ . '/inc.php';\n\
         eval('function own() { return hold_caller(); }');\n\
         var_dump(held(), own());",
    );
    assert_eq!(out, "bool(true)\nbool(false)\n");
    drop(engine);
    std::fs::remove_dir_all(&dir).ok();
    let views = VIEWS.with(RefCell::take);
    assert_eq!(views.len(), 2, "two views were built");
    let mut views = views.into_iter();
    let (Some((held_view, held)), Some((own_view, own))) = (views.next(), views.next()) else {
        return;
    };
    let code = held_view.op_array(held);
    assert!(
        code.is_some_and(|code| code.safepoints.len() >= 3),
        "the entry, the loop header and the return"
    );
    let size = code.map_or(0, |code| code.cache_size);
    assert_eq!(
        held_view.cache_slot(held, CacheSlot(size)),
        None,
        "past its cache"
    );
    assert!(own_view.op_array(own).is_none());
    assert!(held_view.op_array(own).is_none(), "never asked to hold it");
    assert_eq!(held_view.is_main(held), Some(false), "a function's own");
    assert_eq!(own_view.is_main(own), None, "refused, so unread");
}

/// A held file's own code is its main op array, whose variables a symbol
/// table reaches, and the view says so; a function of it is not.
#[test]
fn a_view_tells_a_files_own_code_from_its_functions() {
    VIEWS.with(|views| views.borrow_mut().clear());
    let dir = scratch(
        "main",
        "<?php function inner() { return hold_caller(); } hold_caller(); inner();",
    );
    let engine = Engine::cli_builder().extensions(&[viewing()]).build();
    serve(&engine, &dir, "<?php include __DIR__ . '/inc.php';");
    std::fs::remove_dir_all(&dir).ok();
    let views = VIEWS.with(RefCell::take);
    let mains: Vec<Option<bool>> = views.iter().map(|(view, f)| view.is_main(*f)).collect();
    assert_eq!(mains, [Some(true), Some(false)]);
}

/// A held function's view lists its declaration's parameters, their
/// types and how each is passed, which a `RECV`'s mask does not carry;
/// a file's own code lists none.
#[test]
fn a_view_lists_a_functions_parameters() {
    VIEWS.with(|views| views.borrow_mut().clear());
    let dir = scratch(
        "parameters",
        "<?php function typed(int $n, Foo|int $o, array &$a, $b = 1) { return hold_caller(); }\n\
         hold_caller(); $x = []; typed(1, 2, $x);",
    );
    let engine = Engine::cli_builder().extensions(&[viewing()]).build();
    serve(&engine, &dir, "<?php include __DIR__ . '/inc.php';");
    std::fs::remove_dir_all(&dir).ok();
    let views = VIEWS.with(RefCell::take);
    let listed: Vec<Option<Vec<(String, bool)>>> = views
        .iter()
        .map(|(view, f)| {
            view.parameters(*f).map(|parameters| {
                parameters
                    .iter()
                    .map(|p| (String::from_utf8_lossy(&p.name.0).into_owned(), p.by_ref))
                    .collect()
            })
        })
        .collect();
    let named = |name: &str, by_ref| (name.to_owned(), by_ref);
    assert_eq!(
        listed,
        [
            Some(vec![]),
            Some(vec![
                named("n", false),
                named("o", false),
                named("a", true),
                named("b", false)
            ]),
        ]
    );
    let (typed_view, typed) = &views[1];
    let masks: Vec<u32> = typed_view
        .parameters(*typed)
        .unwrap_or_default()
        .iter()
        .map(|p| p.ty.as_ref().map_or(0, php_unit::decl::Type::mask))
        .collect();
    assert_eq!(
        masks,
        [1 << 4, 1 << 4, 1 << 7, 0],
        "a class name adds no bit to the mask"
    );
}

/// A held function's entry word is zero at publication and one word for
/// every request of the epoch: the second request's bump finds the
/// first's. An `eval`'s function reads the sealed word.
#[test]
fn a_held_functions_words_are_shared_by_requests_and_an_evals_are_sealed() {
    let dir = scratch("words", "<?php function hot() { return bump_entry(); }");
    let engine = Engine::cli_builder().extensions(&[viewing()]).build();
    let program = "<?php include __DIR__ . '/inc.php';\n\
                   eval('function cold() { return bump_entry(); }');\n\
                   echo hot(), ' ', hot(), ' ', cold(), \"\\n\";";
    assert_eq!(serve(&engine, &dir, program), "1 2 -1\n");
    assert_eq!(serve(&engine, &dir, program), "3 4 -1\n");
    std::fs::remove_dir_all(&dir).ok();
}

/// A class the held file bound early is published and the view accepts
/// it, as it accepts one of the engine's; one an `eval` declared is the
/// request's own and is refused.
#[test]
fn a_view_holds_a_published_class_and_refuses_a_request_local_one() {
    let dir = scratch("class", "<?php class Early {}");
    let engine = Engine::cli_builder().extensions(&[viewing()]).build();
    let out = serve(
        &engine,
        &dir,
        "<?php include __DIR__ . '/inc.php';\n\
         eval('class Own {}');\n\
         var_dump(hold_class_named('Early'), hold_class_named('stdClass'), hold_class_named('Own'));",
    );
    assert_eq!(out, "bool(true)\nbool(true)\nbool(false)\n");
    std::fs::remove_dir_all(&dir).ok();
}
