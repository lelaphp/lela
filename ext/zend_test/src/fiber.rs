//! `fiber.c`'s `_ZendTestFiber`: a fiber of the extension's own, laid
//! over the engine's fiber coroutines ([`Coroutine`]) as php's is over
//! `zend_fiber_context`, which interleaves with `Fiber`: a
//! `Fiber::suspend()` inside a test fiber inside a `Fiber` suspends the
//! `Fiber` and leaves the test fiber waiting above it, and
//! `_ZendTestFiber::suspend()` inside a `Fiber` inside a test fiber does
//! the reverse. `ZT_G(active_fiber)`, the test fiber a static `suspend`
//! leaves, is the extension's request state.

use php_ext::abi::{
    Callable, CloneObj, Coroutine, CoroutineStatus, DtorObj, ExtensionBuilder, HoldsCoroutine,
    Instance, NamedVariadic, ObjectHandlers, This,
};
use php_ext::attr::php_class;
use php_ext::call::{call_named, new_object};
use php_ext::raising::{Thrown, throw_error};
use php_ext::request::Request;
use php_ext::values::{Owner, PhpObject, PhpString, Value};

use crate::State;

/// `zend_test_fiber_init`: the class and its handlers.
pub(crate) fn declared(builder: ExtensionBuilder) -> ExtensionBuilder {
    builder.class::<_ZendTestFiber>()
}

/// `zend_test_fiber`, the payload a `_ZendTestFiber` carries.
#[derive(Default)]
pub(crate) struct ZendTestFiber {
    coroutine: Coroutine,
    /// `fci` and `fci_cache`, with php's `Z_TRY_ADDREF` of
    /// `fci.function_name`: the callable and one owner of its value, held
    /// until the body is done with it. php's own owner, the value's
    /// alone, and not an `Owner<Callable>`'s, which would own the
    /// receiver a `Class::method` string resolved to as `zend_fcc_addref`
    /// does; `Fiber` keeps its callable the same way.
    callable: Option<(Callable, Owner<Value>)>,
    /// `fiber->previous` set by `pipeTo`: a target, which only the fiber
    /// it was piped from starts.
    piped: bool,
    /// `fiber->target`: the fiber `pipeTo` made, one owner, started with
    /// what this one's body returned, once it has.
    target: Option<Owner<PhpObject>>,
    /// `fiber->result`: what the body returned, and before that, for a
    /// target, the argument it is started with.
    result: Option<Owner<Value>>,
}

impl HoldsCoroutine for ZendTestFiber {
    fn coroutine(&mut self) -> &mut Coroutine {
        &mut self.coroutine
    }
}

/// `final class _ZendTestFiber`.
///
/// Inside a `Fiber`, a test fiber's `Fiber::suspend()` suspends the
/// `Fiber`, and the `Fiber`'s resume runs the test fiber on from it
/// (`fiber_test_01.phpt`):
///
/// ```php
/// $fiber = new Fiber(function (): int {
///     $test = new _ZendTestFiber(function (): int {
///         $value = Fiber::suspend(123);
///         var_dump($value);
///         return $value;
///     });
///     var_dump($test->start());
///     return 1;
/// });
/// $value = $fiber->start();
/// var_dump($value);
/// $value = $fiber->resume(2 * $value);
/// var_dump($value, $fiber->getReturn());
/// ```
/// ```expect
/// int(123)
/// int(246)
/// NULL
/// NULL
/// int(1)
/// ```
///
/// It is uncloneable (`gh16388.phpt`):
///
/// ```php
/// $fiber = new _ZendTestFiber(function (): int {});
/// try { clone $fiber; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Trying to clone an uncloneable object of class _ZendTestFiber
/// ```
#[php_class(final, payload = ZendTestFiber, handlers = crate::fiber::TEST_FIBER)]
pub(crate) struct _ZendTestFiber;

