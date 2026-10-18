//! `Fiber` and `FiberError` as `zend_fibers.stub.php` declares them, and
//! the switch between a fiber's stack and the stack that resumed it
//! (`Z/Zend/zend_fibers.c` at 8.5.9; design §5.11).
//!
//! A fiber runs its callable on a machine stack of its own,
//! [`php_heap::FiberStack`], the engine's one loop entered again on it
//! through the same `zend_call_function` a native's callback takes: the
//! whole nested execution, native frames with it, stands on the fiber's
//! stack while it runs and waits there while it is suspended. What the
//! switch swaps beside the machine stack is `zend_fiber_vm_state`'s
//! list under this engine's names, the engine's execution state and
//! nothing an extension or the tier owns: the VM stack the request runs
//! on, which carries the current frame and the entry slot with it, the
//! frames' engine half, the error-reporting level and the call-stack
//! limit ([`Execution`]). The swap is made on the resumer's side of the
//! machine switch, before the jump and after it, as
//! `zend_fiber_switch_context` captures and restores its state there, so
//! no save of "the entry frame" is written anywhere: two fibers are two
//! `execute` nestings and two slots.
//!
//! A frame on a fiber's stack is a frame and has no word of its own:
//! which fiber it belongs to is a fact about the stack it is on. A switch
//! happens inside `Fiber::start`, `resume`, `throw` or `suspend`, a native
//! the loop reports as `Same` like any other, so nothing in the loop sees
//! a frame chain change under it. The interrupt is the request's and is
//! not swapped: a timeout inside a fiber is raised at the fiber's next
//! safepoint, on the fiber's stack, and leaves it as the `Bailout` its
//! resumer propagates, which is php's `ZEND_FIBER_TRANSFER_FLAG_BAILOUT`
//! without the `longjmp`.
//!
//! The one piece of `zend_fibers.c` not here is the graceful exit a
//! suspended fiber is destroyed with: php resumes the fiber with
//! `GracefulExit`, which no `catch` matches and every `finally` block
//! runs for, and this engine's unwinder has no such throwable yet. A
//! suspended fiber is torn down instead ([`destroy`]): every owner its
//! frames hold is given up innermost first, so what they held is
//! destructed where php destructs it, and its `finally` blocks are
//! never entered.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use std::any::Any;

use php_compile::StackLimit;
use php_heap::{Block, FiberStack, Host, StackRefused, Suspensions, Switched};
use php_macros::php_class;

use crate::api::abi::{
    Callable, DtorObj, GcTable, Instance, NamedVariadic, ObjectHandlers, Refusals, This,
};
use crate::api::call::call_named_made;
use crate::api::raising::{Thrown, throw, throw_error};
use crate::api::request::{ObservedChain, Raised, Request};
use crate::api::values::{Owner, PhpObject, PhpString, Value};
use crate::classes::{Error, Throwable};
use crate::exec::{self, Vm};
use crate::observer::{AsFiber, Caller, FiberStatus, ObservedFiber};

/// `sizeof(zend_fiber_stack)` on a 64-bit build without Valgrind or
/// sanitizers: the pointer and the size `zend_fiber_stack_allocate`
/// `emalloc`s beside the mapping.
const STACK_RECORD: usize = 16;

/// `ZEND_FIBER_DEFAULT_C_STACK_SIZE` on a 64-bit target, what a fiber's
/// machine stack is where `fiber.stack_size` is unset
/// (`Z/Zend/zend.c:244-249`).
const DEFAULT_STACK: usize = 4096 * 512;

/// `zend_fiber_status`, with php's `caller` folded in: a coroutine php
/// marks `SUSPENDED` because it resumed another, or because a suspend of
/// a coroutine below it passed through it, while its `caller` still
/// stands, is `Running` here, which is how `Fiber::isRunning` and
/// `isSuspended` read the pair and how `_ZendTestFiber::resume` refuses a
/// coroutine that is not suspended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CoroutineStatus {
    /// Not started: no stack laid down yet.
    #[default]
    Init,
    /// Running, or waiting on a coroutine it resumed or one a suspend
    /// passed through it to.
    Running,
    /// Suspended, to be resumed.
    Suspended,
    /// Returned, threw or ended in a fatal; its stack given back.
    Dead,
}

/// php's `coroutine->kind`, as far as the engine reads it: a `Fiber`'s,
/// `zend_ce_fiber`, or an extension's own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Kind {
    /// An extension's coroutine, as `ext/zend_test`'s `_ZendTestFiber`
    /// lays one: its bottom frame names no resumer's, and resuming it
    /// moves no `EG(active_fiber)`.
    #[default]
    Extension,
    /// A `Fiber`'s: its bottom frame names its resumer's, and it is
    /// `EG(active_fiber)` while a resume runs it.
    Fiber,
}

/// `zend_fiber_context` (`Z/Zend/zend_fibers.h:76-100`): a machine stack
/// of its own the coroutine's body runs on, laid down by
/// [`Coroutine::init`], resumed, suspended and destroyed through the
/// engine's one switch ([`switch_into`]), with the engine's execution
/// state swapped as a `Fiber`'s is and the fiber observers told of each
/// switch. A `Fiber` is one; an extension lays one of its own in an
/// object's payload, which answers it through [`HoldsCoroutine`].
///
/// It carries php's `ZEND_FIBER_FLAG_*` with it, which php keeps on
/// `zend_fiber` and `zend_test_fiber` alike.
#[derive(Default)]
pub struct Coroutine {
    status: CoroutineStatus,
    kind: Kind,
    /// `ZEND_FIBER_FLAG_THREW`: the body left a throwable or the unwind
    /// exit.
    threw: bool,
    /// `ZEND_FIBER_FLAG_BAILOUT`: the body ended in a fatal.
    bailout: bool,
    /// `ZEND_FIBER_FLAG_DESTROYED`: the coroutine is being torn down.
    destroyed: bool,
    /// The machine stack while the coroutine is suspended, and before it
    /// starts; none while it runs, since the resume that is running it
    /// holds it, and none once it is dead.
    stack: Option<FiberStack<Request>>,
    /// The coroutine's own execution state while it is off the running
    /// chain: laid down, suspended, or waiting on a suspend that passed
    /// through it ([`run_on`]); the request holds it while it runs.
    execution: Option<Box<Execution>>,
    /// Where php's `fiber->execute_data` was last written, which a read
    /// of a coroutine waiting on a suspend that passed through it reads
    /// ([`view`]).
    execute_data: ExecuteData,
    /// `previous`: the coroutine a resume enters, the innermost one
    /// standing when this one suspended, where that was another one a
    /// suspend passed through.
    previous: Option<PhpObject>,
    /// `transfer->context` of the body's last switch where it names a
    /// coroutine other than `caller`: the one the body ended into
    /// ([`CoroutineEnd::Into`]), written as the body returns and taken by
    /// the switch that ran it ([`switch_into`]), which runs that one in
    /// its place.
    into: Option<PhpObject>,
    /// `caller`, which the fiber observers read
    /// ([`ObservedFiber::resumed_by`]).
    caller: Option<Caller>,
    /// The `emalloc` php makes for a laid-down stack beside its VM
    /// stack's pages, the stack record, counted by the heap as php's is.
    record: Option<Block>,
}

/// Where php's `fiber->execute_data` points, which `zend_fiber_execute`
/// writes at the start, `zend_fiber_suspend_internal` at the fiber's own
/// suspend, and `zend_fiber_resume_internal` when the fiber, active,
/// resumes another (`Z/Zend/zend_fibers.c:590`, `:685`, `:707`). A
/// suspend that passes through the fiber writes it nowhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum ExecuteData {
    /// The bottom frame the start laid down, written by nothing since.
    #[default]
    Bottom,
    /// The frame standing innermost on the fiber's stack when its own
    /// suspend or its resume of another fiber wrote it.
    Innermost,
}

/// php's `zend_fiber_context *`: a coroutine reached through the object
/// whose payload holds it, a `Fiber`'s or an extension's, whatever that
/// payload's type is.
#[derive(Clone, Copy)]
pub(crate) struct Context {
    object: PhpObject,
    /// [`HoldsCoroutine::coroutine`] over the payload's type.
    held: fn(&mut Request, PhpObject) -> Option<&mut Coroutine>,
}

impl Context {
    /// The coroutine `object`'s payload, a `P`, holds.
    fn of<P: HoldsCoroutine>(object: PhpObject) -> Context {
        Context {
            object,
            held: held_by::<P>,
        }
    }

    /// The object that holds the coroutine.
    pub(crate) fn object(self) -> PhpObject {
        self.object
    }

    /// The coroutine; none where the object holds it no more.
    pub(crate) fn coroutine(self, req: &mut Request) -> Option<&mut Coroutine> {
        (self.held)(req, self.object)
    }

    /// Whether this is `object`'s coroutine.
    fn is(self, object: PhpObject) -> bool {
        self.object.index() == object.index()
    }
}

impl std::fmt::Debug for Context {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("Context").field(&self.object).finish()
    }
}

/// The coroutine `object`'s payload, a `P`, holds: what a [`Context`]
/// reaches through.
fn held_by<P: HoldsCoroutine>(req: &mut Request, object: PhpObject) -> Option<&mut Coroutine> {
    object.native_mut::<P>(req).map(P::coroutine)
}

/// An object's payload that holds a [`Coroutine`]: how the engine
/// reaches the coroutine through the object across a switch, as php
/// reaches `zend_fiber_context` inside the object that embeds it.
pub trait HoldsCoroutine: Any + Send {
    /// The coroutine the payload holds.
    fn coroutine(&mut self) -> &mut Coroutine;
}

/// How an extension's coroutine body ends, the last switch
/// `zend_fiber_trampoline` makes: back to what resumed it, with a value;
/// or into another coroutine, which `transfer->context` names, laid down
/// in the same object's payload type ([`Coroutine::init`]) and not
/// started, as `zend_test_fiber_execute` transfers into a `pipeTo`
/// target. That one runs in the ending coroutine's place, resumed by what
/// resumed it, and what it hands back is what that resume returns; the
/// fiber observers see the one switch from the dead coroutine into it,
/// then the dead one's destroy.
#[derive(Debug)]
pub enum CoroutineEnd {
    /// The body returned this value.
    Return(Owner<Value>),
    /// The body transfers into the coroutine this object's payload holds.
    Into(PhpObject),
}

/// The coroutine `object`'s payload holds, which its class always lays;
/// an object without it is the engine's fatal.
fn coroutine_of<P: HoldsCoroutine>(
    req: &mut Request,
    object: PhpObject,
) -> Result<&mut Coroutine, Thrown> {
    object.native_mut_or_fatal::<P>(req).map(P::coroutine)
}

/// What one switch carries, `zend_fiber_transfer`: a value, one owner
/// the other side takes; a throwable the other side raises
/// (`ZEND_FIBER_TRANSFER_FLAG_ERROR`), or the unwind exit an `exit`
/// left standing, which rides the request; the graceful exit
/// `zend_fiber_object_destroy` resumes a suspended fiber with, which
/// the suspend raises; or the `Bailout` of a fatal the fiber already
/// recorded (`ZEND_FIBER_TRANSFER_FLAG_BAILOUT`).
#[derive(Debug)]
pub enum Transfer {
    Value(Value),
    Throw(Thrown),
    GracefulExit,
    Fatal,
}

/// `zend_fiber_vm_state` under this engine's names: what a switch
/// swaps, the stack the request runs on with the frames' engine half,
/// the error-reporting level, the call-stack limit and
/// `EG(active_fiber)`; and the chain of the stack's open observed
/// frames, which php keeps on the `zend_fiber_context` as
/// `top_observed_frame` and swaps at the switch's notice
/// ([`crate::observer`]).
#[derive(Default)]
pub(crate) struct Execution {
    pub(crate) vm: Vm,
    pub(crate) observing: ObservedChain,
    pub(crate) error_reporting: i64,
    pub(crate) stack_limit: StackLimit,
    /// `EG(active_fiber)` as the stack left it, which the switch back
    /// onto it restores: so a suspend that passes through a `Fiber` to
    /// a coroutine below it comes back to the fiber that stack's resumer
    /// had active, and not to the `Fiber` it passed through.
    pub(crate) active: Option<PhpObject>,
}

