//! The callable resolver, php's `zend_is_callable_ex`
//! (`Z/Zend/zend_API.c:4250`) with `zend_is_callable_at_frame`,
//! `zend_is_callable_check_class` and `zend_is_callable_check_func`
//! under it: the one place a value is read as a callable, for the
//! loop's `INIT_USER_CALL`, a `callable` type check, every native's
//! `Z_PARAM_FUNC` and Reflection alike. It resolves at the first user
//! frame ([`CallFrame`], read once and passed down), raises php's
//! deprecations where php raises them and in their order, loads a named
//! class through the autoloaders, and answers a typed [`Resolved`], a
//! callable or a trampoline still to be made, or php's `*error` as a
//! [`Refusal`].
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use std::fmt;

use php_unit::decl::{Bare, Visibility, shown};

use super::{Request, visibility_string};
use crate::api::abi::Callable;
use crate::api::call::{Function, Target};
use crate::api::values::{ClassId, ClosureInfo, Key, PhpObject, Value};

/// php's `IS_CALLABLE_SUPPRESS_DEPRECATIONS`: whether a resolution
/// raises the deprecations of `self`, `parent` and `static` as a class
/// half and of the `["C", "parent::m"]` form, or keeps them unsaid.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub(crate) enum Deprecations {
    Raised,
    #[default]
    Suppressed,
}

/// php's `*error` after `zend_is_callable_ex` refused a value: the
/// sentence it printed with `zend_spprintf`, the one it copied with
/// `estrdup`, or none, where a deprecation's user handler threw and
/// `zend_is_callable_check_class` gave up with the throwable pending,
/// which php's `%s` prints as `(null)`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum Refusal {
    Said(String),
    Copied(&'static str),
    Unsaid,
}

impl Refusal {
    /// `throw` taken with the sentence's `char *` standing on the
    /// request's heap, `zend_spprintf`'s buffer or `estrdup`'s copy,
    /// and freed after, as php holds `error` across the throw that
    /// quotes it and `efree`s it then (`zend_wrong_callback_error`,
    /// `ZEND_INIT_USER_CALL`, `zend_create_closure_from_callable` and
    /// `zend_call_function`). Each refusal peaks with the 256 bytes of
    /// its sentence:
    ///
    /// ```php
    /// function pk($f) { $f(); $s = memory_get_usage(); memory_reset_peak_usage(); $f(); echo memory_get_peak_usage() - $s, " ", memory_get_usage() - $s, "\n"; }
    /// pk(function () { try { array_map('nope', [1]); } catch (TypeError $e) {} });
    /// pk(function () { $a = [2, 1]; try { usort($a, 'nope'); } catch (TypeError $e) {} });
    /// pk(function () { try { Closure::fromCallable('nope'); } catch (TypeError $e) {} });
    /// pk(function () { try { call_user_func('nope'); } catch (TypeError $e) {} });
    /// pk(function () { try { call_user_func([new stdClass, 'nope']); } catch (TypeError $e) {} });
    /// pk(function () { try { call_user_func(1); } catch (TypeError $e) {} });
    /// ```
    /// ```expect
    /// 3144 32
    /// 3176 0
    /// 2952 0
    /// 2392 0
    /// 2648 0
    /// 2136 0
    /// ```
    pub(crate) fn across(
        &self,
        req: &mut Request,
        throw: impl FnOnce(&mut Request) -> crate::api::raising::Thrown,
    ) -> crate::api::raising::Thrown {
        let laid = match self {
            Refusal::Said(sentence) => crate::api::raising::printf_buffer(req, sentence.len()),
            Refusal::Copied(sentence) => {
                let block = req.heap_mut().block(sentence.len() + 1);
                req.fatal_of(block)
            }
            Refusal::Unsaid => return throw(req),
        };
        match laid {
            Ok(block) => {
                let thrown = throw(req);
                req.heap_mut().free_block(block);
                thrown
            }
            Err(refused) => refused,
        }
    }
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refusal::Said(sentence) => f.write_str(sentence),
            Refusal::Copied(sentence) => f.write_str(sentence),
            Refusal::Unsaid => f.write_str("(null)"),
        }
    }
}

impl From<String> for Refusal {
    fn from(sentence: String) -> Refusal {
        Refusal::Said(sentence)
    }
}

impl From<&'static str> for Refusal {
    fn from(sentence: &'static str) -> Refusal {
        Refusal::Copied(sentence)
    }
}

/// What `zend_is_callable_ex` reads off the first user frame: its
/// function's scope (`get_scope`), its called scope
/// (`zend_get_called_scope`) and its `$this` (`zend_get_this_object`).
#[derive(Default, Clone, Copy)]
struct CallFrame {
    scope: Option<ClassId>,
    called: Option<ClassId>,
    this: Option<PhpObject>,
    /// `zend_get_this_object(EG(current_execute_data))`, which
    /// `get_static_method_fallback` reads: the user frame's `$this`
    /// unless a native method stands between, whose receiver it is.
    current_this: Option<PhpObject>,
}

