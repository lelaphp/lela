//! The calling-back group (design §4.4): re-entry from a native into PHP
//! code, always through `&mut Request` (principle 9, ADR-0002). A
//! callable is served here, a native through its entry and a user
//! function by entering the engine's one loop again on the callee's
//! frame; the items that only read the linked tables answer from what
//! the request holds, and the rest still raise php's `Error` for an
//! undefined function or class.

use php_heap::FrameRef;

use php_unit::decl::{Bare, ConstExpr, HookKind, MethodDecl, Name, ParameterDefault};
use php_unit::{ConstIdx, Opcode, Unit};

use crate::api::abi::{Callable, PropertyRead};
use crate::api::raising::{
    Thrown, argument_error, class_not_found, error, non_static_method_call, type_error,
    undefined_function, undefined_method,
};
use crate::api::request::{Request, Strategy};
use crate::api::values::{
    ClassId, Key, Owner, PhpArray, PhpObject, PhpString, Value, ValueRef, adopted, adopted_some,
    walk,
};
use crate::exec::{self, Body};

/// A linked function: user or native, by identity, one word (design
/// §5.14, *Identities*). With its top bit clear it is one every request
/// shares: a native, then a native method, numbered from zero, then a
/// user function or method of the engine's published arena past them;
/// or, from [`Function::CLASS_NATIVE`] on, a native a published class's
/// table lays under its own scope ([`ClassNative`]). With it set it is
/// the request's own: a user function, method or closure among its
/// rows, a native one of its classes lays from `CLASS_NATIVE` on, and
/// from [`Function::TRAMPOLINE`] on a call's trampoline. Either stays
/// the function's for as long as the request runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Function(pub(crate) u32);

impl Function {
    /// `zend_pass_function`: the callee `NEW` serves for a class with
    /// no constructor and arguments to take, which takes them and
    /// returns `null`.
    pub(crate) const PASS: Function = Function(u32::MAX);

    /// The bit that marks a function of the request's own.
    pub(crate) const LOCAL: u32 = 1 << 31;

    /// The bit, under the top one, that marks the request's trampolines.
    pub(crate) const TRAMPOLINE: u32 = 1 << 30;

    /// The bit, under those, that marks a [`ClassNative`]; the numbers
    /// below it are a space's user functions.
    pub(crate) const CLASS_NATIVE: u32 = 1 << 29;

    /// The request's own user function at `at` among its rows.
    pub(crate) fn local(at: usize) -> Option<Function> {
        let at = u32::try_from(at)
            .ok()
            .filter(|at| *at < Function::CLASS_NATIVE)?;
        Some(Function(at | Function::LOCAL))
    }

    /// Where the function stands among the request's own user functions,
    /// if it is one.
    pub(crate) fn local_at(self) -> Option<usize> {
        if self.0 & (Function::LOCAL | Function::TRAMPOLINE | Function::CLASS_NATIVE)
            != Function::LOCAL
        {
            return None;
        }
        usize::try_from(self.0 & !Function::LOCAL).ok()
    }

    /// The number of a native or a published user function, which the
    /// arena's rows are numbered past the natives in.
    pub(crate) fn published_number(self) -> Option<u32> {
        (self.0 < Function::CLASS_NATIVE).then_some(self.0)
    }

    /// The published class native at `at` of the arena's.
    pub(crate) fn published_class_native(at: u32) -> Option<Function> {
        (at < Function::CLASS_NATIVE).then_some(Function(Function::CLASS_NATIVE | at))
    }

    /// Where a published class native stands among the arena's.
    pub(crate) fn published_class_native_at(self) -> Option<u32> {
        (self.0 & !(Function::CLASS_NATIVE - 1) == Function::CLASS_NATIVE)
            .then_some(self.0 & (Function::CLASS_NATIVE - 1))
    }

    /// The request's own class native at `at` among its rows.
    pub(crate) fn local_class_native(at: usize) -> Option<Function> {
        let at = u32::try_from(at)
            .ok()
            .filter(|at| *at < Function::CLASS_NATIVE)?;
        Some(Function(Function::LOCAL | Function::CLASS_NATIVE | at))
    }

    /// Where a class native of the request's own stands among its rows.
    pub(crate) fn local_class_native_at(self) -> Option<usize> {
        if self.0 & !(Function::CLASS_NATIVE - 1) != Function::LOCAL | Function::CLASS_NATIVE {
            return None;
        }
        usize::try_from(self.0 & (Function::CLASS_NATIVE - 1)).ok()
    }
}

/// A native a class's function table lays under that class's scope, a
/// `zend_internal_function` of the class's own whose `common.scope` is
/// the class: one function number per class, as php allocates one, so
/// `func->common.scope` names the class and an observer is asked of it
/// once per class (`zend_observer_fcall_init`). The row beside each
/// number is the class and this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ClassNative {
    /// `zend_enum_register_funcs`: an enum's own `cases`, `from` or
    /// `tryFrom`, which a program's enum is given at its link.
    Enum(crate::enums::EnumMethod),
    /// `zend_add_trait_method` over a native trait's method: the copy
    /// php lays in the using class's function table, whose handler is the
    /// trait's native, `native`, served as the trait's own number is, and
    /// whose scope is the using class. Its declaration is the class's own
    /// row, the method at `at` among the composed class's methods, which
    /// carries the alias's name (`new_fn->common.function_name = name`)
    /// and the alias's visibility (`zend_traits_copy_functions`).
    Trait { native: Function, at: u32 },
}

/// What a [`Callable`] resolved to, `zend_fcall_info_cache`'s three
/// fields after `zend_is_callable_ex` filled them
/// (`Z/Zend/zend_API.c:4250`): the callee, the scope the call is made
/// in, and the receiver it is bound to. A callable is resolved once,
/// where it is taken, and serving it enters what it holds; php's
/// `zend_call_known_fcc` does the same with the cache
/// `zend_fcc_dup` kept.
#[derive(Clone, Copy)]
pub(crate) enum Target {
    /// `fcc.function_handler` alone: a function by name, with no scope
    /// and no receiver.
    Function(Function),
    /// `fcc.called_scope` beside `fcc.object`: a method, its receiver
    /// `None` where the class declares the method on itself and the
    /// name is `Class::method`.
    Method {
        function: Function,
        scope: ClassId,
        this: Option<PhpObject>,
    },
    /// `fcc.closure`: a `Closure`, served through its payload as
    /// `Closure::__invoke` serves it.
    Closure(PhpObject),
}

impl std::fmt::Debug for Target {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Target::Function(function) => write!(f, "Function({})", function.0),
            Target::Method {
                function,
                scope,
                this,
            } => write!(
                f,
                "Method({}, {}, {:?})",
                function.0,
                scope.0,
                this.map(|object| object.index())
            ),
            Target::Closure(object) => write!(f, "Closure({})", object.index()),
        }
    }
}

impl PartialEq for Target {
    /// By identity, as php compares two `zend_fcall_info_cache`s: the
    /// same callee, the same scope and the same object. What
    /// `unregister_tick_function` and `spl_autoload_unregister` compare
    /// a registered callable by.
    fn eq(&self, other: &Target) -> bool {
        match (self, other) {
            (Target::Function(one), Target::Function(two)) => one == two,
            (
                Target::Method {
                    function: one,
                    scope: first,
                    this: here,
                },
                Target::Method {
                    function: two,
                    scope: second,
                    this: there,
                },
            ) => {
                one == two
                    && first == second
                    && here.map(|object| object.index()) == there.map(|object| object.index())
            }
            (Target::Closure(one), Target::Closure(two)) => one.index() == two.index(),
            _ => false,
        }
    }
}

/// The iterator a `Traversable` object's `get_iterator` hands out,
/// `zend_object_iterator` (`Z/Zend/zend_iterators.h`), held by a native
/// and stepped through its functions, as `spl_iterator_apply` and SPL's
/// outer iterators step the iterator of their inner one: a `Generator`
/// resumed, a user's `Iterator` served its methods, with what `current()`
/// answered kept until the walk moves, as `zend_user_iterator` keeps it,
/// and a class that walks its objects in C walked by its own functions,
/// in no frame of their own. [`iterate`] hands out an [`Owner`] of it,
/// the holder's, who names it to the collector by its value
/// ([`Iter::as_value`]) and gives it up with [`Owner::give_up`],
/// `zend_iterator_dtor`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Iter(PhpObject);

impl Iter {
    /// `funcs->rewind`.
    pub fn rewind(self, req: &mut Request) -> Result<(), Thrown> {
        crate::interfaces::rewind(req, self.0)
    }

    /// `funcs->valid`, `SUCCESS` as `true`.
    pub fn valid(self, req: &mut Request) -> Result<bool, Thrown> {
        crate::interfaces::valid(req, self.0)
    }

    /// The value at the position, `funcs->get_current_data`, one owner,
    /// the caller's.
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn value(self, req: &mut Request) -> Result<Owner<Value>, Thrown> {
        adopted(crate::interfaces::current(req, self.0))
    }

    /// `funcs->get_current_key`, one owner, the caller's.
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn key(self, req: &mut Request) -> Result<Owner<Value>, Thrown> {
        adopted(crate::interfaces::key(req, self.0))
    }

    /// `funcs->move_forward`.
    pub fn next(self, req: &mut Request) -> Result<(), Thrown> {
        crate::interfaces::move_forward(req, self.0)
    }

    /// `funcs->invalidate_current`: what `current()` answered last,
    /// which a user's `Iterator` keeps, given up, as `spl_dual_it_free`
    /// asks before a step that moves the inner object behind the
    /// iterator's back, such as `seek()`.
    pub fn invalidate(self, req: &mut Request) {
        crate::interfaces::invalidate_current(req, self.0);
    }

    /// The iterator as a value: what its holder names to the collector.
    #[must_use]
    pub fn as_value(self) -> Value {
        self.0.as_value()
    }
}

/// `zend_call_known_fcc` (`Z/Zend/zend_execute_API.c:1090`): the
/// resolved callee on a call frame of its own, the branch of
/// `zend_call_function` a filled `zend_fcall_info_cache` takes, which
/// is a function with no scope, a method with its scope and receiver,
/// or a closure through its payload.
fn serve(
    req: &mut Request,
    target: Target,
    arguments: &[Value],
    extra_named: &[(PhpString, Value)],
) -> Result<Value, Thrown> {
    let served = match target {
        Target::Function(function) => req.serve_function(function, arguments, extra_named),
        Target::Method {
            function,
            scope,
            this,
        } => req.serve_method_made(function, this, scope, arguments, extra_named),
        Target::Closure(object) => req.serve_closure(object, arguments, extra_named),
    };
    without_frame(req, served)
}

/// The tail of `zend_call_function` (`Z/Zend/zend_execute_API.c:1069`):
/// what the callee left pending, where no frame stands to take it, handed
/// to [`Request::thrown_without_frame`].
fn without_frame(req: &mut Request, served: Result<Value, Thrown>) -> Result<Value, Thrown> {
    match served {
        Err(thrown) if !req.frames().executing() => req.thrown_without_frame(&thrown),
        served => served,
    }
}

/// The `zend_function` behind a target, `fcc.function_handler`: a
/// closure's is its payload's, `zend_get_closure_method_def`. What the
/// named arguments are placed against.
pub(crate) fn callee(req: &Request, target: Target) -> Option<Function> {
    match target {
        Target::Function(function) | Target::Method { function, .. } => Some(function),
        Target::Closure(object) => object.closure(req).map(|closure| closure.function),
    }
}

/// `zend_call_function` (`Z/Zend/zend_execute_API.c:806`): the callee
/// the callable names served on a call frame of its own, as
/// `zend_vm_stack_push_call_frame` and the loop over `fci->params`
/// build one. Each argument is one more owner the frame holds and is
/// released with it, so the caller keeps what it passed; the return is
/// the caller's, one owner; and what the callee left pending is the
/// `Err`, the frame popped before it is handed back, so a native reads
/// a callback's raise the way it reads its own.
///
/// A native is served through its entry, php's
/// `func->internal_function.handler` branch. A user function is served
/// by entering the engine's loop once more on the callee's frame,
/// php's `zend_execute_ex` branch (design §5.6, "Native→PHP re-entry
/// ... recurses via `execute(frame)` on the Rust stack"), which is the
/// same dispatch loop the call came from and the same tier.
///
/// The buffering layer is what calls a handler here
/// (`Z/main/output.c:944`), with the buffer and the
/// `PHP_OUTPUT_HANDLER_*` bits: two arguments,
/// so a native of one arity serves and one of another raises where php
/// raises.
///
/// ```php
/// ob_start('trim'); echo " abc ";
/// ```
/// ```expect
/// abc
/// ```
///
/// A user function serving as the handler: the buffer crosses into the
/// body, the body's return crosses back, and what it returned is what
/// comes out.
///
/// ```php
/// function shout($buffer, $mode) { return strtoupper($buffer); }
/// ob_start('shout');
/// echo "abc";
/// ```
/// ```expect
/// ABC
/// ```
///
/// A throwable the body leaves pending crosses the boundary the other
/// way: the handler is disabled and its buffer comes out whole
/// (`php_output_handler_op`'s `Failure`), the `ob_end_flush()` that
/// called it hands the throwable on, and the trace names the boundary
/// `[internal function]`, as `zend_trace_to_string` does for a frame no
/// opline called.
///
/// ```php
/// function boom($buffer, $mode) { return strtoupper(); }
/// ob_start('boom');
/// echo "abc";
/// ob_end_flush();
/// echo "after\n";
/// ```
/// ```expect exit=255
/// abc
/// Fatal error: Uncaught ArgumentCountError: strtoupper() expects exactly 1 argument, 0 given in %s:2
/// Stack trace:
/// #0 %s(2): strtoupper()
/// #1 [internal function]: boom('abc', 9)
/// #2 %s(5): ob_end_flush()
/// #3 {main}
///   thrown in %s on line 2
/// ```
///
/// The three forms `zend_is_callable_ex` resolves beside a plain name,
/// each serving the same way once resolved: a `[$object, 'method']`
/// pair on its receiver, a `['Class', 'method']` pair and the
/// `Class::method` string on the class, with no receiver.
///
/// ```php
/// class Wrap {
///     public function handle($buffer, $mode) { return "[$buffer]"; }
///     public static function shout($buffer, $mode) { return strtoupper($buffer); }
/// }
/// $wrap = new Wrap();
/// ob_start([$wrap, 'handle']); echo "one"; ob_end_flush(); echo "\n";
/// ob_start(['Wrap', 'shout']); echo "two"; ob_end_flush(); echo "\n";
/// ob_start('Wrap::shout'); echo "three"; ob_end_flush(); echo "\n";
/// ```
/// ```expect
/// [one]
/// TWO
/// THREE
/// ```
///
/// A pair naming a method the class declares as an instance method is
/// no callable at all, and the sentence `zend_is_callable_check_func`
/// writes for it is what the argument error ends with, where the
/// callback is taken and not where it would be served.
///
/// ```php
/// class Wrap { public function handle() { return 1; } }
/// register_shutdown_function(['Wrap', 'handle']);
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught TypeError: register_shutdown_function(): Argument #1 ($callback) must be a valid callback, non-static method Wrap::handle() cannot be called statically in %s:3
/// Stack trace:
/// #0 %s(3): register_shutdown_function(Array)
/// #1 {main}
///   thrown in %s on line 3
/// ```
#[cfg_attr(debug_assertions, track_caller)]
pub fn call(
    req: &mut Request,
    callable: &Callable,
    arguments: &[Value],
) -> Result<Owner<Value>, Thrown> {
    adopted(call_made(req, callable, arguments))
}

/// What [`call`] makes, its one owner not yet named.
pub(crate) fn call_made(
    req: &mut Request,
    callable: &Callable,
    arguments: &[Value],
) -> Result<Value, Thrown> {
    serve(req, callable.target(), arguments, &[])
}

/// `zend_call_function` with named arguments after the positional ones,
/// as its walk of `fci->named_params` places them
/// (`Z/Zend/zend_execute_API.c:894`): each one at the offset the callee
/// declares that name at, the slots between left `UNDEF` for the
/// prologue to fill, which is `zend_handle_named_arg`
/// (`Z/Zend/zend_execute.c:5518`). A name the callee declares no
/// parameter for is php's `Error` for an unknown named parameter, and a
/// name a positional argument already filled is php's other one; a
/// name a variadic callee collects rides on the call as its extra named
/// arguments. What an attribute's constructor is served with; a call
/// whose whole argument list is one array is [`call_array`], and the
/// witnesses for the placement stand beside `call_user_func_array` in
/// `ext/standard`.
#[cfg_attr(debug_assertions, track_caller)]
pub fn call_named(
    req: &mut Request,
    callable: &Callable,
    arguments: &[Value],
    named: &[(PhpString, Value)],
) -> Result<Owner<Value>, Thrown> {
    adopted(call_named_made(req, callable, arguments, named))
}

