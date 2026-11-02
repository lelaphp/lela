//! What a native reads of the call it serves: its arguments, named
//! and variadic, the caller's variables, file, line and scope, `$this`,
//! whether its return value is used, the active function's name as
//! `get_active_function_name` and php's argument messages give it, and
//! whether the caller was compiled under `declare(strict_types=1)`. The
//! area holds no state of its own: it reads the frames and calls the
//! request's VM holds.

use php_heap::{FrameRef, Slot, Zval};

use crate::api::call::Function;
use crate::api::raising::{Thrown, argument_count_error};
use crate::api::values::{ClassId, Owner, PhpObject, PhpString, TypeTag, Value};
use crate::exec;

use super::{Deprecations, Link, Overwrite, Request, Slots};

/// The request's frames facet (design §4.4): the call stack: the native
/// being served, its arguments, `$this`, scope and caller, the frames
/// below it with their variables and op arrays, backtraces, and a
/// generator's or a fiber's execution. [`Request::frames`] reads it and
/// [`Request::frames_mut`] changes it; either is the whole request, since
/// nearly every operation can serve PHP code.
pub struct Frames<R>(pub(crate) R);

impl Request {
    /// The frames facet, to read.
    #[must_use]
    pub fn frames(&self) -> Frames<&Request> {
        Frames(self)
    }

    /// The frames facet, to change.
    pub fn frames_mut(&mut self) -> Frames<&mut Request> {
        Frames(self)
    }
}

/// What called the native being served ([`Request::native_caller`]):
/// a user function's frame and its arguments, a code frame, or another
/// native.
pub(crate) enum NativeCaller {
    Function(FrameRef, Vec<Value>),
    Code(FrameRef),
    Native,
}

impl Request {
    /// [`Frames::set_caller_var`]'s write into `frame`'s table, rebuilt
    /// first where the frame has none.
    fn set_table_var(&mut self, frame: FrameRef, name: PhpString, value: Value, order: Overwrite) {
        let table = self.rebuild_symbol_table(frame);
        self.symbol_update_ind(table, name, value, order);
    }

    /// `zend_set_local_var(name, value, force)`
    /// (`Z/Zend/zend_execute_API.c:1917-1956`) with
    /// `EG(current_execute_data)` at `frame`: the bucket of its symbol
    /// table where it has one, else the CV of that name, else, with
    /// `force`, the table `zend_rebuild_symbol_table` builds for it; what
    /// stood there released, which php's CV arm leaves to its callers,
    /// whose variable is undefined. The release is in place, before the
    /// write ([`Overwrite::InPlace`]): `zend_hash_update` and
    /// `zend_hash_update_ind` release so, and so does the CV arm of
    /// `zend_set_local_var_str`, `$http_response_header`'s, where
    /// `zend_set_local_var`'s writes without a release. `false`, and the
    /// value given up, where none of them takes it.
    pub(super) fn set_local_var_in(
        &mut self,
        frame: FrameRef,
        name: &[u8],
        value: Owner<Value>,
        force: bool,
    ) -> bool {
        if self
            .frame_at(frame)
            .is_some_and(|state| state.symbol_table().is_some())
        {
            self.set_table_var_named(frame, name, value);
            return true;
        }
        let at = self.frame_var_at(frame, name);
        let slot = |req: &mut Request| req.vm.stack.frame_mut(frame)?.slot_ptr(Slot(at?));
        if slot(self).is_none() {
            if force {
                self.set_table_var_named(frame, name, value);
                return true;
            }
            value.give_up(self);
            return false;
        }
        // php's CV arm: `zval_ptr_dtor(var)` and then `ZVAL_COPY_VALUE`.
        self.overwrite_place(slot, value.hand_on(), Overwrite::InPlace);
        true
    }

    /// [`Request::set_table_var`] over a name the caller holds as bytes,
    /// keyed as `zend_hash_str_update` keys it, `zend_string_init`, a
    /// string the table keeps where it adds the name; the value given up
    /// where the key cannot be made.
    fn set_table_var_named(&mut self, frame: FrameRef, name: &[u8], value: Owner<Value>) {
        match self.string_init(name).map(Owner::adopt) {
            Ok(key) => {
                self.set_table_var(frame, *key, value.hand_on(), Overwrite::InPlace);
                key.give_up(self);
            }
            Err(_) => value.give_up(self),
        }
    }

    /// The call being served read once: how many arguments it was given
    /// and the first `N` of them as [`Frames::argument`] reads each,
    /// `UNDEF` past the count; none given and all `UNDEF` outside a
    /// native's body.
    #[inline]
    pub(crate) fn arguments_given<const N: usize>(&self) -> (usize, [Value; N]) {
        let Some(call) = self.serving_call() else {
            return (0, [Value::undef(); N]);
        };
        let given = call.num_args() as usize;
        let slots = call.frame.slots();
        let arguments = std::array::from_fn(|at| match slots.get(at) {
            Some(zval) if at < given => Value::of_zval(*zval),
            _ => Value::undef(),
        });
        (given, arguments)
    }