/// `zend_fcall_info_cache` as `zend_is_callable_at_frame` fills it: the
/// class whose table the method is looked up in, the called scope, the
/// callee and the receiver.
#[derive(Default)]
struct Fcc {
    calling: Option<ClassId>,
    called: Option<ClassId>,
    function: Option<Callee>,
    object: Option<PhpObject>,
}

/// `fcc->function_handler` once `zend_is_callable_check_func` found
/// one: a function the tables have; the trampoline of a class's
/// `__call`, or of its `__callStatic` where `is_static`, under the name
/// written (`zend_get_call_trampoline_func`); or a closure's own
/// `__invoke` (`zend_get_closure_invoke_method`). The resolver answers
/// the last two without making them, and [`Request::callable_made`]
/// makes them.
#[derive(Clone)]
enum Callee {
    Function(Function),
    Trampoline {
        class: ClassId,
        name: Vec<u8>,
        is_static: bool,
    },
    Invoke(PhpObject),
}

/// What `zend_is_callable_ex` resolved a value to: a callable, or one
/// whose callee is still to be made, with the scope and the receiver it
/// is called on.
enum Resolved {
    Made(Callable),
    Pending {
        held: Value,
        name: String,
        callee: Callee,
        scope: ClassId,
        this: Option<PhpObject>,
    },
}

/// Where a compound `Class::method` name splits, as
/// `zend_is_callable_check_func` and `zend_init_dynamic_call_string`
/// both find it: the last `:` of the name, where it is past the first
/// byte and a second `:` stands before it, and the split is at that
/// second one, so `::m` has an empty class half and `C:` none at all.
///
/// ```php
/// class C { static function __callStatic($m, $a) { var_dump($m); } }
/// foreach (['C::', '::m', 'C:', ':m'] as $f) {
///     try { $f(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// }
/// ```
/// ```expect
/// string(0) ""
/// Class "" not found
/// Call to undefined function C:()
/// Call to undefined function :m()
/// ```
pub(crate) fn compound_split(name: &[u8]) -> Option<usize> {
    name.iter()
        .rposition(|byte| *byte == b':')
        .filter(|at| *at > 0 && name[*at - 1] == b':')
        .map(|at| at - 1)
}

impl Request {
    /// Whether `zend_is_callable_ex` accepts a value, no trampoline
    /// made: `is_callable()` and a `callable` type check, which raise
    /// the deprecations (`check_flags` 0), or a caller that suppresses
    /// them.
    ///
    /// ```php
    /// class P { static function m() {} }
    /// class C extends P {
    ///     static function t() { var_dump(is_callable('static::m'), is_callable(['C', 'parent::m']), is_callable('nope')); }
    /// }
    /// C::t();
    /// ```
    /// ```expect
    ///
    /// Deprecated: Use of "static" in callables is deprecated in %s on line 4
    ///
    /// Deprecated: Callables of the form ["C", "parent::m"] are deprecated in %s on line 4
    /// bool(true)
    /// bool(true)
    /// bool(false)
    /// ```
    pub(crate) fn is_callable_value(&mut self, value: &Value, deprecations: Deprecations) -> bool {
        self.callable_resolved(value, deprecations).is_ok()
    }

    /// The callable a resolution answered, the trampoline it names made
    /// where it names one.
    fn callable_made(&mut self, resolved: Resolved) -> Result<Callable, Refusal> {
        match resolved {
            Resolved::Made(callable) => Ok(callable),
            Resolved::Pending {
                held,
                name,
                callee,
                scope,
                this,
            } => {
                let made = match &callee {
                    Callee::Function(function) => Some(*function),
                    Callee::Trampoline {
                        class,
                        name,
                        is_static,
                    } => self.call_trampoline(*class, name, *is_static),
                    Callee::Invoke(closure) => self.invoke_trampoline(*closure),
                };
                let Some(function) = made else {
                    return Err(Refusal::Said(format!(
                        "class {} does not have a method \"{}\"",
                        self.symbols().class_name(scope),
                        shown(name.as_bytes()),
                    )));
                };
                let target = Target::Method {
                    function,
                    scope,
                    this,
                };
                Ok(Callable::of(held, name.as_bytes(), target))
            }
        }
    }

