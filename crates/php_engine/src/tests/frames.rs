//! Frame reads for extensions (#1827): an observer's handler reads the
//! frame it is handed, and the frames below it, through a borrowed
//! [`FrameView`], and keeps a frame past the handler as a [`FrameId`],
//! which [`Request::frame_of`] checks against the stacks that stand before
//! it reads: a frame that left them is refused, and never read as the
//! frame that stands at its old place now. A generator's frame is
//! taken off the stack at each yield and pushed where its next resumer
//! stands, and a fiber switch swaps the whole stack, so a bare
//! position read either way names another live frame.
//!
//! The extension below keeps the frame that calls `keep()`, and at
//! each call of `read()` writes what the kept frame reads as, between
//! the bytes the program echoes.

use crate::api::abi::{Extension, ExtensionBuilder, Observed, ObserverHandlers};
use crate::api::request::{FrameId, FrameView, Request};
use crate::engine::Engine;
use crate::test::Capture;

/// The frame `keep()` was called from, as the extension's request state.
#[derive(Default)]
struct Kept(Option<FrameId>);

/// The function `view` serves, by its declared name, or the file's
/// base name for an included, evaluated or main file's frame.
fn name(req: &Request, view: &FrameView<'_>) -> String {
    if let Some(function) = view.function() {
        return req
            .function_decl(function)
            .map(|decl| String::from_utf8_lossy(&decl.name.0).into_owned())
            .unwrap_or_default();
    }
    let file = view.filename();
    let base = file.rsplit(|&b| b == b'/').next().unwrap_or(file);
    String::from_utf8_lossy(base).into_owned()
}

fn init(observed: &mut Observed<'_>) -> ObserverHandlers {
    let Some(frame) = observed.frame() else {
        return ObserverHandlers::default();
    };
    let begin: fn(&mut Observed<'_>) = match name(observed, &frame).as_str() {
        "keep" => keep,
        "read" => read,
        _ => return ObserverHandlers::default(),
    };
    ObserverHandlers {
        begin: Some(begin),
        end: None,
    }
}

/// `keep()` begun: its caller's frame kept past the handler.
fn keep(observed: &mut Observed<'_>) {
    let caller = observed
        .caller()
        .map(|view| (view.id(), name(observed, &view)));
    let Some((id, named)) = caller else {
        observed.write(b"[kept nothing]");
        return;
    };
    observed.state::<Kept>().0 = Some(id);
    observed.write(format!("[kept {named}]").as_bytes());
}

/// `read()` begun: the kept frame read through its identity, its
/// function and line, or refused.
fn read(observed: &mut Observed<'_>) {
    let kept = observed.state::<Kept>().0;
    let read = kept.and_then(|id| {
        let view = observed.frame_of(id)?;
        Some(format!("{}:{}", name(observed, &view), view.line()))
    });
    let read = read.unwrap_or_else(|| String::from("refused"));
    observed.write(format!("[read {read}]").as_bytes());
}

fn keeping() -> Extension {
    ExtensionBuilder::default()
        .request_state::<Kept>()
        .observer(init)
        .build("frame_reads")
}

fn serve(source: &str) -> String {
    let engine = Engine::cli_builder().extensions(&[keeping()]).build();
    let served = engine.serve_capture(source, Capture::default());
    String::from_utf8_lossy(&served.stdout).into_owned()
}

/// A generator's frame kept while it runs is refused while the
/// generator is suspended, where the frame of `read()` itself now
/// stands at the place it stood, and read again once a resumer pushes
/// it back, at another depth: the identity is php's `execute_data`,
/// which is one across a generator's resumptions. Once the generator
/// has returned, it is refused for good.
#[test]
fn a_kept_generator_frame_is_refused_while_it_is_suspended_and_after_it_returns() {
    let out = serve(
        "<?php\nfunction keep() {}\nfunction read() {}\n\
         function gen() {\n    keep();\n    read();\n    yield 1;\n    read();\n}\n\
         function deeper($g) { $g->next(); }\n\
         $g = gen();\n$g->current();\necho \"|\";\nread();\necho \"|\";\ndeeper($g);\necho \"|\";\nread();\n",
    );
    assert_eq!(
        out,
        "[kept gen][read gen:6]|[read refused]|[read gen:8]|[read refused]"
    );
}