    /// `ZEND_NUM_ARGS()` as `Z_PARAM_VARIADIC` (`Z/Zend/zend_API.h:2181`)
    /// reads it, the tail running from its parameter to the count; zero
    /// outside a native's body. A call that carries extra named arguments is
    /// `ZPP_ERROR_UNEXPECTED_EXTRA_NAMED`, php's `ArgumentCountError`
    /// from `zend_unexpected_extra_named_error`
    /// (`Z/Zend/zend_API.c:394`), since a variadic native that reads
    /// them by name reads [`Request::named_arguments`] instead. The
    /// witness of the text stands beside `array_merge` in `ext/standard`.
    ///
    /// The extra named arguments stand in their table on the request's
    /// heap, `EX(extra_named_params)`, across the throw, and a user
    /// function's untyped variadic takes the table as its array.
    ///
    /// ```php
    /// function pk($f) { $f(); $s = memory_get_usage(); memory_reset_peak_usage(); $f(); echo memory_get_peak_usage() - $s, " ", memory_get_usage() - $s, "\n"; }
    /// pk(function () { try { array_merge(...['x' => [1]]); } catch (ArgumentCountError $e) {} });
    /// function f(...$a) { return memory_get_usage(); }
    /// $m = memory_get_usage(); $u = f(x: 1); echo $u - $m, "\n";
    /// $k = []; for ($i = 0; $i < 9; $i++) $k["k$i"] = $i;
    /// pk(function () use ($k) { f(...$k); });
    /// pk(function () { call_user_func('f', x: 1, y: 2); });
    /// ```
    /// ```expect
    /// 3184 32
    /// 376
    /// 1016 0
    /// 752 0
    /// ```
    pub(crate) fn variadic_count(&mut self) -> Result<u32, Thrown> {
        let Some(call) = self.serving_call() else {
            return Ok(0);
        };
        if call.extra_named().is_empty() {
            return Ok(call.num_args());
        }
        let native = self.active_native();
        let message = format!("{native}() does not accept unknown named parameters");
        Err(argument_count_error(self, &message))
    }

    /// `Z_PARAM_VARIADIC_WITH_NAMED`'s named half: the extra named
    /// arguments the call being served carries, `EX(extra_named_params)`
    /// under `ZEND_CALL_HAS_EXTRA_NAMED_PARAMS`, name and value in the
    /// order they were given, each a view the call keeps one owner of.
    /// What `call_user_func` hands on as its own call's named
    /// arguments; empty for a call that carries none.
    #[must_use]
    pub(crate) fn named_arguments(&self) -> Vec<(PhpString, Value)> {
        self.serving_call()
            .map(|call| call.extra_named().to_vec())
            .unwrap_or_default()
    }

    /// The callee of the call being served, its receiver and its called
    /// scope, php's `EX(func)` and `EX(This)` inside a native.
    pub(crate) fn serving_callee(&self) -> Option<(Function, Option<PhpObject>, Option<ClassId>)> {
        self.serving_call()
            .and_then(|call| Some((call.function()?, call.this(), call.called_scope())))
    }

    /// The arguments of the call being served and its extra named ones,
    /// each owner moved out to the caller and the call left with none.
    pub(crate) fn take_serving_arguments(&mut self) -> (Vec<Value>, Vec<(PhpString, Value)>) {
        let Some(call) = self.vm.serving_at() else {
            return (Vec::new(), Vec::new());
        };
        let named = self
            .annex_mut(call)
            .map(|annex| annex.extra_named.take_rows())
            .unwrap_or_default();
        let Some(frame) = self.vm.stack.frame_mut(call) else {
            return (Vec::new(), named);
        };
        let passed = std::mem::take(&mut frame.header_mut().num_args) as usize;
        let arguments = frame
            .slots_mut()
            .iter_mut()
            .take(passed)
            .map(|slot| Value::of_zval(std::mem::replace(slot, Zval::UNDEF)))
            .collect();
        (arguments, named)
    }

    /// The call being served handed to `function` with `arguments`,
    /// each owner the call's from here: what `ZEND_CALL_TRAMPOLINE`
    /// writes into the frame it reuses.
    pub(crate) fn serve_instead(&mut self, function: Function, arguments: Vec<Value>) {
        let Some(call) = self.vm.serving_at() else {
            for argument in arguments {
                self.release(argument);
            }
            return;
        };
        let len = self
            .vm
            .stack
            .frame(call)
            .map_or(0, crate::exec::HeapFrame::len);
        if len < arguments.len() {
            let grown = self
                .vm
                .stack
                .grow_top(&mut self.heap, call, arguments.len() - len);
            if grown.is_err() {
                for argument in arguments {
                    self.release(argument);
                }
                return;
            }
        }
        let callee = self.callee_at(function);
        if let Some(frame) = self.vm.stack.frame_mut(call) {
            frame.header_mut().function = Some(function);
            frame.header_mut().callee = callee;
            frame.header_mut().num_args = u32::try_from(arguments.len()).unwrap_or(u32::MAX);
            frame.header_mut().info.unlay_entry();
            for (slot, argument) in frame.slots_mut().iter_mut().zip(arguments) {
                *slot = argument.to_zval();
            }
        }
    }

    /// `zend_parse_arg_*_weak` writing what it coerced back into the
    /// argument's zval: the call frame gives up what it held at
    /// `position` and owns `value` from here, so the native reads a view
    /// and the frame frees the string with the call. A position past
    /// what the call was given writes nothing. A frameless call's
    /// argument is the opline's operand, which php's `Z_FLF_PARAM_STR`
    /// never writes: it converts a temporary of the body's and frees it
    /// at the body's end, so the request keeps `value` until the entry
    /// returns ([`Request::serve_frameless`]) and a trace taken after the
    /// coercion prints the operand as passed.
    pub(crate) fn replace_argument(&mut self, position: u32, value: Value) {
        let Some(index) = position
            .checked_sub(1)
            .and_then(|at| usize::try_from(at).ok())
        else {
            return;
        };
        let Some(call) = self.vm.serving_at() else {
            if self.frameless_serving().is_some() {
                self.keep_coerced(value);
            }
            return;
        };
        let Some(header) = self.vm.at(call) else {
            return;
        };
        if index >= header.num_args as usize {
            return;
        }
        let Some(previous) = self
            .vm
            .stack
            .frame_mut(call)
            .and_then(|frame| frame.slots_mut().get_mut(index))
            .map(|slot| std::mem::replace(slot, value.to_zval()))
        else {
            return;
        };
        self.release(Value::of_zval(previous));
    }