/// One coroutine that is running: the object holding it, a `Fiber` or an
/// extension's, php's `EG(current_fiber_context)` while it is the last,
/// and the execution state of the stack that resumed it, which its
/// `stack_bottom->prev_execute_data` names.
struct Running {
    fiber: Context,
    below: Execution,
    by: Resumer,
}

/// What resumed a running coroutine, which is whether php's
/// `stack_bottom->prev_execute_data` names the resumer's frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Resumer {
    /// `zend_fiber_resume`, under `start`, `resume` and `throw`: the
    /// native's frame is the bottom's `prev_execute_data`, and the chain
    /// runs on into the resumer's.
    Native,
    /// `zend_fiber_object_destroy`, whose `zend_fiber_resume_internal`
    /// writes no `prev_execute_data`, or any resume of an extension's
    /// coroutine, whose bottom frame `zend_test_fiber_execute` zeroes: the
    /// chain ends at the coroutine, and a trace inside it closes with
    /// `{main}` after its callable.
    Unlinked,
}

impl Resumer {
    /// What resumes a coroutine of `kind` with `transfer`.
    fn of(kind: Kind, transfer: &Transfer) -> Resumer {
        match (kind, transfer) {
            (Kind::Fiber, Transfer::Value(_) | Transfer::Throw(_) | Transfer::Fatal) => {
                Resumer::Native
            }
            (Kind::Fiber, Transfer::GracefulExit) | (Kind::Extension, _) => Resumer::Unlinked,
        }
    }
}

/// The request's fibers: the running ones, innermost last, and their
/// suspend points; `EG(active_fiber)`; and `EG(fiber_stack_size)` once
/// `ini_set()` has moved it.
#[derive(Default)]
pub(crate) struct Fibers {
    suspensions: Suspensions<Request>,
    running: Vec<Running>,
    /// `EG(active_fiber)`, which a resume sets to the fiber before the
    /// switch's notice and gives back to the one it was only once the
    /// switch back has returned (`zend_fiber_resume_internal`,
    /// `Z/Zend/zend_fibers.c:680-696`), so both notices of a resume read
    /// the fiber; the switch itself swaps it with the rest of the
    /// execution ([`Execution::active`]), as `zend_fiber_vm_state` keeps
    /// it.
    pub(crate) active: Option<PhpObject>,
    stack_size: Option<usize>,
    gc: GcFiber,
    /// `zend_fiber_switch_blocking`: how many of php's
    /// `zend_fiber_switch_block()` stand unmatched, each a caller a fiber
    /// switch would break (a `ZEND_TICKS` round, a signal dispatch),
    /// while which no fiber starts, suspends or resumes.
    switch_blocking: u32,
    /// What each serve on a stack off the running one set aside, the
    /// last set aside last ([`Fibers::set_aside_above`],
    /// [`Fibers::set_aside_for`]).
    aside: Vec<Aside>,
}

/// What one serve on a stack off the running one set aside, for the
/// request to serve on that stack meanwhile.
enum Aside {
    /// A resumer's stack ([`Request::on_stack`]): the running coroutines
    /// above it, innermost last, the first of which held its execution
    /// and holds the one the request ran on in its place.
    Resumer { running: Vec<Running> },
    /// A held coroutine's stack ([`Request::on_held_stack`]): every
    /// running coroutine, the coroutine served, the execution the
    /// request ran on, which no running coroutine's `below` holds, and
    /// the held stacks the chain runs on into below the coroutine's,
    /// innermost first ([`take_chain`]).
    Suspended {
        running: Vec<Running>,
        coroutine: Context,
        displaced: Box<Execution>,
        below: Vec<HeldStack>,
    },
}

/// A coroutine off the running chain, and the execution it held, which
/// a serve took out to stand on the chain below the stack it serves on
/// ([`take_chain`]).
pub(crate) struct HeldStack {
    pub(crate) coroutine: Context,
    pub(crate) execution: Box<Execution>,
}

/// The executions php's `prev_execute_data` chain runs through from a
/// frame of `top`, a coroutine off the running chain, each taken out of
/// its coroutine for a serve on `top`'s ([`Request::on_held_stack`]):
/// `top`'s own, then, while the coroutine reached waits on a suspend
/// that passed through it with its bottom frame naming its resumer's,
/// that resumer's, which a suspend holds too. The chain ends at a
/// suspended coroutine's bottom, where `zend_fiber_suspend` cut it, and
/// at one whose bottom names no resumer. None where `top` holds no
/// execution with a frame in it.
pub(crate) fn take_chain(
    req: &mut Request,
    top: Context,
) -> Option<(Box<Execution>, Vec<HeldStack>)> {
    let coroutine = top.coroutine(req)?;
    let execution = coroutine.take_held()?;
    let mut next = coroutine.linked_resumer();
    let mut below = Vec::new();
    while let Some(resumer) = next {
        let Some(held) = resumer.coroutine(req) else {
            break;
        };
        let Some(execution) = held.take_held() else {
            break;
        };
        next = held.linked_resumer();
        below.push(HeldStack {
            coroutine: resumer,
            execution,
        });
    }
    Some((execution, below))
}

/// Where the request's own VM stack stands, which
/// [`Fibers::request_stack`] reads and writes.
#[derive(Clone, Copy)]
enum Own {
    /// The stack the request runs on.
    Running,
    /// Below the first running coroutine.
    BelowRunning,
    /// Below the first coroutine the serve at this place in
    /// [`Fibers::aside`] set aside.
    BelowAside(usize),
    /// The execution the serve at this place displaced.
    Displaced(usize),
}

/// The stack below the first of `running`, the one that resumed it.
fn first_below(running: &[Running]) -> Option<&crate::exec::Stack> {
    running.first().map(|first| &first.below.vm.stack)
}

/// [`first_below`], to write.
fn first_below_mut(running: &mut [Running]) -> Option<&mut crate::exec::Stack> {
    running.first_mut().map(|first| &mut first.below.vm.stack)
}

/// [`Own`] where no coroutine runs and `aside` stands: the stack the
/// request runs on unless the innermost serve is on a suspended
/// coroutine's stack, where it is wherever it stood when that serve
/// began.
fn own_in(aside: &[Aside]) -> Own {
    let Some((last, before)) = aside.split_last() else {
        return Own::Running;
    };
    match last {
        Aside::Resumer { .. } => Own::Running,
        Aside::Suspended { running, .. } if !running.is_empty() => Own::BelowAside(before.len()),
        Aside::Suspended { .. } => match own_in(before) {
            Own::Running => Own::Displaced(before.len()),
            own => own,
        },
    }
}

/// `GC_G(dtor_fiber)` and the walk it serves (`Z/Zend/zend_gc.c:1859-1990`,
/// `:2321-2354`): the collector's fiber for destructors, one owner while
/// it is the collector's; whether it is serving the walk, which a
/// destructor that suspended it leaves set; and where the walk stands
/// in the root buffer and where it ends.
#[derive(Default)]
struct GcFiber {
    fiber: Option<PhpObject>,
    running: bool,
    at: u32,
    end: u32,
}

impl Fibers {
    /// `zend_fiber_switch_block()`, as
    /// [`Request::with_fiber_switch_blocked`] takes it.
    pub(crate) fn block_switch(&mut self) {
        self.switch_blocking = self.switch_blocking.saturating_add(1);
    }

    /// `zend_fiber_switch_unblock()`.
    pub(crate) fn unblock_switch(&mut self) {
        self.switch_blocking = self.switch_blocking.saturating_sub(1);
    }

    /// `zend_fiber_switch_blocked()`.
    pub(crate) fn switch_blocked(&self) -> bool {
        self.switch_blocking > 0
    }

    /// `EG(active_fiber)`: the fiber a resume is running, none on the
    /// request's own stack. Running on a resumer's stack to serve a
    /// frame there leaves it as it stands, as php moves no
    /// `EG(active_fiber)` for a debugger's evaluation.
    pub(crate) fn active(&self) -> Option<PhpObject> {
        self.active
    }

    /// Whether the request runs on a fiber coroutine's stack, or serves a
    /// frame below one or on a suspended one ([`Fibers::set_aside_above`],
    /// [`Fibers::set_aside_for`]): php's `EG(current_execute_data)` is
    /// never null there, the coroutine's bottom frame standing below all
    /// it runs.
    pub(crate) fn on_context_stack(&self) -> bool {
        !self.running.is_empty() || !self.aside.is_empty()
    }

    /// `EG(current_fiber_context)`: the coroutine running innermost, a
    /// `Fiber`'s or an extension's, none on the request's own stack.
    fn current(&self) -> Option<Context> {
        self.running.last().map(|running| running.fiber)
    }

    /// The coroutine whose stack is the `stack`-th down the chain
    /// [`Request::executions`] walks, the running one zero: the running
    /// coroutine that many below the innermost, and past them the chain's
    /// base, the suspended coroutine a serve runs on, or else the
    /// request's own stack, which is none.
    pub(crate) fn coroutine_on(&self, stack: usize) -> Option<Context> {
        match self.running.len().checked_sub(stack) {
            Some(0) | None => match self.aside.last() {
                Some(Aside::Suspended {
                    coroutine, below, ..
                }) => match stack.saturating_sub(self.running.len()).checked_sub(1) {
                    None => Some(*coroutine),
                    Some(at) => below.get(at).map(|held| held.coroutine),
                },
                Some(Aside::Resumer { .. }) | None => None,
            },
            Some(above) => self.running.get(above - 1).map(|running| running.fiber),
        }
    }

    /// The running fibers above the `stack`-th stack down the chain
    /// [`Fibers::below`] walks set aside, the first of them holding
    /// the execution standing on that stack, which is handed out for
    /// the request to serve on in place of its own; `None` where no stack
    /// stands there. [`Fibers::put_back`] undoes it.
    pub(crate) fn set_aside_above(&mut self, stack: usize) -> Option<&mut Execution> {
        if stack == 0 || stack > self.below().count() {
            return None;
        }
        let base = self.running.len().checked_sub(stack)?;
        let running = self.running.split_off(base);
        self.aside.push(Aside::Resumer { running });
        self.set_aside_execution()
    }

    /// The execution [`Fibers::set_aside_above`] handed out, to be
    /// handed back.
    pub(crate) fn set_aside_execution(&mut self) -> Option<&mut Execution> {
        match self.aside.last_mut() {
            Some(Aside::Resumer { running }) => running.first_mut().map(|first| &mut first.below),
            Some(Aside::Suspended { .. }) | None => None,
        }
    }

    /// The fibers [`Fibers::set_aside_above`] set aside running again,
    /// above the ones running below them.
    pub(crate) fn put_back(&mut self) {
        if let Some(Aside::Resumer { running }) = self
            .aside
            .pop_if(|aside| matches!(aside, Aside::Resumer { .. }))
        {
            self.running.extend(running);
        }
    }

    /// Every running coroutine set aside while the request serves on the
    /// stack of `coroutine`, held off the running chain, `displaced` the
    /// execution it ran on and `below` the held stacks the chain runs on
    /// into. [`Fibers::take_back`] undoes it.
    pub(crate) fn set_aside_for(
        &mut self,
        coroutine: Context,
        displaced: Box<Execution>,
        below: Vec<HeldStack>,
    ) {
        let running = std::mem::take(&mut self.running);
        self.aside.push(Aside::Suspended {
            running,
            coroutine,
            displaced,
            below,
        });
    }

    /// [`Fibers::set_aside_for`] undone: the coroutines set aside running
    /// again, and the coroutine served, the execution displaced and the
    /// held stacks below handed back.
    pub(crate) fn take_back(&mut self) -> Option<(Context, Box<Execution>, Vec<HeldStack>)> {
        match self
            .aside
            .pop_if(|aside| matches!(aside, Aside::Suspended { .. }))?
        {
            Aside::Suspended {
                running,
                coroutine,
                displaced,
                below,
            } => {
                self.running = running;
                Some((coroutine, displaced, below))
            }
            Aside::Resumer { .. } => None,
        }
    }

    /// Where the request's own stack stands.
    fn own(&self) -> Own {
        if self.running.is_empty() {
            own_in(&self.aside)
        } else {
            Own::BelowRunning
        }
    }

