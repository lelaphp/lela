//! The crossing at its seam (`jit-design.md` §4.6 *Tested*): the wrappers
//! over a stand-in request, each `Status` and what it keeps; then, where
//! the target maps code, hand-assembled bodies entered through `enter` on
//! real requests, served by a test tier at a function's entry.

use std::ptr;

use super::*;

/// A kernel that raises a fatal.
fn refuse(_: &mut Tally, _: Room, _: OplineIdx, why: u64) -> Result<u64, Fatal> {
    Err(Fatal::error(format!("refused {why}")))
}

/// A kernel that panics.
fn explode(_: &mut Tally, _: Room, _: OplineIdx, _: u64) -> Result<u64, Fatal> {
    panic!("the kernel panicked")
}

/// A kernel that leaves a throwable pending and answers its value.
#[allow(
    clippy::unnecessary_wraps,
    reason = "a kernel answers a Result; this one never fails"
)]
fn throw(t: &mut Tally, _: Room, _: OplineIdx, n: u64) -> Result<u64, Fatal> {
    t.thrown = true;
    Ok(n)
}

kernels! { Tally => mod failing { refuse(why: u64) -> u64; explode(n: u64) -> u64; throw(n: u64) -> u64; } }

/// A call that returns writes its value through the out slot, leaves
/// the request as the kernel left it, and keeps nothing.
#[test]
fn a_call_that_returns_continues_with_its_value_in_the_slot() {
    let mut t = Tally {
        word: 3,
        thrown: false,
    };
    let mut out = 0;
    let mut cx = ExitCtx::new(&mut t);
    // SAFETY: a live context and a slot on this frame.
    let status = unsafe { tallied::tally(&raw mut cx, ptr::null_mut(), 0, 4, &raw mut out) };
    assert_eq!(status, Status::Continue);
    assert!(cx.take_pending().is_none());
    let expected = 3u64.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(4);
    assert_eq!((out, t.word), (expected, expected));
}

/// A fatal is kept in the `ExitCtx` for `enter` to return, and the slot is
/// not written.
#[test]
fn a_fatal_is_kept_in_the_context_and_answered_as_a_bailout() {
    let mut t = Tally::default();
    let mut out = 7;
    let mut cx = ExitCtx::new(&mut t);
    // SAFETY: a live context and a slot on this frame.
    let status = unsafe { failing::refuse(&raw mut cx, ptr::null_mut(), 0, 9, &raw mut out) };
    assert_eq!(status, Status::Bailout);
    assert!(
        matches!(cx.take_pending(), Some(Pending::Bailout(f)) if f == Fatal::error("refused 9"))
    );
    assert_eq!(out, 7);
}

/// A panic stops at the wrapper, its payload kept for `enter` to
/// resume, so no unwind crosses compiled code.
#[test]
fn a_panic_is_caught_at_the_wrapper_and_its_payload_kept() {
    let mut t = Tally::default();
    let mut out = 0;
    let mut cx = ExitCtx::new(&mut t);
    // SAFETY: a live context and a slot on this frame.
    let status = unsafe { failing::explode(&raw mut cx, ptr::null_mut(), 0, 1, &raw mut out) };
    assert_eq!(status, Status::Panic);
    assert!(matches!(
        cx.take_pending(),
        Some(Pending::Panic(payload)) if payload.downcast_ref::<&str>() == Some(&"the kernel panicked")
    ));
}

/// A throwable the kernel left pending is answered as one, with the
/// kernel's value written and nothing kept: the request holds it.
#[test]
fn a_throwable_left_pending_is_answered_as_an_exception() {
    let mut t = Tally::default();
    let mut out = 0;
    let mut cx = ExitCtx::new(&mut t);
    // SAFETY: a live context and a slot on this frame.
    let status = unsafe { failing::throw(&raw mut cx, ptr::null_mut(), 0, 5, &raw mut out) };
    assert_eq!(status, Status::Exception);
    assert!(cx.take_pending().is_none());
    assert_eq!(out, 5);
}