/// What [`call_named`] makes, its one owner not yet named.
pub(crate) fn call_named_made(
    req: &mut Request,
    callable: &Callable,
    arguments: &[Value],
    named: &[(PhpString, Value)],
) -> Result<Value, Thrown> {
    let target = callable.target();
    let Some(function) = callee(req, target).filter(|_| !named.is_empty()) else {
        return serve(req, target, arguments, &[]);
    };
    let placement = placed(req, function, arguments, named)?;
    serve(req, target, &placement.arguments, &placement.extra_named)
}

/// `zend_call_function`'s walk of `fci->named_params` over `function`:
/// the positional arguments with each named one placed at the offset
/// the function declares its name at, and the extra named ones a
/// variadic collects. What [`call_named`] serves and `Closure::call`
/// hands its own serving.
pub(crate) fn placed(
    req: &mut Request,
    function: Function,
    arguments: &[Value],
    named: &[(PhpString, Value)],
) -> Result<Placement, Thrown> {
    let mut placement = Placement::new(function, arguments);
    for (name, value) in named {
        placement.named(req, *name, *value)?;
    }
    Ok(placement)
}

/// `zend_call_function` with `fci->named_params` the whole argument
/// list and no `fci->params`, which is how `call_user_func_array` and
/// `ReflectionFunction::invokeArgs` hand their array over: its walk
/// (`Z/Zend/zend_execute_API.c:894`) takes an integer-keyed element as
/// the next positional argument, at the offset its turn gives it and
/// never at its key, and a string-keyed one as a named argument placed
/// as [`call_named`] places it, in the array's order, so a positional
/// element after a named one is php's `Error` there and not before an
/// earlier name's own. A reference among the elements is handed on as
/// it is, and the walk follows it for a parameter taken by value but a
/// trampoline's, "don't separate references for __call".
///
/// ```php
/// function inc(&$x) { $x++; }
/// class P { function __call($n, $a) { $a[0]++; } }
/// function v($x) { $x = 5; }
/// $f = 'call_user_func_array';
/// $arg = 1;
/// $f('inc', [&$arg]);
/// $f([new P, 'bar'], [&$arg]);
/// $f('v', [&$arg]);
/// var_dump($arg);
/// ```
/// ```expect
/// int(3)
/// ```
#[cfg_attr(debug_assertions, track_caller)]
pub fn call_array(
    req: &mut Request,
    callable: &Callable,
    array: PhpArray,
) -> Result<Owner<Value>, Thrown> {
    adopted(call_array_made(req, callable, array))
}

/// What [`call_array`] makes, its one owner not yet named.
pub(crate) fn call_array_made(
    req: &mut Request,
    callable: &Callable,
    array: PhpArray,
) -> Result<Value, Thrown> {
    let target = callable.target();
    // `zend_call_function`'s `ZEND_HASH_FOREACH_STR_KEY_VAL(fci->named_params,
    // …)` (`Z/Zend/zend_execute_API.c:924`) places the rows read only, under
    // the borrowed request; the call takes its own owner of each as it
    // binds it, and the array's holder keeps the rest standing.
    let Some(function) = callee(req, target) else {
        let positional: Vec<Value> = walk::rows(req, array).map(|(_, v)| v).collect();
        return serve(req, target, &positional, &[]);
    };
    let mut placement = Placement::new(function, &[]);
    let mut have_named = false;
    let mut refused = None;
    for (key, value) in walk::rows(req, array) {
        match key {
            Key::Int(_) if have_named => {
                refused = Some(None);
                break;
            }
            Key::Int(_) => placement.arguments.push(value),
            Key::Str(name) => {
                have_named = true;
                if let Err(named) = placement.place_named(req, name, value) {
                    refused = Some(Some((named, name)));
                    break;
                }
            }
        }
    }
    match refused {
        None => serve(req, target, &placement.arguments, &placement.extra_named),
        Some(None) => Err(error(
            req,
            "Cannot use positional argument after named argument",
        )),
        Some(Some((named, name))) => Err(named.throw(req, name)),
    }
}

/// One call's argument list while `zend_call_function` builds it: the
/// positional arguments as placed so far, with the slots a named one
/// skipped `UNDEF`, and the extra named arguments a variadic callee
/// collects, against the parameter names the callee declares.
pub(crate) struct Placement {
    function: Function,
    pub(crate) arguments: Vec<Value>,
    pub(crate) extra_named: Vec<(PhpString, Value)>,
}

impl Placement {
    fn new(function: Function, arguments: &[Value]) -> Placement {
        Placement {
            function,
            arguments: arguments.to_vec(),
            extra_named: Vec::new(),
        }
    }

    /// One named argument placed by [`handle_named_arg`], the function
    /// the `SEND_*` handlers place theirs by, and written where it went.
    fn named(&mut self, req: &mut Request, name: PhpString, value: Value) -> Result<(), Thrown> {
        self.place_named(req, name, value)
            .map_err(|refused| refused.throw(req, name))
    }

    /// [`Placement::named`] with php's refusal answered rather than
    /// thrown, so a reader under a borrowed request can place one.
    fn place_named(
        &mut self,
        req: &Request,
        name: PhpString,
        value: Value,
    ) -> Result<(), NamedRefused> {
        let offset = arg_offset_by_name(req, self.function, name.as_bytes());
        let Named { at, .. } =
            handle_named_arg(offset, &mut self.arguments, &mut self.extra_named, name)?;
        if let Some(slot) = at.slot(&mut self.arguments, &mut self.extra_named) {
            *slot = value;
        }
        Ok(())
    }
}

/// `zend_get_arg_offset_by_name` (`Z/Zend/zend_execute.c:5466`): where
/// a callee takes an argument of this name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ArgOffset {
    /// The declared parameter at this offset, the name compared byte for
    /// byte; the variadic's own name is none of them, since php walks
    /// `num_args` alone.
    Declared(usize),
    /// A name the callee declares no parameter for, which its variadic
    /// collects (`ZEND_ACC_VARIADIC`), at `num_args`.
    Variadic(usize),
    /// A name a callee without a variadic declares no parameter for.
    Unknown,
}

impl ArgOffset {
    /// The argument's number, php's `arg_offset + 1`: the variadic's is
    /// the one after the declared parameters, which is the position
    /// whose way of being sent is the variadic's. None for an unknown name.
    pub(crate) fn number(self) -> Option<u32> {
        match self {
            ArgOffset::Declared(at) | ArgOffset::Variadic(at) => u32::try_from(at + 1).ok(),
            ArgOffset::Unknown => None,
        }
    }
}

/// [`ArgOffset`] over the callee's declaration; a callee with none, the
/// pass function a `NEW` without a constructor serves among them, takes
/// no name.
pub(crate) fn arg_offset_by_name(req: &Request, function: Function, name: &[u8]) -> ArgOffset {
    let Some(decl) = req.symbols().function_decl(function) else {
        return ArgOffset::Unknown;
    };
    let declared = decl.parameters.iter().filter(|p| !p.variadic);
    if let Some(at) = declared.clone().position(|p| *p.name.0 == *name) {
        return ArgOffset::Declared(at);
    }
    if decl.parameters.iter().any(|p| p.variadic) {
        return ArgOffset::Variadic(declared.count());
    }
    ArgOffset::Unknown
}

/// Where [`handle_named_arg`] put a named argument: a slot of the call's
/// arguments, or an entry of its extra named ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NamedAt {
    Argument(usize),
    Extra(usize),
}

impl NamedAt {
    /// The slot itself, which [`handle_named_arg`] left `UNDEF`.
    pub(crate) fn slot<'a>(
        self,
        arguments: &'a mut [Value],
        extra_named: &'a mut [(PhpString, Value)],
    ) -> Option<&'a mut Value> {
        match self {
            NamedAt::Argument(at) => arguments.get_mut(at),
            NamedAt::Extra(at) => extra_named.get_mut(at).map(|(_, value)| value),
        }
    }
}

/// A named argument [`handle_named_arg`] placed: where, and the number
/// php's `arg_num_ptr` answers, which the by-reference bit is read at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Named {
    pub(crate) at: NamedAt,
    pub(crate) number: u32,
}

/// Why [`handle_named_arg`] placed nothing, each php's `Error`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NamedRefused {
    /// `Unknown named parameter $%s`.
    Unknown,
    /// `Named parameter $%s overwrites previous argument`: the slot is
    /// filled, or the variadic already collected the name.
    Overwrites,
}

impl NamedRefused {
    pub(crate) fn throw(self, req: &mut Request, name: PhpString) -> Thrown {
        let spelled = String::from_utf8_lossy(name.as_bytes()).into_owned();
        let message = match self {
            NamedRefused::Unknown => format!("Unknown named parameter ${spelled}"),
            NamedRefused::Overwrites => {
                format!("Named parameter ${spelled} overwrites previous argument")
            }
        };
        error(req, &message)
    }
}

/// `zend_handle_undef_args` (`Z/Zend/zend_execute.c:5597`) over the call
/// on top of the call stack, which `CHECK_UNDEF_ARGS` and
/// `zend_call_function` reach once a named argument may have skipped a
/// parameter: each slot left `UNDEF` filled before the callee is served. A
/// user function's is its `RECV_INIT`'s default, evaluated where the
/// parameter declares one, and php's `ArgumentCountError` where it
/// declares none, at the parameter's `RECV`; a native's is the default
/// its declaration carries, evaluated, and made a reference where the
/// parameter takes one, and the same error where the parameter is
/// required or its default is not known. php runs the walk under a
/// frame for the callee (`start_fake_frame`), so what it raises names
/// the callee and a trace carries the call with the arguments placed so
/// far; the call is served for as long as the walk runs, and a user
/// function's raise stands at its parameter's line, where php's frame
/// stands.
///
/// ```php
/// function f($a, $b = PHP_INT_SIZE * 2, $c = 1) { var_dump($a, $b, $c); }
/// f(1, c: 3);
/// var_dump(str_pad('abc', length: 6, pad_type: STR_PAD_LEFT));
/// function g($a, $b, $c = 1) {}
/// try { g(1, c: 3); } catch (ArgumentCountError $e) { echo $e->getMessage(), ' ', $e->getLine(), "\n"; }
/// try { array_keys([], strict: true); } catch (ArgumentCountError $e) { echo $e->getMessage(), "\n"; }
/// try { str_pad('a', pad_type: 1); } catch (ArgumentCountError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(1)
/// int(16)
/// int(3)
/// string(6) "   abc"
/// g(): Argument #2 ($b) not passed 5
/// array_keys(): Argument #2 ($filter_value) must be passed explicitly, because the default value is not known
/// str_pad(): Argument #2 ($length) not passed
/// ```
///
/// A default naming nothing raises where it is evaluated, under the
/// callee's frame:
///
/// ```php
/// function h($a = NOPE, $b = 2) {}
/// h(b: 1);
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught Error: Undefined constant "NOPE" in %s:2
/// Stack trace:
/// #0 %s(3): h(NULL, 1)
/// #1 {main}
///   thrown in %s on line 2
/// ```
pub(crate) fn handle_undef_args(req: &mut Request, call: FrameRef) -> Result<(), Thrown> {
    let Some(header) = req.vm().at(call) else {
        return Ok(());
    };
    let (Some(function), building) = (header.function, header.info.building()) else {
        return Ok(());
    };
    // `start_fake_frame`: the call current while its defaults are found.
    if let Some(header) = req.vm_mut().at_mut(call) {
        header.info.set_building(false);
    }
    let filled = fill_undef_args(req, call, function);
    if let Some(header) = req.vm_mut().at_mut(call) {
        header.info.set_building(building);
    }
    filled
}

/// The walk of [`handle_undef_args`] under the callee's frame.
fn fill_undef_args(req: &mut Request, call: FrameRef, function: Function) -> Result<(), Thrown> {
    let user = req
        .user_function(function)
        .map(|user| (php_unit::PublishedUnit::clone(&user.unit), user.body));
    let count = req.vm().at(call).map_or(0, |header| header.num_args);
    for at in 0..count {
        let undef = req.vm().stack.frame(call).is_some_and(|frame| {
            frame.slot(php_heap::Slot(at)).kind() == php_heap::ZvalType::Undef
        });
        if !undef {
            continue;
        }
        let place = at as usize;
        let value = match &user {
            Some((unit, body)) => user_default(req, unit, *body, place)?,
            None => native_default(req, function, place)?,
        };
        let placed = req
            .vm_mut()
            .stack
            .frame_mut(call)
            .and_then(|frame| frame.slots_mut().get_mut(place));
        match placed {
            Some(slot) => *slot = value.to_zval(),
            None => req.release(value),
        }
    }
    Ok(())
}

/// A user function's parameter at `at` filled as
/// `zend_handle_undef_args` fills it off the op array's own prologue,
/// whose oplines are the parameters in order.
fn user_default(req: &mut Request, unit: &Unit, body: Body, at: usize) -> Result<Value, Thrown> {
    let Some(code) = exec::body_of(unit, body) else {
        return Ok(Value::null());
    };
    let Some(recv) = code.oplines.get(at).copied() else {
        return Ok(Value::null());
    };
    let position = u32::try_from(at + 1).unwrap_or(u32::MAX);
    let filled = if recv.opcode == Opcode::RecvInit {
        exec::default_value(req, code, ConstIdx(recv.op2)).map(|(value, _)| value)
    } else {
        Err(argument_error(
            req,
            "ArgumentCountError",
            position,
            "not passed",
        ))
    };
    filled.map_err(|thrown| at_site(req, thrown, &code.filename.0, recv.lineno))
}

/// A native's parameter at `at` filled from its declaration: the
/// required ones and a default php's stub leaves `UNKNOWN` are
/// `zend_argument_error`'s, and one taken by reference is a reference
/// to the default (`ZVAL_NEW_REF`).
fn native_default(req: &mut Request, function: Function, at: usize) -> Result<Value, Thrown> {
    let position = u32::try_from(at + 1).unwrap_or(u32::MAX);
    let Some(decl) = req.symbols().function_decl(function) else {
        return Ok(Value::null());
    };
    let required = decl
        .parameters
        .iter()
        .rposition(|parameter| {
            matches!(parameter.default, ParameterDefault::None) && !parameter.variadic
        })
        .map_or(0, |last| last + 1);
    let parameter = decl.parameters.get(at).cloned();
    if at < required {
        return Err(argument_error(
            req,
            "ArgumentCountError",
            position,
            "not passed",
        ));
    }
    let Some(parameter) = parameter else {
        return Ok(Value::null());
    };
    // The stub's `= UNKNOWN` is `ParameterDefault::Unshown`: a default
    // php cannot show, so it cannot be filled either.
    let ParameterDefault::Expr(default) = parameter.default else {
        return Err(argument_error(
            req,
            "ArgumentCountError",
            position,
            "must be passed explicitly, because the default value is not known",
        ));
    };
    let scope = req.executed_scope();
    let value = eval_const_expr_made(req, &default, scope)?;
    if parameter.by_ref {
        return Ok(req.reference(value)?.as_value());
    }
    Ok(value)
}

/// A throwable raised under a user callee's frame, which php's
/// `start_fake_frame` stands at the callee's opline: the file and the
/// line it records are that opline's.
fn at_site(req: &mut Request, thrown: Thrown, file: &[u8], line: u32) -> Thrown {
    if let Some(object) = thrown.exception().copied() {
        let placed = PhpString::new(req, file)
            .and_then(|file| write_prop(req, &object, b"file", file.into_value()))
            .and_then(|()| {
                let line = Owner::int(i64::from(line));
                write_prop(req, &object, b"line", line)
            });
        if let Err(other) = placed {
            return other;
        }
    }
    thrown
}

/// `zend_handle_named_arg` (`Z/Zend/zend_execute.c:5518`), the one
/// placement of a named argument, which a `SEND_*` at a call site and
/// `zend_call_function`'s walk of `named_params` both reach: at the
/// declared parameter's offset, the slots between the last argument and
/// it laid down `UNDEF` for `CHECK_UNDEF_ARGS` to fill
/// (`ZEND_CALL_MAY_HAVE_UNDEF`), and a slot something already filled
/// refused; or, for a name a variadic collects, as one more extra named
/// argument (`ZEND_CALL_HAS_EXTRA_NAMED_PARAMS`), the same name twice
/// refused. The slot is left `UNDEF` for the caller to write, as php
/// hands back the zval; the name is pushed as it is given, and whether
/// it carries one more owner is the caller's to say.
/// Where [`named_arg_place`] puts a named argument: the slot of the
/// declared parameter, the call grown to it where it stands past what
/// the call was given, or a new row of the extra named arguments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NamedTo {
    Argument(usize),
    Extra,
}