    /// The request's own VM stack, below every fiber running, which is
    /// the one `EG(symbol_table)`'s attached CVs stand on; none while the
    /// request runs on it.
    pub(crate) fn request_stack(&self) -> Option<&crate::exec::Stack> {
        let below = first_below;
        match self.own() {
            Own::Running => None,
            Own::BelowRunning => below(&self.running),
            Own::BelowAside(at) => match self.aside.get(at)? {
                Aside::Suspended { running, .. } | Aside::Resumer { running } => below(running),
            },
            Own::Displaced(at) => match self.aside.get(at)? {
                Aside::Suspended { displaced, .. } => Some(&displaced.vm.stack),
                Aside::Resumer { .. } => None,
            },
        }
    }

    /// [`Fibers::request_stack`], to write.
    pub(crate) fn request_stack_mut(&mut self) -> Option<&mut crate::exec::Stack> {
        let below = first_below_mut;
        match self.own() {
            Own::Running => None,
            Own::BelowRunning => below(&mut self.running),
            Own::BelowAside(at) => match self.aside.get_mut(at)? {
                Aside::Suspended { running, .. } | Aside::Resumer { running } => below(running),
            },
            Own::Displaced(at) => match self.aside.get_mut(at)? {
                Aside::Suspended { displaced, .. } => Some(&mut displaced.vm.stack),
                Aside::Resumer { .. } => None,
            },
        }
    }

    /// The execution of the stack that resumed the innermost running
    /// coroutine, its entry off the list: what the switch back from it
    /// runs on again.
    fn switch_back(&mut self) -> Execution {
        self.running
            .pop()
            .map_or_else(Execution::default, |running| running.below)
    }

    /// The execution states below the running stack, the innermost
    /// resumer's first and the request's own last: the rest of php's
    /// one `prev_execute_data` chain, which ends at a fiber a destroy
    /// resumed. Serving on a held coroutine's stack, no coroutine runs,
    /// and the chain runs on through the held stacks under it.
    pub(crate) fn below(&self) -> impl Iterator<Item = &Execution> {
        let held = match (self.running.is_empty(), self.aside.last()) {
            (true, Some(Aside::Suspended { below, .. })) => below.as_slice(),
            _ => &[],
        };
        self.running
            .iter()
            .rev()
            .take_while(|running| running.by == Resumer::Native)
            .map(|running| &running.below)
            .chain(held.iter().map(|held| &*held.execution))
    }
}

impl Host for Request {
    type Transfer = Transfer;

    fn suspensions(&mut self) -> &mut Suspensions<Request> {
        &mut self.fibers_mut().suspensions
    }
}

/// `zend_fiber`, the payload a `Fiber` object carries (design §5.4).
#[derive(Default)]
pub(crate) struct ZendFiber {
    coroutine: Coroutine,
    /// `fci` and `fci_cache`, the callable one owner the fiber holds
    /// until its body is done with it.
    callable: Option<Callable>,
    /// `fiber->result`, one owner, `null` until the callable returns.
    result: Value,
}

impl HoldsCoroutine for ZendFiber {
    fn coroutine(&mut self) -> &mut Coroutine {
        &mut self.coroutine
    }
}

/// `zend_fiber_handlers`: `dtor_obj`, `free_obj` and `get_gc` php's,
/// `clone_obj` null.
struct FiberHandlers;

impl ObjectHandlers for FiberHandlers {
    /// `zend_fiber_object_destroy`: a suspended fiber is torn down.
    fn dtor_obj(&self) -> DtorObj {
        DtorObj::Own(Coroutine::destroy::<ZendFiber>)
    }

    /// `zend_fiber_object_free`: the callable and `fiber->result` given
    /// up, and what the coroutine holds of the heap.
    fn free_obj(&self, req: &mut Request, object: PhpObject) {
        let Some(fiber) = object.native_mut::<ZendFiber>(req) else {
            return;
        };
        let callable = fiber.callable.take();
        let result = std::mem::replace(&mut fiber.result, Value::null());
        if let Some(callable) = callable {
            req.release(callable.value());
        }
        req.release(result);
        Coroutine::release::<ZendFiber>(req, object);
    }

    /// `clone_obj = NULL`: a fiber is uncloneable.
    fn refusals(&self) -> Refusals {
        Refusals::NONE.cloning()
    }

    /// `zend_fiber_object_gc` (`Z/Zend/zend_fibers.c:818`): the callable
    /// and `fiber->result`, and for a suspended fiber every frame it
    /// holds, innermost first, each by [`exec::unfinished_execution_gc`]
    /// with the calls it was building or, for a generator running on it,
    /// by [`crate::generator::running_frame_gc`]. Each is an owner the
    /// fiber's frames hold, so a cycle through a suspended fiber's
    /// variables is found. A running fiber's frames are the running
    /// execution's, and are not named.
    ///
    /// The outermost symbol table a frame rebuilt is returned as the
    /// fiber's table and walked after everything else, and the rows of
    /// each one inside it are named where the next frame out with a table
    /// stands, as php's `lastSymTable` does. One table two frames share,
    /// a function's and a file it included, is named once: php names it
    /// twice, which takes two owners from what it holds where the table
    /// is one, so a collection while the fiber is live frees what its
    /// variables hold.
    ///
    /// ```php
    /// function park() {
    ///     $fiber = new Fiber(function () {
    ///         $held = new stdClass;
    ///         $held->fiber = Fiber::getCurrent();
    ///         Fiber::suspend();
    ///         echo "never\n";
    ///     });
    ///     $fiber->start();
    /// }
    /// park();
    /// var_dump(gc_status()['roots']);
    /// var_dump(gc_collect_cycles());
    /// echo "done\n";
    /// ```
    /// ```expect
    /// int(1)
    /// int(1)
    /// done
    /// ```
    ///
    /// A call a frame was building is named with that frame, after its
    /// CVs and before the frames outside it, as php names `ex->call`;
    /// each `D` survives the fiber's destruction through its own cycle,
    /// and is served by the collection in the order the walk found it:
    ///
    /// ```php
    /// class D { public $self; function __construct(public $n) { $this->self = $this; } function __destruct() { echo $this->n, "\n"; } }
    /// function foo($a, $b) {}
    /// function f() { $c = new D('cv'); $h = Fiber::getCurrent(); foo(new D('argument'), Fiber::suspend()); }
    /// $fiber = new Fiber(function () { $q = new D('outer'); f(); });
    /// $fiber->start();
    /// gc_collect_cycles();
    /// $fiber = null;
    /// gc_collect_cycles();
    /// echo "done\n";
    /// ```
    /// ```expect
    /// outer
    /// argument
    /// cv
    /// done
    /// ```
    ///
    /// A variable a frame made by name lives in its symbol table, which
    /// the walk names too, its rows after the fiber's values and the CVs
    /// among them, so what the variables hold is found before what the
    /// callable reaches; each `D` below survives the fiber's destruction
    /// through its own cycle, and is served by the collection in the
    /// order the walk found it:
    ///
    /// ```php
    /// class D { public $self; function __construct(public $n) { $this->self = $this; } function __destruct() { echo $this->n, "\n"; } }
    /// function f() { $c = new D('cv'); $y = 'x'; $$y = new D('by name'); $z = 'fiber'; $$z = Fiber::getCurrent(); Fiber::suspend(); }
    /// $fiber = new Fiber(function () { f(); });
    /// $fiber->start();
    /// gc_collect_cycles();
    /// $fiber = null;
    /// gc_collect_cycles();
    /// echo "done\n";
    /// ```
    /// ```expect
    /// cv
    /// by name
    /// done
    /// ```
    fn get_gc(
        &self,
        req: &mut Request,
        object: PhpObject,
        visit: &mut dyn FnMut(Value),
    ) -> GcTable {
        let Some(fiber) = object.native::<ZendFiber>(req) else {
            return GcTable::Properties;
        };
        if let Some(callable) = &fiber.callable {
            visit(callable.value());
        }
        visit(fiber.result);
        if fiber.coroutine.status != CoroutineStatus::Suspended {
            return GcTable::Properties;
        }
        let Some(execution) = &fiber.coroutine.execution else {
            return GcTable::Properties;
        };
        let stack = &execution.vm.stack;
        let cv = |frame, at| {
            stack.frame(frame).map_or(php_heap::Zval::UNDEF, |frame| {
                frame.slot(php_heap::Slot(at))
            })
        };
        let mut last = None;
        let mut calls = Vec::new();
        for at in (0..execution.vm.depth()).rev() {
            let frame = php_heap::FrameRef(u32::try_from(at).unwrap_or(u32::MAX));
            let Some(slots) = execution.vm.stack.frame(frame) else {
                continue;
            };
            let state = req.kept_frame(slots);
            if state.info().call() {
                calls.push(state);
                continue;
            }
            // The calls standing above a frame are the ones it was
            // building, its `ex->call`. A running generator's are named
            // by none: `zend_generator_frame_gc` reads its frozen call
            // stack, which is empty while it runs.
            calls.reverse();
            let table = match state.generator() {
                Some(generator) => crate::generator::running_frame_gc(req, generator, state, visit),
                None => exec::unfinished_execution_gc(state, &calls, visit),
            };
            calls.clear();
            if let Some(table) = table {
                if let Some(inner) = last.filter(|&inner| inner != table) {
                    req.gc_symbol_rows(inner, &cv, visit);
                }
                last = Some(table);
            }
        }
        let Some(table) = last else {
            return GcTable::Properties;
        };
        let mut rows = Vec::new();
        req.gc_symbol_rows(table, &cv, &mut |value| rows.push(value));
        GcTable::Rows(rows)
    }
}

static FIBER: FiberHandlers = FiberHandlers;

/// Where a fiber's execution stands, php's `fiber->execute_data`, for
/// a view of it ([`crate::api::request::Execution`]).
pub(crate) enum Standing<'a> {
    /// The fiber runs innermost, `EG(active_fiber)`: the stack the
    /// request runs on is its.
    Active,
    /// The fiber is suspended, runs below a fiber it resumed, or waits on
    /// a suspend that passed through it with php's pointer last written
    /// there: the execution it left, which it holds, or that resume.
    Held(&'a Execution),
    /// The fiber waits on a suspend of a coroutine below it that passed
    /// through it, and nothing has written php's `fiber->execute_data`
    /// since its start: the pointer names the bottom frame
    /// `zend_fiber_execute` laid down, under which stands the resumer's
    /// frame, `stack_bottom->prev_execute_data`.
    Bottom {
        /// The execution the fiber left, which it holds.
        own: &'a Execution,
        /// The coroutine that resumed it, whose execution it holds too
        /// while the suspend is out; none where the bottom frame names no
        /// resumer's, a fiber a destroy resumed.
        resumer: Option<Context>,
    },
}