    /// `zend_is_callable_ex` with its `*error`, the deprecations raised,
    /// as `zend_fcall_info_init` asks it: a function name, a
    /// `Class::method` string, a `[$object, 'method']` or `['Class',
    /// 'method']` pair, an object whose class has `__invoke`, and a
    /// `Closure`, resolved at the first user frame to the callee, the
    /// scope and the receiver a call takes, and named as
    /// `zend_get_callable_name_ex` names it. The `Err` is php's
    /// `*error`, which a native's `must be a valid callback` ends with;
    /// where a deprecation's handler threw, the throwable stands
    /// pending beside it.
    ///
    /// Every form, each served where `call_user_func` serves it: a
    /// function by name with and without its leading `\`, a pair on an
    /// object and on a class, the `Class::method` string, an object with
    /// `__invoke`, a closure, and a `static` method named on an object,
    /// which is served with no receiver.
    ///
    /// ```php
    /// class C { public function m($x) { return "m$x "; } public static function s($x) { return "s$x "; } public function __invoke($x) { return "i$x "; } }
    /// function f($x) { return "f$x "; }
    /// $c = new C;
    /// echo call_user_func('f', 1), call_user_func('\f', 2), call_user_func([$c, 'm'], 3), call_user_func(['C', 's'], 4), call_user_func('C::s', 5), call_user_func($c, 6), call_user_func(function ($x) { return "c$x "; }, 7), call_user_func([$c, 's'], 8), "|\n";
    /// ```
    /// ```expect
    /// f1 f2 m3 s4 s5 i6 c7 s8 |
    /// ```
    ///
    /// Each refusal is the sentence `zend_is_callable_check_func` and its
    /// kin write, which the native's `must be a valid callback` ends with.
    ///
    /// ```php
    /// class C { function m() {} private function priv() {} }
    /// foreach ([[], [1, 2, 3], [1 => 'a', 2 => 'b'], [1, 'm'], ['C', 1], 'nope', 'C::nope', 'Nope::m', '::m', 'C::m', ['C', 'priv'], 42, new C] as $cb) {
    ///     try { call_user_func($cb); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// }
    /// ```
    /// ```expect
    /// call_user_func(): Argument #1 ($callback) must be a valid callback, array callback must have exactly two members
    /// call_user_func(): Argument #1 ($callback) must be a valid callback, array callback must have exactly two members
    /// call_user_func(): Argument #1 ($callback) must be a valid callback, array callback has to contain indices 0 and 1
    /// call_user_func(): Argument #1 ($callback) must be a valid callback, first array member is not a valid class name or object
    /// call_user_func(): Argument #1 ($callback) must be a valid callback, second array member is not a valid method
    /// call_user_func(): Argument #1 ($callback) must be a valid callback, function "nope" not found or invalid function name
    /// call_user_func(): Argument #1 ($callback) must be a valid callback, class C does not have a method "nope"
    /// call_user_func(): Argument #1 ($callback) must be a valid callback, class "Nope" not found
    /// call_user_func(): Argument #1 ($callback) must be a valid callback, invalid function name
    /// call_user_func(): Argument #1 ($callback) must be a valid callback, non-static method C::m() cannot be called statically
    /// call_user_func(): Argument #1 ($callback) must be a valid callback, non-static method C::priv() cannot be called statically
    /// call_user_func(): Argument #1 ($callback) must be a valid callback, no array or string given
    /// call_user_func(): Argument #1 ($callback) must be a valid callback, no array or string given
    /// ```
    ///
    /// The class half is read at the first user frame: `self`, `parent`
    /// and `static` are the method's own, each with its deprecation, a
    /// class the scope extends binds the frame's `$this`, the array form
    /// `[$this, 'parent::m']` is deprecated in its own words, and a
    /// method's visibility is held against that frame's scope.
    ///
    /// ```php
    /// class P { public static function who() { return 'P'; } public function inst() { return 'inst'; } private function hidden() { return 'hidden'; } }
    /// class C extends P {
    ///     public static function who() { return 'C'; }
    ///     public function test() { return [call_user_func('self::who'), call_user_func('parent::who'), call_user_func('static::who'), call_user_func(['P', 'inst']), call_user_func([$this, 'parent::who'])]; }
    /// }
    /// var_dump((new C)->test());
    /// try { call_user_func('self::who'); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// try { call_user_func('parent::who'); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// try { call_user_func([new C, 'hidden']); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// Deprecated: Use of "self" in callables is deprecated in %s on line 5
    ///
    /// Deprecated: Use of "parent" in callables is deprecated in %s on line 5
    ///
    /// Deprecated: Use of "static" in callables is deprecated in %s on line 5
    ///
    /// Deprecated: Callables of the form ["C", "parent::who"] are deprecated in %s on line 5
    /// array(5) {
    ///   [0]=>
    ///   string(1) "C"
    ///   [1]=>
    ///   string(1) "P"
    ///   [2]=>
    ///   string(1) "C"
    ///   [3]=>
    ///   string(4) "inst"
    ///   [4]=>
    ///   string(1) "P"
    /// }
    /// call_user_func(): Argument #1 ($callback) must be a valid callback, cannot access "self" when no class scope is active
    /// call_user_func(): Argument #1 ($callback) must be a valid callback, cannot access "parent" when no class scope is active
    /// call_user_func(): Argument #1 ($callback) must be a valid callback, cannot access private method C::hidden()
    /// ```
    ///
    /// A class named as the class half is loaded through the
    /// autoloaders, as `zend_lookup_class` loads it, and a deprecation
    /// whose handler throws leaves the resolution with no sentence,
    /// which `INIT_USER_CALL` chains as `(null)`:
    ///
    /// ```php
    /// spl_autoload_register(function ($class) { echo "load $class\n"; });
    /// try { call_user_func('Missing::m'); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// class K { static function __callStatic($n, $a) {} static function t() { call_user_func('self::y'); } }
    /// set_error_handler(function () { throw new Exception("handler"); });
    /// try { K::t(); } catch (TypeError $e) { echo $e->getMessage(), " / ", $e->getPrevious()->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// load Missing
    /// call_user_func(): Argument #1 ($callback) must be a valid callback, class "Missing" not found
    /// call_user_func(): Argument #1 ($callback) must be a valid callback, (null) / handler
    /// ```
    pub(crate) fn callable_checked(&mut self, value: &Value) -> Result<Callable, Refusal> {
        let resolved = self.callable_resolved(value, Deprecations::Raised)?;
        self.callable_made(resolved)
    }