    /// What called the native being served, `EX(prev_execute_data)` as
    /// `func_get_args` and `get_defined_vars` read it: a user function's
    /// frame, with the arguments it was passed as its slots hold them
    /// now, the extra ones after, `ZEND_CALL_NUM_ARGS` of them in all;
    /// a code frame, the main op array or an included file's, which
    /// php's `ZEND_CALL_CODE` names; or another native, which is a call
    /// `zend_call_function` made and php's `ZEND_CALL_DYNAMIC`, as is a
    /// call whose callee the program named as it served
    /// ([`exec::CallState::dynamic`]).
    ///
    /// ```php
    /// function f() { $g = 'func_get_args'; return $g(); }
    /// function h() { $g = 'func_num_args'; return call_user_func($g); }
    /// function k() { return call_user_func('func_get_args'); }
    /// foreach (['f', 'h', 'k'] as $fn) { try { var_dump($fn(1)); } catch (Error $e) { echo $e->getMessage(), "\n"; } }
    /// ```
    /// ```expect
    /// Cannot call func_get_args() dynamically
    /// Cannot call func_num_args() dynamically
    /// array(1) {
    ///   [0]=>
    ///   int(1)
    /// }
    /// ```
    pub(crate) fn native_caller(&self) -> NativeCaller {
        let mut chain = self.chain();
        if let Some(Link::Call(served)) = chain.next()
            && served.info().dynamic()
        {
            return NativeCaller::Native;
        }
        match chain.next() {
            // A frame on a stack below the running one is a fiber's
            // resumer: a native that is a fiber's own callable was called
            // from the fiber's bottom frame, which is `zend_fiber_function`,
            // a native's, so it answers as a native caller below.
            Some(Link::Frame(Slots::Stack(vm, place), state))
                if std::ptr::eq(vm, &raw const self.vm) && state.function().is_some() =>
            {
                let frame = FrameRef(u32::try_from(place).unwrap_or(u32::MAX));
                NativeCaller::Function(frame, self.arguments_of(frame, state))
            }
            Some(Link::Frame(Slots::Stack(vm, place), _))
                if std::ptr::eq(vm, &raw const self.vm) =>
            {
                NativeCaller::Code(FrameRef(u32::try_from(place).unwrap_or(u32::MAX)))
            }
            Some(Link::Frame(..) | Link::Call(_) | Link::Site(_) | Link::Fake(_)) | None => {
                NativeCaller::Native
            }
        }
    }

    /// The arguments the user function on `frame` was passed, as its
    /// slots hold them now, the extra ones after, `ZEND_CALL_NUM_ARGS`
    /// of them in all: what `ZEND_FUNC_GET_ARGS` copies, views with no
    /// owner taken.
    pub(crate) fn frame_arguments(&self, frame: FrameRef) -> Vec<Value> {
        self.frame_at(frame)
            .map(|state| self.arguments_of(frame, state))
            .unwrap_or_default()
    }

    fn arguments_of(&self, frame: FrameRef, state: exec::FrameAt<'_>) -> Vec<Value> {
        let passed = usize::try_from(state.num_args()).unwrap_or(0);
        let extra = state.extra_args();
        let mut arguments: Vec<Value> = (0..passed.saturating_sub(extra.len()))
            .map(|slot| self.slot(frame, u32::try_from(slot).unwrap_or(u32::MAX)))
            .collect();
        arguments.extend(extra.iter().copied());
        arguments
    }

    /// The `Error` [`Frames::forbid_dynamic_call`] throws, named for the
    /// native being served.
    pub(crate) fn dynamic_call(&mut self) -> Thrown {
        let name = self.frames().active_function_name();
        crate::api::raising::error(self, &format!("Cannot call {name}() dynamically"))
    }

    /// `zend_active_function()`: the native being served, which is the
    /// call frame on top of [`Vm`] while an entry runs, and none while
    /// the loop is between calls or a frame stands above it, a file the
    /// native serves on the loop (`zend_execute`) or a function it calls
    /// back, as `EG(current_execute_data)` is then that frame. The
    /// raisers render its declared name and its parameters' names off
    /// the declaration, never off the caller's spelling.
    pub(crate) fn active_native_function(&self) -> Option<Function> {
        match self.serving_call() {
            Some(call) => call.function(),
            None => self.frameless_serving().map(|(function, _)| function),
        }
    }

