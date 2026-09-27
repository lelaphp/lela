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
use crate::api::call::call_named;
use crate::api::raising::{Thrown, throw};
use crate::api::request::Request;
use crate::api::values::{PhpObject, PhpString, Value};
use crate::classes::{Error, Throwable};
use crate::exec::Vm;

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
/// left standing, which rides the request; or the `Bailout` of a fatal
/// the fiber already recorded (`ZEND_FIBER_TRANSFER_FLAG_BAILOUT`).
#[derive(Debug)]
pub enum Transfer {
    Value(Value),
    Throw(Thrown),
    Fatal,
}

/// `zend_fiber_vm_state` under this engine's names: what a switch
/// swaps, the stack the request runs on with the frames' engine half,
/// the error-reporting level and the call-stack limit.
#[derive(Default)]
pub(crate) struct Execution {
    pub(crate) stack: php_heap::VmStack,
    pub(crate) vm: Vm,
    pub(crate) error_reporting: i64,
    pub(crate) stack_limit: StackLimit,
}

/// One fiber that is running: the fiber, php's `EG(active_fiber)` while
/// it is the last, and the execution state of the stack that resumed
/// it, which its `stack_bottom->prev_execute_data` names.
struct Running {
    fiber: PhpObject,
    below: Execution,
}

/// The request's fibers: the running ones, innermost last, and their
/// suspend points; and `EG(fiber_stack_size)` once `ini_set()` has
/// moved it.
#[derive(Default)]
pub(crate) struct Fibers {
    suspensions: Suspensions<Request>,
    running: Vec<Running>,
    stack_size: Option<usize>,
}

impl Fibers {
    /// `EG(active_fiber)`: the innermost fiber running, none on the
    /// request's own stack.
    pub(crate) fn active(&self) -> Option<PhpObject> {
        self.running.last().map(|running| running.fiber)
    }