    /// `Z_OBJ_HANDLER_P(callable, get_closure)` of a class whose table
    /// answers a function of its own making rather than its `__invoke`,
    /// as `zend_ffi_cdata_get_closure` answers a C function pointer's
    /// trampoline, which `zend_is_callable_ex` asks before anything else
    /// of an object; `ext/ffi`'s `get_closure` carries the witness.
    pub(crate) fn handler_closure(&mut self, object: PhpObject) -> Option<ClosureInfo> {
        let class = object.class();
        let made = self.own_handlers(class)?.get_closure(self, object)?;
        (self.symbols().method_named(class, b"__invoke") != Some(made.function)).then_some(made)
    }

    /// `zend_is_callable_at_frame`'s `case IS_OBJECT`.
    fn callable_object(&mut self, held: Value, object: PhpObject) -> Result<Resolved, Refusal> {
        // The `get_closure` handler: a `Closure`'s payload, a function
        // the class's table makes ([`Request::handler_closure`]), then
        // `zend_std_get_closure`'s `__invoke`, which reads the class's
        // table and no visibility.
        if object.closure(self).is_some() {
            let name = self.callable_name(&held);
            return Ok(Resolved::Made(Callable::of(
                held,
                name.as_bytes(),
                Target::Closure(object),
            )));
        }
        let class = object.class();
        if let Some(made) = self.handler_closure(object) {
            let name = self.callable_name(&held);
            let target = Target::Method {
                function: made.function,
                scope: made.called_scope.unwrap_or(class),
                this: made.this,
            };
            return Ok(Resolved::Made(Callable::of(held, name.as_bytes(), target)));
        }
        let Some(function) = self.symbols().method_named(class, b"__invoke") else {
            return Err("no array or string given".into());
        };
        let this =
            Some(object).filter(|_| self.method_decl(function).is_none_or(|d| d.is_instance));
        let name = self.callable_name(&held);
        Ok(Resolved::Made(Callable::of(
            held,
            name.as_bytes(),
            Target::Method {
                function,
                scope: class,
                this,
            },
        )))
    }

    /// `zend_is_callable_at_frame` (`Z/Zend/zend_API.c:4139`) over the
    /// frame `zend_is_callable_ex` walks to.
    fn callable_resolved(
        &mut self,
        value: &Value,
        deprecations: Deprecations,
    ) -> Result<Resolved, Refusal> {
        let frame = self.callable_frame();
        // `case IS_REFERENCE: goto again`.
        let held = value.deref();
        let mut fcc = Fcc::default();
        if let Some(object) = held.as_object() {
            return self.callable_object(held, object);
        }
        let mut strict = false;
        let method = if let Some(array) = held.as_array() {
            if array.len() != 2 {
                return Err("array callback must have exactly two members".into());
            }
            let (Some(first), Some(second)) = (array.get(&Key::Int(0)), array.get(&Key::Int(1)))
            else {
                return Err("array callback has to contain indices 0 and 1".into());
            };
            let (first, second) = (first.deref(), second.deref());
            if first.as_object().is_none() && first.as_str_bytes().is_none() {
                return Err("first array member is not a valid class name or object".into());
            }
            let Some(method) = second.as_str_bytes() else {
                return Err("second array member is not a valid method".into());
            };
            let method = method.to_vec();
            if let Some(object) = first.as_object() {
                fcc.calling = Some(object.class());
                fcc.object = Some(object);
            } else if let Some(class) = first.as_str_bytes() {
                let class = class.to_vec();
                strict =
                    self.callable_class(&class, frame.scope, &frame, &mut fcc, deprecations)?;
            }
            method
        } else if let Some(name) = held.as_str_bytes() {
            name.to_vec()
        } else {
            return Err("no array or string given".into());
        };
        self.callable_func(&method, &frame, &mut fcc, strict, deprecations)?;
        let Some(callee) = fcc.function.take() else {
            return Err("no array or string given".into());
        };
        let name = self.callable_name(&held);
        let scope = fcc.called.or(fcc.calling);
        let function = match callee {
            Callee::Function(function) => function,
            Callee::Trampoline { class, .. } => {
                return Ok(Resolved::Pending {
                    held,
                    name,
                    scope: scope.unwrap_or(class),
                    callee,
                    this: fcc.object,
                });
            }
            Callee::Invoke(closure) => {
                return Ok(Resolved::Pending {
                    held,
                    name,
                    scope: scope.unwrap_or(closure.class()),
                    callee,
                    this: fcc.object,
                });
            }
        };
        // A receiver is always scoped: `fcc->called_scope` is the
        // object's class where nothing named one (`Z/Zend/zend_API.c:3802`).
        let target = match scope.or(fcc.object.map(|this| this.class())) {
            None => Target::Function(function),
            Some(scope) => Target::Method {
                function,
                scope,
                this: fcc.object,
            },
        };
        Ok(Resolved::Made(Callable::of(held, name.as_bytes(), target)))
    }