impl<'a> Standing<'a> {
    /// The stack the fiber's own frames stand on.
    pub(crate) fn stack(&self, req: &'a Request) -> &'a Vm {
        match self {
            Standing::Active => req.vm(),
            Standing::Held(held) | Standing::Bottom { own: held, .. } => &held.vm,
        }
    }
}

/// A fiber as a view of it reads it: its callable, php's `fci`, from its
/// construction until it is dead, and where its execution stands, none
/// before it starts and once it is dead. None for an object that is no
/// fiber.
///
/// A `Fiber` a suspend passed through stands where php's
/// `fiber->execute_data` was last written ([`ExecuteData`]), at its
/// bottom frame where nothing has written it since its start
/// ([`Standing::Bottom`]); `_ZendTestFiber::suspend` is the witness.
pub(crate) fn view(
    req: &Request,
    object: PhpObject,
) -> Option<(Option<Callable>, Option<Standing<'_>>)> {
    let fiber = object.native::<ZendFiber>(req)?;
    let coroutine = &fiber.coroutine;
    let standing = match coroutine.status {
        CoroutineStatus::Init | CoroutineStatus::Dead => None,
        CoroutineStatus::Suspended => coroutine.execution.as_deref().map(Standing::Held),
        CoroutineStatus::Running => {
            let running = &req.fibers().running;
            match running.iter().position(|running| running.fiber.is(object)) {
                Some(at) => Some(match running.get(at.saturating_add(1)) {
                    Some(above) => Standing::Held(&above.below),
                    None => Standing::Active,
                }),
                // Waiting on a suspend that passed through it.
                None => coroutine
                    .execution
                    .as_deref()
                    .map(|own| match coroutine.execute_data {
                        ExecuteData::Innermost => Standing::Held(own),
                        ExecuteData::Bottom => Standing::Bottom {
                            own,
                            resumer: coroutine.linked_resumer(),
                        },
                    }),
            }
        }
    };
    Some((fiber.callable.clone(), standing))
}

/// `f` served over the execution `context` holds off the running chain,
/// taken out of the coroutine for its length and put back after: how a
/// read with the request borrowed reaches the coroutine of a payload of
/// any type. None where it holds none.
pub(crate) fn with_held<R>(
    req: &mut Request,
    context: Context,
    f: impl FnOnce(&Request, &Execution) -> R,
) -> Option<R> {
    let held = context.coroutine(req)?.execution.take()?;
    let read = f(req, &held);
    if let Some(coroutine) = context.coroutine(req) {
        coroutine.execution = Some(held);
    }
    Some(read)
}

/// The payload of `object`, which a `Fiber`'s object always carries.
fn payload(req: &mut Request, object: PhpObject) -> Result<&mut ZendFiber, Thrown> {
    if object.native::<ZendFiber>(req).is_none() {
        return Err(throw_error(
            req,
            "Error",
            "Fiber object with no fiber payload",
        ));
    }
    object
        .native_mut::<ZendFiber>(req)
        .ok_or_else(|| Thrown::of(None))
}

/// `FiberError` with php's sentence.
fn fiber_error(req: &mut Request, message: &str) -> Thrown {
    throw_error(req, "FiberError", message)
}

/// `Fiber`: `ZEND_ACC_FINAL`, `@strict-properties` and
/// `@not-serializable`, so a write to a name it did not declare is
/// php's `Error`; its methods are the stub's, in the stub's order.
///
/// ```php
/// echo new ReflectionClass("Fiber");
/// ```
/// ```expect
/// Class [ <internal:Core> final class Fiber ] {
///
///   - Constants [0] {
///   }
///
///   - Static properties [0] {
///   }
///
///   - Static methods [2] {
///     Method [ <internal:Core> static public method getCurrent ] {
///
///       - Parameters [0] {
///       }
///       - Return [ ?Fiber ]
///     }
///
///     Method [ <internal:Core> static public method suspend ] {
///
///       - Parameters [1] {
///         Parameter #0 [ <optional> mixed $value = null ]
///       }
///       - Return [ mixed ]
///     }
///   }
///
///   - Properties [0] {
///   }
///
///   - Methods [9] {
///     Method [ <internal:Core, ctor> public method __construct ] {
///
///       - Parameters [1] {
///         Parameter #0 [ <required> callable $callback ]
///       }
///     }
///
///     Method [ <internal:Core> public method start ] {
///
///       - Parameters [1] {
///         Parameter #0 [ <optional> mixed ...$args ]
///       }
///       - Return [ mixed ]
///     }
///
///     Method [ <internal:Core> public method resume ] {
///
///       - Parameters [1] {
///         Parameter #0 [ <optional> mixed $value = null ]
///       }
///       - Return [ mixed ]
///     }
///
///     Method [ <internal:Core> public method throw ] {
///
///       - Parameters [1] {
///         Parameter #0 [ <required> Throwable $exception ]
///       }
///       - Return [ mixed ]
///     }
///
///     Method [ <internal:Core> public method isStarted ] {
///
///       - Parameters [0] {
///       }
///       - Return [ bool ]
///     }
///
///     Method [ <internal:Core> public method isSuspended ] {
///
///       - Parameters [0] {
///       }
///       - Return [ bool ]
///     }
///
///     Method [ <internal:Core> public method isRunning ] {
///
///       - Parameters [0] {
///       }
///       - Return [ bool ]
///     }
///
///     Method [ <internal:Core> public method isTerminated ] {
///
///       - Parameters [0] {
///       }
///       - Return [ bool ]
///     }
///
///     Method [ <internal:Core> public method getReturn ] {
///
///       - Parameters [0] {
///       }
///       - Return [ mixed ]
///     }
///   }
/// }
/// ```
///
/// Its status, asked before the start, while it runs, while it is
/// suspended and once it has returned:
///
/// ```php
/// $f = new Fiber(function () { var_dump(Fiber::getCurrent()->isRunning()); Fiber::suspend(); });
/// var_dump($f->isStarted(), $f->isSuspended(), $f->isTerminated());
/// $f->start();
/// var_dump($f->isStarted(), $f->isSuspended(), $f->isRunning());
/// $f->resume();
/// var_dump($f->isSuspended(), $f->isTerminated());
/// ```
/// ```expect
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(false)
/// bool(false)
/// bool(true)
/// ```
///
/// `zend_fiber_object_create` allocates `sizeof(zend_fiber)` whole, which
/// `memory_get_usage()` counts (design §5.13):
///
/// ```php
/// new Fiber('strlen');
/// $m = memory_get_usage();
/// $f = new Fiber('strlen');
/// echo memory_get_usage() - $m, "\n";
/// ```
/// ```expect
/// 384
/// ```
#[php_class(
    final,
    not_serializable,
    dynamic_properties = forbidden,
    // `emalloc(sizeof(zend_fiber))`, 328 bytes whole.
    size = 344, payload = ZendFiber,
    handlers = FIBER
)]
pub(crate) struct Fiber;

#[php_class]
impl Fiber {
    /// `Fiber::__construct(callable $callback)`: the callable resolved
    /// where it is taken, `Z_PARAM_FUNC`, and one owner of it kept while
    /// the fiber runs; a second call is php's `FiberError`.
    ///
    /// ```php
    /// $f = new Fiber('strlen');
    /// try { $f->__construct('strlen'); } catch (FiberError $e) { echo $e->getMessage(), "\n"; }
    /// try { new Fiber('nope'); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// Cannot call constructor twice
    /// Fiber::__construct(): Argument #1 ($callback) must be a valid callback, function "nope" not found or invalid function name
    /// ```
    fn __construct(req: &mut Request, this: This<Self>, callback: Callable) -> Result<(), Thrown> {
        let callable = callback;
        let this = this.object();
        let fiber = payload(req, this)?;
        if fiber.coroutine.status != CoroutineStatus::Init || fiber.callable.is_some() {
            return Err(fiber_error(req, "Cannot call constructor twice"));
        }
        let held = callable.value();
        fiber.callable = Some(callable);
        req.addref(&held);
        Ok(())
    }

    /// `Fiber::start(mixed ...$args): mixed`: the stack laid down at
    /// `fiber.stack_size`, the callable called on it with the arguments,
    /// positional and named, and what the fiber first suspends with handed
    /// back, or `null` where it returned without suspending.
    ///
    /// ```php
    /// $f = new Fiber(fn() => Fiber::suspend(1));
    /// var_dump($f->start(), $f->resume(2));
    /// ```
    /// ```expect
    /// int(1)
    /// NULL
    /// ```
    ///
    /// The order is php's: the body runs up to its first suspend inside
    /// `start`, each `resume` runs it to the next, and the value a resume
    /// sends is what that suspend returns.
    ///
    /// ```php
    /// $f = new Fiber(function (string $x, string $y) {
    ///     echo "a $x $y\n";
    ///     $v = Fiber::suspend("s1");
    ///     echo "b $v\n";
    ///     $w = Fiber::suspend("s2");
    ///     echo "c $w\n";
    ///     return "r";
    /// });
    /// echo "start\n"; var_dump($f->start("x", "y"));
    /// echo "resume\n"; var_dump($f->resume("v1"));
    /// var_dump($f->resume("v2"), $f->getReturn(), $f->isTerminated());
    /// ```
    /// ```expect
    /// start
    /// a x y
    /// string(2) "s1"
    /// resume
    /// b v1
    /// string(2) "s2"
    /// c v2
    /// NULL
    /// string(1) "r"
    /// bool(true)
    /// ```
    ///
    /// A second start is php's `FiberError`.
    ///
    /// ```php
    /// $f = new Fiber(fn() => 1);
    /// $f->start();
    /// $f->start();
    /// ```
    /// ```expect exit=255
    /// Fatal error: Uncaught FiberError: Cannot start a fiber that has already been started in %s:4
    /// Stack trace:
    /// #0 %s(4): Fiber->start()
    /// #1 {main}
    ///   thrown in %s on line 4
    /// ```
    fn start(
        req: &mut Request,
        this: This<Self>,
        args: NamedVariadic<Value>,
    ) -> Result<Owner<Value>, Thrown> {
        switch_refused(req)?;
        let named: Vec<(PhpString, Value)> = args.named().to_vec();
        let back = start(req, this.object(), args.positional, named)?;
        delegate(req, back)
    }

    /// `Fiber::resume(mixed $value = null): mixed`: the suspended fiber runs
    /// on from its suspend, which returns `$value`, until it suspends again
    /// or returns. A fiber that is not suspended is php's `FiberError`, a
    /// fiber resumed twice past its end among them.
    ///
    /// ```php
    /// $f = new Fiber(fn() => Fiber::suspend());
    /// $f->start();
    /// $f->resume();
    /// $f->resume();
    /// ```
    /// ```expect exit=255
    /// Fatal error: Uncaught FiberError: Cannot resume a fiber that is not suspended in %s:5
    /// Stack trace:
    /// #0 %s(5): Fiber->resume()
    /// #1 {main}
    ///   thrown in %s on line 5
    /// ```
    ///
    /// A fiber resuming itself is running and not suspended, and the trace
    /// walks from the fiber's frames on into its resumer's, as php's
    /// `stack_bottom->prev_execute_data` does.
    ///
    /// ```php
    /// $fiber = new Fiber(function (): void {
    ///     Fiber::getCurrent()->resume();
    /// });
    /// $fiber->start();
    /// ```
    /// ```expect exit=255
    /// Fatal error: Uncaught FiberError: Cannot resume a fiber that is not suspended in %s:3
    /// Stack trace:
    /// #0 %s(3): Fiber->resume()
    /// #1 [internal function]: {closure:%s:2}()
    /// #2 %s(5): Fiber->start()
    /// #3 {main}
    ///   thrown in %s on line 3
    /// ```
    fn resume(
        req: &mut Request,
        this: This<Self>,
        #[php(default = null)] value: Value,
    ) -> Result<Owner<Value>, Thrown> {
        switch_refused(req)?;
        let this = this.object();
        if payload(req, this)?.coroutine.status != CoroutineStatus::Suspended {
            return Err(fiber_error(
                req,
                "Cannot resume a fiber that is not suspended",
            ));
        }
        req.addref(&value);
        let back = switch_into::<ZendFiber>(req, this, Transfer::Value(value))?;
        delegate(req, back)
    }

    /// `Fiber::throw(Throwable $exception): mixed`: the suspended fiber runs
    /// on with `$exception` raised at its suspend.
    ///
    /// ```php
    /// $f = new Fiber(function () {
    ///     try { Fiber::suspend(1); } catch (Exception $e) { echo "caught ", $e->getMessage(), "\n"; }
    ///     return 2;
    /// });
    /// $f->start();
    /// var_dump($f->throw(new Exception("x")), $f->getReturn());
    /// ```
    /// ```expect
    /// caught x
    /// NULL
    /// int(2)
    /// ```
    ///
    /// A throwable the fiber does not catch leaves it through the `throw`
    /// that raised it, and the fiber is dead.
    ///
    /// ```php
    /// $f = new Fiber(function () { Fiber::suspend(); });
    /// $f->start();
    /// try { $f->throw(new TypeError("y")); } catch (TypeError $e) { echo get_class($e), " ", $e->getMessage(), "\n"; }
    /// var_dump($f->isTerminated());
    /// try { $f->getReturn(); } catch (FiberError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// TypeError y
    /// bool(true)
    /// Cannot get fiber return value: The fiber threw an exception
    /// ```
    fn throw(
        req: &mut Request,
        this: This<Self>,
        exception: Instance<Throwable>,
    ) -> Result<Owner<Value>, Thrown> {
        let throwable = exception.object();
        switch_refused(req)?;
        let this = this.object();
        if payload(req, this)?.coroutine.status != CoroutineStatus::Suspended {
            return Err(fiber_error(
                req,
                "Cannot resume a fiber that is not suspended",
            ));
        }
        req.addref(&throwable.as_value());
        let back =
            switch_into::<ZendFiber>(req, this, Transfer::Throw(Thrown::of(Some(throwable))))?;
        delegate(req, back)
    }