#[php_class]
impl _ZendTestFiber {
    /// `__construct(callable $callback)`: the callable kept, with one
    /// owner of it.
    ///
    /// ```php
    /// $test = new _ZendTestFiber(function (): void { var_dump(debug_backtrace()); });
    /// $test->start();
    /// ```
    /// ```expect
    /// array(1) {
    ///   [0]=>
    ///   array(2) {
    ///     ["function"]=>
    ///     string(%d) "{closure:%s:%d}"
    ///     ["args"]=>
    ///     array(0) {
    ///     }
    ///   }
    /// }
    /// ```
    fn __construct(req: &mut Request, this: This<Self>, callback: Callable) -> Result<(), Thrown> {
        let held = Owner::take(req, callback.value());
        let before = payload(req, this.object())?
            .callable
            .replace((callback, held));
        if let Some((_, before)) = before {
            before.give_up(req);
        }
        Ok(())
    }

    /// `start(mixed ...$args): mixed`: the coroutine laid down and the
    /// callable served on it with the arguments, up to its first
    /// suspend, whose value is returned; `null` where the body returned.
    /// A target `pipeTo` made is started by the fiber it was piped from,
    /// and only by it (`fiber_test_07.phpt`):
    ///
    /// ```php
    /// $fiber = new _ZendTestFiber(function (): int { return 1; });
    /// $target = $fiber->pipeTo(function (int $result): void { var_dump($result); });
    /// try { $target->start(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// var_dump($fiber->start());
    /// ```
    /// ```expect
    /// Cannot start a fiber that is the target of another fiber
    /// int(1)
    /// NULL
    /// ```
    fn start(
        req: &mut Request,
        this: This<Self>,
        args: NamedVariadic<Value>,
    ) -> Result<Owner<Value>, Thrown> {
        let this = this.object();
        let fiber = payload(req, this)?;
        if fiber.piped {
            return Err(throw_error(
                req,
                "Error",
                "Cannot start a fiber that is the target of another fiber",
            ));
        }
        if fiber.coroutine.status() != CoroutineStatus::Init {
            return Err(throw_error(
                req,
                "Error",
                "Cannot start a fiber that has already been started",
            ));
        }
        let named = args.named().to_vec();
        lay_down(req, this, args.positional, named)?;
        let back = run(req, this, Owner::null());
        delegate(req, this, back)
    }

    /// `resume(mixed $value = null): mixed`: the suspended fiber runs on
    /// from its suspend, which returns `$value`, until it suspends again
    /// or returns. One that is not suspended, waiting on a coroutine it
    /// resumed or one a suspend passed through it to among them, is
    /// refused (`fiber_test_02.phpt`):
    ///
    /// ```php
    /// $test = new _ZendTestFiber(function (): void {
    ///     $fiber = new Fiber(function (): int {
    ///         $value = _ZendTestFiber::suspend(1);
    ///         var_dump($value);
    ///         return $value;
    ///     });
    ///     $fiber->start();
    /// });
    /// var_dump($test->start());
    /// var_dump($test->resume(2));
    /// try { $test->resume(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// int(1)
    /// int(2)
    /// NULL
    /// Cannot resume a fiber that is not suspended
    /// ```
    fn resume(
        req: &mut Request,
        this: This<Self>,
        #[php(default = null)] value: Value,
    ) -> Result<Owner<Value>, Thrown> {
        let this = this.object();
        if payload(req, this)?.coroutine.status() != CoroutineStatus::Suspended {
            return Err(throw_error(
                req,
                "Error",
                "Cannot resume a fiber that is not suspended",
            ));
        }
        let value = Owner::take(req, value);
        let back = run(req, this, value);
        delegate(req, this, back)
    }

    /// `pipeTo(callable $callback): _ZendTestFiber`: a new fiber over
    /// `$callback`, which this one's body transfers into when it
    /// returns, handing it what it returned; a target piped before is
    /// let go.
    ///
    /// ```php
    /// $fiber = new _ZendTestFiber(fn () => 2);
    /// $fiber->pipeTo(function (int $x) { echo "first $x\n"; });
    /// $fiber->pipeTo(function (int $x) { echo "second $x\n"; return 3; })
    ///     ->pipeTo(function (int $y) { echo "third $y\n"; });
    /// var_dump($fiber->start());
    /// ```
    /// ```expect
    /// second 2
    /// third 3
    /// NULL
    /// ```
    fn pipeTo(
        req: &mut Request,
        this: This<Self>,
        callback: Callable,
    ) -> Result<Owner<Instance<_ZendTestFiber>>, Thrown> {
        let this = this.object();
        let target = new_object(req, this.class())?;
        let held = Owner::take(req, callback.value());
        let made = payload(req, *target)?;
        made.callable = Some((callback, held));
        made.piped = true;
        let kept = Owner::take(req, *target);
        let before = payload(req, this)?.target.replace(kept);
        if let Some(before) = before {
            before.give_up(req);
        }
        Ok(target.into_instance())
    }

