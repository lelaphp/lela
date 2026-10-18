//! The crossing (`jit-design.md` §4.6), so far its kernel half: the
//! [`ExitCtx`] compiled code hands every kernel wrapper, the [`Status`] a
//! wrapper answers with, and [`kernels!`], which expands one wrapper per
//! listed kernel. A wrapper rebuilds the request from the `ExitCtx`, calls
//! the kernel inside `catch_unwind(AssertUnwindSafe(..))`, writes the
//! kernel's value through the out pointer, and maps what came back to a
//! `Status`, a fatal or a panic payload kept in the `ExitCtx` for `enter`
//! to return or resume. Wrappers are `extern "C"` and not `"C-unwind"`:
//! no panic reaches compiled code (`research/llvm-research.r1.md` B.4).
//!
//! `enter` itself, the trampoline and the landing, comes with the first
//! trace; until then the crossing is built for its tests and for the
//! bench bar that measures what the wrapper costs a call that does not
//! unwind (#2742). `ExitCtx` is generic over the request it carries so
//! that both cross it without an engine: every wrapper a trace calls is
//! over `Request`, and the rebuild of `&mut R` from `*mut R` is the same
//! single move whatever `R` is.
#![deny(
    clippy::undocumented_unsafe_blocks,
    clippy::multiple_unsafe_ops_per_block
)]

use std::any::Any;
use std::panic::{AssertUnwindSafe, catch_unwind};

use php_engine::tier::{Fatal, Request};

/// The first argument of every kernel wrapper (`jit-design.md` §4.6): a
/// local of the `enter` that made it, its address held by compiled code
/// for exactly that call. Compiled code reads `req` at offset zero;
/// `pending` has no C layout and is the wrappers' alone. The interrupt
/// pointer at offset eight joins with the first trace that tests it.
#[repr(C)]
pub(crate) struct ExitCtx<R = Request> {
    req: *mut R,
    pending: Option<Pending>,
}

impl<R> ExitCtx<R> {
    /// The `ExitCtx` over `req`, lent for as long as it lives.
    pub(crate) fn new(req: &mut R) -> ExitCtx<R> {
        ExitCtx { req, pending: None }
    }

    /// What a wrapper kept for `enter`, taken: a fatal to return, or a
    /// panic payload to resume.
    pub(crate) fn take_pending(&mut self) -> Option<Pending> {
        self.pending.take()
    }
}

/// What a wrapper caught and compiled code cannot carry.
pub(crate) enum Pending {
    /// The kernel raised a fatal, an `exit` or the memory limit.
    Bailout(Fatal),
    /// The kernel panicked; `enter` resumes the unwind past the code.
    Panic(Box<dyn Any + Send>),
}

/// What a wrapper answers compiled code, which branches on it: on
/// `Continue` the kernel's value is in the out slot, and on any other the
/// trace exits to `enter`, which reads the `ExitCtx`. A throwable left
/// pending joins as its own answer with the first kernel that can raise
/// one.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Status {
    Continue = 0,
    Bailout = 1,
    Panic = 2,
}

/// One kernel call across the crossing: the request rebuilt from `cx`,
/// the kernel called inside `catch_unwind`, its value written to `out`, and
/// what came back mapped to a [`Status`]. Inlined into each wrapper, so a
/// wrapper is one frame around its kernel.
///
/// # Safety
///
/// `cx` is one [`ExitCtx::new`] made, live and not otherwise
/// borrowed for the call, and `out` is valid for one write of a `T`.
#[inline(always)] // jit-design.md §4.6: the wrapper is the one frame between compiled code and the kernel
#[allow(clippy::inline_always)]
pub(crate) unsafe fn crossed<R, T>(
    cx: *mut ExitCtx<R>,
    out: *mut T,
    kernel: impl FnOnce(&mut R) -> Result<T, Fatal>,
) -> Status {
    // SAFETY: the caller hands the live context `enter` made, borrowed by
    // nothing else for the length of this call.
    let cx = unsafe { &mut *cx };
    // SAFETY: `req` came from the `&mut R` the context was made over, which
    // outlives it, and compiled code touches the request only through
    // wrappers, one at a time on its stack.
    let req = unsafe { &mut *cx.req };
    match catch_unwind(AssertUnwindSafe(|| kernel(req))) {
        Ok(Ok(value)) => {
            // SAFETY: the caller hands an out slot valid for one write.
            unsafe { out.write(value) };
            Status::Continue
        }
        Ok(Err(fatal)) => {
            cx.pending = Some(Pending::Bailout(fatal));
            Status::Bailout
        }
        Err(payload) => {
            cx.pending = Some(Pending::Panic(payload));
            Status::Panic
        }
    }
}