    /// `Fiber::isStarted()`, `isSuspended()`, `isRunning()` and
    /// `isTerminated()`, which read the status php keeps with `caller`:
    /// running is running or resuming another.
    ///
    /// ```php
    /// $f = new Fiber(function () use (&$f) {
    ///     var_dump($f->isStarted(), $f->isSuspended(), $f->isRunning(), $f->isTerminated());
    ///     Fiber::suspend();
    /// });
    /// var_dump($f->isStarted(), $f->isSuspended(), $f->isRunning(), $f->isTerminated());
    /// $f->start();
    /// var_dump($f->isStarted(), $f->isSuspended(), $f->isRunning(), $f->isTerminated());
    /// $f->resume();
    /// var_dump($f->isStarted(), $f->isSuspended(), $f->isRunning(), $f->isTerminated());
    /// ```
    /// ```expect
    /// bool(false)
    /// bool(false)
    /// bool(false)
    /// bool(false)
    /// bool(true)
    /// bool(false)
    /// bool(true)
    /// bool(false)
    /// bool(true)
    /// bool(true)
    /// bool(false)
    /// bool(false)
    /// bool(true)
    /// bool(false)
    /// bool(false)
    /// bool(true)
    /// ```
    fn isStarted(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        status_is(req, this.object(), |status| status != CoroutineStatus::Init)
    }

    fn isSuspended(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        status_is(req, this.object(), |status| {
            status == CoroutineStatus::Suspended
        })
    }

    fn isRunning(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        status_is(req, this.object(), |status| {
            status == CoroutineStatus::Running
        })
    }

    fn isTerminated(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        status_is(req, this.object(), |status| status == CoroutineStatus::Dead)
    }

    /// `Fiber::getReturn(): mixed`: what the callable returned, dereferenced
    /// and one more owner, once the fiber is dead; php's `FiberError` naming
    /// why before then or where it ended otherwise.
    ///
    /// ```php
    /// $f = new Fiber(function () { Fiber::suspend(); return 1; });
    /// try { $f->getReturn(); } catch (FiberError $e) { echo $e->getMessage(), "\n"; }
    /// $f->start();
    /// try { $f->getReturn(); } catch (FiberError $e) { echo $e->getMessage(), "\n"; }
    /// $f->resume();
    /// var_dump($f->getReturn());
    /// ```
    /// ```expect
    /// Cannot get fiber return value: The fiber has not been started
    /// Cannot get fiber return value: The fiber has not returned
    /// int(1)
    /// ```
    fn getReturn(req: &mut Request, this: This<Self>) -> Result<Value, Thrown> {
        let this = this.object();
        let fiber = payload(req, this)?;
        let message = match fiber.coroutine.status {
            CoroutineStatus::Dead if fiber.coroutine.threw => "The fiber threw an exception",
            CoroutineStatus::Dead if fiber.coroutine.bailout => {
                "The fiber exited with a fatal error"
            }
            CoroutineStatus::Dead => return Ok(fiber.result.deref()),
            CoroutineStatus::Init => "The fiber has not been started",
            CoroutineStatus::Running | CoroutineStatus::Suspended => "The fiber has not returned",
        };
        Err(fiber_error(
            req,
            &format!("Cannot get fiber return value: {message}"),
        ))
    }

    /// `Fiber::getCurrent(): ?Fiber`: the innermost fiber running, one more
    /// owner, and `null` on the request's own stack.
    ///
    /// ```php
    /// var_dump(Fiber::getCurrent());
    /// $f = new Fiber(function () use (&$f) { var_dump(Fiber::getCurrent() === $f); });
    /// $f->start();
    /// ```
    /// ```expect
    /// NULL
    /// bool(true)
    /// ```
    fn getCurrent(req: &mut Request) -> Option<Instance<Fiber>> {
        let fiber = req.fibers().active()?;
        Some(Instance::of(fiber))
    }

    /// `Fiber::suspend(mixed $value = null): mixed`: the innermost running
    /// fiber suspends, the `start`, `resume` or `throw` that ran it returns
    /// `$value`, and this returns what the next `resume` sends or raises
    /// what the next `throw` does. Outside a fiber it is php's `FiberError`.
    ///
    /// ```php
    /// Fiber::suspend();
    /// ```
    /// ```expect exit=255
    /// Fatal error: Uncaught FiberError: Cannot suspend outside of a fiber in %s:2
    /// Stack trace:
    /// #0 %s(2): Fiber::suspend()
    /// #1 {main}
    ///   thrown in %s on line 2
    /// ```
    ///
    /// A fiber that resumes another is running while the other runs, and
    /// the other's suspend returns to it and not to the request's stack.
    ///
    /// ```php
    /// $outer = new Fiber(function () {
    ///     $inner = new Fiber(function () { $v = Fiber::suspend("i1"); echo "inner got $v\n"; return "done"; });
    ///     $v = $inner->start(); echo "outer got $v\n";
    ///     $v = Fiber::suspend("o1"); echo "outer got $v\n";
    ///     $inner->resume("to inner");
    ///     echo "inner returned ", $inner->getReturn(), "\n";
    /// });
    /// $v = $outer->start(); echo "main got $v\n";
    /// $outer->resume("to outer");
    /// ```
    /// ```expect
    /// outer got i1
    /// main got o1
    /// outer got to outer
    /// inner got to inner
    /// inner returned done
    /// ```
    fn suspend(
        req: &mut Request,
        #[php(default = null)] value: Value,
    ) -> Result<Owner<Value>, Thrown> {
        let Some(fiber) = req.fibers().active() else {
            return Err(fiber_error(req, "Cannot suspend outside of a fiber"));
        };
        if payload(req, fiber)?.coroutine.destroyed {
            return Err(fiber_error(req, "Cannot suspend in a force-closed fiber"));
        }
        switch_refused(req)?;
        req.addref(&value);
        let back = suspend_into::<ZendFiber>(req, fiber, Transfer::Value(value))?;
        delegate(req, back)
    }
}

/// `zend_fiber_switch_blocked()`'s refusal in `Fiber::start`,
/// `resume`, `throw` and `suspend` ([`switch_refused`]).
const SWITCH_BLOCKED: &str = "Cannot switch fibers in current execution context";

/// `final class FiberError extends Error`, which only the engine makes.
#[php_class(final, extends(Error))]
pub(crate) struct FiberError;

#[php_class]
impl FiberError {
    /// `FiberError::__construct()`: php's `Error`, since only the engine
    /// makes one.
    ///
    /// ```php
    /// new FiberError();
    /// ```
    /// ```expect exit=255
    /// Fatal error: Uncaught Error: The "FiberError" class is reserved for internal use and cannot be manually instantiated in %s:2
    /// Stack trace:
    /// #0 %s(2): FiberError->__construct()
    /// #1 {main}
    ///   thrown in %s on line 2
    /// ```
    ///
    /// Its body parses no parameter, so what it is handed changes nothing:
    ///
    /// ```php
    /// try { new FiberError(1, 2); } catch (Error $e) { echo get_class($e), ": ", $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// Error: The "FiberError" class is reserved for internal use and cannot be manually instantiated
    /// ```
    #[php(arity = manual)]
    fn __construct(req: &mut Request, this: This<Self>) -> Result<(), Thrown> {
        let name = req.class_name(this.object().class());
        Err(throw_error(
            req,
            "Error",
            &format!(
                "The \"{name}\" class is reserved for internal use and cannot be manually instantiated"
            ),
        ))
    }
}

/// `EG(fiber_stack_size)`: what `OnUpdateFiberStackSize` last wrote,
/// the startup entry's quantity, or php's 2 MiB where the entry has no
/// value. php's table holds `NULL` there, which the update reads as the
/// default, where an empty value is a quantity of zero and refused at
/// the next `start`.
fn stack_size(req: &Request) -> usize {
    if let Some(size) = req.fibers().stack_size {
        return size;
    }
    match req.ini_value(b"fiber.stack_size") {
        Some(value) => usize::try_from(Request::ini_parse_bytes(value)).unwrap_or(DEFAULT_STACK),
        None => DEFAULT_STACK,
    }
}

/// `OnUpdateFiberStackSize` (`Z/Zend/zend.c:238-252`) for an
/// `ini_set()`: the value read as a quantity, a negative one php's
/// warning and a refusal that keeps the entry as it was, and anything
/// else the size the next `start` lays down; no value, php's `NULL`, is
/// `ZEND_FIBER_DEFAULT_C_STACK_SIZE`, where the empty string is 0.
///
/// ```php
/// var_dump(ini_set("fiber.stack_size", "-1"), ini_get("fiber.stack_size"));
/// ini_set("fiber.stack_size", "");
/// try { (new Fiber(function () {}))->start(); } catch (Exception $e) { echo substr($e->getMessage(), 0, 54), "\n"; }
/// ```
/// ```expect
///
/// Warning: fiber.stack_size must be a positive number in %s on line 2
/// bool(false)
/// string(0) ""
/// Fiber stack size is too small, it needs to be at least
/// ```
pub(crate) fn stack_size_updated(req: &mut Request, value: Option<&[u8]>) -> bool {
    let Some(value) = value else {
        req.fibers_mut().stack_size = Some(DEFAULT_STACK);
        return true;
    };
    let size = Request::ini_parse_bytes(value);
    let Ok(size) = usize::try_from(size) else {
        crate::api::raising::warning(req, "fiber.stack_size must be a positive number");
        return false;
    };
    req.fibers_mut().stack_size = Some(size);
    true
}

/// `zend_fiber_execute` and `zend_test_fiber_execute`, on the coroutine's
/// own stack: the first VM page laid down and the coroutine's body served
/// on it, a `Fiber`'s calling its callable ([`start`]); a throwable or
/// the unwind exit the body left is carried out as an error transfer and
/// a fatal as the `Bailout`, each flagged on the coroutine, and what it
/// returned is the transfer, `null` for a `Fiber`, which is what the
/// `start` or `resume` that ran it last returns.
///
/// The interrupt is the request's and is tested on the fiber's stack as
/// on any other: a timeout inside a fiber is raised at the fiber's own
/// back-edge and line, the fiber ends in the fatal, and the `start` that
/// ran it hands the `Bailout` on, so nothing after it runs and the
/// shutdown functions read a fiber that exited with a fatal error.
///
/// ```php
/// set_time_limit(1);
/// $f = new Fiber(function () {
///     for (;;) {}
/// });
/// register_shutdown_function(function () use ($f) {
///     try { $f->getReturn(); } catch (FiberError $e) { echo $e->getMessage(), "\n"; }
/// });
/// $f->start();
/// echo "never\n";
/// ```
/// ```expect exit=255 wall=1s
/// Fatal error: Maximum execution time of 1 second exceeded in %s on line 4
/// Cannot get fiber return value: The fiber exited with a fatal error
/// ```
///
/// The VM page is php's fiber page, 16 KiB where the request's own grow
/// in 256 KiB pages, and `memory_get_usage()` reads it laid down by the
/// start and given back when the fiber ends.
///
/// ```php
/// $f = new Fiber(function () { Fiber::suspend(); });
/// $before = memory_get_usage();
/// $f->start();
/// $after = memory_get_usage();
/// $f->resume();
/// var_dump(($after - $before) >> 10, ($after - memory_get_usage()) >> 10);
/// ```
/// ```expect
/// int(16)
/// int(16)
/// ```
fn execute<P: HoldsCoroutine>(
    req: &mut Request,
    object: PhpObject,
    body: impl FnOnce(&mut Request) -> Result<CoroutineEnd, Thrown>,
) -> Transfer {
    let opened = req.open_stack();
    if req.fatal_of(opened).is_err() {
        if let Ok(coroutine) = coroutine_of::<P>(req, object) {
            coroutine.bailout = true;
        }
        return Transfer::Fatal;
    }
    let thrown = match body(req) {
        Ok(CoroutineEnd::Return(value)) => return Transfer::Value(value.hand_on()),
        Ok(CoroutineEnd::Into(target)) => {
            let Ok(coroutine) = coroutine_of::<P>(req, object) else {
                return Transfer::Fatal;
            };
            coroutine.into = Some(target);
            return Transfer::Value(Value::null());
        }
        Err(thrown) => thrown,
    };
    let exited = matches!(
        req.raised(),
        Some(Raised::UnwindExit | Raised::GracefulExit)
    );
    let Ok(coroutine) = coroutine_of::<P>(req, object) else {
        return Transfer::Fatal;
    };
    // A coroutine being destroyed that ends in either exit ended as it was
    // asked to, and the exit is cleared, whatever `exit` its `finally`
    // blocks called.
    if coroutine.destroyed && exited {
        req.take_raised();
        return Transfer::Value(Value::null());
    }
    if thrown.exception().is_some() || exited {
        coroutine.threw = true;
        Transfer::Throw(thrown)
    } else {
        coroutine.bailout = true;
        Transfer::Fatal
    }
}