/// `zend_handle_named_arg`'s decision (`Z/Zend/zend_execute.c:5518`),
/// the one rule both a call site's `SEND_*` and `zend_call_function`'s
/// walk place by: an unknown name refused; a name a variadic collects
/// placed as a new extra named row, refused where one of that name
/// stands; a declared parameter placed at its slot, refused where an
/// argument already fills it (`set`), the call's `passed` arguments
/// telling a slot past them, which is not yet filled; and the
/// parameter's number.
pub(crate) fn named_arg_place(
    offset: ArgOffset,
    passed: usize,
    set: impl Fn(usize) -> bool,
    extra_named: &[(PhpString, Value)],
    name: PhpString,
) -> Result<(NamedTo, u32), NamedRefused> {
    let number = offset.number().ok_or(NamedRefused::Unknown)?;
    match offset {
        ArgOffset::Unknown => Err(NamedRefused::Unknown),
        ArgOffset::Variadic(_) => {
            if extra_named
                .iter()
                .any(|(held, _)| held.as_bytes() == name.as_bytes())
            {
                return Err(NamedRefused::Overwrites);
            }
            Ok((NamedTo::Extra, number))
        }
        ArgOffset::Declared(at) => {
            if at < passed && set(at) {
                return Err(NamedRefused::Overwrites);
            }
            Ok((NamedTo::Argument(at), number))
        }
    }
}

pub(crate) fn handle_named_arg(
    offset: ArgOffset,
    arguments: &mut Vec<Value>,
    extra_named: &mut Vec<(PhpString, Value)>,
    name: PhpString,
) -> Result<Named, NamedRefused> {
    let set = |at: usize| {
        arguments
            .get(at)
            .is_some_and(|held| !matches!(held.as_ref(), ValueRef::Undef))
    };
    let (to, number) = named_arg_place(offset, arguments.len(), set, extra_named, name)?;
    let at = match to {
        NamedTo::Extra => {
            extra_named.push((name, Value::undef()));
            NamedAt::Extra(extra_named.len() - 1)
        }
        NamedTo::Argument(at) => {
            if arguments.len() <= at {
                arguments.resize(at + 1, Value::undef());
            }
            NamedAt::Argument(at)
        }
    };
    Ok(Named { at, number })
}

/// `zend_call_method` on `object`: the method looked up on the
/// object's class by its lowered name, and served on a call frame with
/// the object as its receiver and the arguments as one more owner each,
/// the frame popped whether the entry returned or threw. A name the
/// class has no entry for is php's `Error` for an undefined method.
/// `zend_call_method` serves it through `zend_call_function`, so what the
/// method left pending where no frame stands to take it is rendered as
/// the uncaught fatal where it was thrown and the call leaves by
/// `zend_bailout`, as [`call`]'s does ([`Request::thrown_without_frame`]):
/// a user filter's `filter()` as the request's resources are closed, and
/// a session's `__unserialize()` in session's `rinit`, whose warning that
/// the data did not decode then follows the fatal (#1879).
///
/// ```php
/// class F extends php_user_filter { function filter($in, $out, &$consumed, $closing): int { if ($closing) throw new Exception("closing"); return PSFS_PASS_ON; } }
/// stream_filter_register('f', 'F');
/// $fp = fopen('php://memory', 'w');
/// stream_filter_append($fp, 'f', STREAM_FILTER_WRITE);
/// register_shutdown_function(function () { echo "shutdown\n"; });
/// echo "end\n";
/// ```
/// ```expect exit=255
/// end
/// shutdown
///
/// Fatal error: Uncaught Exception: closing in %s:2
/// Stack trace:
/// #0 [internal function]: F->filter(Resource id #%d, Resource id #%d, 0, true)
/// #1 {main}
///   thrown in %s on line 2
/// ```
#[cfg_attr(debug_assertions, track_caller)]
pub fn call_method(
    req: &mut Request,
    object: &PhpObject,
    method: &[u8],
    arguments: &[Value],
) -> Result<Owner<Value>, Thrown> {
    adopted(call_method_made(req, object, method, arguments))
}

/// What [`call_method`] makes, its one owner not yet named.
// The face's signature, which the engine's callers share.
#[allow(clippy::trivially_copy_pass_by_ref)]
pub(crate) fn call_method_made(
    req: &mut Request,
    object: &PhpObject,
    method: &[u8],
    arguments: &[Value],
) -> Result<Value, Thrown> {
    let class = object.class();
    let Some(function) = req.symbols().method_named(class, method) else {
        return Err(undefined_method(req, class, method));
    };
    let served = req.serve_method_made(function, Some(*object), class, arguments, &[]);
    without_frame(req, served)
}

/// A `zend_fcall_info_cache` filled by hand with a method the caller
/// already holds, which is what `reflection_method_invoke` hands
/// `zend_call_known_function` (`Z/ext/reflection/php_reflection.c:3543`):
/// the method `class` declares or inherits under `name`, `class` as
/// the called scope, and `this` as the receiver. Nothing is resolved
/// through `zend_is_callable_ex`, so no visibility is held against the
/// caller's scope and no receiver is asked for; a caller that owes php
/// those refusals makes them first. A name `class` neither declares
/// nor inherits is no callee. The callable holds no value of its own
/// and is served while the caller holds the receiver, through
/// [`call_named`] or [`call_array`].
#[must_use]
pub fn known_method(
    req: &Request,
    class: ClassId,
    name: &Name,
    this: Option<PhpObject>,
) -> Option<Callable> {
    // A hook, `$name::get` or `$name::set`, is no row of the class's
    // table; a reflector over one holds the hook itself, as php's
    // `reflection_method_factory` holds its `zend_function`.
    let function = match hook_name(&name.0) {
        Some((property, kind)) => req.property_hook_function(class, property, kind)?,
        None => req.symbols().method_named(class, &name.0)?,
    };
    let spelled = format!(
        "{}::{}",
        req.symbols().class_name(class),
        String::from_utf8_lossy(&name.0)
    );
    Some(Callable::of(
        Value::null(),
        spelled.as_bytes(),
        Target::Method {
            function,
            scope: class,
            this,
        },
    ))
}

/// A hook's name, `$name::get` or `$name::set` as
/// `zend_compile_property_hooks` names it, read back into the property
/// and the kind.
fn hook_name(name: &[u8]) -> Option<(&[u8], HookKind)> {
    let rest = name.strip_prefix(b"$")?;
    if let Some(property) = rest.strip_suffix(b"::get") {
        return Some((property, HookKind::Get));
    }
    rest.strip_suffix(b"::set")
        .map(|property| (property, HookKind::Set))
}

/// `prop_info->hooks[kind]` of the property `name` as `class`'s table
/// holds it, its own or one inherited: the class whose declaration
/// wrote the hook, which is the hook's scope, and the hook as a method,
/// named `$name::get` or `$name::set`; none where the property has no
/// hook of that kind, or `class` no property of that name. An abstract
/// hook is one, unless a backed declaration below it implements it.
///
/// ```php
/// class A { public $p { get => 1; set {} } }
/// class B extends A { public $p { get => 2; } }
/// $h = (new ReflectionProperty('B', 'p'))->getHooks();
/// var_dump($h['get']->class, $h['set']->class, $h['set']->name, $h['get']->invoke(new B));
/// ```
/// ```expect
/// string(1) "B"
/// string(1) "A"
/// string(7) "$p::set"
/// int(2)
/// ```
#[must_use]
pub fn property_hook(
    req: &Request,
    class: ClassId,
    name: &[u8],
    kind: HookKind,
) -> Option<(ClassId, MethodDecl)> {
    req.property_hook_decl(class, name, kind)
}

/// `ZEND_ACC_VIRTUAL` of the property `name` as `class`'s table holds
/// it: backed by no declaration of it up the chain, a hooked one whose
/// hooks never name it, or an internal class's that its handlers serve.
///
/// ```php
/// class A { public $v { get => 1; } public $b { get => $this->b; } public $p; }
/// foreach (['v', 'b', 'p'] as $n) var_dump((new ReflectionProperty('A', $n))->isVirtual());
/// var_dump((new ReflectionProperty('DatePeriod', 'start'))->isVirtual());
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// bool(false)
/// bool(true)
/// ```
#[must_use]
pub fn property_is_virtual(req: &Request, class: ClassId, name: &[u8]) -> bool {
    req.property_virtual(class, name)
}

/// A hooked property's backing value, read past its hooks as the
/// `get` hook itself reads it, which is what
/// `ReflectionProperty::getRawValue` reaches through
/// `zend_get_property_hook_trampoline`: a virtual property has none,
/// php's `Error`; a typed one nothing wrote is the uninitialised-access
/// `Error`. One more owner of the value, the caller's.
#[cfg_attr(debug_assertions, track_caller)]
pub fn read_raw_property(
    req: &mut Request,
    object: &PhpObject,
    name: &[u8],
) -> Result<Owner<Value>, Thrown> {
    adopted(req.read_raw(*object, name))
}

/// A hooked property's backing value written past its hooks, as the
/// `set` hook itself writes it, `ReflectionProperty::setRawValue`'s
/// trampoline: a virtual property has none, php's `Error`. A sink of
/// both owners, given up whatever arm runs.
pub fn write_raw_property(
    req: &mut Request,
    object: &PhpObject,
    name: Owner<PhpString>,
    value: Owner<Value>,
) -> Result<(), Thrown> {
    req.write_raw(*object, name, value)
}

/// `new $class(...$arguments)`: [`new_object`] and then the
/// constructor the object's `get_constructor` answers, served on it
/// with `arguments`, as `NEW` and its `DO_FCALL` serve them; a class
/// with none takes the arguments and serves nothing
/// (`zend_pass_function`). A constructor that throws leaves the object
/// marked as one whose constructor failed, so it is given up with no
/// destructor served (`zend_object_store_ctor_failed`).
#[cfg_attr(debug_assertions, track_caller)]
pub fn instantiate(
    req: &mut Request,
    class: ClassId,
    arguments: &[Value],
) -> Result<Owner<PhpObject>, Thrown> {
    instantiate_named(req, class, arguments, &[])
}

/// `object_init_with_constructor` (`Z/Zend/zend_API.c:1876`), what
/// `zend_get_attribute_object` makes an attribute's object with:
/// [`new_object`], the constructor the class's `get_constructor`
/// answers, php's `Error` where the scope may not call it, then served
/// with the positional arguments and the named ones placed as
/// [`call_named`] places them. A class with no constructor takes
/// positional arguments and no named one, php's `Unknown named
/// parameter`, the object given up with no destructor served. The
/// object is the caller's, one owner; what the constructor threw is the
/// `Err`, the object given up destructed
/// (`zend_object_store_ctor_failed`). The arguments stay the caller's.
///
/// ```php
/// #[Attribute] class MyAttrib { function __destruct() { echo "d\n"; } }
/// #[MyAttrib(notinterned: '')] class Test1 {}
/// $attr = (new ReflectionClass(Test1::class))->getAttributes()[0];
/// try { $attr->newInstance(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// class T { function __construct() { throw new Exception('c'); } function __destruct() { echo "d\n"; } }
/// try { (new ReflectionClass('T'))->newInstance(); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Unknown named parameter $notinterned
/// c
/// ```
#[cfg_attr(debug_assertions, track_caller)]
pub fn instantiate_named(
    req: &mut Request,
    class: ClassId,
    arguments: &[Value],
    named: &[(PhpString, Value)],
) -> Result<Owner<PhpObject>, Thrown> {
    adopted(instantiate_named_made(req, class, arguments, named))
}

/// What [`instantiate_named`] makes, its one owner not yet named.
pub(crate) fn instantiate_named_made(
    req: &mut Request,
    class: ClassId,
    arguments: &[Value],
    named: &[(PhpString, Value)],
) -> Result<PhpObject, Thrown> {
    let object = new_object_made(req, class)?;
    match construct_object(req, object, arguments, named, Constructed::ByInstantiation) {
        Ok(()) => Ok(object),
        Err(thrown) => {
            req.release(object.as_value());
            Err(thrown)
        }
    }
}

/// Which of php's two constructor calls outside an opcode serves an
/// object, which differ where the class has no constructor to take a
/// named argument and where its `get_constructor` refuses.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Constructed {
    /// `object_init_with_constructor`: a named argument with no
    /// constructor to take it is php's `Unknown named parameter`, and an
    /// object `get_constructor` refused is given up as it stands, its
    /// destructor served unless the refusal marked it.
    ByInstantiation,
    /// `zend_ast_evaluate`'s `ZEND_AST_NEW` arm: named arguments with no
    /// constructor to take them are dropped, and whatever is pending
    /// once the constructor ran, `get_constructor`'s refusal included,
    /// marks the object (`if (EG(exception))
    /// zend_object_store_ctor_failed`).
    ByConstantExpression,
}

/// The one constructor call [`instantiate_named`] and a constant
/// expression's `new` share: the constructor `object`'s
/// `get_constructor` answers, served on it with the positional
/// arguments and the named ones placed as `zend_call_function` places
/// them ([`placed`]), and none served for a class with no constructor,
/// whose named arguments `by` decides. On the `Err` the object is
/// marked as one whose constructor failed where php marks it
/// (`zend_object_store_ctor_failed`), so its destructor is not served,
/// and is the caller's to give up, before or after the arguments as its
/// path orders it. The arguments stay the caller's.
fn construct_object(
    req: &mut Request,
    object: PhpObject,
    arguments: &[Value],
    named: &[(PhpString, Value)],
    by: Constructed,
) -> Result<(), Thrown> {
    let constructor = match object.handlers(req).get_constructor(req, object) {
        Ok(constructor) => constructor,
        Err(thrown) => {
            if by == Constructed::ByConstantExpression {
                object.mark_destructor_called();
            }
            return Err(thrown);
        }
    };
    let Some(constructor) = constructor else {
        if by == Constructed::ByInstantiation
            && let Some((name, _)) = named.first()
        {
            object.mark_destructor_called();
            return Err(NamedRefused::Unknown.throw(req, *name));
        }
        return Ok(());
    };
    let served = placed(req, constructor, arguments, named).and_then(|placement| {
        req.serve_method_made(
            constructor,
            Some(object),
            object.class(),
            &placement.arguments,
            &placement.extra_named,
        )
    });
    match served {
        Ok(returned) => {
            req.release(returned);
            Ok(())
        }
        Err(thrown) => {
            object.mark_destructor_called();
            Err(thrown)
        }
    }
}

/// The arguments a constructor outside an opcode is handed, each
/// evaluated as a constant expression in written order, the named ones
/// with their names; what an argument threw gives up the ones before
/// it, as `zend_ast_evaluate`'s `ZEND_AST_NEW` arm and
/// `zend_get_attribute_object` give them up:
///
/// ```php
/// define('Q', 'x');
/// class P { function __construct($a, $b = null) {} }
/// function f($x = new P([Q], NOPE)) {}
/// try { f(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// function g($x = new P(b: [Q], a: NOPE)) {}
/// try { g(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Undefined constant "NOPE"
/// Undefined constant "NOPE"
/// ```
#[derive(Default)]
struct Arguments {
    positional: Vec<Value>,
    named: Vec<(PhpString, Value)>,
}