/// A fatal a kernel raised and the engine is bailing out from, the memory
/// limit an append past it reaches inside `step`, is answered as a
/// throwable is, so the compiled loop leaves and the loop unwinds, where
/// the wrapper had answered `Continue` and the loop appended again until
/// `max_execution_time` stopped it, silently (`Zend/tests/bug70258.phpt`,
/// which ran past the lane's 60 seconds). The request ends in the
/// fatal well inside the ten seconds the limit is set to.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_bailout_a_step_began_leaves_the_compiled_loop() {
    let program = "class A { public $arr; function core() { $this->arr['no_pack'] = 1; while (1) { $this->arr[] = 1; } } }
        $a = new A; $a->core();";
    for mode in ["force", "exit-fuzz"] {
        let ini = [
            ("jit.mode", mode),
            ("jit.compile", "inline"),
            ("memory_limit", "2M"),
            ("max_execution_time", "10"),
        ];
        let began = std::time::Instant::now();
        let served = crate::testing::serve::<crate::testing::Lane>(&ini, program);
        let took = began.elapsed();
        let fatal = served.fatal.unwrap_or_default();
        assert!(
            fatal.contains("Allowed memory size of 2097152 bytes exhausted"),
            "{mode}: {fatal}"
        );
        assert!(took.as_secs() < 5, "{mode}: the request took {took:?}");
    }
}

/// The engine's list gives `lower` one wrapper address per kernel, each
/// its own.
#[test]
fn the_symbols_name_one_wrapper_per_kernel() {
    let symbols = wrapped::symbols();
    let mut addresses: Vec<usize> = lower::Kernel::ALL
        .iter()
        .map(|kernel| symbols.address(*kernel))
        .collect();
    assert!(addresses.iter().all(|a| *a != 0));
    addresses.sort_unstable();
    addresses.dedup();
    assert_eq!(addresses.len(), lower::Kernel::ALL.len());
}

/// The bench's two arms leave one tally, so the bar's bytes compare.
#[cfg(feature = "bench")]
#[test]
fn the_bench_arms_leave_one_tally() {
    use super::bench::{Through, calls};
    assert_eq!(calls(Through::Wrapper, 1000), calls(Through::Direct, 1000));
    assert_eq!(
        calls(Through::WrapperBare, 1000),
        calls(Through::Direct, 1000)
    );
    assert!(calls(Through::Direct, 1000).is_ok_and(|t| t != 0));
}

/// Hand-assembled bodies entered through `enter` on real requests
/// (`jit-design.md` §4.6 *Tested*), on the one target whose arena maps
/// code today: a test tier enters, at a function's activation, the body
/// its plan assembles for that function, publishes it into the epoch's
/// table, and hands the loop the frame `enter` landed.
#[cfg(all(target_os = "macos", target_arch = "aarch64", not(miri)))]
pub(crate) mod entered {
    use std::marker::PhantomData;
    use std::panic::{AssertUnwindSafe, catch_unwind};

    use php_engine::tier::{
        self, Extension, ExtensionBuilder, Fatal, Flow, FrameRef, Request, Site, Tier, Under,
    };
    use php_unit::OplineIdx;

    use super::super::{Room, enter, standing, wrapped};
    use crate::lower::{Compiled, Stood};
    use crate::record::Start;
    use crate::table::{self, Outputs};
    use crate::testing::bodies::{
        Numbered, STOOD_CALL, STOOD_OPLINE, a64, answering, calling, compiled, landing,
    };
    use crate::testing::{self, Scratch, function_of, stdout};
    use crate::types::Landing;

