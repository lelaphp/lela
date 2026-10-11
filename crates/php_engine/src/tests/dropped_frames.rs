//! Frames a fatal leaves (#2554): a fiber's stack is closed when the
//! fiber ends in one, and a serving with nothing executing puts the
//! frames it set aside back over whatever stands above them. Every
//! serving pops the frames it pushed on its way out, the fatal's way
//! included, through the engine's pop, which takes each frame's
//! [`crate::exec::Annex`] out of the request's slab, so neither finds a
//! frame to drop and no annex stays behind. The test counts the slab
//! across a debugger's evaluation with nothing executing that starts a
//! fiber whose `eval` raises a fatal.

use std::cell::Cell;

use crate::api::abi::{ExtensionBuilder, Observed, ObserverHandlers};
use crate::engine::Engine;
use crate::test::Capture;
use crate::tier::{self, Evaluated, Under};

/// The evaluation: a fiber whose body stands in an `eval`'s frame, an
/// annex of its own, when it raises a fatal, which ends the fiber, then
/// leaves the evaluation's own frame, another annex, and is caught as
/// phpdbg's `ev` catches it.
const CODE: &[u8] =
    b"(new Fiber(function () { eval('trigger_error(\"x\", E_USER_ERROR);'); }))->start()";

thread_local! {
    /// What the stop counted: whether the evaluation bailed out, and the
    /// annexes standing before it and after it.
    static COUNTED: Cell<Option<(bool, usize, usize)>> = const { Cell::new(None) };
}

/// `pause()` begun: the annexes counted around the evaluation.
fn stop(observed: &mut Observed<'_>) {
    let before = observed.annexes().live();
    let evaluated = tier::evaluate(observed, Under::Nothing, CODE);
    let after = observed.annexes().live();
    COUNTED.set(Some((evaluated == Evaluated::BailedOut, before, after)));
}

fn init(observed: &mut Observed<'_>) -> ObserverHandlers {
    let named = observed
        .function()
        .and_then(|function| observed.function_decl(function))
        .is_some_and(|decl| decl.name.0.as_ref() == b"pause");
    ObserverHandlers {
        begin: named.then_some(stop as fn(&mut Observed<'_>)),
        end: None,
    }
}

/// The fiber's frames and the evaluation's leave with their annexes:
/// the slab holds after the evaluation what it held before.
#[test]
fn frames_dropped_after_a_fatal_leave_no_annex() {
    let engine = Engine::cli_builder()
        .extensions(&[ExtensionBuilder::default().observer(init).build("debugger")])
        .build();
    let served = engine.serve_capture(
        "<?php\nfunction pause() {}\npause();\necho \"on\\n\";\n",
        Capture::default(),
    );
    let out = String::from_utf8_lossy(&served.stdout);
    let counted = COUNTED.take();
    assert!(
        counted.is_some_and(|(bailed_out, before, after)| bailed_out && before == after),
        "bailed out, and the annexes before and after: {counted:?}; {out}"
    );
    assert!(out.ends_with("on\n"), "the request went on: {out}");
}
