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

use php_compile::StackLimit;
use php_heap::{Block, FiberStack, Host, StackRefused, Suspensions, Switched};
use php_macros::php_class;

use crate::api::abi::{Callable, CloneObj, Instance, NamedVariadic, ObjectHandlers, This};
use crate::api::call::call_named_made;
use crate::api::raising::{Thrown, throw, throw_error};
use crate::api::request::{ObservedChain, Raised, Request};
use crate::api::values::{Owner, PhpObject, PhpString, Value};
use crate::classes::{Error, Throwable};
use crate::exec::{self, Vm};
use crate::observer::{FiberStatus, ObservedFiber};

/// `sizeof(zend_fiber_stack)` on a 64-bit build without Valgrind or
/// sanitizers: the pointer and the size `zend_fiber_stack_allocate`
/// `emalloc`s beside the mapping.
const STACK_RECORD: usize = 16;

/// `ZEND_FIBER_DEFAULT_C_STACK_SIZE` on a 64-bit target, what a fiber's
/// machine stack is where `fiber.stack_size` is unset
/// (`Z/Zend/zend.c:244-249`).
const DEFAULT_STACK: usize = 4096 * 512;

/// `zend_fiber_status`, with php's `fiber->caller` folded in: a fiber
/// php marks `SUSPENDED` because it resumed another, while its `caller`
/// still stands, is `Running` here, which is how `isRunning` and
/// `isSuspended` read the pair.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum Status {
    #[default]
    Init,
    Running,
    Suspended,
    Dead,
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
/// the error-reporting level and the call-stack limit; and the chain of
/// the stack's open observed frames, which php keeps on the
/// `zend_fiber_context` as `top_observed_frame` and swaps at the
/// switch's notice
/// ([`crate::observer`]).
#[derive(Default)]
pub(crate) struct Execution {
    pub(crate) stack: php_heap::VmStack,
    pub(crate) vm: Vm,
    pub(crate) observing: ObservedChain,
    pub(crate) error_reporting: i64,
    pub(crate) stack_limit: StackLimit,
}

/// One fiber that is running: the fiber, php's `EG(active_fiber)` while
/// it is the last, and the execution state of the stack that resumed
/// it, which its `stack_bottom->prev_execute_data` names.
struct Running {
    fiber: PhpObject,
    below: Execution,
    by: Resumer,
}

/// What resumed a running fiber, which is whether php's
/// `stack_bottom->prev_execute_data` names the resumer's frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Resumer {
    /// `zend_fiber_resume`, under `start`, `resume` and `throw`: the
    /// native's frame is the bottom's `prev_execute_data`, and the chain
    /// runs on into the resumer's.
    Native,
    /// `zend_fiber_object_destroy`, whose `zend_fiber_resume_internal`
    /// writes no `prev_execute_data`: the chain ends at the fiber, and a
    /// trace inside it closes with `{main}` after the fiber's callable.
    Destroy,
}

/// The request's fibers: the running ones, innermost last, and their
/// suspend points; and `EG(fiber_stack_size)` once `ini_set()` has
/// moved it.
#[derive(Default)]
pub(crate) struct Fibers {
    suspensions: Suspensions<Request>,
    running: Vec<Running>,
    stack_size: Option<usize>,
    gc: GcFiber,
    /// `zend_fiber_switch_blocking`: how many of php's
    /// `zend_fiber_switch_block()` stand unmatched, each a caller a fiber
    /// switch would break (a `ZEND_TICKS` round, a signal dispatch),
    /// while which no fiber starts, suspends or resumes.
    switch_blocking: u32,
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

    /// `EG(active_fiber)`: the innermost fiber running, none on the
    /// request's own stack.
    pub(crate) fn active(&self) -> Option<PhpObject> {
        self.running.last().map(|running| running.fiber)
    }

    /// The request's own VM stack, below every fiber running, which is
    /// the one `EG(symbol_table)`'s attached CVs stand on; none while no
    /// fiber runs and the request runs on it.
    pub(crate) fn request_stack(&self) -> Option<&php_heap::VmStack> {
        self.running.first().map(|running| &running.below.stack)
    }

