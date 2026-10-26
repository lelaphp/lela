//! The hand-off from `Nop`'s loop to the tier's (#3362; ADR-0006,
//! amended on #3301): under [`Bound::Runtime`] a request starts in
//! `Nop`'s loop until a unit with counting words is published in the
//! engine, and one that publishes such a unit hands its frame to the
//! tier's loop at the next interrupt check, for the rest of the request.
//! The test's tier counts the activations it is asked at, which `Nop`'s
//! loop never asks, and lays every unit a request publishes counting.

use php_macros::php_function;

use crate::api::abi::{Beneath, ExtensionBuilder, chain};
use crate::api::raising::Thrown;
use crate::api::request::Request;
use crate::api::values::PhpString;
use crate::engine::Engine;
use crate::tier::{self, Bound, Flow, FrameRef, Held, Laid, Site, Started, Tier};

/// The activations the tier was asked at, the request's state.
#[derive(Debug, Default, Clone, Copy)]
struct Entered {
    /// At an activation.
    entries: u32,
    /// At a loop's back edge.
    loops: u32,
    /// Whether the interrupt hook served its code yet.
    hooked: bool,
}

/// A tier set up and off, which a request's publication switches on.
struct Switching;

impl Tier for Switching {
    type State = ();

    fn safepoint(req: &mut Request, _: FrameRef, site: Site, _: Held<'_>) -> Flow {
        match site {
            Site::Entry => req.state::<Entered>().entries += 1,
            Site::Loop(_) => req.state::<Entered>().loops += 1,
            Site::Return(_) => {}
        }
        Flow::Continue
    }

    fn bound(_: &Started<'_>) -> Bound {
        Bound::Runtime
    }

    fn laid(_: &Started<'_>, req: Option<&Request>) -> Laid {
        if req.is_some() {
            Laid::COLD
        } else {
            Laid::Sealed
        }
    }
}

/// An engine bound to [`Switching`], whose request state counts what it
/// was asked at, every file it includes held whatever its stamp.
fn switching() -> Engine {
    Engine::builder()
        .tier::<Switching>()
        .ini("opcache.file_update_protection", "0")
        .extensions(&[ExtensionBuilder::default()
            .request_state::<Entered>()
            .build("switching")])
        .build()
}

/// `held_unit(string $path): bool`: the file compiled into the cache
/// without running it, as `opcache_compile_file` holds it: a counting
/// unit published with no activation of its own, so the native's check
/// cannot hand its frame off and the hand-off is owed to the next check
/// that can ([`tier::owed`]).
#[php_function]
fn held_unit(req: &mut Request, path: PhpString) -> Result<bool, Thrown> {
    req.code_mut().unit_cache().compile(path.as_bytes())
}

/// The first interrupt's hook serves code of its own, as pcntl's signal
/// handlers serve PHP, here a string that calls a closure, whose
/// activation is a check a due hand-off would be taken at.
fn serving_hook(req: &mut Request, beneath: Beneath<'_, chain::Interrupt>) -> Result<(), Thrown> {
    if !req.state::<Entered>().hooked {
        req.state::<Entered>().hooked = true;
        let position = php_compile::syntax::Position::AfterOpenTag;
        req.code_mut()
            .serve_string(b"(function () { return 1; })();", b"hook", position)?;
    }
    beneath.serve(req)
}

/// [`switching`] with [`held_unit`], and [`serving_hook`] where `hooked`.
fn switching_held(hooked: bool) -> Engine {
    let extension = ExtensionBuilder::default()
        .request_state::<Entered>()
        .functions(vec![held_unit::declared()]);
    let extension = if hooked {
        extension.interrupt(serving_hook)
    } else {
        extension
    };
    Engine::builder()
        .tier::<Switching>()
        .ini("opcache.file_update_protection", "0")
        .extensions(&[extension.build("switching")])
        .build()
}

/// A scratch directory of the test's own, `name` and the process apart.
fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("dax-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("the scratch directory");
    std::fs::canonicalize(&dir).expect("the scratch directory")
}

/// The first request serves its own program in `Nop`'s loop, asked at
/// no activation, until the file it includes is published counting: the
/// file's activation hands its frame to the tier's loop, which is asked
/// there, at the call the file makes, and at the program's call after
/// the include, the rest of the serving being the tier's. The next
/// request starts in the tier's loop, asked at its program's activation
/// and its call.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_request_that_publishes_a_counting_unit_hands_its_frame_to_the_tiers_loop() {
    let dir = scratch("hand-off");
    std::fs::write(
        dir.join("inc.php"),
        "<?php function h() { return 1; } return h();",
    )
    .expect("the included file");
    let engine = switching();
    let serve = |source: &str| {
        let (served, entered) = tier::serve_kept::<Entered>(&engine, &dir, b"main.php", source);
        (served.stdout_text(), entered.unwrap_or_default().entries)
    };
    let first =
        serve("<?php function k() { return 2; } echo k(), include __DIR__ . '/inc.php', k();");
    assert_eq!(
        first,
        ("212".to_owned(), 3),
        "inc.php, h() and the second k()"
    );
    let next = serve("<?php function k() { return 2; } echo k();");
    assert_eq!(next, ("2".to_owned(), 2), "the program and k()");
    std::fs::remove_dir_all(&dir).ok();
}

