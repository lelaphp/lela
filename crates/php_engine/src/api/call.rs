//! The calling-back group (design §4.4): re-entry from a native into PHP
//! code, always through `&mut Request` (principle 9, ADR-0002). A
//! callable is served here, a native through its entry and a user
//! function by entering the engine's one loop again on the callee's
//! frame; the items that only read the linked tables answer from what
//! the request holds, and the rest still raise php's `Error` for an
//! undefined function or class.

use std::sync::Arc;

use php_unit::decl::{ConstExpr, HookKind, MethodDecl, Name, ParameterDefault};
use php_unit::{ConstIdx, Opcode, Unit};

use crate::api::abi::{Callable, PropertyRead};
use crate::api::raising::{Thrown, argument_error, throw};
use crate::api::request::{Request, Strategy};
use crate::api::values::{ClassId, Key, PhpArray, PhpObject, PhpString, Value, ValueRef};
use crate::exec::{self, Body, CallStage};

/// A linked function: user or native, by identity. A native method is
/// one too, numbered past the functions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Function(pub(crate) u32);

impl Function {
    /// `zend_pass_function`: the callee `NEW` serves for a class with
    /// no constructor and arguments to take, which takes them and
    /// returns `null`.
    pub(crate) const PASS: Function = Function(u32::MAX);
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

/// A position in an `iterable`: an array's elements, or a `Traversable`
/// served through its iterator methods.
#[derive(Debug, Clone)]
pub struct Iter {
    entries: Vec<(Value, Value)>,
    at: Option<usize>,
}

impl Iter {
    /// Advance one element, serving `Iterator::next()` for an
    /// object; `false` past the end.
    pub fn next(&mut self, req: &mut Request) -> Result<bool, Thrown> {
        let _ = req;
        let at = self.at.map_or(0, |i| i + 1);
        self.at = Some(at);
        Ok(at < self.entries.len())
    }

    /// The key at the position.
    #[must_use]
    pub fn key(&self) -> Option<&Value> {
        self.entries.get(self.at?).map(|(k, _)| k)
    }

    /// The value at the position.
    #[must_use]
    pub fn value(&self) -> Option<&Value> {
        self.entries.get(self.at?).map(|(_, v)| v)
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
        } => req.serve_method(function, this, scope, arguments, extra_named),
        Target::Closure(object) => req.serve_closure(object, arguments, extra_named),
    };
    match served {
        Err(thrown) if !req.executing() => req.thrown_without_frame(&thrown),
        served => served,
    }
}

