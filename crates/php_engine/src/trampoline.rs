//! The functions php makes of a call to a name only `__call` or
//! `__callStatic` answers (`Z/Zend/zend_object_handlers.c:1764`,
//! `Z/Zend/zend_closures.c:878`): the trampoline
//! `zend_get_call_trampoline_func` makes where a method lookup falls
//! through to the magic method, whose one opline, `ZEND_CALL_TRAMPOLINE`,
//! hands the call over with its name and its arguments packed; and the
//! native `zend_closure_from_frame` makes of a trampoline,
//! `zend_closure_call_magic`, which a first-class callable or
//! `Closure::fromCallable` over such a name holds.
//!
//! php allocates one of each per lookup and frees it with the call.
//! Here each is a function number of the request's past
//! [`TRAMPOLINE_FUNCTIONS`], one pair per magic method and name the
//! request has called, so a call frame, a closure and a callable carry
//! it as they carry any other [`Function`], and two closures over one
//! name compare equal as php's do by name.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use php_unit::decl::{
    FunctionDecl, MethodDecl, Name, Parameter, ParameterDefault, Type, Visibility,
};

use crate::api::call::Function;
use crate::api::raising::Thrown;
use crate::api::request::Request;
use crate::api::values::{Key, PhpArray, PhpString, Value};

/// Where the trampolines' function numbers begin: past the enum methods'
/// ([`crate::enums::ENUM_FUNCTIONS`]), so the spaces never meet. Each
/// trampoline takes two numbers, its own and its
/// [`Served::ClosureCall`]'s beside it.
const TRAMPOLINE_FUNCTIONS: u32 = 3 << 30;

/// Which of php's two functions a trampoline's number is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Served {
    /// `ZEND_ACC_CALL_VIA_TRAMPOLINE`: what a method call, a class's
    /// call or a callable resolved to, which [`hand_to_magic`] turns
    /// into the magic method's own call when it is served, so the frame
    /// that stands is `__call`'s.
    Trampoline,
    /// `zend_closure_call_magic`: what a closure made of a trampoline
    /// holds, a native of the trampoline's name whose body,
    /// [`closure_call_magic`], calls the magic method, so its own frame
    /// stands under the magic method's.
    ClosureCall,
}

/// One trampoline: the magic method that serves it, and the method
/// declaration php's trampoline carries, public, variadic over
/// `mixed ...$arguments` as `trampoline_arg_info` is, named as the call
/// was written up to a NUL, returning by reference where the magic
/// method does, and on the class where `__callStatic` serves it.
pub(crate) struct CallTrampoline {
    pub(crate) magic: Function,
    pub(crate) decl: MethodDecl,
}

impl CallTrampoline {
    pub(crate) fn new(magic: Function, name: &[u8], is_static: bool, by_ref: bool) -> Self {
        // `strlen(ZSTR_VAL(method_name))`: "keep compatibility for "\0"
        // characters".
        let name = name.split(|byte| *byte == 0).next().unwrap_or_default();
        let arguments = Parameter {
            name: Name(b"arguments".as_slice().into()),
            ty: Some(Type::Mixed),
            by_ref: false,
            prefer_ref: false,
            promoted: false,
            variadic: true,
            default: ParameterDefault::None,
            attributes: Vec::new(),
        };
        CallTrampoline {
            magic,
            decl: MethodDecl {
                function: FunctionDecl {
                    name: Name(name.into()),
                    parameters: vec![arguments],
                    return_type: None,
                    returns_by_ref: by_ref,
                    is_generator: false,
                    attributes: Vec::new(),
                    doc_comment: None,
                    file: Name(Box::new([])),
                    line: 0,
                    end_line: 0,
                    frameless: Vec::new(),
                },
                visibility: Visibility::Public,
                is_instance: !is_static,
                is_abstract: false,
                is_final: false,
                tentative_return: false,
            },
        }
    }

    /// Whether this is the trampoline of `magic` under `name`.
    pub(crate) fn is(&self, magic: Function, name: &[u8]) -> bool {
        let name = name.split(|byte| *byte == 0).next().unwrap_or_default();
        self.magic == magic && *self.decl.function.name.0 == *name
    }
}

/// The number of the trampoline at `index` of the request's table, as
/// `served` names it.
pub(crate) fn number(index: usize, served: Served) -> Option<Function> {
    let index = u32::try_from(index).ok()?;
    let offset = index.checked_mul(2)?.checked_add(match served {
        Served::Trampoline => 0,
        Served::ClosureCall => 1,
    })?;
    TRAMPOLINE_FUNCTIONS
        .checked_add(offset)
        .filter(|number| *number != Function::PASS.0)
        .map(Function)
}

/// The index and the kind a trampoline's number names; none for any
/// other function.
pub(crate) fn decode(function: Function) -> Option<(usize, Served)> {
    let offset = function.0.checked_sub(TRAMPOLINE_FUNCTIONS)?;
    if function == Function::PASS {
        return None;
    }
    let served = if offset % 2 == 0 {
        Served::Trampoline
    } else {
        Served::ClosureCall
    };
    Some((usize::try_from(offset / 2).ok()?, served))
}

/// What `zend_closure_from_frame` puts in a trampoline's place: the
/// [`Served::ClosureCall`] of the same name and magic method; none for
/// any other function.
pub(crate) fn closure_call(function: Function) -> Option<Function> {
    match decode(function)? {
        (index, Served::Trampoline) => number(index, Served::ClosureCall),
        (_, Served::ClosureCall) => Some(function),
    }
}