    /// `static suspend(mixed $value = null): mixed`: the test fiber
    /// running innermost, `ZT_G(active_fiber)`, suspends with `$value`,
    /// and this returns what the next resume sends. A `Fiber` inside it
    /// waits, and runs on when it is resumed (`fiber_test_06.phpt`):
    ///
    /// ```php
    /// $test = new _ZendTestFiber(function (): void {
    ///     $fiber = new Fiber(function (): void {
    ///         var_dump(_ZendTestFiber::suspend(10));
    ///         Fiber::suspend(20);
    ///         echo "unreachable\n";
    ///     });
    ///     var_dump($fiber->start());
    /// });
    /// $fiber = new Fiber(function (): void {
    ///     var_dump(Fiber::suspend(1));
    ///     var_dump(Fiber::suspend(2));
    /// });
    /// var_dump($test->start());
    /// var_dump($fiber->start());
    /// var_dump($fiber->resume('1'));
    /// var_dump($test->resume('10'));
    /// var_dump($fiber->resume('2'));
    /// ```
    /// ```expect
    /// int(10)
    /// int(1)
    /// string(1) "1"
    /// int(2)
    /// string(2) "10"
    /// int(20)
    /// NULL
    /// string(1) "2"
    /// NULL
    /// ```
    ///
    /// Each kind of suspend leaves the coroutines above its own waiting,
    /// in either nesting (`fiber_test_04.phpt`, `fiber_test_05.phpt`):
    ///
    /// ```php
    /// $fiber = new Fiber(function (): int {
    ///     $test = new _ZendTestFiber(function (): void {
    ///         var_dump(Fiber::suspend(1));
    ///         var_dump(_ZendTestFiber::suspend(3));
    ///         var_dump(Fiber::suspend(4));
    ///         _ZendTestFiber::suspend(5);
    ///         echo "unreachable\n";
    ///     });
    ///     $value = $test->start();
    ///     var_dump($value);
    ///     var_dump($test->resume(2 * $value));
    ///     return -1;
    /// });
    /// $value = $fiber->start();
    /// var_dump($value);
    /// $value = $fiber->resume(2 * $value);
    /// var_dump($value);
    /// var_dump($fiber->resume(2 * $value), $fiber->getReturn());
    /// $test = new _ZendTestFiber(function (): void {
    ///     $fiber = new Fiber(function (): int {
    ///         var_dump(Fiber::suspend(1));
    ///         var_dump(_ZendTestFiber::suspend(3));
    ///         return 2 * Fiber::suspend(4);
    ///     });
    ///     var_dump($fiber->start(), $fiber->resume(2), $fiber->resume(8), $fiber->getReturn());
    /// });
    /// var_dump($test->start(), $test->resume(6));
    /// ```
    /// ```expect
    /// int(1)
    /// int(2)
    /// int(3)
    /// int(6)
    /// int(4)
    /// int(8)
    /// int(5)
    /// NULL
    /// int(-1)
    /// int(2)
    /// int(6)
    /// int(1)
    /// int(4)
    /// NULL
    /// int(16)
    /// int(3)
    /// NULL
    /// ```
    ///
    /// `EG(active_fiber)` is part of what a switch swaps, so once a test
    /// fiber's suspend has passed through a `Fiber` its resumer reads
    /// the fiber it had active, none here, and the `Fiber` again only
    /// on the stack the resume goes back to:
    ///
    /// ```php
    /// $test = new _ZendTestFiber(function (): void {
    ///     $fiber = new Fiber(function (): void {
    ///         _ZendTestFiber::suspend();
    ///         var_dump(Fiber::getCurrent() === $GLOBALS['fiber']);
    ///     });
    ///     $GLOBALS['fiber'] = $fiber;
    ///     $fiber->start();
    ///     var_dump(Fiber::getCurrent());
    /// });
    /// $test->start();
    /// var_dump(Fiber::getCurrent());
    /// try { Fiber::suspend(); } catch (FiberError $e) { echo $e->getMessage(), "\n"; }
    /// $test->resume();
    /// ```
    /// ```expect
    /// NULL
    /// Cannot suspend outside of a fiber
    /// bool(true)
    /// NULL
    /// ```
    ///
    /// A `Fiber` such a suspend passed through waits running, and
    /// `ReflectionFiber` reads it where php's `fiber->execute_data` was
    /// left by its start, its bottom frame: the executing line is the
    /// resumer's, the line that started it, and the trace from it is
    /// empty, php cutting the chain under the bottom (#2505):
    ///
    /// ```php
    /// $test = new _ZendTestFiber(function (): void {
    ///     $fiber = new Fiber(function (): void {
    ///         $x = 1;
    ///         _ZendTestFiber::suspend(10);
    ///         echo "back\n";
    ///     });
    ///     $GLOBALS['f'] = $fiber;
    ///     $fiber->start();
    /// });
    /// var_dump($test->start());
    /// $r = new ReflectionFiber($f);
    /// try { var_dump($r->getExecutingLine()); } catch (Throwable $e) { echo get_class($e), ": ", $e->getMessage(), "\n"; }
    /// try { var_dump(count($r->getTrace())); } catch (Throwable $e) { echo get_class($e), ": ", $e->getMessage(), "\n"; }
    /// var_dump($f->isRunning(), basename($r->getExecutingFile()) === basename(__FILE__));
    /// $test->resume();
    /// ```
    /// ```expect
    /// int(10)
    /// int(9)
    /// int(0)
    /// bool(true)
    /// bool(true)
    /// back
    /// ```
    fn suspend(
        req: &mut Request,
        #[php(default = null)] value: Value,
    ) -> Result<Owner<Value>, Thrown> {
        let Some(fiber) = req.state::<State>().active_fiber else {
            return Err(throw_error(
                req,
                "Error",
                "Cannot suspend outside of a _ZendTestFiber",
            ));
        };
        let value = Owner::take(req, value);
        Coroutine::suspend::<ZendTestFiber>(req, fiber, value)
    }
}