/// A file a callback includes hands the callback's serving to the
/// tier's loop, and a serving started after it, the next callback's,
/// takes the request's loops, the tier's, as does the resumption of a
/// generator made before the hand-off.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_serving_started_after_a_hand_off_takes_the_requests_loop() {
    let dir = scratch("hand-off-cb");
    std::fs::write(dir.join("cb.php"), "<?php return 3;").expect("the callback's file");
    let engine = switching();
    let program = "<?php function g() { yield 4; }\n$gen = g();\nclass O {}\n$o = new O;\n$f = function ($i) { return $i ? 0 : include __DIR__ . '/cb.php'; };\necho $f->call($o, 0), $f->call($o, 1), $f->call($o, 2), $gen->current();";
    let (served, entered) = tier::serve_kept::<Entered>(&engine, &dir, b"main.php", program);
    assert_eq!(served.stdout_text(), "3004");
    // cb.php's activation, then each later callback's serving: the
    // second and third closures' activations.
    assert_eq!(entered.unwrap_or_default().entries, 3);
    std::fs::remove_dir_all(&dir).ok();
}

/// The tier's loop serves the rest of a handed serving on the machine
/// stack above `Nop`'s, one loop's frame deeper, and the limit a scope
/// holds a recursion to is measured from where each stands: past a
/// hand-off, a recursion through `eval` reaches its depth, a fiber
/// started after it serves its body in the request's loop, the tier's,
/// and a generator made before it resumes in the tier's loop.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn past_a_hand_off_recursion_fibers_and_generators_serve_on() {
    let dir = scratch("hand-off-stack");
    std::fs::write(dir.join("inc.php"), "<?php return 1;").expect("the included file");
    let engine = switching();
    let program = r#"<?php
function g() { yield 1; yield 2; }
$gen = g();
include __DIR__ . '/inc.php';
function f($n) { return $n ? eval('return f($n - 1);') + 1 : 0; }
echo f(3000), "
";
$fiber = new Fiber(function () { echo f(1000), "
"; Fiber::suspend(); echo "back
"; });
$fiber->start();
$fiber->resume();
foreach ($gen as $v) { echo $v; }
echo "
";
"#;
    let (served, entered) = tier::serve_kept::<Entered>(&engine, &dir, b"main.php", program);
    assert_eq!(
        served.stdout_text(),
        "3000
1000
back
12
"
    );
    // inc.php, f's 3001 activations and the eval'd code's 3000, the
    // fiber's closure and f's 1001 and the eval'd code's 1000 in it.
    assert_eq!(
        entered.unwrap_or_default().entries,
        1 + 3001 + 3000 + 1 + 1001 + 1000
    );
    std::fs::remove_dir_all(&dir).ok();
}

/// A unit published with no activation of its own, as
/// `opcache_compile_file()` holds one inside a loop, cannot be handed off
/// at the native's check, so the hand-off is owed and raised again past
/// it ([`tier::owed`]); the loop's next back edge takes it, handing its
/// frame stood at the loop's head to the tier's loop (`Stepped::Same`),
/// which skips that crossing's visit and is asked at the two back edges
/// after, and at no activation.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_hand_off_owed_past_a_natives_check_is_taken_at_the_next_back_edge() {
    let dir = scratch("hand-off-back-edge");
    std::fs::write(dir.join("inc.php"), "<?php return 1;").expect("the held file");
    let program = "<?php $n = 0; for ($i = 0; $i < 4; $i++) { if ($i === 0) { held_unit(__DIR__ . '/inc.php'); } $n++; } echo $n;";
    let (served, entered) =
        tier::serve_kept::<Entered>(&switching_held(false), &dir, b"main.php", program);
    assert_eq!(served.stdout_text(), "4");
    let entered = entered.unwrap_or_default();
    assert_eq!((entered.entries, entered.loops), (0, 2), "{entered:?}");
    std::fs::remove_dir_all(&dir).ok();
}

/// The owed hand-off is raised once the extensions' interrupt hooks are
/// served, so the code a hook serves, at the check after the native that
/// published the unit, runs in `Nop`'s loop to its end, its closure's
/// activation asked by no tier, and the interrupted loop's next back edge takes the
/// hand-off as it would with no hook (#3362).
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_hooks_serving_does_not_take_the_hand_off_it_interrupted() {
    let dir = scratch("hand-off-hook");
    std::fs::write(dir.join("inc.php"), "<?php return 1;").expect("the held file");
    let program = "<?php $n = 0; for ($i = 0; $i < 4; $i++) { if ($i === 0) { held_unit(__DIR__ . '/inc.php'); } $n++; } echo $n;";
    let (served, entered) =
        tier::serve_kept::<Entered>(&switching_held(true), &dir, b"main.php", program);
    assert_eq!(served.stdout_text(), "4");
    let entered = entered.unwrap_or_default();
    assert!(entered.hooked, "the hook served its code");
    assert_eq!((entered.entries, entered.loops), (0, 2), "{entered:?}");
    std::fs::remove_dir_all(&dir).ok();
}
