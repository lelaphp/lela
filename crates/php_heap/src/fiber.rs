//! A fiber's machine stack and the switch onto it (design §5.11): what
//! `zend_fiber_stack_allocate` lays down and `zend_fiber_switch_context`
//! jumps to (`Z/Zend/zend_fibers.c:206-280`, `:476`). php's switch is
//! Boost's `jump_fcontext`, the callee-saved registers and the
//! stack pointer and nothing else; `corosensei`'s is the same switch
//! written in Rust, and this file is the one place the engine touches
//! it, because what crosses it is a raw pointer to the request.
//!
//! The switch is asymmetric, as php's use of it is: a fiber is resumed
//! from whatever stack is running and suspends back to exactly that one,
//! so every resume nests inside the one below it and every suspend
//! returns to the resume that is innermost. What the engine swaps beside
//! the machine stack, its VM stack, its frames and its error level, is
//! the engine's to swap on its own side of [`FiberStack::resume`]; this
//! file moves the registers and the one pointer and nothing else.
//!
//! Where a suspend returns to is kept on the request, in [`Suspensions`],
//! because `Fiber::suspend` is reached from deep inside the loop with the
//! request in hand and nothing else: the innermost fiber that is running
//! is the one whose stack the suspend is on, so the list is a stack and
//! its top is the fiber a suspend leaves.

use std::io;
use std::ptr::NonNull;

use corosensei::stack::{DefaultStack, Stack};
use corosensei::{Coroutine, CoroutineResult, Yielder};

use crate::hooks::Fatal;

/// What crosses a switch each way: the request, as the pointer the
/// switch carries, and the transfer. A resume hands a fiber's body the
/// request that resumed it; a suspend hands its own back, so the
/// pointer escapes into the jump as the resume's does, and nothing the
/// suspend read of the request before the jump is taken as still
/// standing after it, when every fiber that ran meanwhile has written
/// it. Private, because turning the pointer back into the request is
/// this file's one reason to be an `unsafe` home.
struct Resume<R: Host>(*mut R, R::Transfer);

/// The one [`Yielder`] shape a fiber of `R` suspends through.
type Suspender<R> = Yielder<Resume<R>, Resume<R>>;

/// The request a fiber runs on, as this crate sees it: what a switch
/// carries each way, and where the running fibers' suspend points are
/// kept, so the fiber a suspend leaves is found from the request alone.
pub trait Host: Sized + 'static {
    /// What a resume hands a fiber and what a suspend or a return hands
    /// back: php's `zend_fiber_transfer`, a value with its flags.
    type Transfer: 'static;
    /// The running fibers' suspend points, the request's own.
    fn suspensions(&mut self) -> &mut Suspensions<Self>;
}

/// The suspend point of every fiber that is running, the innermost last:
/// php's chain of `EG(current_fiber_context)` back through each fiber's
/// `caller`. A fiber's point is pushed when its body starts and each
/// time it is resumed, and taken off when it suspends and when its body
/// returns, so the top is always the fiber whose stack is running.
pub struct Suspensions<R: Host> {
    running: Vec<NonNull<Suspender<R>>>,
}

impl<R: Host> Default for Suspensions<R> {
    fn default() -> Suspensions<R> {
        Suspensions {
            running: Vec::new(),
        }
    }
}

impl<R: Host> std::fmt::Debug for Suspensions<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Suspensions")
            .field("running", &self.running.len())
            .finish()
    }
}

// SAFETY: the points name places on the stacks of fibers the request
// owns, which move with the request, whole, to the thread that serves it
// (design §5.14), and are read on that thread alone.
unsafe impl<R: Host> Send for Suspensions<R> {}

/// Why a stack could not be laid down: php's two `Exception`s out of
/// `zend_fiber_stack_allocate`.
#[derive(Debug)]
pub enum StackRefused {
    /// `fiber.stack_size` below one page and the guard page, the least
    /// php lays down: the least, in bytes.
    TooSmall(usize),
    /// The `mmap` or the `mprotect` failed, with the platform's error.
    Allocate(io::Error),
}

/// How a resume came back: the fiber suspended with a transfer, or its
/// body returned one and the stack is done.
#[derive(Debug)]
pub enum Switched<T> {
    Suspended(T),
    Returned(T),
}

