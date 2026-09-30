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

use php_unit::decl::Visibility;

use super::{Request, visibility_string};
use crate::api::abi::Callable;
use crate::api::call::{Function, Target};
use crate::api::values::{ClassId, Key, PhpObject, Value};

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
/// sentence it wrote, or none, where a deprecation's user handler threw
/// and `zend_is_callable_check_class` gave up with the throwable
/// pending, which php's `%s` prints as `(null)`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum Refusal {
    Said(String),
    Unsaid,
}

impl fmt::Display for Refusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Refusal::Said(sentence) => f.write_str(sentence),
            Refusal::Unsaid => f.write_str("(null)"),
        }
    }
}

impl From<String> for Refusal {
    fn from(sentence: String) -> Refusal {
        Refusal::Said(sentence)
    }
}

impl From<&str> for Refusal {
    fn from(sentence: &str) -> Refusal {
        Refusal::Said(sentence.to_owned())
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
                        "class {} does not have a method \"{name}\"",
                        self.class_name(scope),
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
        if let Some(object) = held.as_object().copied() {
            // `case IS_OBJECT`: the `get_closure` handler, a `Closure`'s
            // payload, then `zend_std_get_closure`'s `__invoke`, which
            // reads the class's table and no visibility.
            if object.closure(self).is_some() {
                let name = self.callable_name(&held);
                return Ok(Resolved::Made(Callable::of(
                    held,
                    name.as_bytes(),
                    Target::Closure(object),
                )));
            }
            let class = object.class();
            let Some(function) = self.method_named(class, b"__invoke") else {
                return Err("no array or string given".into());
            };
            let this =
                Some(object).filter(|_| self.method_decl(function).is_none_or(|d| d.is_instance));
            let name = self.callable_name(&held);
            return Ok(Resolved::Made(Callable::of(
                held,
                name.as_bytes(),
                Target::Method {
                    function,
                    scope: class,
                    this,
                },
            )));
        }
        let mut strict = false;
        let method = if let Some(array) = held.as_array().copied() {
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
            if let Some(object) = first.as_object().copied() {
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
                        && (call.this().is_some() || self.method_class(call.function).is_some())
                    {
                        current_this = call.this();
                        walking = false;
                    }
                    continue;
                }
                super::Link::Frame(_, state) => state,
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
                .closure_scope(state.closure)
                .or_else(|| state.function.and_then(|f| self.method_class(f)))
                .or(state.scope);
            let called = state.this().map(|this| this.class()).or(state.called_scope);
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
    fn callable_class(
        &mut self,
        name: &[u8],
        scope: Option<ClassId>,
        frame: &CallFrame,
        fcc: &mut Fcc,
        deprecations: Deprecations,
    ) -> Result<bool, Refusal> {
        let lowered = name.to_ascii_lowercase();
        let (word, resolved): (&str, Result<(ClassId, ClassId, bool), Refusal>) = match lowered
            .as_slice()
        {
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
                .filter(|called| self.instance_of(*called, bound))
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
                String::from_utf8_lossy(name)
            )));
        };
        fcc.calling = Some(class);
        if let Some(scope) = frame.scope
            && fcc.object.is_none()
        {
            match frame.this {
                Some(this)
                    if self.instance_of(this.class(), scope) && self.instance_of(scope, class) =>
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
    /// and php's refusals, each in its words.
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
            let bare = callable.strip_prefix(b"\\").unwrap_or(callable);
            if let Some(function) = self.function_named(bare) {
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
                let quiet = if ce_org.is_some() {
                    Deprecations::Suppressed
                } else {
                    deprecations
                };
                strict = self.callable_class(&class, scope, frame, fcc, quiet)?;
                if let Some(org) = ce_org {
                    let calling = fcc.calling.unwrap_or(org);
                    if !self.instance_of(org, calling) {
                        return Err(Refusal::Said(format!(
                            "class {} is not a subclass of {}",
                            self.class_name(org),
                            self.class_name(calling)
                        )));
                    }
                    if deprecations == Deprecations::Raised {
                        let form = format!(
                            "Callables of the form [\"{}\", \"{}\"] are deprecated",
                            self.class_name(org),
                            String::from_utf8_lossy(callable)
                        );
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
                    String::from_utf8_lossy(callable)
                )));
            }
        };
        let lowered = method.to_ascii_lowercase();
        let Some(calling) = fcc.calling else {
            return Err(Refusal::Said(format!(
                "function {}() does not exist",
                String::from_utf8_lossy(&method)
            )));
        };
        let found = if strict && lowered == b"__construct" {
            self.method_named(calling, b"__construct")
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
                self.class_name(calling),
                String::from_utf8_lossy(&method)
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
        let mut function = self.method_named(calling, lowered)?;
        if !strict
            && let Some(scope) = frame.scope
            && self
                .method_class(function)
                .is_some_and(|class| class != scope && self.instance_of(class, scope))
            && let Some(own) = self.method_named(scope, lowered)
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
            && self.method_named(calling, magic).is_some()
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
        &self,
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
            if strict && self.method_named(org, b"__call").is_some() {
                return Some((trampoline(org, false), true));
            }
            let class = object.class();
            let (callee, via, scope) = if lowered == b"__invoke" && object.closure(self).is_some() {
                (Callee::Invoke(object), true, self.class_named(b"Closure"))
            } else if let Some(function) = self
                .method_named(class, lowered)
                .filter(|function| self.method_accessible_from(*function, frame.scope))
                .or_else(|| self.trampoline_named(class, lowered))
            {
                (
                    Callee::Function(function),
                    self.trampoline_of(function).is_some(),
                    self.method_class(function),
                )
            } else if self.method_named(class, b"__call").is_some() {
                (
                    trampoline(class, false),
                    true,
                    self.method_class(self.method_named(class, b"__call")?),
                )
            } else {
                return None;
            };
            // `zend_release_fcall_info_cache`: under a strict class half
            // the method must be one `ce_org` has.
            if strict && !scope.is_some_and(|scope| self.instance_of(org, scope)) {
                return None;
            }
            return Some((callee, via));
        }
        let callee = if self.method_named(calling, b"__call").is_some()
            && let Some(this) = frame
                .current_this
                .filter(|this| self.instance_of(this.class(), calling))
        {
            trampoline(this.class(), false)
        } else if self.method_named(calling, b"__callstatic").is_some() {
            trampoline(calling, true)
        } else {
            return None;
        };
        if fcc.object.is_none() {
            fcc.object = frame
                .this
                .filter(|this| self.instance_of(this.class(), calling));
        }
        Some((callee, true))
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
            let shown = || {
                format!(
                    "{}::{}",
                    self.class_name(calling),
                    decl.map_or_else(String::new, |decl| {
                        String::from_utf8_lossy(&decl.function.name.0).into_owned()
                    })
                )
            };
            if decl.is_some_and(|decl| decl.is_abstract) {
                return Err(Refusal::Said(format!(
                    "cannot call abstract method {}()",
                    shown()
                )));
            }
            if fcc.object.is_none() && !is_static {
                return Err(Refusal::Said(format!(
                    "non-static method {}() cannot be called statically",
                    shown()
                )));
            }
            let visibility = self.method_visibility(function);
            if visibility != Visibility::Public
                && !self.method_accessible_from(function, frame.scope)
            {
                return Err(Refusal::Said(format!(
                    "cannot access {} method {}()",
                    visibility_string(visibility),
                    shown()
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