/// The `zend_function` behind a target, `fcc.function_handler`: a
/// closure's is its payload's, `zend_get_closure_method_def`. What the
/// named arguments are placed against.
fn callee(req: &Request, target: Target) -> Option<Function> {
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
pub fn call(req: &mut Request, callable: &Callable, arguments: &[Value]) -> Result<Value, Thrown> {
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
pub fn call_named(
    req: &mut Request,
    callable: &Callable,
    arguments: &[Value],
    named: &[(PhpString, Value)],
) -> Result<Value, Thrown> {
    let target = callable.target();
    let Some(function) = callee(req, target).filter(|_| !named.is_empty()) else {
        return serve(req, target, arguments, &[]);
    };
    let mut placement = Placement::new(function, arguments);
    for (name, value) in named {
        placement.named(req, *name, *value)?;
    }
    serve(req, target, &placement.arguments, &placement.extra_named)
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
pub fn call_array(
    req: &mut Request,
    callable: &Callable,
    array: PhpArray,
) -> Result<Value, Thrown> {
    let target = callable.target();
    let Some(function) = callee(req, target) else {
        let positional: Vec<Value> = array.snapshot_refs().into_iter().map(|(_, v)| v).collect();
        return serve(req, target, &positional, &[]);
    };
    let mut placement = Placement::new(function, &[]);
    let mut have_named = false;
    for (key, value) in array.snapshot_refs() {
        match key {
            Key::Int(_) => {
                if have_named {
                    return Err(throw(
                        req,
                        "Error",
                        "Cannot use positional argument after named argument",
                    ));
                }
                placement.arguments.push(value);
            }
            Key::Str(name) => {
                have_named = true;
                placement.named(req, name, value)?;
            }
        }
    }
    serve(req, target, &placement.arguments, &placement.extra_named)
}

/// One call's argument list while `zend_call_function` builds it: the
/// positional arguments as placed so far, with the slots a named one
/// skipped `UNDEF`, and the extra named arguments a variadic callee
/// collects, against the parameter names the callee declares.
struct Placement {
    function: Function,
    arguments: Vec<Value>,
    extra_named: Vec<(PhpString, Value)>,
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
        let offset = arg_offset_by_name(req, self.function, name.as_bytes());
        match handle_named_arg(offset, &mut self.arguments, &mut self.extra_named, name) {
            Ok(Named { at, .. }) => {
                if let Some(slot) = at.slot(&mut self.arguments, &mut self.extra_named) {
                    *slot = value;
                }
                Ok(())
            }
            Err(refused) => Err(refused.throw(req, name)),
        }
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
    let Some(decl) = req.function_decl(function) else {
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
        throw(req, "Error", &message)
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
pub(crate) fn handle_undef_args(req: &mut Request) -> Result<(), Thrown> {
    let Some(call) = req.vm_mut().building_call() else {
        return Ok(());
    };
    let function = call.function;
    let stage = std::mem::replace(&mut call.stage, CallStage::Serving);
    let filled = fill_undef_args(req, function);
    if let Some(call) = req.vm_mut().building_call() {
        call.stage = stage;
    }
    filled
}

/// The walk of [`handle_undef_args`] under the callee's frame.
fn fill_undef_args(req: &mut Request, function: Function) -> Result<(), Thrown> {
    let user = req
        .user_function(function)
        .map(|user| (Arc::clone(&user.unit), user.body));
    let count = req
        .vm()
        .calls()
        .last()
        .map_or(0, |call| call.arguments.len());
    for at in 0..count {
        let undef = req
            .vm()
            .calls()
            .last()
            .and_then(|call| call.arguments.get(at))
            .is_some_and(|held| matches!(held.as_ref(), ValueRef::Undef));
        if !undef {
            continue;
        }
        let value = match &user {
            Some((unit, body)) => user_default(req, unit, *body, at)?,
            None => native_default(req, function, at)?,
        };
        let placed = req
            .vm_mut()
            .building_call()
            .and_then(|call| call.arguments.get_mut(at));
        match placed {
            Some(slot) => *slot = value,
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
    let Some(decl) = req.function_decl(function) else {
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
    let value = eval_const_expr(req, &default, scope)?;
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
        let placed = PhpString::new(req, b"file")
            .and_then(|name| Ok((name, PhpString::new(req, file)?)))
            .and_then(|(name, file)| object.set_prop(req, name, Value::string(file)))
            .and_then(|()| PhpString::new(req, b"line"))
            .and_then(|name| object.set_prop(req, name, Value::int(i64::from(line))));
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
pub(crate) fn handle_named_arg(
    offset: ArgOffset,
    arguments: &mut Vec<Value>,
    extra_named: &mut Vec<(PhpString, Value)>,
    name: PhpString,
) -> Result<Named, NamedRefused> {
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
            extra_named.push((name, Value::undef()));
            Ok(Named {
                at: NamedAt::Extra(extra_named.len() - 1),
                number,
            })
        }
        ArgOffset::Declared(at) => {
            if arguments
                .get(at)
                .is_some_and(|held| !matches!(held.as_ref(), ValueRef::Undef))
            {
                return Err(NamedRefused::Overwrites);
            }
            if arguments.len() <= at {
                arguments.resize(at + 1, Value::undef());
            }
            Ok(Named {
                at: NamedAt::Argument(at),
                number,
            })
        }
    }
}

/// `zend_call_method` on `object`: the method looked up on the
/// object's class by its lowered name, and served on a call frame with
/// the object as its receiver and the arguments as one more owner each,
/// the frame popped whether the entry returned or threw. A name the
/// class has no entry for is php's `Error` for an undefined method.
pub fn call_method(
    req: &mut Request,
    object: &PhpObject,
    method: &[u8],
    arguments: &[Value],
) -> Result<Value, Thrown> {
    let class = object.class();
    let Some(function) = req.method_named(class, &method.to_ascii_lowercase()) else {
        let class = req.class_name(class);
        return Err(throw(
            req,
            "Error",
            &format!(
                "Call to undefined method {class}::{}()",
                String::from_utf8_lossy(method)
            ),
        ));
    };
    req.serve_method(function, Some(*object), class, arguments, &[])
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
    let lowered = name.0.to_ascii_lowercase();
    // A hook, `$name::get` or `$name::set`, is no row of the class's
    // table; a reflector over one holds the hook itself, as php's
    // `reflection_method_factory` holds its `zend_function`.
    let function = match hook_name(&name.0) {
        Some((property, kind)) => req.property_hook_function(class, property, kind)?,
        None => req.method_named(class, &lowered)?,
    };
    let spelled = format!(
        "{}::{}",
        req.class_name(class),
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

/// `zend_read_property_ex(prop_info->ce, ...)` over a hooked property,
/// what `ReflectionProperty::getValue` reads: the `get` hook called, or
/// the backing value where there is none, the property's own class
/// being the scope, so a private one is read as from inside it; a
/// virtual property with no `get` hook is php's `Error`. A property
/// with no hooks is read through the object's `read_property`. One more
/// owner of the value, the caller's.
pub fn read_hooked_property(
    req: &mut Request,
    object: &PhpObject,
    name: &[u8],
) -> Result<Value, Thrown> {
    req.read_in_class_scope(*object, name)
}

/// `zend_update_property_ex(prop_info->ce, ...)` over a hooked
/// property, what `ReflectionProperty::setValue` writes: the `set` hook
/// called, or the backing value written where there is none, the
/// property's own class being the scope; a virtual property with no
/// `set` hook is php's `Error`. A property with no hooks is written
/// through the object's `write_property`. `value` and `name` are given
/// up whatever arm runs.
pub fn write_hooked_property(
    req: &mut Request,
    object: &PhpObject,
    name: PhpString,
    value: Value,
) -> Result<(), Thrown> {
    req.write_in_class_scope(*object, name, value)
}

/// A hooked property's backing value, read past its hooks as the
/// `get` hook itself reads it, which is what
/// `ReflectionProperty::getRawValue` reaches through
/// `zend_get_property_hook_trampoline`: a virtual property has none,
/// php's `Error`; a typed one nothing wrote is the uninitialised-access
/// `Error`. One more owner of the value, the caller's.
pub fn read_raw_property(
    req: &mut Request,
    object: &PhpObject,
    name: &[u8],
) -> Result<Value, Thrown> {
    req.read_raw(*object, name)
}

/// A hooked property's backing value written past its hooks, as the
/// `set` hook itself writes it, `ReflectionProperty::setRawValue`'s
/// trampoline: a virtual property has none, php's `Error`. `value` and
/// `name` are given up whatever arm runs.
pub fn write_raw_property(
    req: &mut Request,
    object: &PhpObject,
    name: PhpString,
    value: Value,
) -> Result<(), Thrown> {
    req.write_raw(*object, name, value)
}

/// `new $class(...$arguments)`: [`new_object`] and then the class's
/// constructor served on it with `arguments`, as `NEW` and its
/// `DO_FCALL` serve them; a class with none takes the arguments and
/// serves nothing (`zend_pass_function`).
pub fn instantiate(
    req: &mut Request,
    class: ClassId,
    arguments: &[Value],
) -> Result<PhpObject, Thrown> {
    let object = new_object(req, class)?;
    if let Some(constructor) = req.method_named(class, b"__construct") {
        let served = req.serve_method(constructor, Some(object), class, arguments, &[]);
        if let Err(thrown) = served {
            req.release(object.as_value());
            return Err(thrown);
        }
        req.release(served.unwrap_or_default());
    }
    Ok(object)
}

/// `object_init_with_constructor` (`Z/Zend/zend_API.c:1876`), what
/// `zend_get_attribute_object` makes an attribute's object with:
/// [`new_object`], the constructor the class's `get_constructor`
/// answers, php's `Error` where the scope may not call it, then served
/// with the positional arguments and the named ones placed as
/// [`call_named`] places them. A class with no constructor takes
/// positional arguments and no named one, php's `Unknown named
/// parameter`. The object is the caller's, one owner; what the
/// constructor threw is the `Err`, the object given up. The arguments
/// stay the caller's.
///
/// ```php
/// #[Attribute] class MyAttrib {}
/// #[MyAttrib(notinterned: '')] class Test1 {}
/// $attr = (new ReflectionClass(Test1::class))->getAttributes()[0];
/// try { $attr->newInstance(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Unknown named parameter $notinterned
/// ```
pub fn instantiate_named(
    req: &mut Request,
    class: ClassId,
    arguments: &[Value],
    named: &[(PhpString, Value)],
) -> Result<PhpObject, Thrown> {
    let object = new_object(req, class)?;
    let constructor = match req.handlers_of(class).get_constructor(req, object) {
        Ok(constructor) => constructor,
        Err(thrown) => {
            req.release(object.as_value());
            return Err(thrown);
        }
    };
    let Some(constructor) = constructor else {
        if let Some((name, _)) = named.first() {
            req.release(object.as_value());
            let message = format!(
                "Unknown named parameter ${}",
                String::from_utf8_lossy(name.as_bytes())
            );
            return Err(throw(req, "Error", &message));
        }
        return Ok(object);
    };
    let mut placement = Placement::new(constructor, arguments);
    let served = named
        .iter()
        .try_for_each(|(name, value)| placement.named(req, *name, *value))
        .and_then(|()| {
            req.serve_method(
                constructor,
                Some(object),
                class,
                &placement.arguments,
                &placement.extra_named,
            )
        });
    match served {
        Ok(returned) => {
            req.release(returned);
            Ok(object)
        }
        Err(thrown) => {
            req.release(object.as_value());
            Err(thrown)
        }
    }
}

/// `ce->unserialize(rval, ce, buf, buf_len, data)` over an object
/// `object_init_ex` made of the class: the class's unserializer handed
/// the payload of a `C:` record, `true` once it has read it. The
/// unserializer is the one php registers on the class entry:
/// `zend_implement_serializable` gives a class that implements
/// `Serializable`, and every class that inherits from one, php's
/// `zend_user_unserialize`, which serves the object's `unserialize()`
/// with the payload as a string. `false` is the class with none, whose
/// payload `object_custom` warns of and skips; the `Err` is what
/// `unserialize()` threw.
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
    let serializable = req
        .class_named(b"serializable")
        .is_some_and(|interface| req.instance_of(object.class(), interface));
    if !serializable {
        return Ok(false);
    }
    let data = Value::string(PhpString::new(req, data)?);
    let answered = call_method(req, &object, b"unserialize", &[data]);
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
pub fn new_object(req: &mut Request, class: ClassId) -> Result<PhpObject, Thrown> {
    req.instantiable(class)?;
    req.new_object(class)
}

/// `clone`: the class's `clone_obj` (design §5.4), a fresh entry
/// holding what the object holds with one more owner each and `__clone`
/// served on it; a class with a null slot is php's `Error` for an
/// uncloneable object, and a `__clone` the scope may not reach is
/// `zend_bad_method_call`'s. The copy is the caller's, one owner.
pub fn clone(req: &mut Request, object: &PhpObject) -> Result<PhpObject, Thrown> {
    req.clone_object(*object, None)
}

/// A class's `clone_obj` over an object with a native payload, php's
/// `spl_array_object_clone` and its kin: a fresh object of the class
/// with the members copied as `zend_objects_clone_members` copies them,
/// `payload` (the class's copy of the old one) in its slot, and then
/// the class's `__clone` served on it, the copy given up with what
/// `__clone` threw. What the payload names of the heap is the caller's
/// to have taken an owner of. The copy is the caller's, one owner.
pub fn clone_with<T: std::any::Any + Send>(
    req: &mut Request,
    object: &PhpObject,
    payload: T,
) -> Result<PhpObject, Thrown> {
    let copy = req.clone_with_payload(*object, payload)?;
    if let Err(thrown) = req.serve_clone_method(copy) {
        req.release(copy.as_value());
        return Err(thrown);
    }
    Ok(copy)
}

/// `zend_create_internal_iterator_zval`: an `InternalIterator` over
/// `object`, walking the elements its class's
/// [`crate::api::abi::ObjectHandlers::iterator_element`] answers, which
/// is what `getIterator()` hands back for a class php iterates in C.
/// The iterator is the caller's, one owner, and holds one of `object`.
pub fn internal_iterator(req: &mut Request, object: &PhpObject) -> Result<PhpObject, Thrown> {
    crate::interfaces::internal_iterator(req, *object)
}

/// `PHAR_G(orig_fopen)(INTERNAL_FUNCTION_PARAM_PASSTHRU)`: the entry an
/// interception replaced for the native being served
/// ([`crate::api::abi::ExtensionBuilder::intercept`]), served on the same
/// call, its arguments and its return the caller's, the first argument
/// replaced by `first` where one is given, as phar's interceptors open
/// the `phar://` name they made of a relative path; `null` where the
/// native is not intercepted.
pub fn passthrough(req: &mut Request, first: Option<Value>) -> Result<Value, Thrown> {
    let Some(entry) = req.replaced_entry() else {
        if let Some(first) = first {
            req.release(first);
        }
        return Ok(Value::null());
    };
    if let Some(first) = first {
        req.replace_first_argument(first);
    }
    entry(req)
}

/// `zend_lookup_class`: the linked class named, autoloading if needed.
pub fn lookup_class(req: &mut Request, name: &[u8]) -> Result<Option<ClassId>, Thrown> {
    Ok(req.class_named(name))
}

/// The linked function named, without autoloading.
#[must_use]
pub fn lookup_function(req: &Request, name: &[u8]) -> Option<Function> {
    req.function_named(name)
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
) -> Result<Vec<(PhpString, Value)>, Thrown> {
    let rows = req.static_variables(callable.target());
    let mut out = Vec::with_capacity(rows.len());
    for (name, value) in rows {
        out.push((PhpString::new(req, &name)?, value));
    }
    Ok(out)
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
pub fn autoload(req: &mut Request, name: &[u8]) -> Result<Option<ClassId>, Thrown> {
    let bare = name.strip_prefix(b"\\").unwrap_or(name);
    let valid = !bare.is_empty()
        && bare
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || *b == b'_' || *b == b'\\' || *b >= 0x80);
    if !valid {
        return Ok(None);
    }
    if let Some(class) = req.class_named(bare) {
        return Ok(Some(class));
    }
    if req.link_under_way(bare) {
        return Ok(None);
    }
    let lowered = bare.to_ascii_lowercase();
    if !req.autoload_enter(&lowered) {
        return Ok(None);
    }
    let served = req.without_site_override(|req| {
        let mut at = 0;
        while let Some(loader) = req.autoloader(at) {
            let class = crate::api::values::PhpString::new(req, bare)?;
            let returned = call(req, &loader, &[Value::string(class)]);
            req.release(Value::string(class));
            req.release(returned?);
            if let Some(class) = req.class_named(bare) {
                return Ok(Some(class));
            }
            at += 1;
        }
        Ok(None)
    });
    req.autoload_leave(&lowered);
    served
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
pub fn fetch_constant(req: &mut Request, name: &[u8]) -> Result<Value, Thrown> {
    let name = name.strip_prefix(b"\\").unwrap_or(name);
    if let Some(colon) = name.iter().rposition(|b| *b == b':')
        && colon > 0
        && name.get(colon.wrapping_sub(1)) == Some(&b':')
    {
        let (class_name, constant_name) = (&name[..colon - 1], &name[colon + 1..]);
        let class = fetch_class(req, class_name)?;
        let shown = String::from_utf8_lossy(class_name).into_owned();
        return req.class_constant_shown(class, constant_name, &shown);
    }
    let found = if let Some(separator) = name.iter().rposition(|b| *b == b'\\') {
        let (namespace, constant_name) = name.split_at(separator);
        let mut lowered = namespace.to_ascii_lowercase();
        lowered.extend_from_slice(constant_name);
        req.constant_named(&lowered).map(|value| (lowered, value))
    } else {
        req.constant_str(name)
            .map(|value| (name.to_vec(), value))
            .or_else(|| special_constant(name).map(|value| (name.to_vec(), value)))
    };
    let Some((key, value)) = found else {
        return Err(throw(
            req,
            "Error",
            &format!("Undefined constant \"{}\"", String::from_utf8_lossy(name)),
        ));
    };
    crate::api::raising::deprecated_constant(req, &key)?;
    req.addref(&value);
    Ok(value)
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
    if name.eq_ignore_ascii_case(b"self") {
        return req.scope().ok_or_else(|| {
            throw(
                req,
                "Error",
                "Cannot access \"self\" when no class scope is active",
            )
        });
    }
    if name.eq_ignore_ascii_case(b"parent") {
        let Some(scope) = req.scope() else {
            return Err(throw(
                req,
                "Error",
                "Cannot access \"parent\" when no class scope is active",
            ));
        };
        return req.class_parent(scope).ok_or_else(|| {
            throw(
                req,
                "Error",
                "Cannot access \"parent\" when current class scope has no parent",
            )
        });
    }
    if name.eq_ignore_ascii_case(b"static") {
        return req.called_scope().ok_or_else(|| {
            throw(
                req,
                "Error",
                "Cannot access \"static\" when no class scope is active",
            )
        });
    }
    let bare = name.strip_prefix(b"\\").unwrap_or(name);
    let found = match req.class_named(bare) {
        Some(class) => Some(class),
        None => autoload(req, bare)?,
    };
    found.ok_or_else(|| {
        throw(
            req,
            "Error",
            &format!("Class \"{}\" not found", String::from_utf8_lossy(name)),
        )
    })
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
pub fn class_constant(req: &mut Request, class: ClassId, name: &[u8]) -> Result<Value, Thrown> {
    req.updated_class_constant(class, name)
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
pub fn eval_const_expr(
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

/// `zend_ast_evaluate_ex`: one node of a constant expression under its
/// own site ([`Request::evaluate_at`]).
fn evaluate(req: &mut Request, expr: &ConstExpr, scope: Option<ClassId>) -> Result<Value, Thrown> {
    req.evaluate_at(scope, expr.line(), |req| evaluate_node(req, expr, scope))
}

/// `zend_ast_evaluate_inner`: one node of [`eval_const_expr`] under the
/// site `zend_ast_evaluate_ex` stood the request at.
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
        ConstExpr::Str(s) => Ok(Value::string(PhpString::new(req, s)?)),
        // `zend_get_constant_ex`, which words a `CONST_DEPRECATED`
        // constant's deprecation after it found it.
        ConstExpr::Constant { name, .. } => match req.constant_str(&name.0) {
            Some(value) => {
                crate::api::raising::deprecated_constant(req, &name.0)?;
                req.addref(&value);
                Ok(value)
            }
            None => Err(throw(
                req,
                "Error",
                &format!(
                    "Undefined constant \"{}\"",
                    String::from_utf8_lossy(&name.0)
                ),
            )),
        },
        ConstExpr::ClassConstant { class, name, .. } => {
            class_constant_expr(req, class, name, scope)
        }
        // `ZEND_AST_CONST_ENUM_INIT` read back as the case it made: the
        // enum's one instance for the case (`zend_enum_get_case`), which
        // is what its constant of the case's name holds.
        ConstExpr::EnumCase { class, case, .. } => {
            let id = const_class(req, &class.0, scope)?;
            req.class_constant(id, &case.0)
        }
        ConstExpr::ClassName { class, .. } => class_name_node(req, &class.0, scope),
        // `zend_ast_evaluate`'s `ZEND_AST_CONSTANT_CLASS` arm.
        ConstExpr::ScopeClass => {
            let name = scope
                .map(|scope| req.class_name_bytes(scope))
                .unwrap_or_default();
            Ok(Value::string(PhpString::new(req, &name)?))
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
                b"!" => Ok(Value::bool(!crate::ops::is_true(req, &operand))),
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
            let taken = match (crate::ops::is_true(req, &held), then) {
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
                Ok(Value::bool(crate::ops::is_true(req, &value)))
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
        } => construct(req, &class.0, arguments, scope),
        _ => Err(invalid_operations(req)),
    }
}

/// `zend_ast_evaluate`'s `ZEND_AST_ARRAY` arm: each element in written
/// order, a `...` spread into the array under the array's own line.
fn array_node(
    req: &mut Request,
    elements: &[(Option<ConstExpr>, ConstExpr)],
    scope: Option<ClassId>,
) -> Result<Value, Thrown> {
    let mut array = crate::api::values::PhpArray::new(req)?;
    for (key, value) in elements {
        if let ConstExpr::Unpack(inner) = value {
            let spread = evaluate(req, inner, scope)?;
            let added = add_unpacked_element(req, &mut array, &spread);
            req.release(spread);
            added?;
            continue;
        }
        // `zend_ast_add_array_element`: the key evaluated before
        // the value, and the two written as `ADD_ARRAY_ELEMENT`
        // writes them, a key's deprecations and refusals its own.
        let offset = match key {
            Some(key) => crate::ops::Offset::At {
                key: evaluate(req, key, scope)?,
                constant: false,
            },
            None => crate::ops::Offset::Next,
        };
        let value = evaluate(req, value, scope)?;
        crate::ops::add_array_element(req, array, offset, value)?;
    }
    Ok(Value::array(array))
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
fn class_name_node(
    req: &mut Request,
    class: &[u8],
    scope: Option<ClassId>,
) -> Result<Value, Thrown> {
    let parent = class.eq_ignore_ascii_case(b"parent");
    let name = if parent || class.eq_ignore_ascii_case(b"self") {
        let Some(scope) = scope else {
            return Err(throw(
                req,
                "Error",
                "Cannot use \"self\" when no class scope is active",
            ));
        };
        let named = if parent {
            req.class_parent(scope).ok_or_else(|| {
                throw(
                    req,
                    "Error",
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
    Ok(Value::string(PhpString::new(req, &name)?))
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
            if held == or {
                return Ok(Value::bool(or));
            }
            let rhs = evaluate(req, rhs, scope)?;
            let held = crate::ops::is_true(req, &rhs);
            req.release(rhs);
            Ok(Value::bool(held))
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
                return Err(throw(
                    req,
                    "Error",
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
    let Some(instance) = object.as_object().copied() else {
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
        .class_decl(instance.class())
        .is_some_and(|decl| decl.kind == php_unit::decl::ClassKind::Enum);
    if !is_enum {
        return Err(throw(
            req,
            "Error",
            "Fetching properties on non-enums in constant expressions is not allowed",
        ));
    }
    let value = req.read_property(instance, name, PropertyRead::Warning)?;
    Ok(Chained {
        value,
        short: false,
    })
}

/// `zend_ast_evaluate`'s `ZEND_AST_NEW` arm (`Z/Zend/zend_ast.c:1019`):
/// the class fetched in `scope` ([`const_class`]), the instance made,
/// each argument evaluated in written order, and the constructor served
/// with them, the named ones by their names as `zend_call_function`
/// places them. The instance is given up where the constructor threw.
fn construct(
    req: &mut Request,
    class: &[u8],
    arguments: &[(Option<Name>, ConstExpr)],
    scope: Option<ClassId>,
) -> Result<Value, Thrown> {
    let id = const_class(req, class, scope)?;
    let object = new_object(req, id)?;
    let mut positional = Vec::new();
    let mut named = Vec::new();
    for (name, argument) in arguments {
        let value = match evaluate(req, argument, scope) {
            Ok(value) => value,
            Err(thrown) => {
                req.release(object.as_value());
                return Err(thrown);
            }
        };
        match name {
            Some(name) => named.push((PhpString::new(req, &name.0)?, value)),
            None => positional.push(value),
        }
    }
    let served = match req.method_named(id, b"__construct") {
        None => Ok(Value::null()),
        Some(_) if named.is_empty() => {
            let constructor = req.method_named(id, b"__construct");
            match constructor {
                Some(constructor) => {
                    req.serve_method(constructor, Some(object), id, &positional, &[])
                }
                None => Ok(Value::null()),
            }
        }
        Some(_) => match known_method(req, id, &Name(b"__construct"[..].into()), Some(object)) {
            Some(callable) => call_named(req, &callable, &positional, &named),
            None => Ok(Value::null()),
        },
    };
    for value in positional {
        req.release(value);
    }
    for (name, value) in named {
        req.release(Value::string(name));
        req.release(value);
    }
    match served {
        Ok(value) => {
            req.release(value);
            Ok(object.as_value())
        }
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
        return Err(throw(
            req,
            "Error",
            &format!("Cannot access \"{word}\" when no class scope is active"),
        ));
    };
    if !parent {
        return Ok(scope);
    }
    req.class_parent(scope).ok_or_else(|| {
        throw(
            req,
            "Error",
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
    let Some(source) = spread.deref().as_array().copied() else {
        return Err(throw(
            req,
            "Error",
            "Only arrays can be unpacked in constant expression",
        ));
    };
    for (key, value) in source.snapshot_refs() {
        req.addref(&value);
        match key {
            Key::Str(name) => {
                req.addref(&Value::string(name));
                array.set(req, Key::Str(name), value)?;
            }
            Key::Int(_) => array.append(req, value)?,
        }
    }
    Ok(())
}

/// php's `Error` for a node its constant-expression walk does not
/// evaluate.
fn invalid_operations(req: &mut Request) -> Thrown {
    throw(
        req,
        "Error",
        "Constant expression contains invalid operations",
    )
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
        b"===" => Ok(Value::bool(ops::identical(lhs, rhs))),
        b"!==" => Ok(Value::bool(!ops::identical(lhs, rhs))),
        _ => Err(invalid_operations(req)),
    }
}

/// `foreach` over `value`: an array's elements now, a `Traversable`
/// through its iterator, a scalar php's `TypeError`.
pub fn iterate(req: &mut Request, value: &Value) -> Result<Iter, Thrown> {
    let Some(array) = value.as_array() else {
        // `zend_type_error("foreach() argument must be of type
        // array|object, %s given", zend_zval_value_name(array_ptr))` at
        // `ZEND_FE_RESET_R`: the value's name, not its type's.
        let given = crate::api::coerce::value_name(req, value);
        return Err(throw(
            req,
            "TypeError",
            &format!("foreach() argument must be of type array|object, {given} given"),
        ));
    };
    Ok(Iter {
        entries: array
            .snapshot()
            .into_iter()
            .map(|(k, v)| {
                let key = match k {
                    crate::api::values::Key::Int(n) => Value::int(n),
                    crate::api::values::Key::Str(s) => Value::string(s),
                };
                (key, v)
            })
            .collect(),
        at: None,
    })
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
pub fn closure_from_callable(req: &mut Request, callable: &Callable) -> Result<PhpObject, Thrown> {
    crate::classes::fake_closure(req, callable)
}

/// Read a property through the class's `read_property` handler: a
/// view, `null` for one that is not there, as `zend_std_read_property`
/// answers with `BP_VAR_IS`.
pub fn read_prop(req: &mut Request, object: &PhpObject, name: &[u8]) -> Result<Value, Thrown> {
    Ok(object.prop(req, name).unwrap_or_default())
}

/// Write a property through the class's `write_property` (design
/// §5.4): the value is the object's from here.
pub fn write_prop(
    req: &mut Request,
    object: &PhpObject,
    name: &[u8],
    value: Value,
) -> Result<(), Thrown> {
    let name = PhpString::new(req, name)?;
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
pub fn read_static_prop(req: &mut Request, class: ClassId, name: &[u8]) -> Result<Value, Thrown> {
    req.static_property(class, name, Some(class), PropertyRead::Warning)
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
pub fn read_class_prop_silent(
    req: &mut Request,
    class: ClassId,
    name: &[u8],
) -> Result<Option<Value>, Thrown> {
    req.static_property_silent(class, name, Some(class))
}

/// Write a `static` property of `class`,
/// `zend_update_static_property_ex`: the value is the slot's from here
/// and what the slot held is given up. The fake scope is
/// [`read_static_prop`]'s, and a property with a declared type is held
/// to it under that function's own `strict = 0`, so what an extension
/// writes is coerced by the weak rule however the file that called it
/// was declared.
pub fn write_static_prop(
    req: &mut Request,
    class: ClassId,
    name: &[u8],
    value: Value,
) -> Result<(), Thrown> {
    req.set_static_property(class, name, Some(class), value, false)
}

/// `zend_object_make_lazy(NULL, class, ..., ZEND_LAZY_OBJECT_STRATEGY_GHOST
/// | options)`, what `ReflectionClass::newLazyGhost` serves: a new
/// object of `class` whose declared properties wait for `initializer`,
/// served with the object the first time one of them is touched.
/// `options` is php's user mask, which the caller has held to it;
/// `SKIP_INITIALIZATION_ON_SERIALIZE` is the bit read here. An internal
/// class other than `stdClass`, and one below such a class, is php's
/// `Error`.
pub fn new_lazy_ghost(
    req: &mut Request,
    class: ClassId,
    initializer: &Callable,
    options: i64,
) -> Result<PhpObject, Thrown> {
    req.make_lazy(None, class, initializer, Strategy::Ghost, options)
}

/// `zend_object_make_lazy` under `ZEND_LAZY_OBJECT_STRATEGY_PROXY`, what
/// `ReflectionClass::newLazyProxy` serves: a new object of `class`
/// whose accesses, once one is made, go to the instance `factory`
/// returns.
pub fn new_lazy_proxy(
    req: &mut Request,
    class: ClassId,
    factory: &Callable,
    options: i64,
) -> Result<PhpObject, Thrown> {
    req.make_lazy(None, class, factory, Strategy::Proxy, options)
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
/// being lazy once no property waits. `value` is the object's from here.
/// The checks on the property php makes first are the caller's.
pub fn skip_lazy_initialization(
    req: &mut Request,
    object: &PhpObject,
    name: &[u8],
    value: Option<Value>,
) -> Result<(), Thrown> {
    req.lazy_skip(*object, name, value)
}