/// One fiber's machine stack with its body on it: `fiber.stack_size`
/// rounded up to a page with one guard page below it
/// (`ZEND_FIBER_GUARD_PAGES`), and the bounds the call-stack limit inside
/// the fiber is built from (`zend_fiber_stack_base`,
/// `zend_fiber_stack_limit`).
pub struct FiberStack<R: Host> {
    coroutine: Coroutine<Resume<R>, Resume<R>, R::Transfer, DefaultStack>,
    base: usize,
    end: usize,
}

impl<R: Host> std::fmt::Debug for FiberStack<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FiberStack")
            .field("base", &self.base)
            .field("end", &self.end)
            .field("done", &self.coroutine.done())
            .finish()
    }
}

// SAFETY: what waits on a suspended fiber's stack is the engine's own
// loop and natives over the request that owns the fiber, and nothing of
// a thread's; the stack moves with that request, whole, to the thread
// that serves it (design §5.14), and is resumed on that thread alone.
unsafe impl<R: Host> Send for FiberStack<R> where R::Transfer: Send {}

/// `zend_fiber_get_page_size`: the platform's page, or php's 4096 where
/// the platform answers none or one that is no power of two.
fn page_size() -> usize {
    // SAFETY: `sysconf` takes one name, reads and writes nothing of
    // ours and answers a plain number.
    let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
    usize::try_from(page)
        .ok()
        .filter(|page| page.is_power_of_two())
        .unwrap_or(4096)
}

impl<R: Host> FiberStack<R> {
    /// `zend_fiber_init_context` over a stack of `size` bytes: the stack
    /// mapped with its guard page and `body` laid on it, to start on the
    /// first [`FiberStack::resume`] with the request and the transfer
    /// that resume sends. Nothing runs until then.
    pub fn new<F>(size: usize, body: F) -> Result<FiberStack<R>, StackRefused>
    where
        F: FnOnce(&mut R, R::Transfer) -> R::Transfer + 'static,
    {
        let page = page_size();
        let least = page.saturating_mul(2);
        if size < least {
            return Err(StackRefused::TooSmall(least));
        }
        let stack = DefaultStack::new(size).map_err(StackRefused::Allocate)?;
        let base = stack.base().get();
        // `stack->pointer`: the lowest usable address, the guard page
        // being the one below it.
        let end = stack.limit().get().saturating_add(page);
        let coroutine =
            Coroutine::with_stack(stack, move |suspender: &Suspender<R>, first: Resume<R>| {
                let Resume(host, transfer) = first;
                // SAFETY: `host` is the request `resume` was handed as
                // `&mut R` and holds borrowed for as long as this body
                // runs before it suspends or returns, so nothing else
                // names it meanwhile. Across a suspend the reference
                // waits unused on this stack, as every frame of the loop
                // above it waits, and `suspend` returns only into a
                // resume that was handed the same request again, which
                // it asserts by address; the switch is a jump the
                // compiler cannot see through, as a call to an unknown
                // function handed the pointer would be.
                let host = unsafe { &mut *host };
                host.suspensions().running.push(NonNull::from(suspender));
                let out = body(host, transfer);
                host.suspensions().running.pop();
                out
            });
        Ok(FiberStack {
            coroutine,
            base,
            end,
        })
    }

    /// `zend_fiber_stack_base`: the high address, where the stack starts.
    #[must_use]
    pub fn base(&self) -> usize {
        self.base
    }

    /// `stack->pointer`: the lowest address the fiber may use, above the
    /// guard page; `zend_fiber_stack_limit` is this plus the reserve.
    #[must_use]
    pub fn end(&self) -> usize {
        self.end
    }

    /// `zend_fiber_switch_context` onto this stack: the body runs, from
    /// its start or from the suspend it left at, with `host` and
    /// `transfer`, until it suspends or returns. A stack whose body has
    /// returned is the engine's error and never a program's.
    pub fn resume(
        &mut self,
        host: &mut R,
        transfer: R::Transfer,
    ) -> Result<Switched<R::Transfer>, Fatal> {
        if self.coroutine.done() {
            return Err(Fatal::error(
                "InternalError: a fiber resumed after its body returned",
            ));
        }
        let depth = host.suspensions().running.len();
        let switched = self
            .coroutine
            .resume(Resume(std::ptr::from_mut(host), transfer));
        // Every fiber the body resumed has suspended or returned by now,
        // and the body itself has taken its point off, so the list is as
        // this resume found it.
        host.suspensions().running.truncate(depth);
        Ok(match switched {
            CoroutineResult::Yield(Resume(_, back)) => Switched::Suspended(back),
            CoroutineResult::Return(back) => Switched::Returned(back),
        })
    }
}