    /// The frame `zend_is_callable_ex` resolves at: the first link of
    /// the chain from `EG(current_execute_data)` that is user code, its
    /// function's scope (`get_scope`), its called scope
    /// (`zend_get_called_scope`) and its `$this`
    /// (`zend_get_this_object`); none of the three for the main op
    /// array's frame, and none where no user frame stands.
    fn callable_frame(&self) -> CallFrame {
        // `zend_get_this_object(EG(current_execute_data))`: the first
        // receiver down the chain, past the calls of natives that are no
        // method, and none at a user frame or a method that has none.
        let mut current_this = None;
        let mut walking = true;
        for link in self.chain() {
            let state = match link {
                super::Link::Call(call) => {
                    if walking
                        && (call.this().is_some() || self.method_class(call.callee()).is_some())
                    {
                        current_this = call.this();
                        walking = false;
                    }
                    continue;
                }
                super::Link::Frame(_, state) => state,
                super::Link::Fake(_) => continue,
                // `zend_get_attribute_object`'s dummy frame: user code
                // with no scope and no `$this`.
                super::Link::Site(_) => {
                    return CallFrame {
                        current_this: current_this.filter(|_| !walking),
                        ..CallFrame::default()
                    };
                }
            };
            if walking {
                current_this = state.this();
            }
            let scope = self
                .closure_scope(state.closure())
                .or_else(|| state.function().and_then(|f| self.method_class(f)))
                .or(state.scope());
            let called = state
                .this()
                .map(|this| this.class())
                .or(state.called_scope());
            return CallFrame {
                scope,
                called,
                this: state.this(),
                current_this,
            };
        }
        CallFrame {
            current_this,
            ..CallFrame::default()
        }
    }

    /// `zend_is_callable_check_class` (`Z/Zend/zend_API.c:3721`): the
    /// class half of a callable, `self`, `parent` and `static` read off
    /// the frame with their deprecation, raised as it is met, and their
    /// refusals where the frame has no class to give; any other name
    /// looked up with `zend_lookup_class`, the autoloaders served for a
    /// class the tables lack. A class named on the frame's scope, or one
    /// the scope extends, binds the frame's `$this` where it is an
    /// instance of the scope. It leaves `fcc->calling_scope` and the
    /// called scope in `fcc` and answers php's `*strict_class`; a
    /// throwable a deprecation's handler or an autoloader left pending
    /// makes it a refusal, php's `if (EG(exception)) return false`.
    ///
    /// php lowers the name into `ZSTR_ALLOCA_ALLOC(lcname, ...)`, and
    /// `do_alloca` is `emalloc` where php is built for macOS
    /// (`!defined(__APPLE__)`, `Z/Zend/zend_portability.h:219`), so there
    /// the lowered name is a block of the request's for as long as the
    /// check runs, an autoloader's throw included; elsewhere it is the
    /// stack's.
    ///
    /// ```php
    /// function pk($f) { $f(); $s = memory_get_usage(); memory_reset_peak_usage(); $f(); echo memory_get_peak_usage() - $s, "\n"; }
    /// spl_autoload_register(function ($c) { throw new RuntimeException('boom'); });
    /// pk(function () { try { is_callable('Foo::bar'); } catch (Throwable $e) {} });
    /// pk(function () { try { call_user_func('Foo::bar'); } catch (Throwable $e) {} });
    /// ```
    /// ```expect
    /// 2968
    /// 4376
    /// ```
    fn callable_class(
        &mut self,
        name: &[u8],
        scope: Option<ClassId>,
        frame: &CallFrame,
        fcc: &mut Fcc,
        deprecations: Deprecations,
    ) -> Result<bool, Refusal> {
        let lcname = if cfg!(target_vendor = "apple") {
            // `_ZSTR_STRUCT_SIZE`: the header's 24 bytes, the name and
            // its terminator.
            let block = self.heap.block(24 + name.len() + 1);
            match self.fatal_of(block) {
                Ok(block) => Some(block),
                Err(_) => return Err(Refusal::Unsaid),
            }
        } else {
            None
        };
        let checked = self.callable_class_lowered(name, scope, frame, fcc, deprecations);
        if let Some(block) = lcname {
            self.heap.free_block(block);
        }
        checked
    }