    /// The execution states below the running stack, the innermost
    /// resumer's first and the request's own last: the rest of php's
    /// one `prev_execute_data` chain.
    pub(crate) fn below(&self) -> impl Iterator<Item = &Execution> {
        self.running.iter().rev().map(|running| &running.below)
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
    execution: Option<Execution>,
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
    /// holds, innermost first, as `zend_unfinished_execution_gc_ex`
    /// names each: the receiver and the closure, the slots, the extra
    /// arguments and the extra named ones; then the calls its frames
    /// were building, their receivers, closures and arguments
    /// (`zend_unfinished_calls_gc`). Each is an owner the fiber's frames
    /// hold, so a cycle through a suspended fiber's variables is found.
    /// A running fiber's frames are the running execution's, and are
    /// not named.
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
            if let Some(this) = state.this {
                visit(this.as_value());
            }
            if let Some(closure) = state.closure {
                visit(closure.as_value());
            }
            let frame = php_heap::FrameRef(u32::try_from(at).unwrap_or(u32::MAX));
            if let Some(slots) = execution.stack.frame(frame) {
                for slot in 0..slots.len() {
                    let slot = php_heap::Slot(u32::try_from(slot).unwrap_or(u32::MAX));
                    visit(Value::of_zval(slots.slot(slot)));
                }
            }
            for extra in &state.extra_args {
                visit(*extra);
            }
            for (_, value) in &state.extra_named {
                visit(*value);
            }
        }
        for call in execution.vm.calls().iter().rev() {
            if let Some(this) = call.this {
                visit(this.as_value());
            }
            if let Some(closure) = call.closure {
                visit(closure.as_value());
            }
            for argument in &call.arguments {
                visit(*argument);
            }
            for (_, value) in &call.extra_named {
                visit(*value);
            }
        }
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

/// The payload of `object`, which a `Fiber`'s object always carries.
fn payload(req: &mut Request, object: PhpObject) -> Result<&mut ZendFiber, Thrown> {
    if object.native::<ZendFiber>(req).is_none() {
        return Err(throw(req, "Error", "Fiber object with no fiber payload"));
    }
    object
        .native_mut::<ZendFiber>(req)
        .ok_or_else(|| Thrown::of(None))
}

/// `FiberError` with php's sentence.
fn fiber_error(req: &mut Request, message: &str) -> Thrown {
    throw(req, "FiberError", message)
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
#[php_class(
    final,
    not_serializable,
    dynamic_properties = forbidden,
    payload = ZendFiber,
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
    ) -> Result<Value, Thrown> {
        let named: Vec<(PhpString, Value)> = args.named().to_vec();
        let arguments = args.positional;
        let this = this.object();
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
            return Err(throw(
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
                    &format!(
                        "Fiber stack size is too small, it needs to be at least {least} bytes"
                    ),
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
            error_reporting: req.ini_error_reporting(),
            stack_limit: StackLimit::fiber(stack.base(), stack.end(), reserved),
        };
        let record = req.heap_mut().block(STACK_RECORD);
        let record = req.fatal_of(record)?;
        let fiber = payload(req, this)?;
        fiber.blocks.push(record);
        fiber.stack = Some(stack);
        fiber.execution = Some(execution);
        let back = switch_into(req, this, Transfer::Value(Value::null()))?;
        delegate(back)
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
    ) -> Result<Value, Thrown> {
        let this = this.object();
        if payload(req, this)?.status != Status::Suspended {
            return Err(fiber_error(
                req,
                "Cannot resume a fiber that is not suspended",
            ));
        }
        req.addref(&value);
        let back = switch_into(req, this, Transfer::Value(value))?;
        delegate(back)
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
    ) -> Result<Value, Thrown> {
        let throwable = exception.object();
        let this = this.object();
        if payload(req, this)?.status != Status::Suspended {
            return Err(fiber_error(
                req,
                "Cannot resume a fiber that is not suspended",
            ));
        }
        req.addref(&throwable.as_value());
        let back = switch_into(req, this, Transfer::Throw(Thrown::of(Some(throwable))))?;
        delegate(back)
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
            Status::Dead => {
                let value = fiber.result.deref();
                req.addref(&value);
                return Ok(value);
            }
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
        req.addref(&fiber.as_value());
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
    fn suspend(req: &mut Request, #[php(default = null)] value: Value) -> Result<Value, Thrown> {
        let Some(fiber) = req.fibers().active() else {
            return Err(fiber_error(req, "Cannot suspend outside of a fiber"));
        };
        if payload(req, fiber)?.destroyed {
            return Err(fiber_error(req, "Cannot suspend in a force-closed fiber"));
        }
        req.addref(&value);
        let back = php_heap::suspend(req, Transfer::Value(value));
        let back = req.fatal_of(back)?;
        delegate(back)
    }
}

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
    fn __construct(req: &mut Request, this: This<Self>) -> Result<(), Thrown> {
        let name = req.class_name(this.object().class());
        Err(throw(
            req,
            "Error",
            &format!(
                "The \"{name}\" class is reserved for internal use and cannot be manually instantiated"
            ),
        ))
    }
}

/// `EG(fiber_stack_size)`: what `OnUpdateFiberStackSize` last wrote,
/// the startup entry's quantity, or php's 2 MiB where the entry is
/// unset. php's table holds `NULL` there, which the update reads as the
/// default, where an empty value given to `ini_set()` is a quantity of
/// zero and refused at the next `start`.
fn stack_size(req: &Request) -> usize {
    if let Some(size) = req.fibers().stack_size {
        return size;
    }
    match req.ini_get(b"fiber.stack_size") {
        Some(value) if !value.is_empty() => {
            usize::try_from(Request::ini_parse_bytes(value)).unwrap_or(DEFAULT_STACK)
        }
        _ => DEFAULT_STACK,
    }
}

/// `OnUpdateFiberStackSize` (`Z/Zend/zend.c:238-252`) for an
/// `ini_set()`: the value read as a quantity, a negative one php's
/// warning and a refusal that keeps the entry as it was, and anything
/// else the size the next `start` lays down.
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
pub(crate) fn stack_size_updated(req: &mut Request, value: &[u8]) -> bool {
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
    let outcome = call_named(req, callable, arguments, named);
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
            let threw = thrown.exception().is_some() || req.unwind_exit();
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
    let (Some(mut stack), Some(mut execution)) = (fiber.stack.take(), fiber.execution.take())
    else {
        return Err(throw(
            req,
            "Error",
            "InternalError: a fiber resumed with no stack of its own",
        ));
    };
    fiber.status = Status::Running;
    req.switch_execution(&mut execution);
    req.fibers_mut().running.push(Running {
        fiber: object,
        below: execution,
    });
    let switched = stack.resume(req, transfer);
    let mut execution = match req.fibers_mut().running.pop() {
        Some(running) => running.below,
        None => Execution::default(),
    };
    req.switch_execution(&mut execution);
    let fiber = payload(req, object)?;
    match switched {
        Ok(Switched::Suspended(back)) => {
            fiber.status = Status::Suspended;
            fiber.stack = Some(stack);
            fiber.execution = Some(execution);
            Ok(back)
        }
        Ok(Switched::Returned(back)) => {
            fiber.status = Status::Dead;
            let blocks = std::mem::take(&mut fiber.blocks);
            for block in blocks {
                req.heap_mut().free_block(block);
            }
            execution.stack.close(req.heap_mut());
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

/// `zend_fiber_delegate_transfer_result`: a value is the native's
/// return, one owner; an error transfer is raised where the switch
/// returned; the `Bailout` is the fatal already recorded.
fn delegate(transfer: Transfer) -> Result<Value, Thrown> {
    match transfer {
        Transfer::Value(value) => Ok(value),
        Transfer::Throw(thrown) => Err(thrown),
        Transfer::Fatal => Err(Thrown::of(None)),
    }
}

/// One of the four status tests, over no arguments.
fn status_is(req: &mut Request, this: PhpObject, test: fn(Status) -> bool) -> Result<bool, Thrown> {
    let status = payload(req, this)?.status;
    Ok(test(status))
}

/// `zend_fiber_object_destroy` for a fiber that is suspended, in the
/// shape this engine can give it until its unwinder has php's graceful
/// exit: the machine stack unwound without a PHP frame being served on
/// it, then the fiber's own execution state swapped in as a resume
/// would swap it, with the fiber the active one and marked destroyed so
/// a destructor that asks to suspend is refused as php refuses it, and
/// every frame and call on it given up innermost first; the fiber is
/// dead after it. A fiber that is not suspended is left alone, as php
/// leaves it.
///
/// ```php
/// class D { public function __destruct() { echo "destructed\n"; } }
/// $f = new Fiber(function () { $d = new D; Fiber::suspend(); echo "never\n"; });
/// $f->start();
/// unset($f);
/// echo "done\n";
/// ```
/// ```expect
/// destructed
/// done
/// ```
fn destroy(req: &mut Request, object: PhpObject) {
    if req.destructors_marked() {
        return;
    }
    let Some(fiber) = object.native_mut::<ZendFiber>(req) else {
        return;
    };
    if fiber.status != Status::Suspended {
        return;
    }
    fiber.destroyed = true;
    let stack = fiber.stack.take();
    let execution = fiber.execution.take();
    // The machine stack goes first: its frames are Rust's alone and
    // hold no owner the request counts, so unwinding them gives up
    // nothing a program sees.
    drop(stack);
    let Some(mut execution) = execution else {
        return;
    };
    req.switch_execution(&mut execution);
    req.fibers_mut().running.push(Running {
        fiber: object,
        below: execution,
    });
    loop {
        let calls = req.vm().calls().len();
        let above = req
            .vm()
            .frames()
            .last()
            .map_or(0, |frame| usize::try_from(frame.calls_below).unwrap_or(0));
        if calls > above {
            req.pop_call();
        } else if let Some(frame) = req.vm().frames().last() {
            // A generator resumed inside the fiber has its frame here,
            // and is closed with it rather than left pointing at a frame
            // no stack holds.
            match frame.generator {
                Some(generator) => crate::generator::close_unwound(req, generator),
                None => req.pop_frame(),
            }
        } else {
            break;
        }
    }
    let mut execution = match req.fibers_mut().running.pop() {
        Some(running) => running.below,
        None => Execution::default(),
    };
    req.switch_execution(&mut execution);
    execution.stack.close(req.heap_mut());
    let Some(fiber) = object.native_mut::<ZendFiber>(req) else {
        return;
    };
    fiber.status = Status::Dead;
    let callable = fiber.callable.take();
    let blocks = std::mem::take(&mut fiber.blocks);
    if let Some(callable) = callable {
        req.release(callable.value());
    }
    for block in blocks {
        req.heap_mut().free_block(block);
    }
}