    /// `zend_active_function_ex`'s frameless half
    /// (`Z/Zend/zend_execute_API.c:605`): where `EG(current_execute_data)`
    /// is user code standing at a `FRAMELESS_ICALL_*`, the native the
    /// engine's frameless table holds at its `extended_value`, and the
    /// call as the opline reads ([`exec::FrameAt::frameless_call`]). A
    /// frameless call pushes no record, so this is what the raisers'
    /// function name, the arguments a native reads back and whether its
    /// return is read come from while one is served. Its regime is the
    /// frame's own, `ZEND_FLF_ARG_USES_STRICT_TYPES()`, which is
    /// [`Request::strict_types`]'s innermost frame where no call is
    /// served, and the trace a throw out of it takes holds the row php
    /// adds for the call:
    ///
    /// ```php
    /// declare(strict_types=1);
    /// function f($p) { return preg_match($p, "abc"); }
    /// var_dump(f("abc"));
    /// function g($n) { return str_contains($n, "1"); }
    /// try { var_dump(g(12)); } catch (TypeError $e) { echo $e->getMessage(), "\n", $e->getTraceAsString(), "\n"; }
    /// ```
    /// ```expect
    /// Warning: preg_match(): Delimiter must not be alphanumeric, backslash, or NUL byte in %s on line 3
    /// bool(false)
    /// str_contains(): Argument #1 ($haystack) must be of type string, int given
    /// #0 %s(5): str_contains(12, '1')
    /// #1 %s(6): g(12)
    /// #2 {main}
    /// ```
    pub(crate) fn frameless_serving(&self) -> Option<(Function, exec::FramelessCall)> {
        let state = self.frame_at(self.vm.current_at()?)?;
        if !state.info().user() {
            return None;
        }
        let call = state.frameless_call()?;
        let row = self.shared.frameless.get(call.row as usize)?;
        Some((Function(row.function), call))
    }

    /// Whether a native is being served, [`Request::active_native_function`]
    /// standing, which the raisers read before they ask
    /// [`Request::active_native`] for the name `php_verror` puts before
    /// `(): `.
    pub(crate) fn vm_serving_native(&self) -> bool {
        self.active_native_function().is_some()
    }

    /// Whether the native being served is a method, whose name
    /// `get_active_function_or_method_name` makes as a member string
    /// (`zend_create_member_string`) where a function's is its own.
    pub(crate) fn active_native_is_method(&self) -> bool {
        self.active_native_function()
            .is_some_and(|function| self.method_class(function).is_some())
    }

    /// The name `php_verror` puts before `(): `: the construct of an
    /// include being opened, or the native being served. A warning an
    /// include's open raises with a throwable pending names the frame's
    /// function instead, as a filter that could not be made does:
    ///
    /// ```php
    /// class F extends php_user_filter { private $d = \NOPE; }
    /// stream_filter_register("f", F::class);
    /// function g() { include "php://filter/read=f/resource=" . __FILE__; }
    /// try { g(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// Warning: g(): Unable to create or locate filter "f" in %s on line 4
    ///
    /// Warning: g(): Unable to create filter (f) in %s on line 4
    /// Undefined constant "NOPE"
    /// ```
    pub(crate) fn active_native(&self) -> String {
        if let Some(kind) = self.include_being_opened() {
            // php reads the construct off the frame's opline, which a
            // throw has moved to the exception op, so a warning raised
            // with a throwable pending names the frame's function.
            if self.stream_thrown.is_some() || self.is_pending() {
                return self
                    .active_user_function()
                    .unwrap_or_else(|| String::from("main"));
            }
            return String::from(kind.name());
        }
        self.active_native_function().map_or_else(
            || String::from("native"),
            |function| String::from_utf8_lossy(&self.qualified_name(function, None)).into_owned(),
        )
    }

    /// `get_active_function_name` over the innermost user frame, with
    /// the class `get_active_class_name` puts before a method's: `main`
    /// for a file's own code, none while no frame is served. What
    /// `php_verror` names a docref by when no native raises it, as an
    /// object's handlers do from the code that reached them.
    pub(crate) fn active_user_function(&self) -> Option<String> {
        let frame = self.innermost_frame()?;
        Some(frame.function().map_or_else(
            || String::from("main"),
            |function| String::from_utf8_lossy(&self.qualified_name(function, None)).into_owned(),
        ))
    }

    /// `func->common.function_name`: the name a function was declared
    /// under, and `native` for one the table has no declaration for. A
    /// method's is its own name alone, what `get_active_function_name`
    /// answers; the class before it is [`Request::qualified_name`]'s.
    pub(crate) fn declared_name(&self, function: Function) -> Vec<u8> {
        self.symbols()
            .function_decl(function)
            .map_or_else(|| b"native".to_vec(), |decl| decl.name.0.to_vec())
    }

    /// `get_function_or_method_name(EX(func))` over the function `frame`
    /// serves: its declared name after its scope's, a closure's scope
    /// being the one it was made or bound in, which its frame reads
    /// ([`Request::frame_scope`]); empty for a frame that serves none.
    ///
    /// ```php
    /// class C { function m() { return function (int $a): array { return []; }; } }
    /// $f = (new C)->m();
    /// try { $f("x"); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// C::{closure:C::m():2}(): Argument #1 ($a) must be of type int, string given, called in %s on line 4
    /// ```
    pub(crate) fn frame_function_name(&self, frame: FrameRef) -> Vec<u8> {
        let Some(state) = self.vm.at(frame) else {
            return Vec::new();
        };
        let Some(function) = state.function else {
            return Vec::new();
        };
        if state.closure().is_none() {
            return self.qualified_name_of(function);
        }
        let name = self.declared_name(function);
        match self.frame_scope(frame) {
            Some(scope) => bytes!(self.symbols().class_name(scope), b"::", name),
            None => name,
        }
    }