/// `zend_fiber_resume_internal` and the resumer's half of
/// `zend_fiber_switch_context`, for the coroutine `object` holds: its
/// execution state swapped in with the resumer's kept beside it as the
/// stack below, the machine switch, and the two swapped back when it
/// suspends or returns. A coroutine that returned is dead, its stack and
/// its VM page given back (`zend_fiber_destroy_context`). A `Fiber` is
/// `EG(active_fiber)` from before the switch's notice until the switch
/// back has returned; an extension's coroutine moves no `EG(active_fiber)`,
/// as `zend_test_fiber_resume` moves none.
///
/// A suspend of a coroutine below this one passes through it: the switch
/// back finds it still running, php's `caller` still set, and it waits,
/// its machine stack here and its execution by the request
/// ([`Fibers::held`]), while the suspend goes on from
/// the stack that resumed it; when that coroutine is resumed, the suspend
/// returns here and this one runs on with what the resume sent, as php's
/// symmetric switch enters it again through the suspended coroutine's
/// `previous`. The observers are told of the switch php makes and not of
/// these steps: out of the coroutine the suspend was made in, and on the
/// resume, into it.
///
/// A body that ends into another coroutine ([`CoroutineEnd::Into`])
/// hands it the switch: the observers see the dead coroutine switch into
/// it, and it runs in the dead one's place until it suspends or returns
/// ([`run_ending_into`]), the switch back noticed out of it.
///
/// The error-reporting level is swapped with the rest: a fiber starts at
/// the level the entry says, whatever an `@` around the `start` masked,
/// and an `@` inside the fiber masks nothing of the resumer's while the
/// fiber is suspended under it.
///
/// ```php
/// function f() { echo $u; @Fiber::suspend(); echo $v; }
/// $fb = new Fiber('f');
/// @$fb->start();
/// echo "between\n";
/// echo $w;
/// $fb->resume();
/// echo "end\n";
/// ```
/// ```expect
///
/// Warning: Undefined variable $u in %s on line 2
/// between
///
/// Warning: Undefined variable $w in %s on line 6
///
/// Warning: Undefined variable $v in %s on line 2
/// end
/// ```
fn switch_into<P: HoldsCoroutine>(
    req: &mut Request,
    object: PhpObject,
    transfer: Transfer,
) -> Result<Transfer, Thrown> {
    let coroutine = coroutine_of::<P>(req, object)?;
    let (Some(mut stack), Some(own)) = (coroutine.stack.take(), coroutine.execution.take()) else {
        return Err(throw_error(
            req,
            "Error",
            "InternalError: a fiber coroutine resumed with no stack of its own",
        ));
    };
    let entered = if coroutine.status == CoroutineStatus::Init {
        FiberStatus::Init
    } else {
        FiberStatus::Suspended
    };
    // `fiber->previous`: the coroutine the switch enters, the innermost one
    // standing when this one suspended.
    let entering = coroutine.previous.take().unwrap_or(object);
    let kind = coroutine.kind;
    let by = Resumer::of(kind, &transfer);
    // `EG(current_fiber_context)`: the coroutine served now, or the
    // request's own stack, which the switch leaves and comes back to.
    // The handlers are called before the coroutine is marked running, as
    // php's notice comes before `to->status = ZEND_FIBER_STATUS_RUNNING`,
    // and with a `Fiber` `EG(active_fiber)` already, as
    // `zend_fiber_resume_internal` sets it before the switch.
    let resumer = req.fibers().current();
    let active = req.fibers().active;
    let caller = Caller::of(resumer);
    coroutine_of::<P>(req, object)?.caller = Some(caller);
    if kind == Kind::Fiber
        && let Some(previous) = active.and_then(|active| active.native_mut::<ZendFiber>(req))
    {
        // `zend_fiber_resume_internal`: `previous->execute_data`.
        previous.coroutine.execute_data = ExecuteData::Innermost;
    }
    let resumer = resumer.map(Context::object);
    let left = observed(req, resumer, FiberStatus::Running);
    let entering = observed(req, Some(entering), entered);
    if kind == Kind::Fiber {
        req.fibers_mut().active = Some(object);
    }
    crate::observer::fiber_switched(req, left, entering);
    coroutine_of::<P>(req, object)?.status = CoroutineStatus::Running;
    let mut own = *own;
    if entered == FiberStatus::Init {
        // A coroutine's first run starts on `EG(active_fiber)` as the
        // resume left it, a new context having captured nothing yet.
        own.active = req.fibers().active;
    }
    // The coroutine running now: `object`, then each one a body ended into.
    let mut object = object;
    let switched = run_ending_into::<P>(req, &mut object, &mut stack, own, by, transfer, caller)?;
    // The switch back is noticed on the coroutine's side, its frames the
    // ones standing, as php notices it before the jump, and out of the
    // coroutine the suspend was made in.
    let (left, leaving) = match &switched {
        Ok(Switched::Suspended(_)) => (
            FiberStatus::Running,
            coroutine_of::<P>(req, object)?.previous.unwrap_or(object),
        ),
        Ok(Switched::Returned(_)) | Err(_) => {
            // `zend_fiber_execute`'s last word, before its switch out.
            coroutine_of::<P>(req, object)?.caller = None;
            (FiberStatus::Dead, object)
        }
    };
    let leaving = observed(req, Some(leaving), left);
    let back = observed(req, resumer, FiberStatus::Suspended);
    crate::observer::fiber_switched(req, leaving, back);
    let mut execution = req.fibers_mut().switch_back();
    req.switch_execution(&mut execution);
    if kind == Kind::Fiber {
        req.fibers_mut().active = active;
    }
    if left == FiberStatus::Dead {
        crate::observer::fiber_destroyed(req, leaving);
    }
    let back = match switched {
        Ok(Switched::Suspended(back)) => {
            let coroutine = coroutine_of::<P>(req, object)?;
            coroutine.stack = Some(stack);
            coroutine.execution = Some(Box::new(execution));
            back
        }
        Ok(Switched::Returned(back)) => {
            bury::<P>(req, object, execution)?;
            if matches!(back, Transfer::Fatal) {
                // `zend_fiber_switch_to` raises the fiber's bailout again
                // on the resumer, which leaves its own open frames to
                // `zend_observer_fcall_end_all` at the request's shutdown;
                // the fatal is rendered, and the frames still stand.
                crate::observer::bailed_out(req);
            }
            back
        }
        Err(fatal) => {
            coroutine_of::<P>(req, object)?.status = CoroutineStatus::Dead;
            let dropped = execution.vm.stack.close(req.heap_mut());
            req.drop_frames(dropped);
            req.fatal_of::<()>(Err(fatal))?;
            Transfer::Fatal
        }
    };
    Ok(back)
}

/// [`run_on`] for the coroutine `object` names, and in its place each
/// coroutine a body ends into ([`CoroutineEnd::Into`]), resumed by what
/// resumed the first, `caller`, as `zend_test_fiber_execute` hands a
/// `pipeTo` target the dead fiber's: `object` and `stack` are left the
/// last one's, and how it came back is returned. A body that ends into
/// one not laid down is the engine's fatal, which comes back as the
/// transfer that body's fatal makes.
fn run_ending_into<P: HoldsCoroutine>(
    req: &mut Request,
    object: &mut PhpObject,
    stack: &mut FiberStack<Request>,
    mut own: Execution,
    mut by: Resumer,
    mut transfer: Transfer,
    caller: Caller,
) -> Result<Result<Switched<Transfer>, php_heap::Fatal>, Thrown> {
    loop {
        let switched = run_on::<P>(req, *object, stack, own, by, transfer)?;
        let back = match switched {
            Ok(Switched::Returned(back)) => back,
            switched => return Ok(switched),
        };
        let Some(target) = coroutine_of::<P>(req, *object)?.into.take() else {
            return Ok(Ok(Switched::Returned(back)));
        };
        settle(req, Ok(back));
        let Some((next, held, kind)) = laid_down::<P>(req, target) else {
            let _ = crate::api::raising::fatal(
                req,
                "InternalError: a fiber coroutine ended into one not laid down",
            );
            return Ok(Ok(Switched::Returned(Transfer::Fatal)));
        };
        ended_into::<P>(req, *object, target, caller)?;
        *object = target;
        *stack = next;
        own = *held;
        own.active = req.fibers().active;
        transfer = Transfer::Value(Value::null());
        by = Resumer::of(kind, &transfer);
    }
}

/// The coroutine `object` holds, laid down and not started, taken out
/// for its first resume: its machine stack, its first execution and its kind. None where
/// it is not such a one.
fn laid_down<P: HoldsCoroutine>(
    req: &mut Request,
    object: PhpObject,
) -> Option<(FiberStack<Request>, Box<Execution>, Kind)> {
    let coroutine = object.native_mut::<P>(req)?.coroutine();
    if coroutine.status != CoroutineStatus::Init || coroutine.stack.is_none() {
        return None;
    }
    let execution = coroutine.execution.take()?;
    let stack = coroutine.stack.take()?;
    Some((stack, execution, coroutine.kind))
}

/// `zend_fiber_trampoline`'s last switch where the body of the coroutine
/// `object` holds ended into `target`'s ([`CoroutineEnd::Into`]), made
/// from the dead one's side as php makes it on its stack: `target` takes
/// the dead one's `caller`, the observers see the switch from the dead
/// coroutine into it, the dead one's execution is swapped out and given
/// back, and its destroy is noticed, as `target`'s trampoline destroys
/// the coroutine it came from before its body is called.
fn ended_into<P: HoldsCoroutine>(
    req: &mut Request,
    object: PhpObject,
    target: PhpObject,
    caller: Caller,
) -> Result<(), Thrown> {
    coroutine_of::<P>(req, object)?.caller = None;
    let leaving = observed(req, Some(object), FiberStatus::Dead);
    coroutine_of::<P>(req, target)?.caller = Some(caller);
    let entering = observed(req, Some(target), FiberStatus::Init);
    crate::observer::fiber_switched(req, leaving, entering);
    coroutine_of::<P>(req, target)?.status = CoroutineStatus::Running;
    let mut execution = req.fibers_mut().switch_back();
    req.switch_execution(&mut execution);
    bury::<P>(req, object, execution)?;
    crate::observer::fiber_destroyed(req, leaving);
    Ok(())
}

/// `zend_fiber_destroy_context`'s engine half for the coroutine `object`
/// holds, whose body returned: dead, its stack record and the VM stack
/// of `execution`, its own, given back.
fn bury<P: HoldsCoroutine>(
    req: &mut Request,
    object: PhpObject,
    mut execution: Execution,
) -> Result<(), Thrown> {
    let coroutine = coroutine_of::<P>(req, object)?;
    coroutine.status = CoroutineStatus::Dead;
    if let Some(record) = coroutine.record.take() {
        req.heap_mut().free_block(record);
    }
    let dropped = execution.vm.stack.close(req.heap_mut());
    req.drop_frames(dropped);
    Ok(())
}

/// The machine switch onto the stack of the coroutine `object` holds, with
/// its execution `own` swapped in and the resumer's kept below it on
/// [`Fibers::running`], until it suspends or its body returns, its entry
/// on the list standing for the switch back's notice. A suspend of a
/// coroutine below it passes through: the switch back finds it still
/// running, and it waits here, its entry off the list and its execution
/// swapped out again and held by the request ([`Fibers::held`]), where a
/// read of it finds it ([`view`]), while the same suspend is made from
/// the stack that
/// resumed it; when that one is resumed, this one is, with what was sent.
fn run_on<P: HoldsCoroutine>(
    req: &mut Request,
    object: PhpObject,
    stack: &mut FiberStack<Request>,
    mut own: Execution,
    by: Resumer,
    mut transfer: Transfer,
) -> Result<Result<Switched<Transfer>, php_heap::Fatal>, Thrown> {
    loop {
        req.switch_execution(&mut own);
        req.fibers_mut().running.push(Running {
            fiber: Context::of::<P>(object),
            below: own,
            by,
        });
        let switched = stack.resume(req, transfer);
        let passed = object
            .native_mut::<P>(req)
            .is_some_and(|holder| holder.coroutine().status == CoroutineStatus::Running);
        match switched {
            Ok(Switched::Suspended(carried)) if passed => {
                own = req.fibers_mut().switch_back();
                req.switch_execution(&mut own);
                // Held by the coroutine while it waits, where a read of it
                // finds it ([`view`]).
                coroutine_of::<P>(req, object)?.execution = Some(Box::new(own));
                let sent = php_heap::suspend(req, carried);
                let Some(held) = coroutine_of::<P>(req, object)?.execution.take() else {
                    let lost = php_heap::Fatal::error(String::from(
                        "InternalError: a coroutine a suspend passed through lost its execution while it waited",
                    ));
                    return Ok(Err(lost));
                };
                own = *held;
                transfer = req.fatal_of(sent)?;
            }
            switched => return Ok(switched),
        }
    }
}

