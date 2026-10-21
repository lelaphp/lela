//! Frames a fatal leaves (#2554): a fiber's stack is closed when the
//! fiber ends in one, and a serving with nothing executing puts the
//! frames it set aside back over whatever stands above them. Every
//! serving pops the frames it pushed on its way out, the fatal's way
//! included, through the engine's pop, which takes each frame's
//! [`crate::exec::Annex`] out of the request's slab; a frame the close
//! or the put-back lets go of all the same hands its header back, and
//! its annex leaves the slab there. The test counts the slab across a
//! debugger's evaluation with nothing executing that starts a fiber
//! whose `eval` raises a fatal, and across a serving that leaves a
//! frame with an annex standing.

use crate::api::abi::{ExtensionBuilder, Observed, ObserverHandlers};
use crate::engine::Engine;
use crate::exec::Header;
use crate::tier::{self, Evaluated, Under};

/// The evaluation: a fiber whose body stands in an `eval`'s frame, an
/// annex of its own, when it raises a fatal, which ends the fiber, then
/// leaves the evaluation's own frame, another annex, and is caught as
/// phpdbg's `ev` catches it.
const CODE: &[u8] =
    b"(new Fiber(function () { eval('trigger_error(\"x\", E_USER_ERROR);'); }))->start()";

/// What the stop counted, the debugger's request state, read back by
/// the test once the request ended ([`tier::serve_kept`]).
#[derive(Debug, Default, Clone, Copy)]
struct Counted {
    /// Whether the evaluation bailed out.
    bailed_out: bool,
    /// Whether the serving with nothing executing left a frame with an
    /// annex standing.
    left: bool,
    /// The annexes standing before the evaluation, after it, and after
    /// the serving that left its frame.
    annexes: [usize; 3],
}

/// `pause()` begun: the annexes counted around the evaluation, and
/// around a serving with nothing executing that pushes a call, gives it
/// an annex and leaves it standing, which no serving of the engine's
/// does today and the put-back lets go of.
fn stop(observed: &mut Observed<'_>) {
    let before = observed.annexes().live();
    let evaluated = tier::evaluate(observed, Under::Nothing, CODE);
    let after = observed.annexes().live();
    let function = observed.function();
    let left = observed.nothing_executing(|req| {
        function
            .and_then(|function| req.push_room(0, Header::call(function, 0)).ok())
            .is_some_and(|call| req.annex_mut(call).is_some())
    });
    let after_left = observed.annexes().live();
    *observed.state::<Counted>() = Counted {
        bailed_out: evaluated == Evaluated::BailedOut,
        left,
        annexes: [before, after, after_left],
    };
}

fn init(observed: &mut Observed<'_>) -> ObserverHandlers {
    let named = observed
        .function()
        .and_then(|function| observed.symbols().function_decl(function))
        .is_some_and(|decl| decl.name.0.as_ref() == b"pause");
    ObserverHandlers {
        begin: named.then_some(stop as fn(&mut Observed<'_>)),
        end: None,
    }
}

/// The fiber's frames and the evaluation's leave with their annexes,
/// and so does a frame the put-back lets go of: the slab holds after
/// each what it held before.
#[test]
fn frames_let_go_after_a_fatal_or_left_standing_leave_no_annex() {
    let engine = Engine::cli_builder()
        .extensions(&[ExtensionBuilder::default()
            .observer(init)
            .request_state::<Counted>()
            .build("debugger")])
        .build();
    let (served, counted) = tier::serve_kept::<Counted>(
        &engine,
        &std::env::current_dir().expect("the working directory"),
        b"Standard input code",
        "<?php\nfunction pause() {}\npause();\necho \"on\\n\";\n",
    );
    let out = served.stdout_text();
    assert!(
        counted.is_some_and(|counted| counted.bailed_out
            && counted.left
            && counted.annexes.iter().all(|&n| n == counted.annexes[0])),
        "{counted:?}; {out}"
    );
    assert!(out.ends_with("on\n"), "the request went on: {out}");
}