    /// [`Request::callable_class`] once its lowered name is held.
    fn callable_class_lowered(
        &mut self,
        name: &[u8],
        scope: Option<ClassId>,
        frame: &CallFrame,
        fcc: &mut Fcc,
        deprecations: Deprecations,
    ) -> Result<bool, Refusal> {
        let lowered = crate::exec::Lowered::of(name);
        let (word, resolved): (&str, Result<(ClassId, ClassId, bool), Refusal>) = match &*lowered {
            b"self" => (
                "self",
                scope
                    .map(|scope| (scope, scope, false))
                    .ok_or_else(|| "cannot access \"self\" when no class scope is active".into()),
            ),
            b"parent" => (
                "parent",
                match scope {
                    None => Err("cannot access \"parent\" when no class scope is active".into()),
                    Some(scope) => self
                        .symbols()
                        .class_parent(scope)
                        .map(|parent| (parent, parent, true))
                        .ok_or_else(|| {
                            "cannot access \"parent\" when current class scope has no parent".into()
                        }),
                },
            ),
            b"static" => (
                "static",
                frame
                    .called
                    .map(|called| (called, called, true))
                    .ok_or_else(|| "cannot access \"static\" when no class scope is active".into()),
            ),
            _ => ("", Err(Refusal::Unsaid)),
        };
        if word.is_empty() {
            return self.callable_class_named(name, frame, fcc);
        }
        let (calling, bound, strict) = resolved?;
        if deprecations == Deprecations::Raised {
            crate::api::raising::deprecated(
                self,
                &format!("Use of \"{word}\" in callables is deprecated"),
            );
        }
        fcc.called = if word == "static" {
            Some(bound)
        } else {
            frame
                .called
                .filter(|called| self.symbols().instance_of(*called, bound))
                .or(Some(bound))
        };
        fcc.calling = Some(calling);
        fcc.object = fcc.object.or(frame.this);
        if self.is_pending() {
            return Err(Refusal::Unsaid);
        }
        Ok(strict)
    }

    /// [`Request::callable_class`] over a class by name,
    /// `zend_lookup_class` and what php leaves in `fcc` after it.
    fn callable_class_named(
        &mut self,
        name: &[u8],
        frame: &CallFrame,
        fcc: &mut Fcc,
    ) -> Result<bool, Refusal> {
        let class = match crate::api::call::autoload(self, name) {
            Ok(class) => class,
            Err(thrown) => {
                self.leave_pending(&thrown);
                None
            }
        };
        let Some(class) = class else {
            return Err(Refusal::Said(format!(
                "class \"{}\" not found",
                shown(name)
            )));
        };
        fcc.calling = Some(class);
        if let Some(scope) = frame.scope
            && fcc.object.is_none()
        {
            match frame.this {
                Some(this)
                    if self.symbols().instance_of(this.class(), scope)
                        && self.symbols().instance_of(scope, class) =>
                {
                    fcc.object = Some(this);
                    fcc.called = Some(this.class());
                }
                _ => fcc.called = Some(class),
            }
        } else {
            fcc.called = Some(fcc.object.map_or(class, |object| object.class()));
        }
        if self.is_pending() {
            return Err(Refusal::Unsaid);
        }
        Ok(true)
    }

    /// `zend_is_callable_check_func` (`Z/Zend/zend_API.c:3829`): the
    /// function half. With no class resolved yet, a function by name,
    /// a leading `\` skipped; then a compound `Class::method`, split at
    /// its last `::`, its class half resolved as
    /// [`Request::callable_class`] resolves it, which under an array's
    /// own class is php's deprecated `["C", "parent::m"]` form and must
    /// name a class that one extends; then the method under the lowered
    /// name, `__construct` being the class's constructor under a strict
    /// class, a parent's private method where the frame's scope is that
    /// parent (`ZEND_ACC_CHANGED`), and the handler's answer where the
    /// class has none the scope may call ([`Request::callable_via_handler`]);
    /// and php's refusals, each in its words, a name in them up to its
    /// first NUL as php's `%s` prints it:
    ///
    /// ```php
    /// try { call_user_func("a\0b"); } catch (Throwable $e) { echo $e->getMessage(), "\n"; }
    /// try { array_map("a\0b", [1]); } catch (Throwable $e) { echo $e->getMessage(), "\n"; }
    /// class C {}
    /// foreach (["C::m\0n", ["C", "m\0n"], "N\0o::m"] as $f) {
    ///     try { call_user_func($f); } catch (Throwable $e) { echo $e->getMessage(), "\n"; }
    /// }
    /// ```
    /// ```expect
    /// call_user_func(): Argument #1 ($callback) must be a valid callback, function "a" not found or invalid function name
    /// array_map(): Argument #1 ($callback) must be a valid callback or null, function "a" not found or invalid function name
    /// call_user_func(): Argument #1 ($callback) must be a valid callback, class C does not have a method "m"
    /// call_user_func(): Argument #1 ($callback) must be a valid callback, class C does not have a method "m"
    /// call_user_func(): Argument #1 ($callback) must be a valid callback, class "N" not found
    /// ```
    fn callable_func(
        &mut self,
        callable: &[u8],
        frame: &CallFrame,
        fcc: &mut Fcc,
        mut strict: bool,
        deprecations: Deprecations,
    ) -> Result<(), Refusal> {
        let ce_org = fcc.calling.take();
        if ce_org.is_none() {
            let fetched = self.fetch_function(Bare::of(callable));
            if let Some(function) = self.fatal_of(fetched).map_err(|_| Refusal::Unsaid)? {
                fcc.function = Some(Callee::Function(function));
                return Ok(());
            }
        }
        let split = compound_split(callable);
        let method: Vec<u8> = match split {
            Some(0) => return Err("invalid function name".into()),
            Some(at) => {
                let scope = ce_org.or(frame.scope);
                let class = callable[..at].to_vec();
                // `zend_string_init_interned` of the class part, `cname`,
                // held while `zend_is_callable_check_class` looks the class
                // up and released after it.
                let Ok(cname) = self.interned(&class) else {
                    return Err(Refusal::Unsaid);
                };
                let quiet = if ce_org.is_some() {
                    Deprecations::Suppressed
                } else {
                    deprecations
                };
                let checked = self.callable_class(&class, scope, frame, fcc, quiet);
                cname.give_up(self);
                strict = checked?;
                if let Some(org) = ce_org {
                    let calling = fcc.calling.unwrap_or(org);
                    if !self.symbols().instance_of(org, calling) {
                        return Err(Refusal::Said(format!(
                            "class {} is not a subclass of {}",
                            self.symbols().class_name(org),
                            self.symbols().class_name(calling)
                        )));
                    }
                    if deprecations == Deprecations::Raised {
                        let class = self.symbols().class_name(org);
                        let form = [
                            b"Callables of the form [\"",
                            class.as_bytes(),
                            b"\", \"",
                            php_unit::decl::shown_bytes(callable),
                            b"\"] are deprecated",
                        ]
                        .concat();
                        crate::api::raising::deprecated(self, &form);
                    }
                }
                callable[at + 2..].to_vec()
            }
            None if ce_org.is_some() => {
                fcc.calling = ce_org;
                callable.to_vec()
            }
            None => {
                return Err(Refusal::Said(format!(
                    "function \"{}\" not found or invalid function name",
                    shown(callable)
                )));
            }
        };
        let lowered = crate::exec::Lowered::of(&method);
        let Some(calling) = fcc.calling else {
            return Err(Refusal::Said(format!(
                "function {}() does not exist",
                shown(&method)
            )));
        };
        let found = if strict && *lowered == *b"__construct" {
            self.constructor(calling)
                .map(|function| (Callee::Function(function), false))
        } else {
            match self.callable_method(calling, &lowered, strict, frame, fcc) {
                Some(function) => Some((Callee::Function(function), false)),
                None => self
                    .callable_via_handler(calling, ce_org, &method, &lowered, strict, frame, fcc),
            }
        };
        let Some((callee, via_handler)) = found else {
            return Err(Refusal::Said(format!(
                "class {} does not have a method \"{}\"",
                self.symbols().class_name(calling),
                shown(&method)
            )));
        };
        self.callable_settle(callee, frame, fcc, via_handler)
    }