    /// The name a raise or a trace prints a callee under: a function's
    /// declared name, and `Class::method` for a method, as
    /// `zend_wrong_parameters_count_error` and `zend_trace_to_string`
    /// join `func->common.scope->name` to it; a trace joins with `->`
    /// where the call had a receiver.
    pub(super) fn qualified_name(&self, function: Function, this: Option<PhpObject>) -> Vec<u8> {
        let name = self.declared_name(function);
        match self.method_class(function) {
            Some(class) => {
                let join: &[u8] = if this.is_some() { b"->" } else { b"::" };
                bytes!(self.symbols().class_name(class), join, name)
            }
            None => name,
        }
    }

    /// `arg_info->type` of the active native's parameter at `position`,
    /// what `zend_null_arg_deprecated` names the type by and what
    /// [`crate::api::coerce::argument`] holds the argument to: the
    /// variadic's own past the declared count, as php's `arg_offset`
    /// stops at `num_args` there; none where the native declares no
    /// type there.
    pub(crate) fn active_parameter_type(&self, position: u32) -> Option<php_unit::decl::Type> {
        let decl = self
            .symbols()
            .function_decl(self.active_native_function()?)?;
        let at = usize::try_from(position.checked_sub(1)?).ok()?;
        let parameter = match decl.parameters.get(at) {
            Some(parameter) => parameter,
            None => decl.parameters.last().filter(|last| last.variadic)?,
        };
        parameter.ty.clone()
    }

    /// The class the parameter at `position` of the native being served
    /// holds its argument to, where its type names one class, resolved
    /// as the engine started ([`crate::engine::Shared::parameter_classes`]);
    /// none for every other parameter and past the declared ones.
    pub(crate) fn active_parameter_class(&self, position: u32) -> Option<ClassId> {
        let function = self.active_native_function()?;
        let classes = self
            .shared
            .parameter_classes
            .get(usize::try_from(function.0).ok()?)?;
        *classes.get(usize::try_from(position.checked_sub(1)?).ok()?)?
    }

    /// `func->common.scope` and `Z_CE(EX(This))` of the native being
    /// served: the class its method is declared on and the class it was
    /// called on, what a parameter typed `self`, `parent` or `static`
    /// resolves against; none for a function.
    pub(crate) fn active_native_scopes(&self) -> crate::api::coerce::Scopes {
        let own = self
            .active_native_function()
            .and_then(|function| self.method_class(function));
        let called = self.serving_call().and_then(exec::FrameAt::called_scope);
        crate::api::coerce::Scopes {
            own,
            called: called.or(own),
            callables: Deprecations::Suppressed,
        }
    }

    /// The `($name)` `zend_argument_error_variadic` and
    /// `zend_param_must_be_nullable` write after `#N`, with its leading
    /// space, and nothing where the parameter has no name.
    fn argument_name_suffix(&self, position: u32) -> String {
        crate::api::raising::arg_name_suffix(self, self.active_native_function(), position)
    }

    /// `zend_argument_error_variadic`'s message:
    /// `{fn}(): Argument #N ($name) must {tail}`, as bytes, so a class
    /// name the tail prints stands as it was declared (#3428).
    pub(crate) fn argument_message(&self, position: u32, tail: &[u8]) -> Vec<u8> {
        bytes!(
            self.active_native(),
            "(): Argument #",
            position.to_string(),
            self.argument_name_suffix(position),
            " must ",
            tail
        )
    }

    /// `zend_param_must_be_nullable`'s message:
    /// `{fn}(): Passing null to parameter #N ($name) of type {type} is
    /// deprecated`, one sentence, built as php builds it.
    pub(crate) fn null_parameter_message(&self, position: u32, type_name: &str) -> String {
        format!(
            "{}(): Passing null to parameter #{position}{} of type {type_name} is deprecated",
            self.active_native(),
            self.argument_name_suffix(position)
        )
    }

    /// `ZEND_ARG_USES_STRICT_TYPES()` (`Z/Zend/zend_compile.h:723`) for
    /// the native being served, and `EX_USES_STRICT_TYPES()` of the
    /// innermost frame where none is: whether its caller was compiled
    /// under `declare(strict_types=1)`, which is what a native's own
    /// argument parsing reads. A native holds no frame of this engine's,
    /// so its `prev_execute_data` is the innermost frame, unless another
    /// native is being served above that frame, which is a call
    /// `zend_call_function` made: php's caller there is that native's
    /// frame, which carries no `ZEND_ACC_STRICT_TYPES`, so a native a
    /// callback names coerces its arguments though the file that named
    /// it is strict.
    ///
    /// ```php
    /// declare(strict_types=1);
    /// var_dump(array_map('strlen', [12]), array_map('str_repeat', ['a'], ['2']));
    /// try { str_repeat(12, 2); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// array(1) {
    ///   [0]=>
    ///   int(2)
    /// }
    /// array(1) {
    ///   [0]=>
    ///   string(2) "aa"
    /// }
    /// str_repeat(): Argument #1 ($string) must be of type string, int given
    /// ```
    ///
    /// `call_user_func` in a namespace is such a native too, since php
    /// compiles it to `INIT_USER_CALL` only where it resolves the name at
    /// compile time (`zend_compile_func_cuf`), so its callee coerces.
    ///
    /// ```php
    /// declare(strict_types=1);
    /// namespace Foo;
    /// var_dump(call_user_func('strlen', false));
    /// ```
    /// ```expect
    /// int(0)
    /// ```
    pub(crate) fn strict_types(&self) -> bool {
        let Some(serving) = self.vm.serving_at() else {
            return self
                .innermost_frame_on_running()
                .is_some_and(exec::FrameAt::strict_types);
        };
        // A native called from a site standing between it and the record
        // below it on the chain is called from the site.
        if let Some(site) = self.site_below(serving) {
            return site.strict;
        }
        // The record below: a user frame's file, and a native's own
        // frame, which carries no `ZEND_ACC_STRICT_TYPES`.
        self.vm
            .below_record(serving.0 as usize)
            .and_then(|at| self.frame_on(&self.vm, at))
            .is_some_and(|below| below.info().user() && below.strict_types())
    }