/// `zend_test_fiber_handlers`: `std_object_handlers` with `dtor_obj`
/// and `free_obj` the extension's, and `clone_obj` null.
pub(crate) struct TestFiberHandlers;

pub(crate) static TEST_FIBER: TestFiberHandlers = TestFiberHandlers;

impl ObjectHandlers for TestFiberHandlers {
    /// `zend_test_fiber_object_destroy`: a suspended fiber is resumed,
    /// the active one meanwhile, to unwind with the graceful exit
    /// (`fiber_test_03.phpt`'s shape, the throw a destroy's instead):
    ///
    /// ```php
    /// $test = new _ZendTestFiber(function (): void {
    ///     try { _ZendTestFiber::suspend(1); } finally { echo "finally\n"; }
    ///     echo "unreachable\n";
    /// });
    /// var_dump($test->start());
    /// unset($test);
    /// echo "done\n";
    /// ```
    /// ```expect
    /// int(1)
    /// finally
    /// done
    /// ```
    fn dtor_obj(&self) -> DtorObj {
        DtorObj::Own(|req, object| {
            let previous = req.state::<State>().active_fiber.replace(object);
            Coroutine::destroy::<ZendTestFiber>(req, object);
            req.state::<State>().active_fiber = previous;
        })
    }

    /// `zend_test_fiber_object_free`: the callable where the body never
    /// gave it up, the target and `fiber->result` let go, and what the
    /// coroutine holds of the heap.
    fn free_obj(&self, req: &mut Request, object: PhpObject) {
        let Some(fiber) = object.native_mut::<ZendTestFiber>(req) else {
            return;
        };
        let owners = [
            fiber.callable.take().map(|(_, held)| held),
            fiber.target.take().map(Owner::into_value),
            fiber.result.take(),
        ];
        for owner in owners.into_iter().flatten() {
            owner.give_up(req);
        }
        Coroutine::release::<ZendTestFiber>(req, object);
    }