    /// [`Fibers::request_stack`], to write.
    pub(crate) fn request_stack_mut(&mut self) -> Option<&mut php_heap::VmStack> {
        self.running
            .first_mut()
            .map(|running| &mut running.below.stack)
    }

    /// The execution states below the running stack, the innermost
    /// resumer's first and the request's own last: the rest of php's
    /// one `prev_execute_data` chain, which ends at a fiber a destroy
    /// resumed.
    pub(crate) fn below(&self) -> impl Iterator<Item = &Execution> {
        self.running
            .iter()
            .rev()
            .take_while(|running| running.by == Resumer::Native)
            .map(|running| &running.below)
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
    status: Status,
    /// `ZEND_FIBER_FLAG_THREW`: the callable left a throwable or the
    /// unwind exit.
    threw: bool,
    /// `ZEND_FIBER_FLAG_BAILOUT`: the fiber ended in a fatal.
    bailout: bool,
    /// `ZEND_FIBER_FLAG_DESTROYED`: the fiber is being torn down.
    destroyed: bool,
    /// `fci` and `fci_cache`, the callable one owner the fiber holds
    /// until its body is done with it.
    callable: Option<Callable>,
    /// The machine stack while the fiber is suspended, and before it
    /// starts; none while it runs, since the resume that is running it
    /// holds it, and none once it is dead.
    stack: Option<FiberStack<Request>>,
    /// The fiber's own execution state while it is suspended; the
    /// request holds it while it runs.
    execution: Option<Box<Execution>>,
    /// `fiber->result`, one owner, `null` until the callable returns.
    result: Value,
    /// The `emalloc` php makes for a started fiber beside its VM stack's
    /// pages, the stack record, counted by the heap as php's is.
    blocks: Vec<Block>,
}

/// `zend_fiber_handlers`: `dtor_obj`, `free_obj` and `get_gc` php's,
/// `clone_obj` null.
struct FiberHandlers;

impl ObjectHandlers for FiberHandlers {
    /// `zend_fiber_object_destroy`: a suspended fiber is torn down.
    fn dtor_obj(&self, req: &mut Request, object: PhpObject) {
        destroy(req, object);
    }

    /// `zend_fiber_object_free`: the callable and `fiber->result` given up.
    fn free_obj(&self, req: &mut Request, object: PhpObject) {
        let Some(fiber) = object.native_mut::<ZendFiber>(req) else {
            return;
        };
        let callable = fiber.callable.take();
        let result = std::mem::replace(&mut fiber.result, Value::null());
        let blocks = std::mem::take(&mut fiber.blocks);
        if let Some(callable) = callable {
            req.release(callable.value());
        }
        req.release(result);
        for block in blocks {
            req.heap_mut().free_block(block);
        }
    }

    /// `clone_obj = NULL`: a fiber is uncloneable.
    fn clone_obj(&self) -> Option<CloneObj> {
        None
    }