/// `zend_fiber_suspend_internal`'s switch, for the coroutine `object`
/// holds, which runs: marked suspended and its `previous` the innermost
/// coroutine standing, before the switch, as php clears `caller` and sets
/// `previous` before it; then the machine switch out of the innermost
/// coroutine, which passes through every coroutine above this one
/// ([`switch_into`]). What comes back is what the next resume sends.
fn suspend_into<P: HoldsCoroutine>(
    req: &mut Request,
    object: PhpObject,
    transfer: Transfer,
) -> Result<Transfer, Thrown> {
    let innermost = req.fibers().current();
    let running = req
        .fibers()
        .running
        .iter()
        .any(|running| running.fiber.is(object));
    if !running {
        return Err(throw_error(
            req,
            "Error",
            "InternalError: a suspend of a fiber coroutine that is not running",
        ));
    }
    let coroutine = coroutine_of::<P>(req, object)?;
    coroutine.status = CoroutineStatus::Suspended;
    coroutine.caller = None;
    coroutine.previous = innermost
        .filter(|innermost| !innermost.is(object))
        .map(Context::object);
    // `fiber->execute_data = EG(current_execute_data)`.
    coroutine.execute_data = ExecuteData::Innermost;
    let back = php_heap::suspend(req, transfer);
    req.fatal_of(back)
}

/// `fiber` as a fiber observer is handed it at a notice, with `status`,
/// php's at that point, and for a `Fiber` its flags and `caller`; the
/// request's own stack where `fiber` is none.
fn observed(req: &Request, fiber: Option<PhpObject>, status: FiberStatus) -> ObservedFiber {
    let as_fiber = fiber
        .and_then(|object| object.native::<ZendFiber>(req))
        .map(|payload| AsFiber {
            threw: payload.coroutine.threw,
            destroyed: payload.coroutine.destroyed,
            caller: payload.coroutine.caller,
        });
    ObservedFiber::of(fiber, status, as_fiber)
}

/// The natives no function table carries that fibers serve:
/// `gc_destructor_fiber`, the collector's fiber's function
/// ([`crate::engine::MethodAt::Unlisted`]).
pub(crate) fn unlisted_functions() -> Vec<(php_unit::decl::MethodDecl, crate::api::abi::Entry)> {
    let function = crate::api::__private::native_function(b"gc_destructor_fiber");
    let method = crate::trampoline::public_method(function, false);
    vec![(method, gc_destructor_fiber)]
}

/// `gc_call_destructors_in_fiber` (`Z/Zend/zend_gc.c:1947`): while a
/// fiber runs, the collector's destructor phase is served in a fiber of
/// its own, so a destructor that suspends suspends that one and not the
/// fiber the collection interrupted. The collector's fiber is resumed,
/// or made and started where it has none; where a destructor suspended
/// it, it is the collector's no more, and a new one goes on from the
/// entry after that destructor's. What was pending before, and what
/// each start left, is kept aside and chained, and pending again after.
/// `false`, the phase left to the heap, where no fiber runs.
///
/// ```php
/// class Cycle {
///     public $self; public $id;
///     function __construct($id) { $this->id = $id; $this->self = $this; }
///     function __destruct() {
///         echo "{$this->id}: start\n";
///         if ($this->id === 0) { global $held; $held = Fiber::getCurrent(); Fiber::suspend(); }
///         echo "{$this->id}: end\n";
///     }
/// }
/// $f = new Fiber(function () { new Cycle(0); new Cycle(1); gc_collect_cycles(); $GLOBALS['held']->resume(); });
/// $f->start();
/// ```
/// ```expect
/// 1: start
/// 1: end
/// 0: start
/// 0: end
/// ```
pub(crate) fn gc_destructors_in_fiber(req: &mut Request, from: u32, end: u32) -> bool {
    if req.fibers().active().is_none() {
        return false;
    }
    {
        let gc = &mut req.fibers_mut().gc;
        gc.at = from;
        gc.end = end;
    }
    let mut kept = None;
    remember_raised(req, &mut kept);
    match req.fibers().gc.fiber {
        Some(fiber) => {
            let back = switch_into::<ZendFiber>(req, fiber, Transfer::Value(Value::null()));
            settle(req, back);
        }
        None => start_gc_fiber(req),
    }
    remember_raised(req, &mut kept);
    while req.fibers().gc.running {
        // A destructor suspended the collector's fiber: it is the
        // program's from here, and the walk goes on after it in a new one.
        let gc = &mut req.fibers_mut().gc;
        let abandoned = gc.fiber.take();
        gc.at = gc.at.saturating_add(1);
        gc.running = false;
        if let Some(abandoned) = abandoned {
            req.release(abandoned.as_value());
        }
        remember_raised(req, &mut kept);
        start_gc_fiber(req);
        remember_raised(req, &mut kept);
    }
    req.set_raised(kept);
    true
}

/// `remember_prev_exception`: what is pending taken aside, with what was
/// taken aside before chained under it.
fn remember_raised(req: &mut Request, kept: &mut Option<Raised>) {
    let Some(now) = req.take_raised() else {
        return;
    };
    req.set_raised(Some(now));
    if let Some(before) = kept.take() {
        req.raise_under(before);
    }
    *kept = req.take_raised();
}

/// What a switch into the collector's fiber handed back, given up; a
/// throwable is left pending for [`remember_raised`].
fn settle(req: &mut Request, back: Result<Transfer, Thrown>) {
    match back {
        Ok(Transfer::Value(value)) => req.release(value),
        Ok(Transfer::Throw(thrown)) | Err(thrown) => req.leave_pending(&thrown),
        Ok(Transfer::GracefulExit | Transfer::Fatal) => {}
    }
}

/// `gc_create_destructor_fiber`: a `Fiber` whose callable is
/// `gc_destructor_fiber`, the collector's from here, started, which
/// serves the walk from where it stands up to its first suspend.
fn start_gc_fiber(req: &mut Request) {
    let Some(function) = req.unlisted_function(b"gc_destructor_fiber") else {
        return;
    };
    let Ok(fiber) = req.new_object_of("Fiber") else {
        return;
    };
    let callable = Callable::of(
        Value::null(),
        b"gc_destructor_fiber",
        crate::api::call::Target::Function(function),
    );
    if let Some(payload) = fiber.native_mut::<ZendFiber>(req) {
        payload.callable = Some(callable);
    }
    req.fibers_mut().gc.fiber = Some(fiber);
    let started = start(req, fiber, Vec::new(), Vec::new());
    settle(req, started);
}

/// `ZEND_FUNCTION(gc_destructor_fiber)` (`Z/Zend/zend_gc.c:2321`): the
/// walk served from where it stands to its end, each destructor from
/// its own entry; then the fiber suspends until the next collection
/// resumes it. A destructor that suspended it and was resumed later by
/// the program finds the fiber the collector's no more, and the
/// function returns, the object a possible root again. A fiber
/// destroyed at the request's end while it waits returns too.
fn gc_destructor_fiber(req: &mut Request) -> Result<Owner<Value>, Thrown> {
    let Some(fiber) = req.fibers().gc.fiber else {
        return Err(throw_error(
            req,
            "Error",
            "InternalError: gc_destructor_fiber served with no collector's fiber",
        ));
    };
    loop {
        req.fibers_mut().gc.running = true;
        let (from, end) = (req.fibers().gc.at, req.fibers().gc.end);
        for at in from..end {
            req.fibers_mut().gc.at = at;
            let served = req.gc_call_destructor(at);
            if let Some(object) = served
                && req.fibers().gc.fiber.map(|gc| gc.index()) != Some(fiber.index())
            {
                req.gc_possible_root(&object.as_value());
                // What the destructor left, the graceful exit of a destroy
                // among it, is what the fiber ends in.
                crate::api::raising::pending(req)?;
                return Ok(Owner::null());
            }
        }
        req.fibers_mut().gc.running = false;
        let back = suspend_into::<ZendFiber>(req, fiber, Transfer::Value(Value::null()))?;
        if payload(req, fiber)?.coroutine.destroyed {
            // Destroyed by the store's walk at the request's end: the
            // collector's owner given up and the fiber a possible root
            // again, php's `GC_DELREF` and `gc_check_possible_root`, so
            // the shutdown collection frees it. What destroys a fiber
            // holds an owner of its own across the destroy (the store's
            // walk and the destructor phase both do); where nothing does,
            // the collector's owner is the last and is kept, and the
            // request ends, since this runs on the fiber's own stack.
            let gc = &mut req.fibers_mut().gc;
            if gc.fiber.map(|gc| gc.index()) == Some(fiber.index()) {
                gc.fiber = None;
            }
            if !req.release_held(fiber.as_value()) {
                let fatal = php_heap::Fatal::error(format!(
                    "InternalError: the collector's fiber #{} destroyed with the collector's owner its last",
                    fiber.index()
                ));
                return req.fatal_of(Err(fatal));
            }
            return delegate(req, back);
        }
        if let Transfer::Value(value) = back {
            req.release(value);
        }
    }
}

/// `zend_fiber_start`: the stack laid down at `fiber.stack_size` for
/// a fiber not started yet, and its callable served on it with the
/// arguments up to its first suspend; what came back is the caller's.
/// The callable is given up once its call is done, and what it returned
/// is kept as `fiber->result`.
fn start(
    req: &mut Request,
    this: PhpObject,
    arguments: Vec<Value>,
    named: Vec<(PhpString, Value)>,
) -> Result<Transfer, Thrown> {
    let fiber = payload(req, this)?;
    if fiber.coroutine.status != CoroutineStatus::Init {
        return Err(fiber_error(
            req,
            "Cannot start a fiber that has already been started",
        ));
    }
    let Some(callable) = fiber.callable.clone() else {
        return Err(throw_error(
            req,
            "Error",
            "Cannot start a fiber whose constructor was not called",
        ));
    };
    let body = move |req: &mut Request| {
        let outcome = call_named_made(req, &callable, &arguments, &named);
        let held = payload(req, this)
            .ok()
            .and_then(|fiber| fiber.callable.take());
        if let Some(held) = held {
            req.release(held.value());
        }
        let value = outcome?;
        if let Ok(fiber) = payload(req, this) {
            let previous = std::mem::replace(&mut fiber.result, value);
            req.release(previous);
        }
        Ok(CoroutineEnd::Return(Owner::null()))
    };
    let error_reporting = req.ini_error_reporting();
    lay_down::<ZendFiber>(req, this, Kind::Fiber, error_reporting, body)?;
    switch_into::<ZendFiber>(req, this, Transfer::Value(Value::null()))
}