/// One `extern "C"` wrapper per listed kernel (`jit-design.md` §4.6,
/// §5.1), in a `mod` the invocation names, over the request type it
/// names: `kernels! { Request => mod wrapped { add_slow(a: &Zval, b:
/// &Zval) -> Zval; } }` writes `wrapped::add_slow(cx, a, b, out) ->
/// Status`, which calls the `add_slow` beside the invocation. The frame
/// and opline arguments join the shape with the first kernel that reads
/// them.
macro_rules! kernels {
    ($request:ty => mod $module:ident { $( $name:ident ( $($arg:ident : $ty:ty),* ) -> $out:ty ; )* }) => {
        pub(crate) mod $module {
            #[allow(unused_imports, reason = "the types a list names resolve beside the invocation, and a list may name none")]
            use super::*;
            $(
                /// The wrapper compiled code calls for the kernel of this name.
                ///
                /// # Safety
                ///
                /// As [`crossed`](crate::enter::crossed): `cx` is live and
                /// unborrowed, and `out` is valid for one write.
                pub(crate) unsafe extern "C" fn $name(
                    cx: *mut $crate::enter::ExitCtx<$request>,
                    $($arg: $ty,)*
                    out: *mut $out,
                ) -> $crate::enter::Status {
                    // SAFETY: this wrapper's own contract, handed on whole.
                    unsafe { $crate::enter::crossed(cx, out, |req| super::$name(req, $($arg),*)) }
                }
            )*
        }
    };
}

/// The stand-in request of the bench and the tests: one word a kernel
/// writes, so a call has an effect neither arm can fold away.
#[derive(Debug, Default)]
pub(crate) struct Tally(u64);

/// The test kernel: one step of a linear congruential generator over the
/// tally, kept out of line so that each arm calls it. It raises a fatal
/// at the last `n`, which no loop of calls reaches, so that it can fail and unwind
/// as a kernel does and the optimiser keeps the wrapper's whole shape: a
/// kernel it could prove never fails nor unwinds would lose both the
/// mapping and the `catch_unwind` the bar measures.
#[inline(never)]
fn tally(t: &mut Tally, n: u64) -> Result<u64, Fatal> {
    if n == u64::MAX {
        return Err(Fatal::error("the tally's last step"));
    }
    t.0 = t.0.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(n);
    Ok(t.0)
}

kernels! { Tally => mod wrapped { tally(n: u64) -> u64; } }

/// The bench bar's program (#2742): the same loop of kernel calls, through
/// the wrapper or straight to the kernel, each through a function pointer
/// the optimiser cannot see through, as compiled code calls a wrapper by
/// its address.
#[cfg(feature = "bench")]
pub mod bench {
    use std::hint::black_box;
    use std::panic::resume_unwind;

    use super::{ExitCtx, Fatal, Pending, Status, Tally, wrapped};