    /// [`Request::strict_types`] over one frame rather than the
    /// innermost: the regime a write standing in `frame` verifies under,
    /// which is what every opcode's handler asks, since a handler holds
    /// its own frame and the innermost is only the same frame by luck.
    pub(crate) fn frame_strict_types(&self, frame: FrameRef) -> bool {
        self.frame_at(frame)
            .is_some_and(exec::FrameAt::strict_types)
    }

    /// `property_uses_strict_types()`
    /// (`Z/Zend/zend_object_handlers.c:1022`): `ZEND_ACC_STRICT_TYPES` on
    /// `EG(current_execute_data)`, the regime `zend_std_write_property`
    /// holds a typed property to. While a native is being served above
    /// the innermost frame, php's current frame is the native's own,
    /// which carries no such bit, so the weak rule; otherwise the
    /// innermost frame's own file, where the write a handler table makes
    /// on an opcode's behalf stands.
    pub(crate) fn property_strict_types(&self) -> bool {
        self.current_user_frame()
            .and_then(|frame| self.frame_at(frame))
            .is_some_and(exec::FrameAt::strict_types)
    }

    /// The innermost frame of the running stack read where it stands.
    fn innermost_frame_on_running(&self) -> Option<exec::FrameAt<'_>> {
        self.frame_on(&self.vm, self.vm.innermost_at()?)
    }

    /// `EG(current_execute_data)` where it is user code: the innermost
    /// frame, and nothing while a native is being served above it or
    /// no frame stands.
    pub(crate) fn current_user_frame(&self) -> Option<FrameRef> {
        let at = self.vm.current_at()?;
        self.vm
            .at(at)
            .is_some_and(|header| header.info.user())
            .then_some(at)
    }

    /// `ZEND_ARG_USES_STRICT_TYPES()` (`Z/Zend/zend_compile.h:723`):
    /// `ZEND_ACC_STRICT_TYPES` on the op array the call into `frame` was
    /// made from, which is what an argument's type check reads and never
    /// the entered op array's own. False where the frame has no caller,
    /// which is the frame `serve` pushed and one a native re-entered the
    /// loop on: php's there is the native's own frame, and no native
    /// this engine declares carries the bit.
    pub(crate) fn caller_strict_types(&self, frame: FrameRef) -> bool {
        let Some(state) = self.vm.at(frame) else {
            return false;
        };
        match state.caller().frame() {
            Some(caller) => self.frame_strict_types(caller),
            None => self.site_below(frame).is_some_and(|site| site.strict),
        }
    }

    /// The site `frame`, a user frame or a native's call, was called
    /// from: one standing where it was pushed, with no record of php's
    /// chain, a user frame or a native's call served, between the two.
    pub(crate) fn site_below(&self, frame: FrameRef) -> Option<&exec::Site> {
        let at = usize::try_from(frame.0).ok()?;
        let below = self.vm.below_record(at);
        self.vm
            .sites()
            .iter()
            .rev()
            .find(|site| site.frame <= at && below.is_none_or(|below| site.frame > below))
    }

    /// `fn_flags & ZEND_ACC_STRICT_TYPES` over a user function: whether the
    /// file it was declared in is `strict_types`. A native carries no such
    /// bit.
    pub(crate) fn function_strict_types(&self, function: Function) -> bool {
        self.user_function(function).is_some_and(|user| {
            crate::exec::body_of(&user.unit, user.body).is_some_and(|code| code.strict_types)
        })
    }
}

