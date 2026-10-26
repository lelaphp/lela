//! `fiber.c`'s `_ZendTestFiber`: a fiber of the extension's own, laid
//! over the engine's fiber coroutines ([`Coroutine`]) as php's is over
//! `zend_fiber_context`, which interleaves with `Fiber`: a
//! `Fiber::suspend()` inside a test fiber inside a `Fiber` suspends the
//! `Fiber` and leaves the test fiber waiting above it, and
//! `_ZendTestFiber::suspend()` inside a `Fiber` inside a test fiber does
//! the reverse. `ZT_G(active_fiber)`, the test fiber a static `suspend`
//! leaves, is the extension's request state.

use php_ext::abi::{
    Callable, CloneObj, Coroutine, CoroutineEnd, CoroutineStatus, DtorObj, ExtensionBuilder,
    HoldsCoroutine, Instance, NamedVariadic, ObjectHandlers, This,
};
use php_ext::attr::php_class;
use php_ext::call::{call_named, new_object};
use php_ext::raising::{Thrown, clear, throw_error};
use php_ext::request::Request;
use php_ext::values::{Owner, PhpObject, PhpString, Value};

use crate::State;
use crate::throwables::Throwables;

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
            let ce = Throwables::of(req)?;
            return Err(throw_error(
                req,
                ce.error,
                "Cannot start a fiber that is the target of another fiber",
            ));
        }
        if fiber.coroutine.status() != CoroutineStatus::Init {
            let ce = Throwables::of(req)?;
            return Err(throw_error(
                req,
                ce.error,
                "Cannot start a fiber that has already been started",
            ));
        }
        let named = args.named().to_vec();
        lay_down(req, this, args.positional.into_iter().collect(), named)?;
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
            let ce = Throwables::of(req)?;
            return Err(throw_error(
                req,
                ce.error,
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
    ///
    /// The body's end lays the target down on its own stack and
    /// transfers into it from there, as `zend_test_fiber_execute` does:
    /// the fiber observers see the target's init, then one switch from
    /// the dead fiber into it, and the dead fiber's destroy before the
    /// target runs. The child is served with the observers on, which
    /// only startup sets, and its addresses numbered by first
    /// appearance (#2506):
    ///
    /// ```php
    /// $code = '$fiber = new _ZendTestFiber(fn () => 2);
    /// $fiber->pipeTo(function (int $x) { echo "second $x\n"; return 3; });
    /// var_dump($fiber->start());';
    /// $args = [PHP_BINARY, '-n'];
    /// foreach (['enabled', 'fiber_init', 'fiber_switch', 'fiber_destroy'] as $name) {
    ///     array_push($args, '-d', "zend_test.observer.$name=1");
    /// }
    /// $p = proc_open([...$args, '-r', $code], [1 => ['pipe', 'w']], $pipes);
    /// $out = stream_get_contents($pipes[1]);
    /// proc_close($p);
    /// $seen = [];
    /// echo preg_replace_callback('/0x[0-9a-f]+/', function ($m) use (&$seen) {
    ///     return '#' . ($seen[$m[0]] ??= count($seen) + 1);
    /// }, $out);
    /// ```
    /// ```expect process
    /// <!-- init 'Command line code' -->
    /// <!-- init _ZendTestFiber::__construct() -->
    /// <!-- init _ZendTestFiber::pipeTo() -->
    /// <!-- init _ZendTestFiber::start() -->
    /// <!-- alloc: #1 -->
    /// <!-- switching from fiber #2 to #1 -->
    /// <init '#1'>
    /// <!-- init {closure:Command line code:1}() -->
    /// <!-- alloc: #3 -->
    /// <!-- switching from fiber #1 to #3 -->
    /// <init '#3'>
    /// <returned '#1'>
    /// <!-- destroy: #1 -->
    /// <!-- init {closure:Command line code:2}() -->
    /// second 2
    /// <!-- switching from fiber #3 to #2 -->
    /// <returned '#3'>
    /// <!-- destroy: #3 -->
    /// <!-- init var_dump() -->
    /// NULL
    /// ```
    ///
    /// A body that throws ends into its target all the same: the
    /// throwable is lost to the transfer the target's start overwrites,
    /// and the target is served with the value nothing wrote, an
    /// argument not passed (#2871):
    ///
    /// ```php
    /// $f = new _ZendTestFiber(function () { throw new Exception('boom'); });
    /// $f->pipeTo(function ($x) { echo "target "; var_dump($x); });
    /// try { var_dump($f->start()); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
    /// echo "end\n";
    /// ```
    /// ```expect exit=255
    /// Fatal error: Uncaught ArgumentCountError: {closure:%s:3}(): Argument #1 ($x) not passed in %s:3
    /// Stack trace:
    /// #0 [internal function]: {closure:%s:3}(NULL)
    /// #1 {main}
    ///   thrown in %s on line 3
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
    ///
    /// A `Fiber` that resumed another before the suspend passed through
    /// both stands where that resume wrote php's pointer, its `start`
    /// call, which still stands, and is read from there:
    ///
    /// ```php
    /// $test = new _ZendTestFiber(function (): void {
    ///     $a = new Fiber(function (): void {
    ///         $b = new Fiber(function (): void {
    ///             _ZendTestFiber::suspend(10);
    ///         });
    ///         $GLOBALS['b'] = $b;
    ///         $b->start();
    ///     });
    ///     $GLOBALS['a'] = $a;
    ///     $a->start();
    /// });
    /// $test->start();
    /// foreach (['a', 'b'] as $n) {
    ///     $r = new ReflectionFiber($GLOBALS[$n]);
    ///     echo $n, ": ", $r->getExecutingLine(), " ", count($r->getTrace()), "\n";
    /// }
    /// $test->resume();
    /// ```
    /// ```expect
    /// a: 8 2
    /// b: 8 0
    /// ```
    ///
    /// Inside both, the chain from the inner `Fiber`'s frame goes through
    /// each `start` into the test fiber's closure, whose bottom frame names
    /// no resumer's; a suspend that passes through both leaves the chain
    /// standing as it is, and a debugger's evaluation in the inner fiber's
    /// frame reads it so (`tests/frames.rs`, #2765):
    ///
    /// ```php
    /// function f() {
    ///     try { throw new Exception("no"); } catch (Exception $e) { echo $e->getTraceAsString(), "\n"; }
    /// }
    /// $test = new _ZendTestFiber(function () {
    ///     $a = new Fiber(function () {
    ///         $b = new Fiber(function () { f(); });
    ///         $b->start();
    ///     });
    ///     $a->start();
    /// });
    /// $test->start();
    /// ```
    /// ```expect
    /// #0 %s(7): f()
    /// #1 [internal function]: {closure:{closure:{closure:%s:5}:6}:7}()
    /// #2 %s(8): Fiber->start()
    /// #3 [internal function]: {closure:{closure:%s:5}:6}()
    /// #4 %s(10): Fiber->start()
    /// #5 [internal function]: {closure:%s:5}()
    /// #6 {main}
    /// ```
    fn suspend(
        req: &mut Request,
        #[php(default = null)] value: Value,
    ) -> Result<Owner<Value>, Thrown> {
        let Some(fiber) = req.state::<State>().active_fiber else {
            let ce = Throwables::of(req)?;
            return Err(throw_error(
                req,
                ce.error,
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
/// once the call is done, with the argument a pipe passed; what it
/// returned kept as `fiber->result` and handed back, or, where `pipeTo`
/// made a target, the target laid down on this stack with that value as
/// its one argument, the active fiber from here, and transferred into
/// ([`CoroutineEnd::Into`]).
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
        let ce = Throwables::of(req)?;
        return Err(throw_error(
            req,
            ce.error,
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
        let Some(target) = payload(req, object)?.target.as_deref().copied() else {
            let value = outcome?;
            let returned = *value;
            payload(req, object)?.result = Some(value);
            return Ok(CoroutineEnd::Return(Owner::take(req, returned)));
        };
        // The target is laid down whatever the body ended with. A
        // throwable is cleared into the error transfer the target's own
        // start overwrites, so it is lost, and the target's argument is
        // `fiber->result`, which nothing wrote, `IS_UNDEF`.
        let argument = match outcome {
            Ok(value) => {
                let returned = *value;
                payload(req, object)?.result = Some(value);
                let argument = Owner::take(req, returned);
                payload(req, target)?.result = Some(argument);
                returned
            }
            Err(thrown) => {
                clear(req, thrown)?;
                Value::unpassed()
            }
        };
        lay_down(req, target, vec![argument], Vec::new())?;
        req.state::<State>().active_fiber = Some(target);
        Ok(CoroutineEnd::Into(target))
    })
}

/// `zend_test_fiber_resume`, with `ZT_G(active_fiber)` the fiber for its
/// length, and a target the fiber's body ended into the active one while
/// it runs in its place.
fn run(req: &mut Request, object: PhpObject, value: Owner<Value>) -> Result<Owner<Value>, Thrown> {
    let previous = req.state::<State>().active_fiber.replace(object);
    let back = Coroutine::resume::<ZendTestFiber>(req, object, value);
    req.state::<State>().active_fiber = previous;
    back
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