    /// `clone_obj = NULL`.
    fn clone_obj(&self) -> Option<CloneObj> {
        None
    }
}

/// The payload of `object`, which a `_ZendTestFiber` always carries.
fn payload(req: &mut Request, object: PhpObject) -> Result<&mut ZendTestFiber, Thrown> {
    object.native_mut_or_fatal::<ZendTestFiber>(req)
}

/// `zend_fiber_init_context` over `zend_test_fiber_execute`: the
/// callable called with `arguments` on the coroutine's stack and given up
/// once the call is done; what it returned kept as `fiber->result`, the
/// argument a pipe passed given up first, and handed back.
fn lay_down(
    req: &mut Request,
    object: PhpObject,
    arguments: Vec<Value>,
    named: Vec<(PhpString, Value)>,
) -> Result<(), Thrown> {
    let Some(callable) = payload(req, object)?
        .callable
        .as_ref()
        .map(|(callable, _)| callable.clone())
    else {
        return Err(throw_error(
            req,
            "Error",
            "Cannot start a fiber whose constructor was not called",
        ));
    };
    Coroutine::init::<ZendTestFiber>(req, object, move |req: &mut Request| {
        let outcome = call_named(req, &callable, &arguments, &named);
        let fiber = payload(req, object)?;
        let held = fiber.callable.take().map(|(_, held)| held);
        let param = fiber.result.take();
        for owner in [held, param].into_iter().flatten() {
            owner.give_up(req);
        }
        let value = outcome?;
        let handed = Owner::take(req, *value);
        payload(req, object)?.result = Some(value);
        Ok(handed)
    })
}

/// `zend_test_fiber_resume`, with `ZT_G(active_fiber)` the fiber for its
/// length; and the end of `zend_test_fiber_execute` for a fiber with a
/// target: once its body has returned, the target is laid down with the
/// return value as its one argument and runs in its place, the active fiber
/// from then, as php's transfers into it.
fn run(req: &mut Request, object: PhpObject, value: Owner<Value>) -> Result<Owner<Value>, Thrown> {
    let previous = req.state::<State>().active_fiber.replace(object);
    let mut back = Coroutine::resume::<ZendTestFiber>(req, object, value);
    let mut ran = object;
    while let Ok(handed) = back {
        let Some((target, result)) = piped(req, ran) else {
            back = Ok(handed);
            break;
        };
        handed.give_up(req);
        let argument = *result;
        let started = payload(req, target).map(|fiber| fiber.result = Some(result));
        back = started
            .and_then(|()| lay_down(req, target, vec![argument], Vec::new()))
            .and_then(|()| {
                req.state::<State>().active_fiber = Some(target);
                let null = Owner::null();
                Coroutine::resume::<ZendTestFiber>(req, target, null)
            });
        ran = target;
    }
    req.state::<State>().active_fiber = previous;
    back
}

/// The target of a dead fiber and one more owner of what it returned, the
/// target's argument: what `zend_test_fiber_execute` transfers into;
/// none where the fiber has not returned or has no target.
fn piped(req: &mut Request, fiber: PhpObject) -> Option<(PhpObject, Owner<Value>)> {
    let ended = payload(req, fiber).ok()?;
    if ended.coroutine.status() != CoroutineStatus::Dead {
        return None;
    }
    let target = ended.target.as_deref().copied()?;
    let result = ended.result.as_deref().copied().unwrap_or_else(Value::null);
    Some((target, Owner::take(req, result)))
}

/// `delegate_transfer_result`: what came back raised where it is a
/// throwable, `null` where `this` is dead, and the value otherwise.
fn delegate(
    req: &mut Request,
    this: PhpObject,
    back: Result<Owner<Value>, Thrown>,
) -> Result<Owner<Value>, Thrown> {
    let value = back?;
    if payload(req, this)?.coroutine.status() == CoroutineStatus::Dead {
        value.give_up(req);
        return Ok(Owner::null());
    }
    Ok(value)
}