impl<'r> Frames<&'r Request> {
    /// The file of the frame that called the native,
    /// `zend_get_executed_filename`: the scope's file while a constant
    /// expression is evaluated ([`site_override`]), and an attribute's
    /// while its object is made and nothing stands above its site
    /// ([`exec::Site`]). Where no frame of user code stands, the file the
    /// request was described with, `script_name`.
    #[must_use]
    pub fn current_file(self) -> Option<&'r [u8]> {
        let script = self.0.description.script_name.as_deref();
        self.executed_file().or(script)
    }

    /// `zend_get_executed_filename_ex`: the file [`Self::current_file`]
    /// names, and `None` where no frame of user code stands, a native a
    /// shutdown function or an extension's request end calls with
    /// nothing beneath it, which a diagnostic names as `Unknown`.
    pub(crate) fn executed_file(self) -> Option<&'r [u8]> {
        if let Some((file, _)) = self.0.overridden_site() {
            return Some(file);
        }
        if let Some(site) = self.0.vm.innermost_site() {
            return Some(&site.file);
        }
        self.0.innermost_frame().and_then(exec::FrameAt::file)
    }

    /// `zend_get_executed_lineno`: the line the innermost frame stands
    /// on, which is the line a diagnostic and a backtrace name.
    #[must_use]
    ///
    /// The line of the opline the innermost frame stands at, zero while
    /// no frame is being served; inside a fiber whose callable is a
    /// native, the resumer's frame's; and the node's own while a
    /// constant expression is evaluated ([`site_override`]).
    pub fn current_line(self) -> u32 {
        if let Some((_, line)) = self.0.overridden_site() {
            return line;
        }
        if let Some(site) = self.0.vm.innermost_site() {
            return site.line;
        }
        self.0.innermost_frame().map_or(0, exec::FrameAt::line)
    }

    /// `USED_RET()` (`Z/Zend/zend_API.h`): whether the caller reads what
    /// the native returns, false only for a call written as a statement,
    /// whose `DO_*CALL` writes its return to an `UNUSED` operand; a call
    /// made by a native, `call_user_func`'s, reads it. `array_splice`
    /// builds the array of what it removed only where it is read, which
    /// decides when the removed elements' destructors are served.
    #[must_use]
    pub fn return_value_used(self) -> bool {
        match self.0.serving_call() {
            Some(call) => call.info().return_used(),
            None => self.0.frameless_serving().is_none_or(|(_, call)| call.used),
        }
    }

    /// `ZEND_NUM_ARGS()`: how many arguments the call being served was
    /// given, and zero outside a native's body.
    #[must_use]
    pub fn argument_count(self) -> usize {
        match self.0.serving_call() {
            Some(call) => call.num_args() as usize,
            None => self.0.frameless_serving().map_or(0, |(_, call)| call.arity),
        }
    }

    /// `ZEND_CALL_ARG(execute_data, position)`: the argument at
    /// `position`, one-based as php counts them and as `Argument #N`
    /// names them, read off the call frame the native is being served
    /// from; none past what the call was given. What is read is the
    /// value as passed, a reference not followed, as `Z_PARAM_*` reads
    /// its slot before coercing it.
    #[must_use]
    pub fn argument(self, position: u32) -> Option<Value> {
        let index = position.checked_sub(1)?;
        let Some(call) = self.0.serving_call() else {
            let (_, call) = self.0.frameless_serving()?;
            return call.arguments().get(index as usize).copied();
        };
        (index < call.num_args()).then(|| Value::of_zval(call.frame.slot(Slot(index))))
    }

    /// `zend_get_executed_scope`, the class scope where the native being
    /// served was called: a native method's own class
    /// (`func->common.scope`), and for a native function the scope of
    /// the user code that called it, a method's class or a closure's
    /// bound scope; none under a plain function or the main op array.
    #[must_use]
    pub fn scope(self) -> Option<ClassId> {
        self.0.executed_scope()
    }

    /// `zend_get_called_scope` (`Z/Zend/zend_execute_API.c:727`), the
    /// called scope of the native being served, `static`: the chain
    /// walked from `EG(current_execute_data)` to the first link with a
    /// receiver, whose class it is, or a called scope of its own; a
    /// user link or a native method without one ends the walk with
    /// none, and a native function without one, this native's own
    /// call, is walked past. So `static` inside a native is the
    /// calling method's, as `constant("static::X")` reads it.
    #[must_use]
    pub fn called_scope(self) -> Option<ClassId> {
        for link in self.0.chain() {
            let (function, this, called) = match link {
                Link::Frame(_, state) => (state.function(), state.this(), state.called_scope()),
                Link::Call(call) => (call.function(), call.this(), call.called_scope()),
                Link::Fake(_) => continue,
                Link::Site(_) => return None,
            };
            if let Some(this) = this {
                return Some(this.class());
            }
            if called.is_some() {
                return called;
            }
            let function = function?;
            if self.0.user_function(function).is_some() || self.0.method_class(function).is_some() {
                return None;
            }
        }
        None
    }

    /// `ZEND_THIS`, `getThis()`: the receiver of the method being
    /// served, a name the frame holds an owner of while the call
    /// stands; none for a call on the class, a function, or outside a
    /// native's body.
    #[must_use]
    pub fn this(self) -> Option<PhpObject> {
        if let Some(call) = self.0.serving_call() {
            return call.this();
        }
        self.0.vm.innermost()?.this()
    }

    /// `EX(func)->internal_function.reserved[]`: the value the native
    /// trampoline being served was made with
    /// ([`Symbols::native_trampoline_reserving`]), where it is a `T`;
    /// none in any other native, or outside a native's body.
    ///
    /// [`Symbols::native_trampoline_reserving`]: crate::api::request::Symbols::native_trampoline_reserving
    #[must_use]
    pub fn reserved<T: std::any::Any + Copy>(self) -> Option<T> {
        self.0.active_reserved()
    }

    /// `zend_get_this_object(EG(current_execute_data))`
    /// (`Z/Zend/zend_execute_API.c:1317`): the receiver of the first link
    /// of the chain that has one, walked past the natives that have no
    /// class, and none where a user frame or a native method without one
    /// comes first. So `compact('this')` inside a method, or a closure
    /// bound to an object, finds the object its caller runs on.
    ///
    /// ```php
    /// class A { function m() { return compact('this'); } function c() { return (fn() => compact('this'))(); } }
    /// $a = new A; var_dump($a->m()['this'] === $a, $a->c()['this'] === $a, compact('this'));
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(true)
    /// array(0) {
    /// }
    /// ```
    #[must_use]
    pub fn this_object(self) -> Option<PhpObject> {
        for link in self.0.chain() {
            let (function, this) = match link {
                Link::Frame(_, state) => (state.function(), state.this()),
                Link::Call(call) => (call.function(), call.this()),
                Link::Fake(_) => continue,
                Link::Site(_) => return None,
            };
            if this.is_some() {
                return this;
            }
            let function = function?;
            if self.0.user_function(function).is_some() || self.0.method_class(function).is_some() {
                return None;
            }
        }
        None
    }

    /// `get_function_or_method_name(EX(prev_execute_data)->func)`: the
    /// function the native being served was called from, as a message
    /// names it, `Class::method` for a method and a native's name for a
    /// native's call, and `main` for a file's own code. What the test
    /// extension's `zend_get_current_func_name` answers, the witness
    /// there.
    #[must_use]
    pub fn caller_function_name(self) -> String {
        let mut chain = self.0.chain();
        let _served = chain.next();
        let function = match chain.next() {
            Some(Link::Frame(_, state)) => state.function(),
            Some(Link::Call(call)) => call.function(),
            Some(Link::Site(_) | Link::Fake(_)) | None => None,
        };
        function.map_or_else(
            || String::from("main"),
            |function| String::from_utf8_lossy(&self.0.qualified_name(function, None)).into_owned(),
        )
    }

    /// `get_active_function_or_method_name`: the native being served as
    /// a message names it, `Class::method` for a method, which is what
    /// a native writes before `(): ` in a sentence of its own that no
    /// raiser here words, as `pdo_sqlite`'s callback refusals do. Where
    /// no native is served, as when a handler runs for an opcode, it is
    /// the innermost user frame's function, `main` for a file's own code,
    /// as `EG(current_execute_data)->func` is: ext/intl's
    /// `ResourceBundle` words a missing element read as `$b['x']` so.
    #[must_use]
    pub fn active_function_name(self) -> String {
        if self.0.vm_serving_native() || self.0.include_being_opened().is_some() {
            return self.0.active_native();
        }
        self.0
            .active_user_function()
            .unwrap_or_else(|| self.0.active_native())
    }
}