    /// `zend_hash_find(ftable, lmname)` in `zend_is_callable_check_func`:
    /// the method `calling` has under the lowered name, the frame's
    /// scope's own private method in its place where that scope is a
    /// parent of the method's class that declares one and the class
    /// half was not named strictly (`ZEND_ACC_CHANGED`), and none where
    /// the method is not public, the scope may not call it and the
    /// class has the magic method a handler would serve it through,
    /// `__call` with a receiver and `__callStatic` without.
    fn callable_method(
        &self,
        calling: ClassId,
        lowered: &[u8],
        strict: bool,
        frame: &CallFrame,
        fcc: &Fcc,
    ) -> Option<Function> {
        let mut function = self.symbols().method_named(calling, lowered)?;
        if !strict
            && let Some(scope) = frame.scope
            && self
                .method_class(function)
                .is_some_and(|class| class != scope && self.symbols().instance_of(class, scope))
            && let Some(own) = self.symbols().method_named(scope, lowered)
            && self.method_class(own) == Some(scope)
            && self.method_visibility(own) == Visibility::Private
        {
            function = own;
        }
        let magic: &[u8] = if fcc.object.is_some() {
            b"__call"
        } else {
            b"__callstatic"
        };
        let withheld = self.method_visibility(function) != Visibility::Public
            && self.symbols().method_named(calling, magic).is_some()
            && !self.method_accessible_from(function, frame.scope);
        (!withheld).then_some(function)
    }