/// `ZEND_CALL_TRAMPOLINE` (`Z/Zend/zend_vm_def.h:8950`) over the call
/// being served, where its callee is a [`Served::Trampoline`]: the
/// callee made the magic method, and the arguments the two it takes,
/// the name the call was made by and an array of what was passed, the
/// positional ones first and then the named ones the trampoline
/// collected (`ZEND_CALL_HAS_EXTRA_NAMED_PARAMS`). The call's receiver
/// and called scope stand as they were. What the callee is from here is
/// the answer; any other call is left as it is.
///
/// ```php
/// class P {
///     public function __call($name, $arguments) { return "$name(" . json_encode($arguments) . ")"; }
///     public static function __callStatic($name, $arguments) { return static::class . "::$name(" . json_encode($arguments) . ")"; }
///     private function hidden() { return 'hidden'; }
///     public function viaSelf() { return self::fromInside(3); }
/// }
/// class C extends P {}
/// $c = new C;
/// echo $c->anything(1, 2), "\n", $c->hidden(), "\n", C::onClass(x: 1), "\n", $c->viaSelf(), "\n";
/// echo call_user_func([$c, 'cb'], 5), "\n", call_user_func('C::scb'), "\n", [$c, 'arr'](), "\n";
/// $m = 'dyn';
/// echo $c->$m(), ' ', C::$m(), "\n";
/// ```
/// ```expect
/// anything([1,2])
/// hidden([])
/// C::onClass({"x":1})
/// fromInside([3])
/// cb([5])
/// C::scb([])
/// arr([])
/// dyn([]) C::dyn([])
/// ```
///
/// The frame that stands is the magic method's, called with the name
/// and the array:
///
/// ```php
/// class C { public function __call($n, $a) { throw new Exception($n); } }
/// (new C)->missing(1);
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught Exception: missing in %s:2
/// Stack trace:
/// #0 %s(3): C->__call('missing', Array)
/// #1 {main}
///   thrown in %s on line 2
/// ```
pub(crate) fn hand_to_magic(req: &mut Request, function: Function) -> Result<Function, Thrown> {
    let Some((magic, name)) = req
        .trampoline_of(function)
        .filter(|(_, served)| *served == Served::Trampoline)
        .map(|(trampoline, _)| (trampoline.magic, trampoline.decl.function.name.clone()))
    else {
        return Ok(function);
    };
    let name = PhpString::new(req, &name.0)?;
    let mut packed = PhpArray::new(req)?;
    let (arguments, named) = req.take_serving_arguments();
    for argument in arguments {
        packed.append(req, argument)?;
    }
    for (key, value) in named {
        packed.set(req, Key::from_string(key), value)?;
    }
    req.serve_instead(magic, vec![Value::string(name), Value::array(packed)]);
    Ok(magic)
}

/// `zend_closure_call_magic` (`Z/Zend/zend_closures.c:293`), the body
/// of a [`Served::ClosureCall`]: the magic method called through
/// `zend_call_function` on the frame's receiver, or on its called scope
/// where it has none, with the name and an array of the arguments, the
/// named ones after the positional.
///
/// ```php
/// class C {
///     public function __call($name, $arguments) { return "$name:" . json_encode($arguments); }
///     public static function __callStatic($name, $arguments) { return static::class . "::$name"; }
/// }
/// class D extends C {}
/// $f = (new C)->anything(...);
/// $g = D::onClass(...);
/// var_dump($f(1, x: 2), $g(), $f == (new C)->anything(...), $f == $f, $f == (new C)->other(...));
/// var_dump($f);
/// ```
/// ```expect
/// string(22) "anything:{"0":1,"x":2}"
/// string(10) "D::onClass"
/// bool(false)
/// bool(true)
/// bool(false)
/// object(Closure)#2 (3) {
///   ["function"]=>
///   string(11) "C::anything"
///   ["this"]=>
///   object(C)#1 (0) {
///   }
///   ["parameter"]=>
///   array(1) {
///     ["$arguments"]=>
///     string(10) "<optional>"
///   }
/// }
/// ```
///
/// Its own frame stands under the magic method's, named as the call
/// was:
///
/// ```php
/// class C { public function __call($n, $a) { throw new Exception($n); } }
/// $f = (new C)->missing(...);
/// $f(1);
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught Exception: missing in %s:2
/// Stack trace:
/// #0 [internal function]: C->__call('missing', Array)
/// #1 %s(4): C->missing(1)
/// #2 {main}
///   thrown in %s on line 2
/// ```
pub(crate) fn closure_call_magic(req: &mut Request) -> Result<Value, Thrown> {
    let Some((function, this, called)) = req.serving_callee() else {
        return Ok(Value::null());
    };
    let Some((magic, name)) = req
        .trampoline_of(function)
        .map(|(trampoline, _)| (trampoline.magic, trampoline.decl.function.name.clone()))
    else {
        return Ok(Value::null());
    };
    let count = u32::try_from(req.argument_count()).unwrap_or(u32::MAX);
    let passed: Vec<Value> = (1..=count)
        .filter_map(|position| req.argument(position))
        .collect();
    let named = req.named_arguments();
    let name = Value::string(PhpString::new(req, &name.0)?);
    let mut packed = PhpArray::new(req)?;
    for argument in passed {
        req.addref(&argument);
        packed.append(req, argument)?;
    }
    for (key, value) in named {
        req.addref(&Value::string(key));
        req.addref(&value);
        packed.set(req, Key::from_string(key), value)?;
    }
    let packed = Value::array(packed);
    let arguments = [name, packed];
    let called = this.map(|this| this.class()).or(called);
    let served = match called {
        Some(called) => req.serve_method(magic, this, called, &arguments, &[]),
        None => req.serve_function(magic, &arguments, &[]),
    };
    for argument in arguments {
        req.release(argument);
    }
    served
}