    /// What a test enters, by the function whose activation enters it.
    trait Plan: 'static {
        /// The trace entered at the activation of `frame`, which serves
        /// `function`; none to enter nothing.
        fn body(req: &mut Request, frame: FrameRef, function: &[u8]) -> Option<Compiled>;
    }

    /// What the requests' entries left, kept for the test.
    #[derive(Debug, Default)]
    struct Seen {
        /// Each entry's function and the exit it left through.
        exits: Vec<(Vec<u8>, u16)>,
        /// The engine offset of `g`'s op array, once `g` was activated.
        g: Option<u32>,
        /// How many times `f` was activated.
        calls: u32,
    }

    /// The tier that enters its plan's body at an activation.
    struct Entering<P>(PhantomData<P>);

    impl<P: Plan> Tier for Entering<P> {
        fn safepoint(req: &mut Request, frame: FrameRef, site: Site) -> Flow {
            if site != Site::Entry {
                return Flow::Continue;
            }
            let Some(function) = function_of(req, frame) else {
                return Flow::Continue;
            };
            let Some(body) = P::body(req, frame, &function) else {
                return Flow::Continue;
            };
            let policy = testing::policy(&[("opcache.jit_buffer_size", "64K")]);
            let epoch = table::holding(req, &policy, &Outputs::none());
            let Some(table) = table::of_epoch(&epoch) else {
                return Flow::Bail(Fatal::error("no table"));
            };
            let Some(trace) = table
                .publish(body, Start::Enter)
                .ok()
                .and_then(|id| table.get(id))
            else {
                return Flow::Bail(Fatal::error("the body was not published"));
            };
            match enter(req, frame, trace) {
                Ok(exited) => {
                    req.state::<Seen>().exits.push((function, exited.exit.0));
                    Flow::Resume(exited.frame)
                }
                Err(fatal) => Flow::Bail(fatal),
            }
        }

        fn extension() -> Option<Extension> {
            Some(
                ExtensionBuilder::default()
                    .request_state::<Seen>()
                    .build("entering"),
            )
        }
    }

    /// `program` served once by a tier that enters `P`'s bodies, and
    /// what its entries left.
    fn served<P: Plan>(program: &str) -> (String, Seen) {
        let engine = testing::engine::<Entering<P>>(&[]);
        let (served, seen) = Scratch::new().serve_kept::<Seen>(&engine, program);
        (stdout(&served), seen.unwrap_or_default())
    }

    /// The opline of `frame`'s op array whose opcode php names `name`.
    fn opline_named(req: &Request, frame: FrameRef, name: &str) -> Option<u32> {
        let code = tier::frame(req, frame)?.op_array()?;
        code.oplines()
            .find(|op| op.opcode().name() == name)
            .map(|op| op.at().0)
    }

    /// `f`'s body calls the `step` wrapper on its first opline, `echo
    /// "a"`, and answers the opline the `Stood` says the frame goes on
    /// at as its exit: exit 1, which lands at `echo "c"`, past `echo "b"`.
    struct Stepping;

    impl Plan for Stepping {
        fn body(_: &mut Request, _: FrameRef, function: &[u8]) -> Option<Compiled> {
            (function == b"f").then(|| {
                let mut words = calling((wrapped::step as *const ()).addr(), 0, &[], true);
                words.extend([
                    a64::unless_zero(2),
                    a64::load_w0(STOOD_OPLINE),
                    a64::over(1),
                    a64::ABANDONED,
                ]);
                words.extend(a64::EPILOGUE);
                compiled(
                    &words,
                    vec![landing(1, Landing::Plain), landing(2, Landing::Plain)],
                )
            })
        }
    }

    #[test]
    fn a_body_calls_the_step_wrapper_through_its_exit_ctx() {
        let (out, seen) =
            served::<Stepping>(r#"function f() { echo "a"; echo "b"; echo "c"; } f();"#);
        assert_eq!(out, "ac");
        assert_eq!(seen.exits, [(b"f".to_vec(), 1)]);
    }

    /// A kernel that panics.
    fn explode(_: &mut Request, _: Room, _: OplineIdx) -> Result<u64, Fatal> {
        panic!("the kernel panicked")
    }

    /// A kernel that re-enters PHP, calling `inner` from the frame it serves.
    fn reenter(req: &mut Request, room: Room, at: OplineIdx) -> Result<u64, Fatal> {
        let frame = standing(req, room, at)?;
        tier::call(req, Under::Frame(frame), b"inner", &[]);
        Ok(0)
    }

    kernels! { Request => mod over_request { explode() -> u64; reenter() -> u64; } }

    /// The kernel `jit-design.md` §4.9 *Tested* asks for: the
    /// number of the trace that calls it kept, then the opline served
    /// whole as the `step` kernel serves it.
    fn number(req: &mut Request, room: Room, at: OplineIdx) -> Result<Stood, Fatal> {
        let running = tier::trace_num(req).raw();
        req.state::<Numbered>().0.push(running);
        super::super::step(req, room, at)
    }

    kernels! { Request => mod numbered { number() -> Stood; } }

    /// `f`'s body, and `inner`'s, call the panicking kernel; `outer`'s
    /// calls the one that re-enters PHP into `inner`.
    struct Panicking;

    impl Plan for Panicking {
        fn body(_: &mut Request, _: FrameRef, function: &[u8]) -> Option<Compiled> {
            let kernel = match function {
                b"f" | b"inner" => over_request::explode as *const (),
                b"outer" => over_request::reenter as *const (),
                _ => return None,
            };
            let words = answering(calling(kernel.addr(), 0, &[], true), 0);
            Some(compiled(&words, vec![landing(0, Landing::Plain)]))
        }
    }

    /// What reached the request's boundary: the payload of the one panic
    /// that did, or what it wrote where none did.
    fn boundary(program: &str) -> Result<String, String> {
        catch_unwind(AssertUnwindSafe(|| served::<Panicking>(program).0)).map_err(|payload| {
            payload
                .downcast_ref::<&str>()
                .map_or_else(|| "another payload".to_owned(), |s| (*s).to_owned())
        })
    }

    /// A panic in a kernel is caught at its wrapper, kept, and resumed by
    /// `enter` past the code, so it unwinds to the request's boundary.
    #[test]
    fn a_panicking_kernel_is_resumed_at_the_request_boundary() {
        assert_eq!(
            boundary(r#"function f() { echo "f"; } f(); echo "after";"#),
            Err("the kernel panicked".to_owned())
        );
    }

    /// The nested crossing (`jit-design.md` §4.6, *Where `pending`
    /// lives*): `outer`'s kernel re-enters PHP into `inner`, whose trace's
    /// kernel panics; the inner wrapper keeps it, the inner `enter`
    /// resumes it through the nested call and the outer kernel, the outer
    /// wrapper keeps it, and the outer `enter` resumes it: one panic, the
    /// inner payload, at the boundary.
    #[test]
    fn a_panic_in_a_nested_trace_reaches_the_boundary_once() {
        assert_eq!(
            boundary(r#"function inner() { echo "i"; } function outer() { echo "o"; } outer();"#),
            Err("the kernel panicked".to_owned())
        );
    }

    /// `f` stepped by the plan to its `FETCH_DIM_R` over a temporary
    /// array holding the one `D`, and a body that leaves at once through a
    /// `FreeTemps { op1 }` exit past it, as an exit past an inline fetch
    /// does: the landing frees the array, so `D`'s destructor runs there.
    struct Freeing;

    impl Plan for Freeing {
        fn body(req: &mut Request, frame: FrameRef, function: &[u8]) -> Option<Compiled> {
            if function != b"f" {
                return None;
            }
            let fetch = opline_named(req, frame, "ZEND_FETCH_DIM_R")?;
            while tier::frame(req, frame)?.opline().0 < fetch {
                tier::step(req, frame).ok()?;
            }
            let words = [a64::movz(0, 0), a64::EPILOGUE[1]];
            let exit = landing(
                fetch + 1,
                Landing::FreeTemps {
                    op1: true,
                    op2: false,
                },
            );
            Some(compiled(&words, vec![exit]))
        }
    }

    #[test]
    fn a_free_temps_landing_frees_what_the_opline_before_read() {
        let (out, seen) = served::<Freeing>(
            r#"class D { function __destruct() { echo "freed\n"; } }
            function f() { [new D, 1][1]; echo "after\n"; }
            f();"#,
        );
        assert_eq!(out, "freed\nafter\n");
        assert_eq!(seen.exits, [(b"f".to_vec(), 0)]);
    }

    /// `f`'s body calls `push_call` on its `INIT_FCALL` of `g` with the
    /// callee the plan holds for each call: on the first, nothing, the
    /// slot not primed; on the second, `g`'s; on the third, another. It
    /// leaves through exit 0, a `RestoreCall` landing at the `SEND_VAL`
    /// with the call under way, where the call was pushed, and through
    /// exit 1, a plain landing at the `INIT_FCALL`, where it was refused.
    struct Calling;

    impl Plan for Calling {
        fn body(req: &mut Request, frame: FrameRef, function: &[u8]) -> Option<Compiled> {
            if function == b"g" {
                let g = tier::func(req, frame).and_then(super::super::engine_offset);
                req.state::<Seen>().g = g;
                return None;
            }
            if function != b"f" {
                return None;
            }
            let seen = req.state::<Seen>();
            seen.calls += 1;
            let callee = match (seen.calls, seen.g) {
                (2, Some(g)) => g,
                (_, Some(g)) => g.wrapping_add(1000),
                (_, None) => u32::MAX,
            };
            let mut words = calling((wrapped::push_call as *const ()).addr(), 0, &[callee], true);
            words.extend([
                a64::unless_zero(4),
                a64::load_x0(STOOD_CALL),
                a64::if_null(2),
                a64::movz(0, 0),
                a64::over(1),
                a64::movz(0, 1),
            ]);
            words.extend(a64::EPILOGUE);
            Some(compiled(
                &words,
                vec![landing(1, Landing::RestoreCall), landing(0, Landing::Plain)],
            ))
        }
    }

    #[test]
    fn a_restore_call_landing_goes_on_with_the_call_pushed_and_a_refusal_pushes_none() {
        let (out, seen) = served::<Calling>(
            r#"function g($x) { echo "g$x\n"; }
            function f() { g(1); echo "f\n"; }
            f(); f(); f();"#,
        );
        assert_eq!(out, "g1\nf\ng1\nf\ng1\nf\n");
        let f = || b"f".to_vec();
        assert_eq!(seen.exits, [(f(), 1), (f(), 0), (f(), 1)]);
    }
}