    /// `get_function_via_handler` in `zend_is_callable_check_func`
    /// (`Z/Zend/zend_API.c:3980`), answering the callee and whether php
    /// marks it `call_via_handler`. On the object a pair named: its
    /// class's `__call` where the class half was named strictly and
    /// `ce_org` has one, and otherwise the object's `get_method`
    /// (`zend_std_get_method`: `Closure`'s `__invoke`, the method the
    /// scope may call, else the `__call` trampoline), which under a
    /// strict class half must be a method of a class `ce_org` descends
    /// from. On a class, `zend_std_get_static_method`'s fallback:
    /// `__call` on the frame's `$this` where it is an instance of the
    /// class, which then is the receiver, and `__callStatic` otherwise.
    #[allow(clippy::too_many_arguments)] // `zend_is_callable_check_func`'s state, each one fact
    fn callable_via_handler(
        &mut self,
        calling: ClassId,
        ce_org: Option<ClassId>,
        method: &[u8],
        lowered: &[u8],
        strict: bool,
        frame: &CallFrame,
        fcc: &mut Fcc,
    ) -> Option<(Callee, bool)> {
        let trampoline = |class: ClassId, is_static: bool| Callee::Trampoline {
            class,
            name: method.to_vec(),
            is_static,
        };
        if let Some(org) = ce_org
            && let Some(object) = fcc.object.filter(|_| calling == org)
        {
            if strict && self.symbols().method_named(org, b"__call").is_some() {
                return Some((trampoline(org, false), true));
            }
            let class = object.class();
            let (callee, via, scope) = if lowered == b"__invoke" && object.closure(self).is_some() {
                (
                    Callee::Invoke(object),
                    true,
                    self.class_named(Bare::resolved(b"Closure")),
                )
            } else if let Some(function) = self
                .symbols()
                .method_named(class, lowered)
                .filter(|function| self.method_accessible_from(*function, frame.scope))
                .or_else(|| self.trampoline_named(class, lowered))
            {
                (
                    Callee::Function(function),
                    self.trampoline_of(function).is_some(),
                    self.method_class(function),
                )
            } else if self.symbols().method_named(class, b"__call").is_some() {
                (
                    trampoline(class, false),
                    true,
                    self.method_class(self.symbols().method_named(class, b"__call")?),
                )
            } else {
                // `get_method(&fcc->object, …)` may answer on the object
                // its class hands the call to, which then is the receiver.
                let (function, receiver) = self.callable_on_receiver(object, lowered, method)?;
                fcc.object = Some(receiver);
                (
                    Callee::Function(function),
                    self.trampoline_of(function).is_some(),
                    self.method_class(function),
                )
            };
            // `zend_release_fcall_info_cache`: under a strict class half
            // the method must be one `ce_org` has.
            if strict && !scope.is_some_and(|scope| self.symbols().instance_of(org, scope)) {
                return None;
            }
            return Some((callee, via));
        }
        let callee = if self.symbols().method_named(calling, b"__call").is_some()
            && let Some(this) = frame
                .current_this
                .filter(|this| self.symbols().instance_of(this.class(), calling))
        {
            trampoline(this.class(), false)
        } else if self
            .symbols()
            .method_named(calling, b"__callstatic")
            .is_some()
        {
            trampoline(calling, true)
        } else {
            return None;
        };
        if fcc.object.is_none() {
            fcc.object = frame
                .this
                .filter(|this| self.symbols().instance_of(this.class(), calling));
        }
        Some((callee, true))
    }

    /// The rest of `get_method` where the object's own class has no
    /// method of the name: the object its class hands the call to
    /// ([`ObjectHandlers::method_receiver`]), and the method found on it
    /// as a call finds one ([`Request::method_found`]), with the
    /// object that answered. A throwable the lookup raised is left
    /// pending, php's `get_method` answering `NULL` with it.
    ///
    /// [`ObjectHandlers::method_receiver`]: crate::api::abi::ObjectHandlers::method_receiver
    fn callable_on_receiver(
        &mut self,
        object: PhpObject,
        lowered: &[u8],
        written: &[u8],
    ) -> Option<(Function, PhpObject)> {
        let inner = self
            .object_handlers(object.heap())
            .method_receiver(self, object)?;
        match self.method_found(inner, lowered, written) {
            Ok((found, deeper)) => found.map(|function| (function, deeper.unwrap_or(inner))),
            Err(thrown) => {
                self.leave_pending(&thrown);
                None
            }
        }
    }

    /// The end of `zend_is_callable_check_func` once a callee is found:
    /// php's refusals of an abstract method, of an instance method with
    /// no receiver and of a method the frame's scope may not call, each
    /// in its words, and a `static` method's receiver dropped. A callee
    /// reached through a trampoline is held to none of them.
    fn callable_settle(
        &self,
        callee: Callee,
        frame: &CallFrame,
        fcc: &mut Fcc,
        via_handler: bool,
    ) -> Result<(), Refusal> {
        let is_static = match &callee {
            Callee::Function(function) => self
                .method_decl(*function)
                .is_some_and(|decl| !decl.is_instance),
            Callee::Trampoline { is_static, .. } => *is_static,
            Callee::Invoke(_) => false,
        };
        if let Callee::Function(function) = callee
            && let Some(calling) = fcc.calling
            && !via_handler
        {
            let decl = self.method_decl(function);
            let method_shown = || {
                format!(
                    "{}::{}",
                    self.symbols().class_name(calling),
                    decl.map_or_else(String::new, |decl| {
                        shown(&decl.function.name.0).into_owned()
                    })
                )
            };
            if decl.is_some_and(|decl| decl.is_abstract) {
                return Err(Refusal::Said(format!(
                    "cannot call abstract method {}()",
                    method_shown()
                )));
            }
            if fcc.object.is_none() && !is_static {
                return Err(Refusal::Said(format!(
                    "non-static method {}() cannot be called statically",
                    method_shown()
                )));
            }
            let visibility = self.method_visibility(function);
            if visibility != Visibility::Public
                && !self.method_accessible_from(function, frame.scope)
            {
                return Err(Refusal::Said(format!(
                    "cannot access {} method {}()",
                    visibility_string(visibility),
                    method_shown()
                )));
            }
        }
        fcc.function = Some(callee);
        if let Some(object) = fcc.object {
            fcc.called = Some(object.class());
            if is_static {
                fcc.object = None;
            }
        }
        Ok(())
    }
}