    /// Which way the loop's calls go.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum Through {
        /// Each call through the kernel's `extern "C"` wrapper.
        Wrapper,
        /// Each call to the kernel itself.
        Direct,
    }

    impl Through {
        /// The way an arm's argument names: `wrapper` or `direct`.
        #[must_use]
        pub fn named(name: &str) -> Option<Through> {
            match name {
                "wrapper" => Some(Through::Wrapper),
                "direct" => Some(Through::Direct),
                _ => None,
            }
        }
    }

    /// `n` calls of the test kernel `through` one way, and the tally they
    /// leave, which is the same whichever way they went. A fatal is
    /// returned and a panic resumed, as `enter` answers them, though the
    /// test kernel raises neither.
    pub fn calls(through: Through, n: u64) -> Result<u64, Fatal> {
        let mut tally = Tally::default();
        let mut last = 0;
        match through {
            Through::Wrapper => {
                let wrapper: unsafe extern "C" fn(*mut ExitCtx<Tally>, u64, *mut u64) -> Status =
                    black_box(wrapped::tally);
                let mut cx = ExitCtx::new(&mut tally);
                for i in 0..n {
                    // SAFETY: `cx` is this frame's live context, borrowed by
                    // nothing else, and `last` takes one write.
                    let status = unsafe { wrapper(&raw mut cx, i, &raw mut last) };
                    match (status, cx.take_pending()) {
                        (Status::Continue, _) => {}
                        (_, Some(Pending::Panic(payload))) => resume_unwind(payload),
                        (_, Some(Pending::Bailout(fatal))) => return Err(fatal),
                        (_, None) => {
                            return Err(Fatal::error("a wrapper stopped and kept nothing"));
                        }
                    }
                }
            }
            Through::Direct => {
                let kernel: fn(&mut Tally, u64) -> Result<u64, Fatal> = black_box(super::tally);
                for i in 0..n {
                    last = kernel(&mut tally, i)?;
                }
            }
        }
        Ok(last)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A kernel that raises a fatal.
    fn refuse(_: &mut Tally, why: u64) -> Result<u64, Fatal> {
        Err(Fatal::error(format!("refused {why}")))
    }

    /// A kernel that panics.
    #[allow(clippy::panic, reason = "the panic is what the test crosses")]
    fn explode(_: &mut Tally, _: u64) -> Result<u64, Fatal> {
        panic!("the kernel panicked")
    }

    kernels! { Tally => mod failing { refuse(why: u64) -> u64; explode(n: u64) -> u64; } }

    /// A call that returns writes its value through the out slot, leaves
    /// the request as the kernel left it, and keeps nothing.
    #[test]
    fn a_call_that_returns_continues_with_its_value_in_the_slot() {
        let mut t = Tally(3);
        let mut out = 0;
        let mut cx = ExitCtx::new(&mut t);
        // SAFETY: a live context and a slot on this frame.
        let status = unsafe { wrapped::tally(&raw mut cx, 4, &raw mut out) };
        assert_eq!(status, Status::Continue);
        assert!(cx.take_pending().is_none());
        let expected = 3u64.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(4);
        assert_eq!((out, t.0), (expected, expected));
    }

    /// A fatal is kept for `enter` to return, and the slot is not written.
    #[test]
    fn a_fatal_is_kept_in_the_context_and_answered_as_a_bailout() {
        let mut t = Tally::default();
        let mut out = 7;
        let mut cx = ExitCtx::new(&mut t);
        // SAFETY: a live context and a slot on this frame.
        let status = unsafe { failing::refuse(&raw mut cx, 9, &raw mut out) };
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
        let status = unsafe { failing::explode(&raw mut cx, 1, &raw mut out) };
        assert_eq!(status, Status::Panic);
        assert!(matches!(
            cx.take_pending(),
            Some(Pending::Panic(payload)) if payload.downcast_ref::<&str>() == Some(&"the kernel panicked")
        ));
    }

    /// The bench's two arms leave one tally, so the bar's bytes compare.
    #[cfg(feature = "bench")]
    #[test]
    fn the_bench_arms_leave_one_tally() {
        use bench::{Through, calls};
        assert_eq!(calls(Through::Wrapper, 1000), calls(Through::Direct, 1000));
        assert!(calls(Through::Direct, 1000).is_ok_and(|t| t != 0));
    }
}