/// A fiber's frame kept while the fiber runs is refused once it
/// suspends, where the resumer's frame stands at the same position of
/// its own stack, read again when the fiber is resumed, and refused
/// once the fiber has returned.
#[test]
fn a_kept_fiber_frame_is_refused_while_the_fiber_is_suspended() {
    let out = serve(
        "<?php\nfunction keep() {}\nfunction read() {}\n\
         function inner() {\n    keep();\n    read();\n    Fiber::suspend();\n    read();\n}\n\
         $fiber = new Fiber(function () { inner(); });\n\
         $fiber->start();\necho \"|\";\nread();\necho \"|\";\n$fiber->resume();\necho \"|\";\nread();\n",
    );
    assert_eq!(
        out,
        "[kept inner][read inner:6]|[read refused]|[read inner:8]|[read refused]"
    );
}

/// The resumer's frame, kept before a fiber starts, is read from inside
/// the fiber on the stack it stands on, not as the fiber's frame at the
/// same position, and refused once it has returned.
#[test]
fn a_kept_frame_below_a_fiber_is_read_on_its_own_stack() {
    let out = serve(
        "<?php\nfunction keep() {}\nfunction read() {}\n\
         function outer() {\n    keep();\n    $fiber = new Fiber(function () { read(); });\n    $fiber->start();\n}\n\
         outer();\necho \"|\";\nread();\n",
    );
    assert_eq!(out, "[kept outer][read outer:7]|[read refused]");
}

/// The frames below the observed one, walked by [`FrameView::caller`]:
/// php's `prev_execute_data` chain over user frames, through a fiber
/// into the stack that resumed it.
#[test]
fn a_frame_view_walks_its_callers_into_the_resumer_of_a_fiber() {
    fn walk(observed: &mut Observed<'_>) {
        let mut names = Vec::new();
        let mut at = observed.caller();
        while let Some(view) = at {
            names.push(format!("{}:{}", name(observed, &view), view.line()));
            at = view.caller();
        }
        let line = format!("[{}]", names.join(" < "));
        observed.write(line.as_bytes());
    }
    fn init(observed: &mut Observed<'_>) -> ObserverHandlers {
        let walks = observed
            .frame()
            .is_some_and(|frame| name(observed, &frame) == "walk");
        ObserverHandlers {
            begin: walks.then_some(walk as fn(&mut Observed<'_>)),
            end: None,
        }
    }
    let engine = Engine::cli_builder()
        .extensions(&[ExtensionBuilder::default()
            .observer(init)
            .build("frame_walk")])
        .build();
    let served = engine.serve_capture(
        "<?php\nfunction walk() {}\nfunction f() {\n    $fiber = new Fiber(function () {\n        walk();\n    });\n    $fiber->start();\n}\nf();\n",
        Capture::default(),
    );
    assert_eq!(
        String::from_utf8_lossy(&served.stdout),
        "[{closure:f():4}:5 < f:7 < Standard input code:9]"
    );
}

/// A native's call stands in no frame of the loop's, so
/// [`Observed::frame`] answers none for it, and [`Observed::caller`]
/// answers the user frame it was called from, which a kept identity
/// reads as any other.
#[test]
fn a_native_call_has_no_frame_and_its_caller_is_the_user_frame_it_stands_on() {
    fn native(observed: &mut Observed<'_>) {
        let framed = observed.frame().is_some();
        let caller = observed
            .caller()
            .map(|view| format!("{}:{}", name(observed, &view), view.line()));
        let line = format!(
            "[strcmp framed={framed} from {}]",
            caller.unwrap_or_default()
        );
        observed.write(line.as_bytes());
    }
    fn init(observed: &mut Observed<'_>) -> ObserverHandlers {
        let named = observed.frame().is_none()
            && observed
                .function()
                .and_then(|function| observed.function_decl(function))
                .is_some_and(|decl| decl.name.0.as_ref() == b"strcmp");
        ObserverHandlers {
            begin: named.then_some(native as fn(&mut Observed<'_>)),
            end: None,
        }
    }
    let engine = Engine::cli_builder()
        .extensions(&[ExtensionBuilder::default()
            .observer(init)
            .build("frame_native")])
        .build();
    let served = engine.serve_capture(
        "<?php\nfunction f($s) {\n    return strcmp($s, 'abc');\n}\necho f('abc');\n",
        Capture::default(),
    );
    assert_eq!(
        String::from_utf8_lossy(&served.stdout),
        "[strcmp framed=false from f:3]0"
    );
}
