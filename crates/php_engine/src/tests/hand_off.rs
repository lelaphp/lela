//! The hand-off from `Nop`'s loop to the tier's (#3362; ADR-0006,
//! amended on #3301): under [`Bound::Runtime`] a request starts in
//! `Nop`'s loop until a unit with counting words is published in the
//! engine, and one that publishes such a unit hands its frame to the
//! tier's loop at the next interrupt check, for the rest of the request.
//! The test's tier counts the activations it is asked at, which `Nop`'s
//! loop never asks, and lays every unit a request publishes counting.

use crate::api::abi::ExtensionBuilder;
use crate::api::request::Request;
use crate::engine::Engine;
use crate::tier::{self, Bound, Flow, FrameRef, Held, Laid, Site, Started, Tier};

/// The activations the tier was asked at, the request's state.
#[derive(Debug, Default, Clone, Copy)]
struct Entered(u32);

/// A tier set up and off, which a request's publication switches on.
struct Switching;

impl Tier for Switching {
    type State = ();

    fn safepoint(req: &mut Request, _: FrameRef, site: Site, _: Held<'_>) -> Flow {
        if site == Site::Entry {
            req.state::<Entered>().0 += 1;
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
    let dir = std::env::temp_dir().join(format!("dax-{}-hand-off", std::process::id()));
    std::fs::create_dir_all(&dir).expect("the scratch directory");
    let dir = std::fs::canonicalize(&dir).expect("the scratch directory");
    std::fs::write(
        dir.join("inc.php"),
        "<?php function h() { return 1; } return h();",
    )
    .expect("the included file");
    let engine = Engine::builder()
        .tier::<Switching>()
        .ini("opcache.file_update_protection", "0")
        .extensions(&[ExtensionBuilder::default()
            .request_state::<Entered>()
            .build("switching")])
        .build();
    let serve = |source: &str| {
        let (served, entered) = tier::serve_kept::<Entered>(&engine, &dir, b"main.php", source);
        (served.stdout_text(), entered.unwrap_or_default().0)
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
    let dir = std::env::temp_dir().join(format!("dax-{}-hand-off-cb", std::process::id()));
    std::fs::create_dir_all(&dir).expect("the scratch directory");
    let dir = std::fs::canonicalize(&dir).expect("the scratch directory");
    std::fs::write(dir.join("cb.php"), "<?php return 3;").expect("the callback's file");
    let engine = Engine::builder()
        .tier::<Switching>()
        .ini("opcache.file_update_protection", "0")
        .extensions(&[ExtensionBuilder::default()
            .request_state::<Entered>()
            .build("switching")])
        .build();
    let program = "<?php function g() { yield 4; }\n$gen = g();\nclass O {}\n$o = new O;\n$f = function ($i) { return $i ? 0 : include __DIR__ . '/cb.php'; };\necho $f->call($o, 0), $f->call($o, 1), $f->call($o, 2), $gen->current();";
    let (served, entered) = tier::serve_kept::<Entered>(&engine, &dir, b"main.php", program);
    assert_eq!(served.stdout_text(), "3004");
    // cb.php's activation, then each later callback's serving: the
    // second and third closures' activations.
    assert_eq!(entered.unwrap_or_default().0, 3);
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
    let dir = std::env::temp_dir().join(format!("dax-{}-hand-off-stack", std::process::id()));
    std::fs::create_dir_all(&dir).expect("the scratch directory");
    let dir = std::fs::canonicalize(&dir).expect("the scratch directory");
    std::fs::write(dir.join("inc.php"), "<?php return 1;").expect("the included file");
    let engine = Engine::builder()
        .tier::<Switching>()
        .ini("opcache.file_update_protection", "0")
        .extensions(&[ExtensionBuilder::default()
            .request_state::<Entered>()
            .build("switching")])
        .build();
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
        entered.unwrap_or_default().0,
        1 + 3001 + 3000 + 1 + 1001 + 1000
    );
    std::fs::remove_dir_all(&dir).ok();
}