/// `zend_fiber_init_context` for the coroutine `object` holds: a stack of
/// `fiber.stack_size` laid down with `body` to serve on it
/// ([`execute`]), its VM page not yet, its first execution state at
/// `error_reporting`, and the stack record php allocates beside it; then
/// the observers' notice, no switch made yet. A size below two pages, or
/// a mapping the platform refuses, is php's `Exception`.
fn lay_down<P: HoldsCoroutine>(
    req: &mut Request,
    object: PhpObject,
    kind: Kind,
    error_reporting: i64,
    body: impl FnOnce(&mut Request) -> Result<CoroutineEnd, Thrown> + 'static,
) -> Result<(), Thrown> {
    let size = stack_size(req);
    let reserved = u64::try_from(Request::ini_parse_bytes(
        req.ini_get(b"zend.reserved_stack_size").unwrap_or(b"0"),
    ))
    .unwrap_or(0);
    let stack = FiberStack::new(size, move |req: &mut Request, _first: Transfer| {
        execute::<P>(req, object, body)
    });
    let stack = match stack {
        Ok(stack) => stack,
        Err(StackRefused::TooSmall(least)) => {
            return Err(throw(
                req,
                "Exception",
                &format!("Fiber stack size is too small, it needs to be at least {least} bytes"),
            ));
        }
        Err(StackRefused::Allocate(error)) => {
            let errno = error.raw_os_error().unwrap_or(0);
            let text = error.to_string();
            let text = text
                .strip_suffix(&format!(" (os error {errno})"))
                .unwrap_or(&text)
                .to_string();
            return Err(throw(
                req,
                "Exception",
                &format!("Fiber stack allocate failed: mmap failed: {text} ({errno})"),
            ));
        }
    };
    let execution = Execution {
        vm: Vm::fiber(),
        observing: ObservedChain::default(),
        error_reporting,
        stack_limit: StackLimit::fiber(stack.base(), stack.end(), reserved),
        // Taken from the first resume ([`switch_into`]).
        active: None,
    };
    let record = req.heap_mut().block(STACK_RECORD);
    let record = req.fatal_of(record)?;
    let coroutine = coroutine_of::<P>(req, object)?;
    coroutine.kind = kind;
    coroutine.record = Some(record);
    coroutine.stack = Some(stack);
    coroutine.execution = Some(Box::new(execution));
    coroutine.execute_data = ExecuteData::Bottom;
    let created = observed(req, Some(object), FiberStatus::Init);
    crate::observer::fiber_initialized(req, created);
    Ok(())
}

/// `zend_fiber_switch_blocked`: php's `FiberError` where a caller that
/// a switch would break stands, which `start`, `resume`, `throw` and
/// `suspend` each refuse to go past. The callers that block the switch
/// are a `ZEND_TICKS` round and `ext/pcntl`'s dispatch, each through
/// [`Request::with_fiber_switch_blocked`].
fn switch_refused(req: &mut Request) -> Result<(), Thrown> {
    if !req.fibers().switch_blocked() {
        return Ok(());
    }
    Err(fiber_error(req, SWITCH_BLOCKED))
}

/// `zend_fiber_delegate_transfer_result`: a value is the native's
/// return, one owner; an error transfer is raised where the switch
/// returned, the graceful exit among them; the `Bailout` is the fatal
/// already recorded.
fn delegate(req: &mut Request, transfer: Transfer) -> Result<Owner<Value>, Thrown> {
    match transfer {
        Transfer::Value(value) => Ok(Owner::adopt(value)),
        // `zend_throw_exception_internal`, which tells the hooks again of
        // a throwable a switch carried.
        Transfer::Throw(thrown) => Err(match thrown.exception().copied() {
            Some(object) => crate::api::raising::throw_object(req, object),
            None => thrown,
        }),
        Transfer::GracefulExit => {
            req.throw_graceful_exit();
            Err(Thrown::of(None))
        }
        Transfer::Fatal => Err(Thrown::of(None)),
    }
}

/// One of the four status tests, over no arguments.
fn status_is(
    req: &mut Request,
    this: PhpObject,
    test: fn(CoroutineStatus) -> bool,
) -> Result<bool, Thrown> {
    let status = payload(req, this)?.coroutine.status;
    Ok(test(status))
}

impl Coroutine {
    /// The coroutine's status, php's with `caller` folded in.
    #[must_use]
    pub fn status(&self) -> CoroutineStatus {
        self.status
    }

    /// The execution a coroutine off the running chain holds, where a
    /// kept frame of it is looked for ([`Request::in_frame`]): one
    /// suspended, or one running that waits on a suspend that passed
    /// through it, which holds its execution while it waits ([`run_on`]);
    /// one on the running chain holds none. None for one laid down and
    /// not started, which has no frame.
    pub(crate) fn held(&self) -> Option<&Execution> {
        match self.status {
            CoroutineStatus::Suspended | CoroutineStatus::Running => self.execution.as_deref(),
            CoroutineStatus::Init | CoroutineStatus::Dead => None,
        }
    }

    /// [`Coroutine::held`], taken out for the request to serve on
    /// ([`take_chain`]); [`Coroutine::hold`] puts it back.
    fn take_held(&mut self) -> Option<Box<Execution>> {
        self.held()?;
        self.execution.take()
    }

    /// What [`Coroutine::take_held`] took, held again.
    pub(crate) fn hold(&mut self, execution: Box<Execution>) {
        self.execution = Some(execution);
    }

    /// The coroutine whose frame this one's bottom frame names, php's
    /// `stack_bottom->prev_execute_data`, while a suspend that passed
    /// through it leaves it waiting: a `Fiber`'s resumer, `caller`, where
    /// a resume and not a destroy ran it. None for an extension's
    /// coroutine, whose bottom `zend_test_fiber_execute` zeroes, and for
    /// a suspended one, whose suspend cleared `caller`.
    fn linked_resumer(&self) -> Option<Context> {
        match (self.kind, self.caller) {
            (Kind::Fiber, Some(Caller::Coroutine(resumer))) if !self.destroyed => Some(resumer),
            _ => None,
        }
    }

    /// `zend_fiber_init_context` for an extension's coroutine, the one
    /// `object`'s payload holds and which has not started: a stack of
    /// `fiber.stack_size` laid down, with `body` to serve on it on the
    /// first [`Coroutine::resume`], and the fiber observers told of
    /// it. The body runs on a VM page of its own at the error-reporting
    /// level standing now, its bottom frame naming no resumer's, as
    /// `zend_test_fiber_execute` lays it; the value it returns is what
    /// that resume, or the last one, returns, a coroutine it ends into
    /// runs in its place ([`CoroutineEnd`]), and what it throws is raised
    /// there. A size below two pages, or a mapping the platform refuses,
    /// is php's `Exception`; a coroutine laid down before is the engine's
    /// fatal, as php asserts `ZEND_FIBER_STATUS_INIT`.
    pub fn init<P: HoldsCoroutine>(
        req: &mut Request,
        object: PhpObject,
        body: impl FnOnce(&mut Request) -> Result<CoroutineEnd, Thrown> + 'static,
    ) -> Result<(), Thrown> {
        let coroutine = coroutine_of::<P>(req, object)?;
        if coroutine.status != CoroutineStatus::Init || coroutine.stack.is_some() {
            return Err(crate::api::raising::fatal(
                req,
                "InternalError: a fiber coroutine laid down twice",
            ));
        }
        let error_reporting = req.error_reporting();
        lay_down::<P>(req, object, Kind::Extension, error_reporting, body)
    }

    /// `zend_fiber_switch_context` into the coroutine `object`'s payload
    /// holds, laid down and not running, with `value`: it runs from its
    /// start, or on from the suspend it waits in, which returns `value`,
    /// until it suspends or its body returns, and what it handed back
    /// is returned, or raised where it threw. A suspend made inside it
    /// of a coroutine below it passes through it ([`switch_into`]).
    pub fn resume<P: HoldsCoroutine>(
        req: &mut Request,
        object: PhpObject,
        value: Owner<Value>,
    ) -> Result<Owner<Value>, Thrown> {
        let back = switch_into::<P>(req, object, Transfer::Value(value.hand_on()))?;
        delegate(req, back)
    }

    /// The coroutine `object`'s payload holds, which runs, suspended back
    /// to what resumed it with `value`; this returns what the next resume
    /// sends, or raises the graceful exit where the coroutine is being
    /// destroyed. Coroutines running above it wait, and go on when it is
    /// resumed, as php's symmetric switch leaves them.
    pub fn suspend<P: HoldsCoroutine>(
        req: &mut Request,
        object: PhpObject,
        value: Owner<Value>,
    ) -> Result<Owner<Value>, Thrown> {
        let back = suspend_into::<P>(req, object, Transfer::Value(value.hand_on()))?;
        delegate(req, back)
    }

    /// What the coroutine `object`'s payload holds of the request's heap
    /// given back, the stack record of one that never ran to its end:
    /// what the object's `free_obj` does for its coroutine.
    pub fn release<P: HoldsCoroutine>(req: &mut Request, object: PhpObject) {
        let record = object
            .native_mut::<P>(req)
            .and_then(|holder| holder.coroutine().record.take());
        if let Some(record) = record {
            req.heap_mut().free_block(record);
        }
    }

    /// `zend_fiber_object_destroy` (`Z/Zend/zend_fibers.c:769`) and
    /// `zend_test_fiber_object_destroy`, for the coroutine `object`'s
    /// payload holds, what its `dtor_obj` does: a suspended coroutine is
    /// resumed with the graceful exit, marked destroyed so a suspend
    /// inside a `Fiber` is refused, and runs its `finally` blocks on the
    /// way out of every frame it holds, as an unwind would; what was
    /// pending before is kept aside meanwhile. A throwable the unwind
    /// raised is pending after it, with what was pending before chained
    /// under it, and where no frame stands to take it, at the request's
    /// end, it is rendered uncaught there (`zend_exception_error`). A
    /// coroutine that is not suspended is left alone, as php leaves it.
    ///
    /// ```php
    /// $f = new Fiber(function () {
    ///     try {
    ///         try { Fiber::suspend(); } finally { echo "inner finally\n"; }
    ///     } catch (Throwable $t) {
    ///         echo "never caught\n";
    ///     } finally {
    ///         try { Fiber::suspend(); } catch (FiberError $e) { echo $e->getMessage(), "\n"; }
    ///     }
    ///     echo "never\n";
    /// });
    /// $f->start();
    /// unset($f);
    /// echo "done\n";
    /// ```
    /// ```expect
    /// inner finally
    /// Cannot suspend in a force-closed fiber
    /// done
    /// ```
    pub fn destroy<P: HoldsCoroutine>(req: &mut Request, object: PhpObject) {
        let Some(holder) = object.native_mut::<P>(req) else {
            return;
        };
        let coroutine = holder.coroutine();
        if coroutine.status != CoroutineStatus::Suspended {
            return;
        }
        coroutine.destroyed = true;
        let before = req.take_raised();
        let transfer = switch_into::<P>(req, object, Transfer::GracefulExit);
        match transfer {
            Ok(Transfer::Throw(thrown)) => {
                req.leave_pending(&thrown);
                if let Some(before) = before {
                    req.raise_under(before);
                }
                if !req.executing() {
                    req.exception_error(crate::api::raising::Level::Error).ok();
                }
            }
            Ok(Transfer::Value(value)) => {
                req.release(value);
                if let Some(before) = before {
                    req.raise_under(before);
                }
            }
            Ok(Transfer::GracefulExit | Transfer::Fatal) | Err(_) => {
                if let Some(before) = before {
                    req.release_raised(before);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::engine::Engine;
    use crate::test::Capture;

    /// A fiber suspended inside a file its function included holds one
    /// symbol table on two frames, `f`'s and `body.php`'s, and
    /// [`Fiber::get_gc`] names it once, so a collection while the fiber
    /// is live takes no owner from what its variables hold: nothing is
    /// destructed inside `gc_collect_cycles()`, and both destructors are
    /// called at the request's end. php names the table twice and calls
    /// both inside the collection (#2582); `docs/divergence.toml`'s
    /// `trace:fiber-gc-shared-symbol-table` row records that as php's.
    #[test]
    fn a_collection_frees_nothing_a_table_two_suspended_frames_share_holds() {
        let dir = std::env::temp_dir().join(format!(
            "php_engine_fiber_shared_table_{}",
            std::process::id()
        ));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).ok();
        let main = dir.join("main.php");
        let source = r#"<?php
class D { public $self; function __construct(public $n) { $this->self = $this; } function __destruct() { echo "dtor ", $this->n, "\n"; } }
function f() { $c = new D('cv'); include __DIR__ . '/body.php'; }
$fiber = new Fiber(function () { f(); });
$fiber->start();
echo "collecting\n";
echo gc_collect_cycles(), "\n";
echo "collected\n";
"#;
        std::fs::write(&main, source).ok();
        std::fs::write(
            dir.join("body.php"),
            "<?php\n$k = new D('included'); $fib = Fiber::getCurrent(); Fiber::suspend();\n",
        )
        .ok();
        let engine = Engine::cli_builder().build();
        let served = engine.serve_capture_in(
            &dir,
            main.to_string_lossy().as_bytes(),
            source,
            Capture::default(),
        );
        assert_eq!(
            String::from_utf8_lossy(&served.stdout),
            "collecting\n0\ncollected\ndtor cv\ndtor included\n"
        );
        assert_eq!(served.exit_code, 0);
    }
}