impl Arguments {
    /// Each of `written` evaluated by `evaluate`, in order.
    fn evaluated<'a, E: 'a>(
        req: &mut Request,
        written: impl IntoIterator<Item = (Option<&'a Name>, &'a E)>,
        mut evaluate: impl FnMut(&mut Request, &'a E) -> Result<Value, Thrown>,
    ) -> Result<Arguments, Thrown> {
        let mut arguments = Arguments::default();
        for (name, expr) in written {
            let pushed = evaluate(req, expr).and_then(|value| {
                let Some(name) = name else {
                    arguments.positional.push(value);
                    return Ok(());
                };
                // The name the program or the stub wrote, interned.
                let name = req
                    .declared_string(&name.0)
                    .inspect_err(|_| req.release(value))?;
                arguments.named.push((name.hand_on(), value));
                Ok(())
            });
            if let Err(thrown) = pushed {
                arguments.give_up(req);
                return Err(thrown);
            }
        }
        Ok(arguments)
    }

    /// `named_params` as `zend_get_attribute_object` lays it down,
    /// `zend_new_array(0)` with each named argument added by its name,
    /// each one more owner; none where no argument is named.
    fn named_table(&self, req: &mut Request) -> Result<Option<Owner<PhpArray>>, Thrown> {
        if self.named.is_empty() {
            return Ok(None);
        }
        let mut table = PhpArray::new(req)?;
        for (name, value) in &self.named {
            let key = Owner::take(req, Key::Str(*name));
            let value = Owner::take(req, *value);
            if let Err(thrown) = table.add_new(req, key, value) {
                table.give_up(req);
                return Err(thrown);
            }
        }
        Ok(Some(table))
    }

    /// Every argument's owner given up, its name's with it.
    fn give_up(self, req: &mut Request) {
        for value in self.positional {
            req.release(value);
        }
        for (name, value) in self.named {
            req.release(Value::string(name));
            req.release(value);
        }
    }
}

/// `zend_get_attribute_object` (`Z/Zend/zend_attributes.c:336`): an
/// object of `class` made for `attribute`, each argument evaluated as a
/// constant expression of `scope` ([`eval_const_expr`]) and handed to
/// the constructor, the named ones by name ([`instantiate_named`]).
/// Where the attribute is a program's, `file` is the file it was
/// written in, and the arguments are evaluated and the constructor
/// served under php's dummy frame, a site of the attribute's own at
/// that file and the attribute's line which no trace row shows: the
/// constructor's row names it as its caller, a throwable an argument
/// raises names it as the executing site, so no `[constant expression]`
/// row stands over an argument written on the attribute's line, and
/// the file's `strict_types` is the regime the constructor's arguments
/// are checked under. A native's attribute, `file` none, has no site.
///
/// The traces are pinned beside `ReflectionAttribute::newInstance`, in
/// `ext/reflection`.
///
/// The site answers `strict_types` as the attribute's file declares it,
/// for a user constructor and a native one alike:
///
/// ```php
/// declare(strict_types=1);
/// #[Attribute]
/// class A { public function __construct(int $x) {} }
/// #[A("1")]
/// class C {}
/// #[Attribute("1")]
/// class D {}
/// foreach (['C', 'D'] as $c) {
///     try { (new ReflectionClass($c))->getAttributes()[0]->newInstance(); echo "made\n"; }
///     catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// }
/// ```
/// ```expect
/// A::__construct(): Argument #1 ($x) must be of type int, string given, called in %s on line 5
/// Attribute::__construct(): Argument #1 ($flags) must be of type int, string given
/// ```
#[cfg_attr(debug_assertions, track_caller)]
pub fn attribute_object(
    req: &mut Request,
    class: ClassId,
    attribute: &php_unit::decl::Attribute,
    scope: Option<ClassId>,
    file: Option<&[u8]>,
) -> Result<Owner<PhpObject>, Thrown> {
    adopted(attribute_object_made(req, class, attribute, scope, file))
}

/// What [`attribute_object`] makes, its one owner not yet named.
pub(crate) fn attribute_object_made(
    req: &mut Request,
    class: ClassId,
    attribute: &php_unit::decl::Attribute,
    scope: Option<ClassId>,
    file: Option<&[u8]>,
) -> Result<PhpObject, Thrown> {
    if let Some(file) = file {
        req.vm_mut()
            .push_site(file, attribute.line, attribute.strict);
    }
    let made = attribute_object_at(req, class, attribute, scope);
    if file.is_some() {
        req.vm_mut().pop_site();
    }
    made
}

/// [`attribute_object`]'s body, under whatever site stands, with what
/// php lays down for the constructor's call standing for its length:
/// the argument array, `emalloc(argc * sizeof(zval))`, and the named
/// arguments' table, `zend_new_array(0)` filled as each is evaluated,
/// given back after the call, the positional arguments first, then the
/// array, then the table with the named ones. A native's argument is
/// `ZVAL_COPY_OR_DUP`'d out of the string its stub made persistent at
/// startup, so the request holds a copy of each but a known string
/// (`ZSTR_KNOWN`), which is interned; a program's are its unit's,
/// interned by opcache's persist.
///
/// ```php
/// function pk($f) { $k = $f(); unset($k); $s = memory_get_usage(); memory_reset_peak_usage(); $k = $f(); echo memory_get_peak_usage() - $s, ' ', memory_get_usage() - $s, "\n"; }
/// #[Attribute] class A { function __construct(public $x = null, public $y = null) {} }
/// #[A(1, 2)] function f1() {}
/// #[A(y: 2)] function f2() {}
/// $r1 = (new ReflectionFunction('f1'))->getAttributes()[0];
/// $r2 = (new ReflectionFunction('f2'))->getAttributes()[0];
/// $r3 = (new ReflectionFunction('strftime'))->getAttributes()[0];
/// pk(fn () => $r1->newInstance());
/// pk(fn () => $r2->newInstance());
/// pk(fn () => $r3->newInstance());
/// ```
/// ```expect
/// 112 112
/// 472 80
/// 552 144
/// ```
fn attribute_object_at(
    req: &mut Request,
    class: ClassId,
    attribute: &php_unit::decl::Attribute,
    scope: Option<ClassId>,
) -> Result<PhpObject, Thrown> {
    // `sizeof(zval)`.
    const ZVAL: usize = 16;
    let argc = attribute.arguments.len();
    let array = if argc == 0 {
        None
    } else {
        let laid = req.heap_mut().block(argc * ZVAL);
        Some(req.fatal_of(laid)?)
    };
    // A native's attribute is the one with no line (`php_unit::decl::Attribute`).
    let native = attribute.line == 0;
    let written = attribute
        .arguments
        .iter()
        .map(|argument| (argument.name.as_ref(), &argument.value));
    let evaluated = Arguments::evaluated(req, written, |req, expr| match expr {
        ConstExpr::Str(s) if native && !crate::engine::is_known(s) => {
            req.string(s).map(Value::string)
        }
        _ => eval_declared_made(req, expr, scope),
    })
    .and_then(|arguments| match arguments.named_table(req) {
        Ok(table) => Ok((arguments, table)),
        Err(thrown) => {
            arguments.give_up(req);
            Err(thrown)
        }
    });
    let (arguments, table) = match evaluated {
        Ok(evaluated) => evaluated,
        Err(thrown) => {
            if let Some(array) = array {
                req.heap_mut().free_block(array);
            }
            return Err(thrown);
        }
    };
    let made = instantiate_named_made(req, class, &arguments.positional, &arguments.named);
    arguments.give_up(req);
    if let Some(array) = array {
        req.heap_mut().free_block(array);
    }
    if let Some(table) = table {
        table.give_up(req);
    }
    made
}

/// `ce->unserialize(rval, ce, buf, buf_len, data)` over an object
/// `object_init_ex` made of the class: the class's unserializer handed
/// the payload of a `C:` record, `true` once it has read it. The
/// unserializer is the one php registers on the class entry: an
/// internal class's own, its handlers' [`ObjectHandlers::unserialize`]
/// (`gmp_unserialize`); otherwise `zend_implement_serializable` gives a
/// class that implements `Serializable`, and every class that inherits
/// from one, php's `zend_user_unserialize`, which serves the object's
/// `unserialize()` with the payload as a string. `false` is the class
/// with none, whose payload `object_custom` warns of and skips; the
/// `Err` is what the unserializer threw.
///
/// [`ObjectHandlers::unserialize`]: crate::api::abi::ObjectHandlers::unserialize
///
/// ```php
/// class S implements Serializable {
///     public $got;
///     function serialize() { return ''; }
///     function unserialize($data) { $this->got = $data; }
///     function __serialize(): array { return []; }
///     function __unserialize(array $a): void {}
/// }
/// var_dump(unserialize('C:1:"S":3:{abc}'), unserialize('C:11:"ArrayObject":21:{x:i:0;a:0:{};m:a:0:{}}'));
/// var_dump(unserialize('C:8:"stdClass":0:{}'));
/// ```
/// ```expect
/// object(S)#1 (1) {
///   ["got"]=>
///   string(3) "abc"
/// }
/// object(ArrayObject)#2 (1) {
///   ["storage":"ArrayObject":private]=>
///   array(0) {
///   }
/// }
///
/// Warning: Class stdClass has no unserializer in %s on line 10
/// object(stdClass)#2 (0) {
/// }
/// ```
pub fn class_unserialize(
    req: &mut Request,
    object: PhpObject,
    data: &[u8],
) -> Result<bool, Thrown> {
    if let Some(unserialize) = object.handlers(req).unserialize() {
        req.serve_as_table(object.class(), |req| unserialize(req, object, data))?;
        return Ok(true);
    }
    let serializable = req
        .class_named(Bare::resolved(b"serializable"))
        .is_some_and(|interface| req.symbols().instance_of(object.class(), interface));
    if !serializable {
        return Ok(false);
    }
    let data = Value::string(req.string(data)?);
    let answered = call_method_made(req, &object, b"unserialize", &[data]);
    req.release(data);
    req.release(answered?);
    Ok(true)
}

/// `zend_compile_file` and `zend_execute` over the file at `path`, what
/// `spl_autoload` does with the class file it finds: the path opened
/// along the `include_path` with nothing raised where nothing opens it
/// (`php_stream_open_for_zend_ex` without `REPORT_ERRORS`), a file the
/// request has included already left alone, and any other added to the
/// included files, compiled as a `require` (a `ParseError` is the `Err`)
/// and executed to its end on a frame that shares the calling user
/// frame's variables and has no `$this` and no scope. `true` once the
/// file ran; the `Err` is what it threw.
///
/// ```php
/// $dir = sys_get_temp_dir() . '/witness_load_file_' . getmypid();
/// @mkdir($dir);
/// file_put_contents("$dir/loaded.php", '<?php class Loaded {} $seen = "set by the file";');
/// set_include_path($dir);
/// function load() { spl_autoload('Loaded', '.php'); return $seen ?? 'unset'; }
/// var_dump(load(), class_exists('Loaded', false));
/// spl_autoload('Loaded', '.php');
/// var_dump(count(array_filter(get_included_files(), fn ($f) => str_ends_with($f, 'loaded.php'))));
/// unlink("$dir/loaded.php"); rmdir($dir);
/// ```
/// ```expect
/// string(15) "set by the file"
/// bool(true)
/// int(1)
/// ```
pub fn load_file(req: &mut Request, path: &[u8]) -> Result<bool, Thrown> {
    req.load_file(path)
}

/// `object_init_ex`: an instance with its declared properties' defaults
/// and no constructor call, a store entry of the request's heap with
/// one owner, the caller's; an interface, a trait, an enum or an
/// abstract class is php's `Error` for what cannot be instantiated.
#[cfg_attr(debug_assertions, track_caller)]
pub fn new_object(req: &mut Request, class: ClassId) -> Result<Owner<PhpObject>, Thrown> {
    adopted(new_object_made(req, class))
}

/// What [`new_object`] makes, its one owner not yet named.
pub(crate) fn new_object_made(req: &mut Request, class: ClassId) -> Result<PhpObject, Thrown> {
    req.instantiable(class)?;
    req.new_object(class)
}

/// `clone`: the class's `clone_obj` (design §5.4), a fresh entry
/// holding what the object holds with one more owner each and `__clone`
/// served on it; a class with a null slot is php's `Error` for an
/// uncloneable object, and a `__clone` the scope may not reach is
/// `zend_bad_method_call`'s. The copy is the caller's, one owner.
#[cfg_attr(debug_assertions, track_caller)]
pub fn clone(req: &mut Request, object: &PhpObject) -> Result<Owner<PhpObject>, Thrown> {
    req.clone_object(*object, None)
}

/// A class's `clone_obj` over an object with a native payload, php's
/// `spl_array_object_clone` and its kin: a fresh object of the class
/// with the members copied as `zend_objects_clone_members` copies them,
/// `payload` (the class's copy of the old one) in its slot, and then
/// the class's `__clone` served on it, the copy given up with what
/// `__clone` threw. What the payload names of the heap is the caller's
/// to have taken an owner of. The copy is the caller's, one owner.
#[cfg_attr(debug_assertions, track_caller)]
pub fn clone_with<T: std::any::Any + Send>(
    req: &mut Request,
    object: &PhpObject,
    payload: T,
) -> Result<Owner<PhpObject>, Thrown> {
    let copy = req.clone_with_payload(*object, payload)?;
    if let Err(thrown) = req.serve_clone_method(copy) {
        req.release(copy.as_value());
        return Err(thrown);
    }
    Ok(Owner::adopt(copy))
}

/// `zend_objects_clone_members(new, old)`, what a class's `clone_obj`
/// that makes its copy itself calls once the copy holds what the class
/// keeps (`spl_object_storage_clone`, after `addall`): the members of
/// `old` copied onto `new`, a fresh object of its class, and `__clone`
/// served on `new`. Until then `new`'s members are its class's
/// defaults.
///
/// ```php
/// class H extends SplObjectStorage {
///     public $tag = 'default';
///     public function getHash($o): string { echo "getHash {$this->tag}\n"; return spl_object_hash($o); }
///     public function __clone() { echo "__clone {$this->tag}\n"; }
/// }
/// $h = new H; $h->tag = 'set'; $h[new stdClass] = 1;
/// $k = clone $h;
/// ```
/// ```expect
/// getHash set
/// getHash default
/// __clone set
/// ```
pub fn clone_members(req: &mut Request, new: &PhpObject, old: &PhpObject) -> Result<(), Thrown> {
    req.clone_members_into(*new, *old)
}

/// `zend_create_internal_iterator_zval`: an `InternalIterator` over
/// `object`, walking the elements its class's
/// [`crate::api::abi::ObjectHandlers::iterator_element`] answers, which
/// is what `getIterator()` hands back for a class php iterates in C.
/// The iterator is the caller's, one owner, and holds one of `object`.
#[cfg_attr(debug_assertions, track_caller)]
pub fn internal_iterator(
    req: &mut Request,
    object: &PhpObject,
) -> Result<Owner<PhpObject>, Thrown> {
    adopted(crate::interfaces::internal_iterator(req, *object))
}

/// `PHAR_G(orig_fopen)(INTERNAL_FUNCTION_PARAM_PASSTHRU)`: the entry the
/// interception serving the call replaced
/// ([`crate::api::abi::ExtensionBuilder::intercept`]), served on the same
/// call, its arguments and its return's owner the caller's, the first
/// argument replaced by `first`, a sink of its owner, where one is
/// given, as phar's interceptors open
/// the `phar://` name they made of a relative path; `null` where the
/// entry serving is the native's own. Interceptions stack in bundle
/// order, as php saves each handler over the one before it, so the
/// last passes through to the one it replaced and that one on down to
/// the native: `file_exists` is opcache's over phar's over
/// `ext/standard`'s, each handing on what it does not answer itself.
///
/// ```php
/// var_dump(file_exists(__FILE__), is_file(__DIR__), is_readable(__DIR__ . '/nowhere'));
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// bool(false)
/// ```
pub fn passthrough(req: &mut Request, first: Option<Owner<Value>>) -> Result<Owner<Value>, Thrown> {
    let Some((entry, beneath, by)) = req.replaced_entry() else {
        if let Some(first) = first {
            first.give_up(req);
        }
        return Ok(Owner::null());
    };
    if let Some(first) = first {
        // The frame's argument slot holds the count from here.
        req.replace_first_argument(first.hand_on());
    }
    let above = beneath.saturating_sub(1);
    req.serve_beneath(beneath);
    // The replaced entry served as the extension that supplied it, the
    // interception before or the native's own (#2627).
    let served = match by {
        Some(by) => req.serve_for(by, entry),
        None => entry(req),
    };
    req.serve_beneath(above);
    served
}

/// The linked class named, `zend_lookup_class_ex` with
/// `ZEND_FETCH_CLASS_NO_AUTOLOAD`: one leading backslash stands for the
/// global namespace and is not part of the name.
///
/// ```php
/// namespace NS { class P {} class C extends P {} }
/// namespace { var_dump(class_parents('\\NS\\C', false), class_parents('\\\\NS\\C', false)); }
/// ```
/// ```expect
/// Warning: class_parents(): Class \\NS\C does not exist in %s on line 3
/// array(1) {
///   ["NS\P"]=>
///   string(4) "NS\P"
/// }
/// bool(false)
/// ```
///
/// A registered autoloader is not asked; the lookup that asks it is
/// [`autoload`].
///
/// ```php
/// spl_autoload_register(function ($c) { echo "load $c\n"; });
/// var_dump(class_parents('Nope', false), class_parents('Nope'));
/// ```
/// ```expect
/// Warning: class_parents(): Class Nope does not exist in %s on line 3
/// load Nope
///
/// Warning: class_parents(): Class Nope does not exist and could not be loaded in %s on line 3
/// bool(false)
/// bool(false)
/// ```
pub fn lookup_class(req: &mut Request, name: &[u8]) -> Result<Option<ClassId>, Thrown> {
    Ok(req.class_named(Bare::of(name)))
}

/// The linked function named, without autoloading: `function_exists`'s
/// lookup, a leading `\` skipped and the case folded, as php's
/// `zend_hash_find(EG(function_table), lcname)` past the `\`.
///
/// ```php
/// function g() {}
/// var_dump(function_exists('\\G'), function_exists('\\\\g'), function_exists('\\STRLEN'));
/// $f = function () {};
/// var_dump(function_exists((new ReflectionFunction($f))->getName()));
/// echo (new ReflectionFunction('\\G'))->getName(), "\n";
/// try { new ReflectionFunction('\\\\g'); } catch (ReflectionException $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// bool(true)
/// bool(false)
/// g
/// Function \\g() does not exist
/// ```
#[must_use]
pub fn lookup_function(req: &Request, name: &[u8]) -> Option<Function> {
    req.function_named(Bare::of(name))
}

/// The `static` variables of what `callable` names, name and value in the
/// order the op array's table holds them, as
/// `ReflectionFunctionAbstract::getStaticVariables` copies
/// `ZEND_MAP_PTR_GET(static_variables_ptr)`: a closure's own table, its
/// `use` variables first, and a function's or a method's the request's
/// copy, laid down at its starting values where no call has touched it,
/// as php lays it down there. A variable a `static` bound is the cell
/// the function binds, as php's bucket holds it. Empty for a native and
/// for a function with none. The values are views, no owner taken; the
/// `Err` is the memory limit refusing a name.
pub fn bound_variables(
    req: &mut Request,
    callable: &Callable,
) -> Result<Vec<(Owner<PhpString>, Value)>, Thrown> {
    let rows = req.static_variables(callable.target());
    (rows.into_iter())
        .map(|(name, value)| Ok((PhpString::new(req, &name)?, value)))
        .collect()
}

/// Serve the autoloaders for `name`, `zend_lookup_class_ex` after the
/// table miss (`Z/Zend/zend_execute_API.c:1228`) over
/// `spl_perform_autoload` (`Z/ext/spl/php_spl.c:414`): a name that is
/// no valid class name, or one an autoloader is already loading
/// (`EG(in_autoload)`), loads nothing, and nor does one the table holds
/// for a class whose link is under way; otherwise each callable of the
/// stack `spl_autoload_register` built is served with the name as it
/// was written, a leading `\` dropped, until the table has the class,
/// one throws, or none is left. The stack is read afresh at each
/// step, so a loader registered or removed by a loader counts as php's
/// walk of the live table counts it.
///
/// ```php
/// class Real { const LOADED = 'yes'; }
/// spl_autoload_register(function ($class) {
///     echo "first: $class\n";
/// });
/// spl_autoload_register(function ($class) {
///     echo "second: $class\n";
///     if ($class === 'Late') { class_alias('Real', $class); }
/// });
/// echo \Late::LOADED, "\n";
/// var_dump(class_exists('late'), class_exists('Never', false));
/// new Missing;
/// ```
/// ```expect exit=255
/// first: Late
/// second: Late
/// yes
/// bool(true)
/// bool(false)
/// first: Missing
/// second: Missing
///
/// Fatal error: Uncaught Error: Class "Missing" not found in %s:12
/// Stack trace:
/// #0 {main}
///   thrown in %s on line 12
/// ```
///
/// The first lookup that may autoload lays `EG(in_autoload)` down, its
/// header and the data of the lowered name it adds and deletes, and
/// the table stands until the request ends, so a second costs nothing.
///
/// ```php
/// $m = memory_get_usage(); class_exists('F'); echo memory_get_usage() - $m, "\n";
/// $m = memory_get_usage(); class_exists('G'); echo memory_get_usage() - $m, "\n";
/// ```
/// ```expect
/// 376
/// 0
/// ```
///
/// Each loader is handed the name as written, the unit's interned
/// string where the source wrote the name, which costs nothing; a name
/// past a leading `\` is a new string, as php's `zend_string_init` of
/// it is.
///
/// ```php
/// spl_autoload_register(function ($c) { $GLOBALS['held'] = $c; });
/// class_exists('First');
/// $m = memory_get_usage(); class_exists('Second'); echo memory_get_usage() - $m, "\n";
/// $m = memory_get_usage(); class_exists('\Third'); echo memory_get_usage() - $m, "\n";
/// ```
/// ```expect
/// 0
/// 32
/// ```
///
/// The lowered name `EG(in_autoload)` holds while the loaders are
/// served is the string php's `lc_name` is: the caller's key where it
/// has one ([`autoload_keyed`]), the lowered constant of a `new`, a
/// static call or a class constant, and the name itself where it has no
/// upper-case byte, both strings already standing; a new string only
/// past a leading `\` or where `zend_string_tolower` lowers a byte. So
/// a loader's throw costs what php's costs at its peak:
///
/// ```php
/// spl_autoload_register(function ($c) { throw new RuntimeException('boom'); });
/// function pk($f) { $f(); $s = memory_get_usage(); memory_reset_peak_usage(); $f(); echo memory_get_peak_usage() - $s, " ", memory_get_usage() - $s, "\n"; }
/// pk(function () { try { class_exists('boom'); } catch (Exception $e) {} });
/// pk(function () { try { new Boom; } catch (Exception $e) {} });
/// pk(function () { try { Boom::f(); } catch (Exception $e) {} });
/// pk(function () { try { class_exists('Boom'); } catch (Exception $e) {} });
/// ```
/// ```expect
/// 3064 32
/// 2312 0
/// 2312 0
/// 3096 0
/// ```
///
/// A name the program computed with no upper-case byte, which php hands
/// on as the string it is, is the interned string of its bytes where one
/// stands and a new one otherwise, since the name reaches this lookup as
/// bytes.
pub fn autoload(req: &mut Request, name: &[u8]) -> Result<Option<ClassId>, Thrown> {
    autoload_as(req, name, false)
}

/// [`autoload`] for a caller that holds the name's lowered form as a
/// string, `zend_lookup_class_ex`'s `key`: the lowered constant of the
/// pair the compiler wrote for a class it names, which `EG(in_autoload)`
/// takes as it stands.
pub(crate) fn autoload_keyed(req: &mut Request, name: &[u8]) -> Result<Option<ClassId>, Thrown> {
    autoload_as(req, name, true)
}

/// [`autoload`], `keyed` where the caller holds the lowered name.
fn autoload_as(req: &mut Request, name: &[u8], keyed: bool) -> Result<Option<ClassId>, Thrown> {
    if name.is_empty() {
        return Ok(None);
    }
    let key = Bare::of(name);
    let bare = key.bytes();
    // The table first, which holds an anonymous class under its name
    // with the NUL, and `zend_is_valid_class_name` of the name as
    // written only before the autoloaders.
    if let Some(class) = req.class_named(key) {
        return Ok(Some(class));
    }
    let valid = name
        .iter()
        .all(|b| b.is_ascii_alphanumeric() || *b == b'_' || *b == b'\\' || *b >= 0x80);
    if !valid {
        return Ok(None);
    }
    if req.link_under_way(key) {
        return Ok(None);
    }
    let lowered = crate::exec::Lowered::of(bare);
    // `lc_name`: the key, or `zend_string_tolower(name)`, which is the
    // name itself where it lowers nothing; a name past a leading `\` is
    // lowered into a string of its own.
    let standing =
        bare.len() == name.len() && (keyed || matches!(lowered, crate::exec::Lowered::Borrowed(_)));
    let Some(lowered) = req.autoload_enter(&lowered, standing)? else {
        return Ok(None);
    };
    let served = req.without_site_override(|req| {
        let mut at = 0;
        while let Some(loader) = req.autoloader(at) {
            // php hands each loader the name it was given, a string
            // already standing (the unit's interned string for a name the
            // source wrote), and makes a new one only for the name past
            // a leading `\`.
            let class = if bare.len() < name.len() {
                req.string(bare)?
            } else {
                req.standing_string(bare)?
            };
            let returned = serve(req, loader.target(), &[Value::string(class)], &[]);
            req.release(Value::string(class));
            req.release(returned?);
            if let Some(class) = req.class_named(key) {
                return Ok(Some(class));
            }
            at += 1;
        }
        Ok(None)
    });
    req.autoload_leave(lowered);
    served
}

/// `spl_perform_autoload(class_name, lc_name)`
/// (`Z/ext/spl/php_spl.c:413`), what `spl_autoload_call()` runs: each
/// autoloader handed `name` as it was given, no leading `\` dropped and
/// no validity asked, whatever the table already holds, until one
/// declares the class under `name` lowered, which a name with a leading
/// `\` never is; a loader that throws ends the walk with the throwable
/// pending. [`autoload`] is `zend_lookup_class`'s, which asks the table
/// and the name first.
///
/// ```php
/// spl_autoload_register(function ($c) { echo "[$c]"; });
/// foreach (['\\Zap', '\Zip', 'stdclass', '\\', '', 'a b'] as $n) { spl_autoload_call($n); }
/// echo "\n";
/// ```
/// ```expect exit=0
/// [\Zap][\Zip][stdclass][\][][a b]
/// ```
pub fn perform_autoload(req: &mut Request, name: PhpString) -> Result<Option<ClassId>, Thrown> {
    let lowered = name.as_bytes().to_ascii_lowercase();
    req.without_site_override(|req| {
        let mut at = 0;
        while let Some(loader) = req.autoloader(at) {
            let returned = serve(req, loader.target(), &[Value::string(name)], &[]);
            req.release(returned?);
            if !lowered.starts_with(b"\\")
                && let Some(class) = req.class_named(Bare::resolved(&lowered))
            {
                return Ok(Some(class));
            }
            at += 1;
        }
        Ok(None)
    })
}

/// A constant by the key the table holds it under, `zend_get_constant`:
/// `None` when undefined, nothing raised, no owner taken.
pub fn constant(req: &mut Request, name: &[u8]) -> Result<Option<Value>, Thrown> {
    Ok(req.constant_str(name))
}

/// `zend_get_constant_ex` under `ZEND_FETCH_CLASS_EXCEPTION`
/// (`Z/Zend/zend_constants.c:416`), a name as a program spells it
/// while it is served: a leading `\` is dropped; a `::` names a class constant,
/// the class fetched as `zend_fetch_class` fetches it, `self`, `parent`
/// and `static` against the executed and called scopes and anything
/// else by lookup with autoload, and the constant read through
/// [`Request::class_constant`] with the class named in an error as it
/// was written; a `\` left in the name is a namespaced constant, the
/// namespace lowered and the rest as written; and anything else is the
/// table and then `true`, `false` and `null` in any case
/// (`zend_get_special_const`). An undefined name is php's `Error`,
/// `Undefined constant "X"`, and a class not found `Class "X" not
/// found`. A constant the `#[\Deprecated]` attribute marks raises its
/// deprecation before the value is handed over, and a handler that
/// throws at it hands nothing over. The value is one more owner
/// (`ZVAL_COPY_OR_DUP`), the caller's.
///
/// `constant()` in `ext/standard` is this and nothing else, and its
/// witness is the one that pins each arm at the oracle.
#[cfg_attr(debug_assertions, track_caller)]
pub fn fetch_constant(req: &mut Request, name: &[u8]) -> Result<Owner<Value>, Thrown> {
    // `zend_get_constant_ex`'s own strip; the class part is then handed
    // to the class lookup as written, which strips a `\` of its own, as
    // `zend_fetch_class` does: `constant("\\\\ArrayObject::STD_PROP_LIST")`
    // is `int(1)` under php.
    let name = name.strip_prefix(b"\\").unwrap_or(name);
    if let Some(colon) = name.iter().rposition(|b| *b == b':')
        && colon > 0
        && name.get(colon.wrapping_sub(1)) == Some(&b':')
    {
        let (class_name, constant_name) = (&name[..colon - 1], &name[colon + 1..]);
        let class = fetch_class(req, class_name)?;
        let shown = String::from_utf8_lossy(class_name).into_owned();
        return adopted(req.class_constant_shown(class, constant_name, &shown));
    }
    adopted(global_constant(req, name, None))
}

/// `zend_get_constant_ex`'s arm for a constant that is no class's
/// (`Z/Zend/zend_constants.c:448`), `name` without its leading `\`: a
/// `\` in the name is a namespaced constant, looked up with the
/// namespace lowered and the rest as written, and where it misses,
/// `fallback`, the global name of php's
/// `IS_CONSTANT_UNQUALIFIED_IN_NAMESPACE`; a name with no `\` is the
/// table, the halt offset and then `true`, `false` and `null` in any
/// case (`zend_get_constant_str_impl`). A miss is php's `Error` naming
/// `name`; a constant `#[\Deprecated]` marks raises its deprecation,
/// worded from the key it was found by, before the value is handed
/// over, one more owner, the caller's.
fn global_constant(
    req: &mut Request,
    name: &[u8],
    fallback: Option<&[u8]>,
) -> Result<Value, Thrown> {
    let found = if name.contains(&b'\\') {
        let key = Request::constant_key(name);
        req.constant_named(&key)
            .map(|value| (key, value))
            .or_else(|| fallback.and_then(|global| unqualified_constant(req, global)))
    } else {
        unqualified_constant(req, name)
    };
    let Some((key, value)) = found else {
        return Err(error(
            req,
            &format!("Undefined constant \"{}\"", String::from_utf8_lossy(name)),
        ));
    };
    crate::api::raising::deprecated_constant(req, &key)?;
    req.addref(&value);
    Ok(value)
}

/// `zend_get_constant_str_impl`: a name with no namespace, by the key
/// the table holds it under, the halt offset, and then
/// `zend_get_special_const`.
fn unqualified_constant(req: &Request, name: &[u8]) -> Option<(Vec<u8>, Value)> {
    req.constant_str(name)
        .or_else(|| special_constant(name))
        .map(|value| (name.to_vec(), value))
}

/// `zend_get_special_const`: `true`, `false` and `null` in any case,
/// the three names the table never holds; `None` for every other.
fn special_constant(name: &[u8]) -> Option<Value> {
    match name.to_ascii_lowercase().as_slice() {
        b"true" => Some(Value::bool(true)),
        b"false" => Some(Value::bool(false)),
        b"null" => Some(Value::null()),
        _ => None,
    }
}

/// `zend_fetch_class` under `ZEND_FETCH_CLASS_EXCEPTION`
/// (`Z/Zend/zend_execute_API.c:1712`) as `zend_get_class_constant_ex` calls
/// it on a name a program spelled: `self` and `parent` are the
/// executed scope's and its parent's, `static` the called scope's, each
/// php's `Error` where there is no such scope, and any other name is
/// `zend_lookup_class_ex`, the table and then the autoloaders, with
/// `Class "X" not found` where neither has it.
pub(crate) fn fetch_class(req: &mut Request, name: &[u8]) -> Result<ClassId, Thrown> {
    if name.eq_ignore_ascii_case(b"self") || name.eq_ignore_ascii_case(b"parent") {
        let scope = req.frames().scope();
        return const_class(req, name, scope);
    }
    if name.eq_ignore_ascii_case(b"static") {
        return req.frames().called_scope().ok_or_else(|| {
            error(
                req,
                "Cannot access \"static\" when no class scope is active",
            )
        });
    }
    autoload(req, name)?.ok_or_else(|| class_not_found(req, name))
}

/// `zend_update_class_constants` (`Z/Zend/zend_API.c:1535`): the
/// constants, the `static` properties and the defaults of `class` and of
/// its parents evaluated once in the request, each held to its type, as
/// `new` and a `static` property's first read ask for it. A failure is the
/// throwable it raised, and the next call tries again where it failed.
pub fn update_class_constants(req: &mut Request, class: ClassId) -> Result<(), Thrown> {
    req.update_class_constants(class)
}

/// `zend_update_class_constant` over the constant `name` of `class`, as
/// Reflection reads one: its expression evaluated on first use in the
/// scope that declared it and held to its type, with no access checked
/// and no deprecation raised, and php's `Error` for a name the class has
/// not. A constant of an enum is a case, so what comes back is the
/// enum's one instance for that case (`zend_enum_get_case`), by
/// identity. One owner is the caller's.
///
/// ```php
/// class A { const array CONST1 = C; }
/// class B { private const object CONST1 = C; }
/// define("C", new stdClass());
/// $rc = new ReflectionClassConstant(A::class, 'CONST1');
/// try { $rc->getValue(); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { echo $rc; } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// var_dump((new ReflectionClass('B'))->getConstant('CONST1'));
/// ```
/// ```expect
/// Cannot assign stdClass to class constant A::CONST1 of type array
/// Cannot assign stdClass to class constant A::CONST1 of type array
/// object(stdClass)#1 (0) {
/// }
/// ```
#[cfg_attr(debug_assertions, track_caller)]
pub fn class_constant(
    req: &mut Request,
    class: ClassId,
    name: &[u8],
) -> Result<Owner<Value>, Thrown> {
    adopted(req.updated_class_constant(class, name))
}

/// Evaluate a constant expression in `scope`, `zval_update_constant_ex`:
/// the value with one owner, the caller's, whatever the node, a
/// constant by name among them, whose table keeps its own.
///
/// A constant's value in an array the expression builds is that array's
/// to own, so the array outlives the evaluation that read it:
///
/// ```php
/// define('V1', strtoupper('x'));
/// #[A1([V1 => V1])]
/// class C1 {}
/// $attr = (new ReflectionClass('C1'))->getAttributes();
/// $attr[0]->getArguments();
/// var_dump($attr[0]->getArguments(), V1);
/// ```
/// ```expect
/// array(1) {
///   [0]=>
///   array(1) {
///     ["X"]=>
///     string(1) "X"
///   }
/// }
/// string(1) "X"
/// ```
///
/// A native's default of flags joined by `|` is one, evaluated where a
/// named argument skipped the parameter:
///
/// ```php
/// echo htmlentities("The < character is encoded as &lt;", double_encode: false), "\n";
/// echo htmlspecialchars("'a' & \"b\"", double_encode: true), "\n";
/// ```
/// ```expect
/// The &lt; character is encoded as &lt;
/// &#039;a&#039; &amp; &quot;b&quot;
/// ```
///
/// `zend_ast_evaluate`'s operators over a class constant, each the
/// kernel its opcode serves, a raise the kernel's own; an enum case a
/// native's default names is the case:
///
/// ```php
/// class K { const A = 1|2; const B = K::A | 4; const C = -K::A; const D = K::A . "x"; const E = !K::A; const F = +K::A; }
/// function f($x = K::A * 2, $r = RoundingMode::HalfEven) { return [$x, $r]; }
/// var_dump(K::B, K::C, K::D, K::E, K::F, f());
/// class M { const Z = K::A % 0; }
/// try { var_dump(M::Z); } catch (DivisionByZeroError $e) { echo get_class($e), ": ", $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(7)
/// int(-3)
/// string(2) "3x"
/// bool(false)
/// int(3)
/// array(2) {
///   [0]=>
///   int(6)
///   [1]=>
///   enum(RoundingMode::HalfEven)
/// }
/// DivisionByZeroError: Modulo by zero
/// ```
///
/// An expression that is a constant's name alone is fetched as
/// `zval_update_constant_ex` fetches it, with no site of its own, so its
/// error names the line executing; one inside anything else names its
/// own, `zend_ast_evaluate_ex`'s:
///
/// ```php
/// class D {
///     const Y = NOPE;
///     const Z = [NOPE];
/// }
/// try { D::Y; } catch (Error $e) { echo $e->getLine(), "\n"; }
/// try { D::Z; } catch (Error $e) { echo $e->getLine(), "\n"; }
/// ```
/// ```expect
/// 6
/// 4
/// ```
#[cfg_attr(debug_assertions, track_caller)]
pub fn eval_const_expr(
    req: &mut Request,
    expr: &ConstExpr,
    scope: Option<ClassId>,
) -> Result<Owner<Value>, Thrown> {
    adopted(eval_const_expr_made(req, expr, scope))
}

/// What [`eval_const_expr`] makes, its one owner not yet named.
pub(crate) fn eval_const_expr_made(
    req: &mut Request,
    expr: &ConstExpr,
    scope: Option<ClassId>,
) -> Result<Value, Thrown> {
    // `zval_update_constant_ex`'s fast path: an expression that is a
    // constant's name alone is fetched with no site of its own.
    if let ConstExpr::Constant { .. } = expr {
        return evaluate_node(req, expr, scope);
    }
    evaluate(req, expr, scope)
}

/// A declared value as `zend_declare_typed_property` and
/// `zend_declare_class_constant_ex` keep it (`Z/Zend/zend_API.c:4510`,
/// `:4852`): a string the declaration wrote is `zval_make_interned_string`'s,
/// so a slot or a constant copied from it takes no owner; anything else
/// is [`eval_const_expr`]'s. An attribute's argument and a backed enum
/// case's value are read through it too: a unit's declarations hold
/// their strings as its literals do, interned as opcache persists them
/// (design §5.2, revision 64), where `php -n` copies them per request,
/// the difference `trace:immutable-literal-arrays` covers.
#[cfg_attr(debug_assertions, track_caller)]
pub fn eval_declared(
    req: &mut Request,
    expr: &ConstExpr,
    scope: Option<ClassId>,
) -> Result<Owner<Value>, Thrown> {
    adopted(eval_declared_made(req, expr, scope))
}

/// What [`eval_declared`] makes, its one owner not yet named.
pub(crate) fn eval_declared_made(
    req: &mut Request,
    expr: &ConstExpr,
    scope: Option<ClassId>,
) -> Result<Value, Thrown> {
    if let ConstExpr::Str(s) = expr {
        return Ok(req.declared_string(s)?.into_value().hand_on());
    }
    eval_const_expr_made(req, expr, scope)
}

/// `zend_ast_evaluate_ex`: one node of a constant expression under its
/// own site ([`Request::evaluate_at`]).
fn evaluate(req: &mut Request, expr: &ConstExpr, scope: Option<ClassId>) -> Result<Value, Thrown> {
    req.evaluate_at(scope, expr.line(), |req| evaluate_node(req, expr, scope))
}

/// `zend_ast_evaluate_inner`: one node of [`eval_const_expr`] under the
/// site `zend_ast_evaluate_ex` stood the request at.
///
/// A unit's array is copied with an owner, so an `eval`'s default costs
/// a call nothing and an attributed `const`'s keeps the constant's own
/// owner past the `eval`'s main op array:
///
/// ```php
/// eval('function h($p = [5 => 1]) { return $p; }');
/// h(); $m = memory_get_usage(); h(); h(); echo memory_get_usage() - $m, "\n";
/// eval('#[Attribute] class B {} #[B] const Y = [1, 2]; debug_zval_dump(Y);');
/// debug_zval_dump(Y);
/// ```
/// ```expect regime=jit-off
/// 0
/// array(2) packed refcount(3){
///   [0]=>
///   int(1)
///   [1]=>
///   int(2)
/// }
/// array(2) packed refcount(2){
///   [0]=>
///   int(1)
///   [1]=>
///   int(2)
/// }
/// ```
fn evaluate_node(
    req: &mut Request,
    expr: &ConstExpr,
    scope: Option<ClassId>,
) -> Result<Value, Thrown> {
    match expr {
        ConstExpr::Null => Ok(Value::null()),
        ConstExpr::Bool(b) => Ok(Value::bool(*b)),
        ConstExpr::Long { value: n, .. } => Ok(Value::int(*n)),
        ConstExpr::Double(f) => Ok(Value::float(*f)),
        ConstExpr::Str(s) => Ok(Value::string(req.string(s)?)),
        // `ZVAL_COPY` of the unit's array: one more owner of a request's,
        // none of an `IS_ARRAY_IMMUTABLE`.
        ConstExpr::UnitValue(held) => {
            let value = Value::of_zval(held.value());
            req.addref(&value);
            Ok(value)
        }
        // `zend_get_constant_ex` under the node's `attr`, which words a
        // `CONST_DEPRECATED` constant's deprecation after it found it.
        ConstExpr::Constant { name, .. } => {
            global_constant(req, &name.resolved().0, name.fallback())
        }
        ConstExpr::ClassConstant { class, name, .. } => {
            class_constant_expr(req, class, name, scope)
        }
        // `zend_ast_evaluate`'s `ZEND_AST_CLASS_CONST` arm over a name
        // that is no string the compiler folded: the name first, its
        // `TypeError` where it is no string
        // (`zend_invalid_class_constant_type_error`), and then the class.
        ConstExpr::DynamicClassConstant { class, name, .. } => {
            let named = evaluate(req, name, scope)?;
            let Some(bytes) = named.as_str_bytes().map(<[u8]>::to_vec) else {
                let message = format!(
                    "Cannot use value of type {} as class constant name",
                    named.type_name()
                );
                req.release(named);
                return Err(type_error(req, &message));
            };
            req.release(named);
            class_constant_expr(req, class, &Name(bytes.into_boxed_slice()), scope)
        }
        // `ZEND_AST_CONST_ENUM_INIT` read back as the case it made: the
        // enum's one instance for the case (`zend_enum_get_case`), which
        // is what its constant of the case's name holds.
        ConstExpr::EnumCase { class, case, .. } => {
            let id = const_class(req, &class.0, scope)?;
            req.class_constant(id, &case.0)
        }
        ConstExpr::ClassName { class, .. } => class_name_node(req, &class.0, scope),
        ConstExpr::Closure { .. }
        | ConstExpr::FunctionCallable { .. }
        | ConstExpr::MethodCallable { .. } => callable_node(req, expr, scope),
        // `zend_ast_evaluate`'s `ZEND_AST_CONSTANT_CLASS` arm.
        ConstExpr::ScopeClass => {
            let name = scope
                .map(|scope| req.class_name_bytes(scope))
                .unwrap_or_default();
            Ok(Value::string(req.string(&name)?))
        }
        ConstExpr::Array { elements, .. } => array_node(req, elements, scope),
        // `zend_ast_evaluate`'s `ZEND_AST_BINARY_OP`, `ZEND_AST_GREATER`
        // and unary arms: both sides evaluated, then the operator's own
        // kernel, which is what a native's default of flags joined by
        // `|` is evaluated by where a named argument skipped it,
        // each operand given up once the kernel has answered.
        ConstExpr::Binary { op, lhs, rhs, .. } => binary_node(req, &op.0, lhs, rhs, scope),
        // `ZEND_AST_UNARY_PLUS` is `0 + x` and `ZEND_AST_UNARY_MINUS`
        // `-1 * x`, as `zend_ast_evaluate` serves them.
        ConstExpr::Unary { op, operand, .. } => {
            let operand = evaluate(req, operand, scope)?;
            let answered = match &*op.0 {
                b"-" => crate::ops::mul(req, &Value::int(-1), &operand),
                b"+" => crate::ops::add(req, &Value::int(0), &operand),
                b"~" => crate::ops::bitwise_not(req, &operand),
                b"!" => crate::ops::is_true(req, &operand).map(|truth| Value::bool(!truth)),
                _ => Err(invalid_operations(req)),
            };
            req.release(operand);
            answered
        }
        ConstExpr::Conditional {
            cond,
            then,
            otherwise,
        } => {
            let held = evaluate(req, cond, scope)?;
            let truth = match crate::ops::is_true(req, &held) {
                Ok(truth) => truth,
                Err(thrown) => {
                    req.release(held);
                    return Err(thrown);
                }
            };
            let taken = match (truth, then) {
                (true, None) => return Ok(held),
                (true, Some(then)) => then,
                (false, _) => otherwise,
            };
            req.release(held);
            evaluate(req, taken, scope)
        }
        // `ZEND_AST_CAST`: the operand as the `CAST` opline converts it,
        // `_IS_BOOL` by its truth.
        ConstExpr::Cast { to, operand, .. } => {
            let value = evaluate(req, operand, scope)?;
            let cast = if *to == IS_BOOL {
                crate::ops::is_true(req, &value).map(Value::bool)
            } else {
                match crate::ops::CastTo::of(*to) {
                    Some(to) => crate::ops::cast(req, &value, to),
                    None => Err(invalid_operations(req)),
                }
            };
            req.release(value);
            cast
        }
        ConstExpr::Dim { .. } | ConstExpr::Prop { .. } => Ok(chain(req, expr, scope, false)?.value),
        ConstExpr::New {
            class, arguments, ..
        } => new_expression(req, &class.0, arguments, scope),
        ConstExpr::Unpack(_) => Err(invalid_operations(req)),
    }
}

/// `zend_ast_evaluate`'s `ZEND_AST_ARRAY` arm: each element in written
/// order, a `...` spread into the array under the array's own line; no
/// element at all the process's empty array, so a native's omitted `[]`
/// costs the request no table, as php's C default costs none:
///
/// ```php
/// $m = memory_get_usage(); $h = hash('md5', ''); $b = memory_get_usage();
/// $i = hash('md5', ' ');
/// var_dump($b - $m, memory_get_usage() - $b);
/// ```
/// ```expect
/// int(64)
/// int(64)
/// ```
fn array_node(
    req: &mut Request,
    elements: &[(Option<ConstExpr>, ConstExpr)],
    scope: Option<ClassId>,
) -> Result<Value, Thrown> {
    // `if (!list->children) ZVAL_EMPTY_ARRAY(result)`.
    if elements.is_empty() {
        return Ok(Value::array(crate::api::values::PhpArray::empty()));
    }
    let mut array = req.array()?;
    // `zval_ptr_dtor_nogc(result); return FAILURE;`: an element that
    // threw gives up the array built so far, whichever element it was.
    match array_elements(req, &mut array, elements, scope) {
        Ok(()) => Ok(Value::array(array)),
        Err(thrown) => {
            req.release_nogc(Value::array(array));
            Err(thrown)
        }
    }
}

/// [`array_node`]'s elements written into `array`, which stays the
/// caller's whether they all were or one threw: a spread's operand and
/// an element's key given up once written, as
/// `zend_ast_add_unpacked_element`'s caller and
/// `zend_ast_add_array_element` give them up, and the key where its
/// value threw. An element with no key past the last integer one, or a
/// spread's, throws php's `Error`, and the array is given up with it:
///
/// ```php
/// function test($x = [PHP_INT_MAX => 42, 'foo']) {}
/// try { test(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// class K { const S = ['k' . 'x' => 1, NOPE]; const U = [PHP_INT_MAX => 1, ...[2]]; }
/// try { K::S; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// try { K::U; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// function f($x = ['a' . 'b' => 'c' . 'd', 'e' . 'f' => NOPE]) {}
/// try { f(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Cannot add element to the array as the next element is already occupied
/// Undefined constant "NOPE"
/// Cannot add element to the array as the next element is already occupied
/// Undefined constant "NOPE"
/// ```
fn array_elements(
    req: &mut Request,
    array: &mut crate::api::values::PhpArray,
    elements: &[(Option<ConstExpr>, ConstExpr)],
    scope: Option<ClassId>,
) -> Result<(), Thrown> {
    for (key, value) in elements {
        if let ConstExpr::Unpack(inner) = value {
            let spread = evaluate(req, inner, scope)?;
            let added = add_unpacked_element(req, array, &spread);
            req.release_nogc(spread);
            added?;
            continue;
        }
        // `zend_ast_add_array_element`: the key evaluated before
        // the value, and the two written as `ADD_ARRAY_ELEMENT`
        // writes them, a key's deprecations and refusals its own.
        let key = match key {
            Some(key) => Some(evaluate(req, key, scope)?),
            None => None,
        };
        let value = match evaluate(req, value, scope) {
            Ok(value) => value,
            Err(thrown) => {
                if let Some(key) = key {
                    req.release_nogc(key);
                }
                return Err(thrown);
            }
        };
        let offset = match key {
            Some(key) => crate::ops::Offset::At {
                key,
                constant: false,
            },
            None => crate::ops::Offset::Next,
        };
        // The kernel takes the value and reads the key, which the
        // array holds an owner of where it wrote it.
        let added = crate::ops::add_array_element(req, *array, offset, value);
        if let Some(key) = key {
            req.release_nogc(key);
        }
        added?;
    }
    Ok(())
}

/// `zend_ast_evaluate`'s `ZEND_AST_CLASS_NAME` arm: `self` the scope's
/// name and `parent` its parent's, php's `Error` where there is none,
/// which names `self` whichever was written where no scope is; and a
/// name written out the name itself, as
/// `zend_get_default_from_internal_arg_info` folds a native's
/// `SplFileObject::class`, with no class looked up.
///
/// ```php
/// trait T { public $p = [self::class, parent::class]; }
/// class P {}
/// class C extends P { use T; }
/// var_dump((new C)->p);
/// $p = (new ReflectionMethod('SplFileInfo', 'setFileClass'))->getParameters()[0];
/// var_dump($p->getDefaultValue(), $p->isDefaultValueConstant());
/// echo $p, "\n";
/// class D { use T; }
/// try { new D; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// array(2) {
///   [0]=>
///   string(1) "C"
///   [1]=>
///   string(1) "P"
/// }
/// string(13) "SplFileObject"
/// bool(false)
/// Parameter #0 [ <optional> string $class = SplFileObject::class ]
/// Cannot use "parent" when current class scope has no parent
/// ```
///
/// A native's omitted default of a class's name is that name, so the
/// first `ArrayObject` of a request costs what php's does:
///
/// ```php
/// $m = memory_get_usage(); $a = new ArrayObject(); $b = memory_get_usage();
/// $c = new ArrayObject();
/// var_dump($b - $m, memory_get_usage() - $b);
/// ```
/// ```expect
/// int(216)
/// int(216)
/// ```
fn class_name_node(
    req: &mut Request,
    class: &[u8],
    scope: Option<ClassId>,
) -> Result<Value, Thrown> {
    let parent = class.eq_ignore_ascii_case(b"parent");
    let name = if parent || class.eq_ignore_ascii_case(b"self") {
        let Some(scope) = scope else {
            return Err(error(
                req,
                "Cannot use \"self\" when no class scope is active",
            ));
        };
        let named = if parent {
            req.symbols().class_parent(scope).ok_or_else(|| {
                error(
                    req,
                    "Cannot use \"parent\" when current class scope has no parent",
                )
            })?
        } else {
            scope
        };
        req.class_name_bytes(named)
    } else {
        class.to_vec()
    };
    // `ZVAL_STR_COPY(result, ce->name)`: the class's name, which for a
    // class the engine or the bundle declared costs the request nothing.
    Ok(req.declared_string(&name)?.into_value().hand_on())
}

/// `_IS_BOOL`, the type code a `(bool)` cast carries.
const IS_BOOL: u32 = 18;

/// A binary operator of a constant expression: `ZEND_AST_AND`,
/// `ZEND_AST_OR` and `ZEND_AST_COALESCE` evaluate the left side, and
/// the right one only where the left leaves the answer open, a
/// dimension on the left of a `??` read as `isset` reads it
/// (`ZEND_DIM_IS`); every other operator both sides and then its kernel
/// ([`binary`]).
fn binary_node(
    req: &mut Request,
    op: &[u8],
    lhs: &ConstExpr,
    rhs: &ConstExpr,
    scope: Option<ClassId>,
) -> Result<Value, Thrown> {
    match op {
        b"&&" | b"||" => {
            let or = op == b"||";
            let lhs = evaluate(req, lhs, scope)?;
            let held = crate::ops::is_true(req, &lhs);
            req.release(lhs);
            if held? == or {
                return Ok(Value::bool(or));
            }
            let rhs = evaluate(req, rhs, scope)?;
            let held = crate::ops::is_true(req, &rhs);
            req.release(rhs);
            held.map(Value::bool)
        }
        b"??" => {
            let lhs = chain(req, lhs, scope, true)?.value;
            if !matches!(lhs.deref().as_ref(), ValueRef::Null | ValueRef::Undef) {
                return Ok(lhs);
            }
            req.release(lhs);
            evaluate(req, rhs, scope)
        }
        _ => {
            let lhs = evaluate(req, lhs, scope)?;
            let rhs = match evaluate(req, rhs, scope) {
                Ok(rhs) => rhs,
                Err(thrown) => {
                    req.release(lhs);
                    return Err(thrown);
                }
            };
            let answered = binary(req, op, &lhs, &rhs);
            req.release(lhs);
            req.release(rhs);
            answered
        }
    }
}

/// What a dimension or a property of a constant expression read, and
/// whether a `?->` under it met `null` and cut the chain short, which
/// leaves every link above it `null` in silence (`short_circuited`).
struct Chained {
    value: Value,
    short: bool,
}

/// `zend_ast_evaluate`'s `ZEND_AST_DIM`, `ZEND_AST_PROP` and
/// `ZEND_AST_NULLSAFE_PROP` arms (`Z/Zend/zend_ast.c:866`, `:1167`),
/// which alone pass `short_circuited` up; `silent` is `ZEND_DIM_IS`, set
/// on a dimension a `??` stands over and on each dimension under it.
///
/// A dimension over an object is php's `Error`; any other container is
/// read as `FETCH_DIM_R` reads it, or as `FETCH_DIM_IS` under `silent`
/// ([`crate::handlers::dimension_value`]). A property is read off an
/// enum's case alone, php's `Error` for any other object and
/// `zend_wrong_property_read`'s warning for what is no object; the name
/// is converted to a string first.
///
/// ```php
/// enum A: string { case B = 'b'; const N = [self::B->name, self::B?->value]; }
/// const C = (null)?->x->y;
/// const D = [1, [2]][1][0] ?? 0;
/// class K { const E = A::N[1]; const F = K::G['k'] ?? 'f'; const G = []; }
/// var_dump(A::N, C, D, K::E, K::F);
/// class P { public $p = 1; }
/// function f($x = (new P)->p) {}
/// try { f(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// const H = (42)->p;
/// var_dump(H);
/// ```
/// ```expect
/// array(2) {
///   [0]=>
///   string(1) "B"
///   [1]=>
///   string(1) "b"
/// }
/// NULL
/// int(2)
/// string(1) "b"
/// string(1) "f"
/// Fetching properties on non-enums in constant expressions is not allowed
///
/// Warning: Attempt to read property "p" on int in %s on line 10
/// NULL
/// ```
fn chain(
    req: &mut Request,
    expr: &ConstExpr,
    scope: Option<ClassId>,
    silent: bool,
) -> Result<Chained, Thrown> {
    req.evaluate_at(scope, expr.line(), |req| {
        chain_node(req, expr, scope, silent)
    })
}

/// One link of [`chain`] under the site `zend_ast_evaluate_ex` stood
/// the request at.
fn chain_node(
    req: &mut Request,
    expr: &ConstExpr,
    scope: Option<ClassId>,
    silent: bool,
) -> Result<Chained, Thrown> {
    let link = |req: &mut Request, expr: &ConstExpr, silent: bool| match expr {
        ConstExpr::Dim { .. } | ConstExpr::Prop { .. } => chain(req, expr, scope, silent),
        other => Ok(Chained {
            value: evaluate(req, other, scope)?,
            short: false,
        }),
    };
    let short = || Chained {
        value: Value::null(),
        short: true,
    };
    match expr {
        ConstExpr::Dim { container, dim, .. } => {
            let under = link(
                req,
                container,
                silent && matches!(**container, ConstExpr::Dim { .. }),
            )?;
            if under.short {
                return Ok(short());
            }
            if under.value.deref().as_object().is_some() {
                req.release(under.value);
                return Err(error(
                    req,
                    "Cannot use [] on objects in constant expression",
                ));
            }
            let dim = match evaluate(req, dim, scope) {
                Ok(dim) => dim,
                Err(thrown) => {
                    req.release(under.value);
                    return Err(thrown);
                }
            };
            let mode = if silent {
                crate::ops::DimRead::Silent
            } else {
                crate::ops::DimRead::Warning
            };
            let value = crate::ops::dimension_const(req, &under.value, &dim, mode);
            req.release(dim);
            req.release(under.value);
            Ok(Chained {
                value: value?,
                short: false,
            })
        }
        ConstExpr::Prop {
            object,
            name,
            nullsafe,
            ..
        } => {
            let under = link(req, object, false)?;
            let read = prop_link(req, &under, name, *nullsafe, scope);
            req.release(under.value);
            read
        }
        other => link(req, other, silent),
    }
}

/// A property link of [`chain`] over what the link under it read, which
/// stays the caller's: cut short under a `?->` over `null`, the name
/// converted to a string, then the read off an enum's case, php's
/// `Error` for any other object and `zend_wrong_property_read`'s warning
/// for what is no object.
fn prop_link(
    req: &mut Request,
    under: &Chained,
    name: &ConstExpr,
    nullsafe: bool,
    scope: Option<ClassId>,
) -> Result<Chained, Thrown> {
    let object = under.value.deref();
    if under.short || (nullsafe && matches!(object.as_ref(), ValueRef::Null)) {
        return Ok(Chained {
            value: Value::null(),
            short: true,
        });
    }
    let name = evaluate(req, name, scope)?;
    let text = crate::ops::to_string(req, &name);
    req.release(name);
    let name = text?;
    let found = prop_read(req, &object, name.as_bytes());
    req.release(Value::string(name));
    found
}

/// [`prop_link`]'s read of `name` off `object`.
fn prop_read(req: &mut Request, object: &Value, name: &[u8]) -> Result<Chained, Thrown> {
    let Some(instance) = object.as_object() else {
        let shown = crate::ops::value_name(req, &object.undef_as_null());
        crate::api::raising::warning(
            req,
            &format!(
                "Attempt to read property \"{}\" on {shown}",
                String::from_utf8_lossy(name)
            ),
        );
        return Ok(Chained {
            value: Value::null(),
            short: false,
        });
    };
    let is_enum = req
        .symbols()
        .class_decl(instance.class())
        .is_some_and(|decl| decl.kind == php_unit::decl::ClassKind::Enum);
    if !is_enum {
        return Err(error(
            req,
            "Fetching properties on non-enums in constant expressions is not allowed",
        ));
    }
    let value = req
        .read_property(instance, name, PropertyRead::Warning)?
        .hand_on();
    Ok(Chained {
        value,
        short: false,
    })
}

/// `zend_ast_evaluate`'s `ZEND_AST_NEW` arm (`Z/Zend/zend_ast.c:969`):
/// the class fetched in `scope` ([`const_class`]), the instance made,
/// each argument evaluated in written order ([`Arguments::evaluated`]),
/// and the constructor served with them ([`construct_object`]). An
/// argument that throws gives the instance up as it stands, its
/// destructor served; a constructor that throws, or one
/// `get_constructor` refuses, gives it up marked, with none, after the
/// arguments. A class with no constructor drops named arguments where
/// `new` in code refuses them:
///
/// ```php
/// class C { function __construct($x) { throw new Exception("ctor"); } function __destruct() { echo "dtor\n"; } }
/// function f($a = new C(1)) { }
/// try { f(); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
/// function g($a = new C(NOPE)) { }
/// try { g(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// class P { private function __construct() {} function __destruct() { echo "P dtor\n"; } }
/// function h($p = new P) { }
/// try { h(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// class N { function __destruct() { echo "N dtor\n"; } }
/// function k($n = new N(a: 1)) { echo get_class($n), "\n"; }
/// k();
/// try { new N(a: 1); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// ctor
/// dtor
/// Undefined constant "NOPE"
/// Call to private P::__construct() from global scope
/// N
/// N dtor
/// Unknown named parameter $a
/// ```
fn new_expression(
    req: &mut Request,
    class: &[u8],
    arguments: &[(Option<Name>, ConstExpr)],
    scope: Option<ClassId>,
) -> Result<Value, Thrown> {
    let id = const_class(req, class, scope)?;
    let object = new_object_made(req, id)?;
    let written = arguments.iter().map(|(name, expr)| (name.as_ref(), expr));
    let arguments = match Arguments::evaluated(req, written, |req, expr| evaluate(req, expr, scope))
    {
        Ok(arguments) => arguments,
        Err(thrown) => {
            req.release(object.as_value());
            return Err(thrown);
        }
    };
    let constructed = construct_object(
        req,
        object,
        &arguments.positional,
        &arguments.named,
        Constructed::ByConstantExpression,
    );
    arguments.give_up(req);
    match constructed {
        Ok(()) => Ok(object.as_value()),
        Err(thrown) => {
            req.release(object.as_value());
            Err(thrown)
        }
    }
}

/// `zend_ast_evaluate`'s `ZEND_AST_CLASS_CONST` arm: the class fetched in
/// `scope` ([`const_class`]) and its constant read through
/// `zend_get_class_constant_ex`, whose access is checked against that
/// scope; `static` is refused while the file compiles. An error names
/// the class as the expression writes it, which is the string
/// `zend_get_class_constant_ex` is handed, `self` and `parent` among
/// them:
///
/// ```php
/// class A { const X = self::NOPE; const Z = [A::NOPE]; }
/// class B extends A { const Y = parent::NOPE; }
/// foreach (['A::X', 'B::Y', 'A::Z'] as $c) {
///     try { constant($c); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// }
/// ```
/// ```expect
/// Undefined constant self::NOPE
/// Undefined constant parent::NOPE
/// Undefined constant A::NOPE
/// ```
fn class_constant_expr(
    req: &mut Request,
    class: &Name,
    name: &Name,
    scope: Option<ClassId>,
) -> Result<Value, Thrown> {
    let id = const_class(req, &class.0, scope)?;
    let written = String::from_utf8_lossy(&class.0);
    req.class_constant_from(
        id,
        &name.0,
        &written,
        scope,
        crate::api::request::ConstantFetch::Lookup,
    )
}

/// The three nodes of a constant expression that evaluate to a
/// `Closure` (8.5): `ZEND_AST_OP_ARRAY`, `zend_create_closure(result,
/// func, scope, scope, NULL)` over the unit's function, and the
/// first-class callables of `ZEND_AST_CALL` and `ZEND_AST_STATIC_CALL`.
fn callable_node(
    req: &mut Request,
    expr: &ConstExpr,
    scope: Option<ClassId>,
) -> Result<Value, Thrown> {
    match expr {
        ConstExpr::Closure { at, unit, .. } => {
            let Some(unit) = unit.unit() else {
                return Err(crate::api::raising::fatal(
                    req,
                    "InternalError: a constant expression's closure whose unit was never published",
                ));
            };
            crate::classes::const_expr_closure(req, &unit, *at, scope).map(|made| made.as_value())
        }
        ConstExpr::FunctionCallable {
            name,
            name_kind,
            at,
            unit,
            ..
        } => function_callable(req, name, *name_kind, (unit, *at)),
        ConstExpr::MethodCallable { class, method, .. } => {
            method_callable(req, class, method, scope)
        }
        _ => Err(invalid_operations(req)),
    }
}

/// `zend_ast_evaluate`'s `ZEND_AST_CALL` arm over `name(...)`: the
/// function the node found earlier in the request, else the one the
/// lowered name finds, and for a name not written fully qualified the
/// global one its last segment names where the namespaced one is none,
/// kept for the node's next evaluation; php's `Error` where neither is;
/// then `zend_create_fake_closure` over it, with no called scope. What
/// a node found once it keeps, so a namespaced function declared later
/// in the request does not take a fallback's place.
///
/// ```php
/// namespace N;
/// function mine($s) { return "mine $s"; }
/// const A = strrev(...);
/// const B = \strrev(...);
/// const C = mine(...);
/// var_dump((A)('ab'), (B)('cd'), (C)('x'));
/// #[\Attribute] class At { public function __construct(public \Closure $c) {} }
/// #[At(strtoupper(...))] class U {}
/// var_dump(((new \ReflectionClass(U::class))->getAttributes()[0]->newInstance()->c)('attr'));
/// class Z { const D = nope(...); }
/// try { Z::D; } catch (\Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(2) "ba"
/// string(2) "dc"
/// string(6) "mine x"
/// string(4) "ATTR"
/// Call to undefined function N\nope()
/// ```
///
/// ```php
/// namespace N;
/// function f($c = strrev(...)) { return $c('ab'); }
/// echo f(), "\n";
/// if (true) { function strrev($s) { return 'mine'; } }
/// echo f(), " ", strrev('ab'), "\n";
/// ```
/// ```expect
/// ba
/// ba mine
/// ```
fn function_callable(
    req: &mut Request,
    name: &Name,
    name_kind: php_unit::decl::NameKind,
    (unit, at): (&php_unit::UnitCell, u32),
) -> Result<Value, Thrown> {
    if let Some(function) = req.kept_callable(unit, at) {
        let frame = crate::classes::CallFrame::Callee {
            function,
            this: None,
            called_scope: None,
        };
        return crate::classes::closure_from_frame(req, frame).map(|made| made.as_value());
    }
    let found = req.function_named(name.into()).or_else(|| {
        if name_kind == php_unit::decl::NameKind::FullyQualified {
            return None;
        }
        let at = name.0.iter().rposition(|b| *b == b'\\')?;
        req.function_named(Bare::resolved(name.0.get(at + 1..)?))
    });
    let Some(function) = found else {
        return Err(undefined_function(req, &name.0));
    };
    req.keep_callable(unit, at, function);
    let frame = crate::classes::CallFrame::Callee {
        function,
        this: None,
        called_scope: None,
    };
    crate::classes::closure_from_frame(req, frame).map(|made| made.as_value())
}

/// `zend_ast_evaluate`'s `ZEND_AST_STATIC_CALL` arm over
/// `Class::method(...)`: the class fetched in the evaluating scope, the
/// method the lowered name finds where that scope may call it, an
/// instance method, an abstract one and a trait's each refused as php
/// refuses them, a class with `__callStatic` refused in the words 8.5
/// has for a constant expression; then `zend_create_fake_closure` over
/// the method with the class as its called scope.
///
/// ```php
/// class A {
///     public static function make() { return static::class; }
///     private static function hidden() { return 'hidden'; }
///     public function inst() {}
///     const M = self::make(...);
///     const H = self::hidden(...);
/// }
/// class B extends A { const N = parent::make(...); const I = A::inst(...); const J = A::hidden(...); }
/// var_dump((A::M)(), (B::N)(), (A::H)());
/// try { B::I; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// try { B::J; } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(1) "A"
/// string(1) "A"
/// string(6) "hidden"
/// Non-static method A::inst() cannot be called statically
/// Call to private method A::hidden() from scope B
/// ```
fn method_callable(
    req: &mut Request,
    class: &Name,
    method: &Name,
    scope: Option<ClassId>,
) -> Result<Value, Thrown> {
    let ce = const_class(req, &class.0, scope)?;
    let magic = req.symbols().method_named(ce, b"__callstatic").is_some();
    let magic_refused = |req: &mut Request| {
        error(
            req,
            "Creating a callable for the magic __callStatic() method is not supported in constant expressions",
        )
    };
    let found = req.symbols().method_named(ce, &method.0);
    let reached = found.is_some_and(|function| req.method_accessible_from(function, scope));
    if magic && !reached {
        return Err(magic_refused(req));
    }
    let Some(function) = found else {
        return Err(undefined_method(req, ce, &method.0));
    };
    req.check_method_access_from(function, &method.0, scope)?;
    if req
        .method_decl(function)
        .is_some_and(|decl| decl.is_instance)
    {
        return Err(non_static_method_call(req, function));
    }
    req.static_method_found(function)?;
    let frame = crate::classes::CallFrame::Callee {
        function,
        this: None,
        called_scope: Some(ce),
    };
    crate::classes::closure_from_frame(req, frame).map(|made| made.as_value())
}

/// `zend_fetch_class_with_scope` under `ZEND_FETCH_CLASS_EXCEPTION`, the
/// class a constant expression names: `self` and `parent` the scope the
/// expression is evaluated in and its parent, each php's `Error` where
/// there is none, and any other name [`fetch_class`]'s, the table and
/// then the autoloaders.
fn const_class(req: &mut Request, class: &[u8], scope: Option<ClassId>) -> Result<ClassId, Thrown> {
    let parent = class.eq_ignore_ascii_case(b"parent");
    if !parent && !class.eq_ignore_ascii_case(b"self") {
        return fetch_class(req, class);
    }
    let word = if parent { "parent" } else { "self" };
    let Some(scope) = scope else {
        return Err(error(
            req,
            &format!("Cannot access \"{word}\" when no class scope is active"),
        ));
    };
    if !parent {
        return Ok(scope);
    }
    req.symbols().class_parent(scope).ok_or_else(|| {
        error(
            req,
            "Cannot access \"parent\" when current class scope has no parent",
        )
    })
}

/// `zend_ast_add_unpacked_element` (`Z/Zend/zend_ast.c:531`), a `...`
/// inside an array of a constant expression: an array's string keys
/// written as they stand and its integer keys dropped for the next
/// index, a reference among its elements kept as it is; anything else,
/// a `Traversable` among them, is php's `Error`.
///
/// ```php
/// const A = [1, 'k' => 'a'];
/// const X = 1;
/// class C { const D = [...A, ...self::E, 'k' => 'b']; const E = [7 => 2]; const F = [...X]; }
/// var_dump(C::D);
/// try { var_dump(C::F); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// array(3) {
///   [0]=>
///   int(1)
///   ["k"]=>
///   string(1) "b"
///   [1]=>
///   int(2)
/// }
/// Only arrays can be unpacked in constant expression
/// ```
fn add_unpacked_element(
    req: &mut Request,
    array: &mut PhpArray,
    spread: &Value,
) -> Result<(), Thrown> {
    let Some(source) = spread.deref().as_array() else {
        return Err(error(
            req,
            "Only arrays can be unpacked in constant expression",
        ));
    };
    // `zend_ast_add_unpacked_element`'s walk, which no code of the program
    // reaches: the constant array's rows read under the borrowed request,
    // then written.
    let rows: Vec<(Key, Value)> = walk::rows(req, source).collect();
    for (key, value) in rows {
        let value = Owner::take(req, value);
        match key {
            Key::Str(_) => {
                let key = Owner::take(req, key);
                array.set(req, key, value)?;
            }
            Key::Int(_) => array.append(req, value)?,
        }
    }
    Ok(())
}

/// php's `Error` for a node its constant-expression walk does not
/// evaluate.
fn invalid_operations(req: &mut Request) -> Thrown {
    error(req, "Constant expression contains invalid operations")
}

/// One binary operator of a constant expression, by the spelling the
/// declaration keeps it under, over the kernel the opcode of that
/// operator is served by.
fn binary(req: &mut Request, op: &[u8], lhs: &Value, rhs: &Value) -> Result<Value, Thrown> {
    use crate::ops;
    let ordered = |req: &mut Request, test: fn(std::cmp::Ordering) -> bool| {
        ops::compare(req, lhs, rhs).map(|order| Value::bool(test(order)))
    };
    match op {
        b"+" => ops::add(req, lhs, rhs),
        b"-" => ops::sub(req, lhs, rhs),
        b"*" => ops::mul(req, lhs, rhs),
        b"/" => ops::div(req, lhs, rhs),
        b"%" => ops::r#mod(req, lhs, rhs),
        b"**" => ops::pow(req, lhs, rhs),
        b"<<" => ops::shift_left(req, lhs, rhs),
        b">>" => ops::shift_right(req, lhs, rhs),
        b"." => ops::concat(req, lhs, rhs),
        b"|" => ops::bitwise_or(req, lhs, rhs),
        b"^" => ops::bitwise_xor(req, lhs, rhs),
        b"&" => ops::bitwise_and(req, lhs, rhs),
        b"<" => ordered(req, std::cmp::Ordering::is_lt),
        b"<=" => ordered(req, std::cmp::Ordering::is_le),
        b">" => ordered(req, std::cmp::Ordering::is_gt),
        b">=" => ordered(req, std::cmp::Ordering::is_ge),
        b"==" => ordered(req, std::cmp::Ordering::is_eq),
        b"!=" => ordered(req, std::cmp::Ordering::is_ne),
        b"<=>" => ops::compare(req, lhs, rhs).map(|order| {
            Value::int(match order {
                std::cmp::Ordering::Less => -1,
                std::cmp::Ordering::Equal => 0,
                std::cmp::Ordering::Greater => 1,
            })
        }),
        b"===" => ops::identical(req, lhs, rhs).map(Value::bool),
        b"!==" => ops::identical(req, lhs, rhs).map(|same| Value::bool(!same)),
        _ => Err(invalid_operations(req)),
    }
}

/// `ce->get_iterator(ce, object, 0)` over a `Traversable` object: its
/// iterator ([`Iter`]), one owner, the caller's, given up as
/// `zend_iterator_dtor` gives it up. An `IteratorAggregate`
/// is asked `getIterator()` until it answers an `Iterator`, php's
/// `Exception` naming the class where it answers anything else; a
/// finished `Generator` is php's refusal to traverse it.
///
/// ```php
/// function g() { yield 'k' => 1; }
/// $gen = g();
/// var_dump(iterator_to_array($gen));
/// try { iterator_to_array($gen); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
/// class Two implements Iterator {
///     private $i = 0;
///     function current(): mixed { echo "current "; return $this->i; }
///     function key(): mixed { return $this->i; }
///     function next(): void { $this->i++; }
///     function rewind(): void { $this->i = 0; }
///     function valid(): bool { return $this->i < 2; }
/// }
/// $n = new NoRewindIterator(new Two);
/// echo $n->current(), $n->current(), "\n";
/// ```
/// ```expect
/// array(1) {
///   ["k"]=>
///   int(1)
/// }
/// Cannot traverse an already closed generator
/// current 00
/// ```
#[cfg_attr(debug_assertions, track_caller)]
pub fn iterate(req: &mut Request, object: &PhpObject) -> Result<Owner<Iter>, Thrown> {
    adopted(crate::interfaces::get_iterator(req, *object, false).map(Iter))
}

/// `zend_create_fake_closure` over a resolved callable, what
/// `Closure::fromCallable` and `ReflectionFunction::getClosure` make: a
/// closure hands back itself, one more owner; a function is closed over
/// with no scope; a method over its class, with the receiver the
/// callable holds and its class as the called scope, or the class the
/// callable named where it holds none. The closure is fake
/// (`ZEND_ACC_FAKE_CLOSURE`), so `bind` may not move its scope.
///
/// ```php
/// class A { static function who() { return static::class; } function me() { return $this; } }
/// class B extends A {}
/// var_dump((new ReflectionMethod('A', 'who'))->getClosure()(), (new ReflectionMethod('B', 'who'))->getClosure()());
/// $b = new B; var_dump((new ReflectionMethod('A', 'me'))->getClosure($b)() === $b, (new ReflectionFunction('strlen'))->getClosure()('ab'));
/// ```
/// ```expect
/// string(1) "A"
/// string(1) "A"
/// bool(true)
/// int(2)
/// ```
#[cfg_attr(debug_assertions, track_caller)]
pub fn closure_from_callable(
    req: &mut Request,
    callable: &Callable,
) -> Result<Owner<PhpObject>, Thrown> {
    adopted(crate::classes::fake_closure(req, callable))
}

/// Read a property through the class's `read_property` handler: a
/// view, `null` for one that is not there, as `zend_std_read_property`
/// answers with `BP_VAR_IS`.
pub fn read_prop(req: &mut Request, object: &PhpObject, name: &[u8]) -> Result<Value, Thrown> {
    Ok(object.prop(req, name).unwrap_or_default())
}

/// Write a property through the class's `write_property` (design
/// §5.4), [`PhpObject::set_prop`] with the name made here: a sink, the
/// value the object's from here, and given up where the name cannot be
/// made.
pub fn write_prop(
    req: &mut Request,
    object: &PhpObject,
    name: &[u8],
    value: Owner<Value>,
) -> Result<(), Thrown> {
    let name = match PhpString::new(req, name) {
        Ok(name) => name,
        Err(thrown) => {
            value.give_up(req);
            return Err(thrown);
        }
    };
    object.set_prop(req, name, value)
}

/// Read a `static` property of `class`, `zend_read_static_property_ex`
/// (`Z/Zend/zend_API.c`): the slot of the `static` members table that
/// `name` names, one more owner of the value taken for the caller. php
/// sets `EG(fake_scope)` to the class it was asked about for the length
/// of the read, so the visibility check
/// `zend_std_get_static_property_with_info` does is measured against
/// `class` and not against whatever scope the extension was called
/// from; a property a class above declared private is still php's
/// `Error`, since this is `zend_read_static_property_ex`'s
/// `silent = false`, php's `BP_VAR_R`.
#[cfg_attr(debug_assertions, track_caller)]
pub fn read_static_prop(
    req: &mut Request,
    class: ClassId,
    name: &[u8],
) -> Result<Owner<Value>, Thrown> {
    adopted(req.static_property(class, name, Some(class), PropertyRead::Warning))
}

/// Address a `static` property of `class` silently — a property of the
/// class and not of an instance, which is what the name has of
/// [`read_prop`]'s — `zend_std_get_static_property(ce, name,
/// BP_VAR_IS)`, which
/// `ReflectionClass::getStaticPropertyValue` and
/// `setStaticPropertyValue` reach the slot through: `None` where php
/// hands back `NULL`, which is a name the class does not have, one
/// declared as an instance property and one the fake scope may not
/// reach; an undefined value for a typed property nothing has written,
/// which is the slot php hands back holding `IS_UNDEF`; and otherwise
/// one more owner of the value, a reference followed. Nothing is
/// raised on those paths, so the caller says in its own words what a
/// name the class does not have is. It is [`read_static_prop`]'s read
/// under [`PropertyRead::Silent`] rather than
/// [`PropertyRead::Warning`], stopping at the pointer where that one
/// goes on to the value; the fake scope is the same.
#[cfg_attr(debug_assertions, track_caller)]
pub fn read_class_prop_silent(
    req: &mut Request,
    class: ClassId,
    name: &[u8],
) -> Result<Option<Owner<Value>>, Thrown> {
    let made = req.static_property_silent(class, name, Some(class));
    adopted_some(made)
}

/// Write a `static` property of `class`,
/// `zend_update_static_property_ex`: a sink, the value's owner the
/// slot's from here, and what the slot held is given up. The fake scope is
/// [`read_static_prop`]'s, and a property with a declared type is held
/// to it under that function's own `strict = 0`, so what an extension
/// writes is coerced by the weak rule however the file that called it
/// was declared.
pub fn write_static_prop(
    req: &mut Request,
    class: ClassId,
    name: &[u8],
    value: Owner<Value>,
) -> Result<(), Thrown> {
    req.set_static_property(class, name, Some(class), value.hand_on(), false)
}

/// `zend_object_make_lazy(NULL, class, ..., ZEND_LAZY_OBJECT_STRATEGY_GHOST
/// | options)`, what `ReflectionClass::newLazyGhost` serves: a new
/// object of `class` whose declared properties wait for `initializer`,
/// served with the object the first time one of them is touched.
/// `options` is php's user mask, which the caller has held to it;
/// `SKIP_INITIALIZATION_ON_SERIALIZE` is the bit read here. An internal
/// class other than `stdClass`, and one below such a class, is php's
/// `Error`.
#[cfg_attr(debug_assertions, track_caller)]
pub fn new_lazy_ghost(
    req: &mut Request,
    class: ClassId,
    initializer: &Callable,
    options: i64,
) -> Result<Owner<PhpObject>, Thrown> {
    adopted(req.make_lazy(None, class, initializer, Strategy::Ghost, options))
}

/// `zend_object_make_lazy` under `ZEND_LAZY_OBJECT_STRATEGY_PROXY`, what
/// `ReflectionClass::newLazyProxy` serves: a new object of `class`
/// whose accesses, once one is made, go to the instance `factory`
/// returns.
#[cfg_attr(debug_assertions, track_caller)]
pub fn new_lazy_proxy(
    req: &mut Request,
    class: ClassId,
    factory: &Callable,
    options: i64,
) -> Result<Owner<PhpObject>, Thrown> {
    adopted(req.make_lazy(None, class, factory, Strategy::Proxy, options))
}

/// `zend_object_make_lazy(obj, class, ...)` under the ghost strategy,
/// what `ReflectionClass::resetAsLazyGhost` serves: `object`, an
/// instance of `class`, reset, its destructor served first unless
/// `options` carries `SKIP_DESTRUCTOR`, and left waiting for
/// `initializer`. An object that is lazy and not initialised is the
/// caller's to refuse.
pub fn reset_as_lazy_ghost(
    req: &mut Request,
    object: &PhpObject,
    class: ClassId,
    initializer: &Callable,
    options: i64,
) -> Result<(), Thrown> {
    req.make_lazy(Some(*object), class, initializer, Strategy::Ghost, options)
        .map(|_| ())
}

/// [`reset_as_lazy_ghost`] under the proxy strategy,
/// `ReflectionClass::resetAsLazyProxy`.
pub fn reset_as_lazy_proxy(
    req: &mut Request,
    object: &PhpObject,
    class: ClassId,
    factory: &Callable,
    options: i64,
) -> Result<(), Thrown> {
    req.make_lazy(Some(*object), class, factory, Strategy::Proxy, options)
        .map(|_| ())
}

/// `zend_lazy_object_init`, what `ReflectionClass::initializeLazyObject`
/// serves: a lazy object nothing has initialised is initialised now,
/// and the answer is the object its properties are read on, the ghost
/// itself or the proxy's instance, a view. An object that is not lazy is
/// its own answer; what the initializer threw is the `Err`, the object
/// left waiting.
pub fn initialize_lazy(req: &mut Request, object: &PhpObject) -> Result<PhpObject, Thrown> {
    req.lazy_init(*object)
}

/// `zend_lazy_object_mark_as_initialized`, what
/// `ReflectionClass::markLazyObjectAsInitialized` serves: each property
/// still waiting takes its default and the initializer is never served.
/// The answer is [`initialize_lazy`]'s, a view.
pub fn mark_lazy_initialized(req: &mut Request, object: &PhpObject) -> Result<PhpObject, Thrown> {
    if object.is_lazy() {
        req.lazy_mark_initialized(*object)?;
    }
    req.lazy_init(*object)
}

/// `ReflectionProperty::skipLazyInitialization` under `None` and
/// `ReflectionProperty::setRawValueWithoutLazyInitialization` under
/// `Some`: the declared property `name`, on the object the initialised
/// proxies lead to, stops waiting for the initializer, holding its
/// default or `value` written past `__set` and hooks; the object stops
/// being lazy once no property waits. `value` is a sink's, the object's
/// from here. The checks on the property php makes first are the
/// caller's.
pub fn skip_lazy_initialization(
    req: &mut Request,
    object: &PhpObject,
    name: &[u8],
    value: Option<Owner<Value>>,
) -> Result<(), Thrown> {
    req.lazy_skip(*object, name, value)
}