/// `zend_fiber_suspend_internal`'s switch: the innermost running fiber
/// suspends with `transfer`, its resume returns, and this returns what
/// the next resume sends. Called from the fiber's own stack with the
/// request it runs on; with no fiber running it is the engine's error,
/// since the native asks php's question first.
pub fn suspend<R: Host>(host: &mut R, transfer: R::Transfer) -> Result<R::Transfer, Fatal> {
    let Some(point) = host.suspensions().running.pop() else {
        return Err(Fatal::error(
            "InternalError: a suspend with no fiber running",
        ));
    };
    let expected = std::ptr::from_mut(host);
    // SAFETY: `point` was pushed by the body of the innermost running
    // fiber, whose stack this call is on, and the `Yielder` it names
    // lives on that stack for as long as the body runs; it is taken off
    // before the body returns, so a point on the list names a live one.
    let suspender = unsafe { point.as_ref() };
    let Resume(resumer, back) = suspender.suspend(Resume(expected, transfer));
    host.suspensions().running.push(point);
    if resumer != expected {
        return Err(Fatal::error(
            "InternalError: a fiber resumed on a request it was not started on",
        ));
    }
    Ok(back)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A request with a log, the transfers being numbers.
    #[derive(Default)]
    struct Probe {
        suspensions: Suspensions<Probe>,
        log: Vec<i64>,
    }

    impl Host for Probe {
        type Transfer = i64;
        fn suspensions(&mut self) -> &mut Suspensions<Probe> {
            &mut self.suspensions
        }
    }

    const SIZE: usize = 256 * 1024;

    #[test]
    #[cfg_attr(
        miri,
        ignore = "corosensei maps a fiber's stack with a guard page Miri's `mmap` does not take, and switches onto it in assembly Miri does not run"
    )]
    fn a_body_suspends_and_returns_in_the_order_it_is_resumed() -> Result<(), Fatal> {
        let mut probe = Probe::default();
        let Ok(mut fiber) = FiberStack::new(SIZE, |probe: &mut Probe, first| {
            probe.log.push(first);
            let second = suspend(probe, 10).unwrap_or(-1);
            probe.log.push(second);
            second * 3
        }) else {
            return Err(Fatal::error("no stack"));
        };
        assert!(fiber.base() > fiber.end());
        assert!(matches!(
            fiber.resume(&mut probe, 1)?,
            Switched::Suspended(10)
        ));
        assert!(matches!(
            fiber.resume(&mut probe, 2)?,
            Switched::Returned(6)
        ));
        assert!(fiber.resume(&mut probe, 3).is_err());
        assert_eq!(probe.log, vec![1, 2]);
        assert!(probe.suspensions.running.is_empty());
        Ok(())
    }

    #[test]
    #[cfg_attr(
        miri,
        ignore = "corosensei maps a fiber's stack with a guard page Miri's `mmap` does not take, and switches onto it in assembly Miri does not run"
    )]
    fn a_suspend_leaves_the_innermost_fiber_and_nothing_below_it() -> Result<(), Fatal> {
        let mut probe = Probe::default();
        let Ok(mut outer) = FiberStack::new(SIZE, |probe: &mut Probe, _| {
            let Ok(mut inner) = FiberStack::new(SIZE, |probe: &mut Probe, _| {
                probe.log.push(20);
                suspend(probe, 21).ok();
                probe.log.push(22);
                23
            }) else {
                return -1;
            };
            inner.resume(probe, 0).ok();
            probe.log.push(11);
            suspend(probe, 12).ok();
            inner.resume(probe, 0).ok();
            probe.log.push(13);
            14
        }) else {
            return Err(Fatal::error("no stack"));
        };
        assert!(matches!(
            outer.resume(&mut probe, 0)?,
            Switched::Suspended(12)
        ));
        assert!(matches!(
            outer.resume(&mut probe, 0)?,
            Switched::Returned(14)
        ));
        assert_eq!(probe.log, vec![20, 11, 22, 13]);
        assert!(suspend(&mut probe, 0).is_err());
        Ok(())
    }

    #[test]
    fn a_stack_below_two_pages_is_refused() {
        let refused = FiberStack::new(1, |_: &mut Probe, first| first);
        assert!(matches!(refused, Err(StackRefused::TooSmall(least)) if least == page_size() * 2));
    }
}