    /// `zend_fiber_object_gc` (`Z/Zend/zend_fibers.c:817`): the callable
    /// and `fiber->result`, and for a suspended fiber every frame it
    /// holds, innermost first, each by [`exec::unfinished_execution_gc`]
    /// or, for a generator running on it, by
    /// [`crate::generator::running_frame_gc`]; then the calls its frames
    /// were building. Each is an owner the fiber's frames hold, so a
    /// cycle through a suspended fiber's variables is found. A running
    /// fiber's frames are the running execution's, and are not named.
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
    /// A variable a frame made by name lives in its symbol table, which
    /// the walk names too:
    ///
    /// ```php
    /// class C { function __destruct() { echo "C::__destruct\n"; } }
    /// function f() { $c = new C; $y = 'a'; $$y = Fiber::getCurrent(); Fiber::suspend(); }
    /// $fiber = new Fiber(function () { f(); });
    /// $fiber->start();
    /// $fiber = null;
    /// gc_collect_cycles();
    /// echo "done\n";
    /// ```
    /// ```expect
    /// C::__destruct
    /// done
    /// ```
    fn get_gc(&self, req: &mut Request, object: PhpObject, visit: &mut dyn FnMut(Value)) {
        let Some(fiber) = object.native::<ZendFiber>(req) else {
            return;
        };
        if let Some(callable) = &fiber.callable {
            visit(callable.value());
        }
        visit(fiber.result);
        if fiber.status != Status::Suspended {
            return;
        }
        let Some(execution) = &fiber.execution else {
            return;
        };
        for (at, state) in execution.vm.frames().iter().enumerate().rev() {
            let frame = php_heap::FrameRef(u32::try_from(at).unwrap_or(u32::MAX));
            let Some(slots) = execution.stack.frame(frame) else {
                continue;
            };
            match state.generator {
                Some(generator) => {
                    crate::generator::running_frame_gc(req, generator, slots, state, visit);
                }
                None => exec::unfinished_execution_gc(req, slots, state, &[], visit),
            }
        }
        exec::unfinished_calls_gc(execution.vm.calls(), visit);
    }
}

static FIBER: FiberHandlers = FiberHandlers;

/// Whether `table` is `Fiber`'s, whose `dtor_obj` is
/// `zend_fiber_object_destroy` and not `zend_objects_destroy_object`,
/// so the cycle collector serves it a destructor whatever the class
/// declares.
pub(crate) fn serves_destructor(table: &'static dyn ObjectHandlers) -> bool {
    std::ptr::addr_eq(table, &raw const FIBER)
}

/// Where a fiber's execution stands, php's `fiber->execute_data`, for
/// a view of it ([`crate::api::request::Execution`]).
pub(crate) enum Standing<'a> {
    /// The fiber runs innermost, `EG(active_fiber)`: the stack the
    /// request runs on is its.
    Active,
    /// The fiber is suspended, or runs below a fiber it resumed: the
    /// execution it left, which the fiber holds, or that resume.
    Held(&'a Execution),
}

/// A fiber as a view of it reads it: its callable, php's `fci`, from its
/// construction until it is dead, and where its execution stands, none
/// before it starts and once it is dead. None for an object that is no
/// fiber.
pub(crate) fn view(
    req: &Request,
    object: PhpObject,
) -> Option<(Option<Callable>, Option<Standing<'_>>)> {
    let fiber = object.native::<ZendFiber>(req)?;
    let standing = match fiber.status {
        Status::Init | Status::Dead => None,
        Status::Suspended => fiber.execution.as_deref().map(Standing::Held),
        Status::Running => {
            let running = &req.fibers().running;
            running
                .iter()
                .position(|running| running.fiber.index() == object.index())
                .map(|at| match running.get(at.saturating_add(1)) {
                    Some(above) => Standing::Held(&above.below),
                    None => Standing::Active,
                })
        }
    };
    Some((fiber.callable.clone(), standing))
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
        if fiber.status != Status::Init || fiber.callable.is_some() {
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
        if payload(req, this)?.status != Status::Suspended {
            return Err(fiber_error(
                req,
                "Cannot resume a fiber that is not suspended",
            ));
        }
        req.addref(&value);
        let back = switch_into(req, this, Transfer::Value(value))?;
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
        if payload(req, this)?.status != Status::Suspended {
            return Err(fiber_error(
                req,
                "Cannot resume a fiber that is not suspended",
            ));
        }
        req.addref(&throwable.as_value());
        let back = switch_into(req, this, Transfer::Throw(Thrown::of(Some(throwable))))?;
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
        status_is(req, this.object(), |status| status != Status::Init)
    }

    fn isSuspended(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        status_is(req, this.object(), |status| status == Status::Suspended)
    }

    fn isRunning(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        status_is(req, this.object(), |status| status == Status::Running)
    }

    fn isTerminated(req: &mut Request, this: This<Self>) -> Result<bool, Thrown> {
        status_is(req, this.object(), |status| status == Status::Dead)
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
        let message = match fiber.status {
            Status::Dead if fiber.threw => "The fiber threw an exception",
            Status::Dead if fiber.bailout => "The fiber exited with a fatal error",
            Status::Dead => return Ok(fiber.result.deref()),
            Status::Init => "The fiber has not been started",
            Status::Running | Status::Suspended => "The fiber has not returned",
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
        if payload(req, fiber)?.destroyed {
            return Err(fiber_error(req, "Cannot suspend in a force-closed fiber"));
        }
        switch_refused(req)?;
        req.addref(&value);
        let back = php_heap::suspend(req, Transfer::Value(value));
        let back = req.fatal_of(back)?;
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

/// `zend_fiber_execute`, on the fiber's own stack: the first VM page
/// laid down, the callable called with the start arguments, the
/// callable given up after it, and what it returned kept as
/// `fiber->result`; a throwable or the unwind exit it left is carried
/// out as an error transfer and a fatal as the `Bailout`. The value the
/// transfer carries when the callable returns is `null`, which is what
/// the `start` or `resume` that ran it last returns.
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
fn run(
    req: &mut Request,
    object: PhpObject,
    callable: &Callable,
    arguments: &[Value],
    named: &[(PhpString, Value)],
) -> Transfer {
    let opened = req.open_stack();
    if req.fatal_of(opened).is_err() {
        if let Some(fiber) = object.native_mut::<ZendFiber>(req) {
            fiber.bailout = true;
        }
        return Transfer::Fatal;
    }
    let outcome = call_named_made(req, callable, arguments, named);
    let held = object
        .native_mut::<ZendFiber>(req)
        .and_then(|fiber| fiber.callable.take());
    if let Some(held) = held {
        req.release(held.value());
    }
    match outcome {
        Ok(value) => {
            if let Some(fiber) = object.native_mut::<ZendFiber>(req) {
                let previous = std::mem::replace(&mut fiber.result, value);
                req.release(previous);
            }
            Transfer::Value(Value::null())
        }
        Err(thrown) => {
            let exited = matches!(
                req.raised(),
                Some(Raised::UnwindExit | Raised::GracefulExit)
            );
            let destroyed = object
                .native::<ZendFiber>(req)
                .is_some_and(|fiber| fiber.destroyed);
            // A fiber being destroyed that ends in either exit ended as
            // it was asked to, and the exit is cleared, whatever `exit`
            // its `finally` blocks called.
            if destroyed && exited {
                req.take_raised();
                return Transfer::Value(Value::null());
            }
            let threw = thrown.exception().is_some() || exited;
            if let Some(fiber) = object.native_mut::<ZendFiber>(req) {
                if threw {
                    fiber.threw = true;
                } else {
                    fiber.bailout = true;
                }
            }
            if threw {
                Transfer::Throw(thrown)
            } else {
                Transfer::Fatal
            }
        }
    }
}

/// `zend_fiber_resume_internal` and the resumer's half of
/// `zend_fiber_switch_context`: the fiber's execution state swapped in
/// with the resumer's kept beside it as the stack below, the machine
/// switch, and the two swapped back when the fiber suspends or returns.
/// A fiber that returned is dead, its stack and its VM page given back
/// (`zend_fiber_destroy_context`).
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
fn switch_into(
    req: &mut Request,
    object: PhpObject,
    transfer: Transfer,
) -> Result<Transfer, Thrown> {
    let fiber = payload(req, object)?;
    let (Some(mut stack), Some(execution)) = (fiber.stack.take(), fiber.execution.take()) else {
        return Err(throw_error(
            req,
            "Error",
            "InternalError: a fiber resumed with no stack of its own",
        ));
    };
    let entered = if fiber.status == Status::Init {
        FiberStatus::Init
    } else {
        FiberStatus::Suspended
    };
    let by = match transfer {
        Transfer::GracefulExit => Resumer::Destroy,
        Transfer::Value(_) | Transfer::Throw(_) | Transfer::Fatal => Resumer::Native,
    };
    // `EG(current_fiber_context)`: the fiber served now, or the
    // request's own stack, which the switch leaves and comes back to.
    // The handlers are called before the fiber is marked running, as
    // php's notice comes before `to->status = ZEND_FIBER_STATUS_RUNNING`.
    let resumer = req.fibers().active();
    let left = observed(req, resumer, FiberStatus::Running);
    let entering = observed(req, Some(object), entered);
    crate::observer::fiber_switched(req, left, entering);
    payload(req, object)?.status = Status::Running;
    let mut execution = *execution;
    req.switch_execution(&mut execution);
    req.fibers_mut().running.push(Running {
        fiber: object,
        below: execution,
        by,
    });
    let switched = stack.resume(req, transfer);
    // The switch back is noticed on the fiber's side, its frames the
    // ones standing, as php notices it before the jump.
    let left = if matches!(switched, Ok(Switched::Suspended(_))) {
        FiberStatus::Running
    } else {
        FiberStatus::Dead
    };
    let leaving = observed(req, Some(object), left);
    let back = observed(req, resumer, FiberStatus::Suspended);
    crate::observer::fiber_switched(req, leaving, back);
    let mut execution = match req.fibers_mut().running.pop() {
        Some(running) => running.below,
        None => Execution::default(),
    };
    req.switch_execution(&mut execution);
    if left == FiberStatus::Dead {
        crate::observer::fiber_destroyed(req, leaving);
    }
    let fiber = payload(req, object)?;
    match switched {
        Ok(Switched::Suspended(back)) => {
            fiber.status = Status::Suspended;
            fiber.stack = Some(stack);
            fiber.execution = Some(Box::new(execution));
            Ok(back)
        }
        Ok(Switched::Returned(back)) => {
            fiber.status = Status::Dead;
            let blocks = std::mem::take(&mut fiber.blocks);
            for block in blocks {
                req.heap_mut().free_block(block);
            }
            execution.stack.close(req.heap_mut());
            if matches!(back, Transfer::Fatal) {
                // `zend_fiber_switch_to` raises the fiber's bailout again
                // on the resumer, which leaves its own open frames to
                // `zend_observer_fcall_end_all`; the fatal is rendered,
                // and the frames still stand.
                crate::observer::end_all(req);
            }
            Ok(back)
        }
        Err(fatal) => {
            fiber.status = Status::Dead;
            execution.stack.close(req.heap_mut());
            req.fatal_of::<()>(Err(fatal))?;
            Ok(Transfer::Fatal)
        }
    }
}

/// `fiber` as a fiber observer is handed it at a notice, with `status`,
/// php's at that point, and whether it threw or is being destroyed; the
/// request's own stack where `fiber` is none.
fn observed(req: &Request, fiber: Option<PhpObject>, status: FiberStatus) -> ObservedFiber {
    let flags = fiber
        .and_then(|object| object.native::<ZendFiber>(req))
        .map_or((false, false), |payload| (payload.threw, payload.destroyed));
    ObservedFiber::of(fiber, status).flagged(flags.0, flags.1)
}

/// The natives no function table carries that fibers serve:
/// `gc_destructor_fiber`, the collector's fiber's function
/// ([`crate::engine::MethodAt::Unlisted`]).
pub(crate) fn unlisted_functions() -> Vec<(php_unit::decl::MethodDecl, crate::api::abi::Entry)> {
    let function = php_unit::decl::FunctionDecl {
        name: php_unit::decl::Name(b"gc_destructor_fiber".as_slice().into()),
        parameters: Vec::new(),
        return_type: None,
        returns_by_ref: false,
        is_generator: false,
        attributes: Vec::new(),
        doc_comment: None,
        file: php_unit::decl::Name(Box::new([])),
        line: 0,
        end_line: 0,
        frameless: Vec::new(),
    };
    let method = php_unit::decl::MethodDecl {
        function,
        visibility: php_unit::decl::Visibility::Public,
        is_instance: false,
        is_abstract: false,
        is_final: false,
        tentative_return: false,
    };
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
            let back = switch_into(req, fiber, Transfer::Value(Value::null()));
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
fn gc_destructor_fiber(req: &mut Request) -> Result<Value, Thrown> {
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
                req.gc_possible_root(object);
                // What the destructor left, the graceful exit of a destroy
                // among it, is what the fiber ends in.
                crate::api::raising::pending(req)?;
                return Ok(Value::null());
            }
        }
        req.fibers_mut().gc.running = false;
        let back = php_heap::suspend(req, Transfer::Value(Value::null()));
        let back = req.fatal_of(back)?;
        if payload(req, fiber)?.destroyed {
            // Destroyed by the store's walk at the request's end: the
            // collector's owner is dropped with the store, as php's
            // `GC_DELREF` drops it without freeing what it is running on.
            let gc = &mut req.fibers_mut().gc;
            if gc.fiber.map(|gc| gc.index()) == Some(fiber.index()) {
                gc.fiber = None;
            }
            return delegate(req, back).map(Owner::hand_on);
        }
        if let Transfer::Value(value) = back {
            req.release(value);
        }
    }
}

/// `zend_fiber_start`: the stack laid down at `fiber.stack_size` for
/// a fiber not started yet, and its callable served on it with the
/// arguments up to its first suspend; what came back is the caller's.
fn start(
    req: &mut Request,
    this: PhpObject,
    arguments: Vec<Value>,
    named: Vec<(PhpString, Value)>,
) -> Result<Transfer, Thrown> {
    let size = stack_size(req);
    let reserved = u64::try_from(Request::ini_parse_bytes(
        req.ini_get(b"zend.reserved_stack_size").unwrap_or(b"0"),
    ))
    .unwrap_or(0);
    let fiber = payload(req, this)?;
    if fiber.status != Status::Init {
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
    let stack = FiberStack::new(size, move |req: &mut Request, _first: Transfer| {
        run(req, this, &callable, &arguments, &named)
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
        stack: php_heap::VmStack::fiber(),
        vm: Vm::default(),
        observing: ObservedChain::default(),
        error_reporting: req.ini_error_reporting(),
        stack_limit: StackLimit::fiber(stack.base(), stack.end(), reserved),
    };
    let record = req.heap_mut().block(STACK_RECORD);
    let record = req.fatal_of(record)?;
    let fiber = payload(req, this)?;
    fiber.blocks.push(record);
    fiber.stack = Some(stack);
    fiber.execution = Some(Box::new(execution));
    // `zend_fiber_init_context` ends in the observers' notice, its
    // stack laid down and no switch made yet.
    let created = observed(req, Some(this), FiberStatus::Init);
    crate::observer::fiber_initialized(req, created);
    switch_into(req, this, Transfer::Value(Value::null()))
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
        Transfer::Value(value) => Ok(Owner::adopt(req, value)),
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
fn status_is(req: &mut Request, this: PhpObject, test: fn(Status) -> bool) -> Result<bool, Thrown> {
    let status = payload(req, this)?.status;
    Ok(test(status))
}

/// `zend_fiber_object_destroy` (`Z/Zend/zend_fibers.c:769`): a
/// suspended fiber is resumed with the graceful exit, marked destroyed
/// so a suspend inside it is refused, and runs its `finally` blocks on
/// the way out of every frame it holds, as an unwind would; what was
/// pending before is kept aside meanwhile. A throwable the unwind
/// raised is pending after it, with what was pending before chained
/// under it, and where no frame stands to take it, at the request's
/// end, it is rendered uncaught there (`zend_exception_error`). A
/// fiber that is not suspended is left alone, as php leaves it.
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
fn destroy(req: &mut Request, object: PhpObject) {
    let Some(fiber) = object.native_mut::<ZendFiber>(req) else {
        return;
    };
    if fiber.status != Status::Suspended {
        return;
    }
    fiber.destroyed = true;
    let before = req.take_raised();
    let transfer = switch_into(req, object, Transfer::GracefulExit);
    match transfer {
        Ok(Transfer::Throw(thrown)) => {
            req.leave_pending(&thrown);
            if let Some(before) = before {
                req.raise_under(before);
            }
            if !req.executing()
                && let Err(fatal) = req.exception_error(crate::api::raising::Level::Error)
            {
                req.end_in(&fatal);
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