impl Frames<&mut Request> {
    /// `zend_hash_find_ind(zend_rebuild_symbol_table(), name)`: the
    /// variable `name` of the innermost user frame, the one that called
    /// the native, as it stands, a reference not followed; none where the
    /// frame has no such variable or it holds nothing. The frame's symbol
    /// table is rebuilt over its CVs first where it has none
    /// ([`Request::rebuild_symbol_table`]), as `extract` and `compact`
    /// rebuild it in php. A view: no owner taken.
    #[must_use]
    pub fn caller_var(self, name: &[u8]) -> Option<Value> {
        let frame = self.0.innermost_user_frame()?;
        let table = self.0.rebuild_symbol_table(frame);
        let value = self.0.symbol_value(table, name)?;
        (value.tag() != TypeTag::Undef).then_some(value)
    }

    /// The variable `name` of the innermost user frame written with
    /// `value`: a CV's slot where the table points the name at one, the
    /// bucket otherwise, a name the table has not added to it; what stood
    /// there before is given up in `order`, the order of the php function
    /// the native copies: `extract` writes a value as
    /// `ZEND_TRY_ASSIGN_COPY_EX` does, [`Overwrite::Nulled`], and a
    /// reference as `php_extract_ref_overwrite` does,
    /// [`Overwrite::After`]. A write through a reference the variable
    /// holds is the native's to make. A sink: the variable takes the
    /// value's owner, given up where no user frame is running. `name` is
    /// the caller's string, `extract`'s the array's key or the prefixed
    /// name it made, of which a table that adds the name takes an owner,
    /// as php's `zend_hash_update_ind` takes one of its key.
    pub fn set_caller_var(self, name: PhpString, value: Owner<Value>, order: Overwrite) {
        let Some(frame) = self.0.innermost_user_frame() else {
            value.give_up(self.0);
            return;
        };
        self.0.set_table_var(frame, name, value.hand_on(), order);
    }

    /// `zend_set_local_var_str(name, value, 0)`
    /// (`Z/Zend/zend_execute_API.c:1960`), what the http wrapper leaves
    /// `$http_response_header` with: the variable `name` of the innermost
    /// user frame written with `value` where the frame has a symbol table,
    /// as the main op array's frame always has, or where `name` is one of
    /// its CVs, what it held released; `false`, and the value given up,
    /// where neither is so or no user frame is running, since without
    /// `force` php adds no table to a frame that has none. A sink: the
    /// variable takes the value's owner.
    pub fn set_local_var(self, name: &[u8], value: Owner<Value>) -> bool {
        let Some(frame) = self.0.innermost_user_frame() else {
            value.give_up(self.0);
            return false;
        };
        self.0.set_local_var_in(frame, name, value, false)
    }

    /// `zend_forbid_dynamic_call()` (`Z/Zend/zend_API.h:901`): php's
    /// `Error` "Cannot call `name()` dynamically" where the native being
    /// served was called dynamically, `ZEND_CALL_DYNAMIC`, which is where
    /// [`Request::native_caller`] answers another native: a callee the
    /// program named as it served (`$f()`), or one `zend_call_function`
    /// served, a user buffer handler's among them. What reads or writes the
    /// caller's variables, `compact` and `extract`, refuses such a call.
    ///
    /// ```php
    /// function f() { $a = 1; $g = 'compact'; try { $g('a'); } catch (Error $e) { echo $e->getMessage(), "\n"; } var_dump(call_user_func('compact', 'a')); }
    /// f();
    /// ob_start('compact'); try { ob_end_clean(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// Cannot call compact() dynamically
    /// array(1) {
    ///   ["a"]=>
    ///   int(1)
    /// }
    /// Cannot call compact() dynamically
    /// ```
    pub fn forbid_dynamic_call(self) -> Result<(), Thrown> {
        match self.0.native_caller() {
            NativeCaller::Native => Err(self.0.dynamic_call()),
            NativeCaller::Function(..) | NativeCaller::Code(_) => Ok(()),
        }
    }
}
