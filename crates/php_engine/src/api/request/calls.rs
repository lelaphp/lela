//! How a request serves a call: a user function, a method, a closure
//! or a native, through `zend_call_function`'s checks and the frame it
//! pushes, by reference where the callee takes one, through
//! `__call`'s and `__callStatic`'s trampolines and the trampolines a
//! native class makes, with the natives' evaluated defaults and the
//! functions a constant expression's `name(...)` found. The
//! trampolines, the defaults, those functions and the natives entered
//! under the witness runner are [`Calls`]'s, whose fields no file but
//! this one reads.

use std::sync::atomic::Ordering;

use crate::api::call::{ClassNative, Function};
use crate::api::raising::Thrown;
use crate::api::values::{
    ClassId, ClosureInfo, Owner, PhpObject, PhpRef, PhpString, Value, ValueRef,
};
use crate::engine::{MethodAt, NativeMethod};
use crate::exec;
use crate::observer::OnThrow;
use crate::trampoline::CallTrampoline;
use php_heap::FrameRef;
use php_unit::decl::HookKind;
use php_unit::{Opcode, OperandKind};

use super::Code;
use super::Symbols;
use super::{ExtensionId, Link, Request, UserFunction};

/// The trampolines, the natives' defaults, the functions `name(...)`
/// found and the natives entered, whose fields no file but this one
/// reads.
#[derive(Default)]
pub(super) struct Calls {
    /// The functions `zend_get_call_trampoline_func` makes of a call to
    /// a name only `__call` or `__callStatic` answers, one per magic
    /// method and name, two function numbers each
    /// ([`crate::trampoline`]).
    trampolines: Vec<crate::trampoline::CallTrampoline>,
    /// The evaluated defaults of natives' absent arguments, each kept by
    /// the request under its native and position (design §4.7).
    defaults: Vec<((Function, u32), Value)>,
    /// The function each `name(...)` of a constant expression found the
    /// first time this request evaluated it, php's `zend_ast_fcc.fptr`
    /// map pointer, by the node's unit and number
    /// ([`php_unit::decl::ConstExpr::FunctionCallable`]); the cell held
    /// here keeps the key's identity while the entry stands.
    function_callables: Vec<(php_unit::UnitCell, u32, Function)>,
    /// The natives the request has entered, in order, where the witness
    /// runner asked for them (harness §4.5 condition 2, revision 58);
    /// `None` records nothing, and is what a request holds unless its
    /// engine or its test asked (#2168).
    trace: Option<Vec<crate::trace::Entered>>,
}

/// What a call carries beyond its function and arguments, never two of
/// them at once: the closure it serves (`ZEND_CALL_CLOSURE`), or the
/// property a hook trampoline reads or writes, of the hook of `kind`
/// (`zend_get_property_hook_trampoline`'s `reserved[0]`).
#[derive(Clone, Copy)]
enum Carried<'a> {
    Nothing,
    Closure(PhpObject),
    HookTrampoline(&'a [u8], HookKind),
}

/// The function a call serves, the object it runs on and the class
/// `static` names in it: `zend_fcall_info_cache`'s `function_handler`,
/// `object` and `called_scope`.
#[derive(Clone, Copy)]
pub(crate) struct Callee {
    function: Function,
    this: Option<PhpObject>,
    called_scope: Option<ClassId>,
}

impl Callee {
    /// A method on `this`, or on none, with `called_scope` the class
    /// `static` names in it.
    pub(crate) const fn method(
        function: Function,
        this: Option<PhpObject>,
        called_scope: ClassId,
    ) -> Callee {
        Callee {
            function,
            this,
            called_scope: Some(called_scope),
        }
    }
}

/// Which of `zend_call_function`'s two families of callers a call is
/// made for. Once the memory limit was met php refuses every call
/// (`ZEND_HAS_OOM()`, `Z/Zend/zend_execute_API.c:838` at 8.5.12), and the
/// two families answer that refusal differently ([`Request::serve_call`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Family {
    /// `zend_call_function`'s own callers (`array_map`, `usort`, an error
    /// handler, an `ob_start` callback): `FAILURE` with
    /// `retval` left `UNDEF`, which they go on past with no value.
    Fcall,
    /// `zend_call_known_function` and what goes through it,
    /// `zend_call_method`, `zend_call_known_instance_method*` and
    /// `zend_call_known_fcc`: the magic methods, a property's hooks,
    /// constructors, `Closure::__invoke`, a lazy object's initializer, a
    /// user stream wrapper's methods (`zend_call_method_if_exists`) and
    /// what `register_tick_function` and `register_shutdown_function`
    /// named. Where no throwable or exit is pending, the refusal is `E_CORE_ERROR` `Couldn't execute method`
    /// (`Z/Zend/zend_execute_API.c:1136-1143`).
    Known,
}

/// Where a user callee's return goes when its body returned and then
/// threw, from a local's destructor at its end: php's `retval` pointer,
/// which `zend_call_function` writes and its caller frees.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Retval {
    /// A buffer of the caller's own, `zval retval` on a native's stack,
    /// which the native gives up as it meets `EG(exception)`: given up
    /// with the `Err`.
    Native,
    /// The `result` temporary of the opline the calling frame stands at,
    /// `zend_std_read_property`'s `rv`, which is `FETCH_OBJ_R`'s and
    /// `FETCH_OBJ_IS`'s `EX_VAR(opline->result.var)`: written there, for
    /// `ZEND_HANDLE_EXCEPTION` to free after the opline's operands
    /// (`free_throwing_result`).
    Result,
}

impl Request {
    /// `zend_call_function` over a native method: the call frame pushed
    /// as `INIT_METHOD_CALL` and the `SEND_*` oplines would push it, the
    /// receiver and each argument one more owner the frame holds, the
    /// entry served, and the frame popped whether it returned or threw.
    /// `extra_named` is what `zend_handle_named_arg` collected for a
    /// variadic callee, carried on the call as
    /// `ZEND_CALL_HAS_EXTRA_NAMED_PARAMS`. What it makes is one owner
    /// not yet named.
    pub(crate) fn serve_method_made(
        &mut self,
        function: Function,
        this: Option<PhpObject>,
        called_scope: ClassId,
        arguments: &[Value],
        extra_named: &[(PhpString, Value)],
    ) -> Result<Value, Thrown> {
        self.serve_method_into(
            Retval::Native,
            Family::Fcall,
            Callee::method(function, this, called_scope),
            arguments,
            extra_named,
        )
    }

    /// [`Request::serve_method_made`] whose callee's return, where its
    /// body returned and then threw, goes where `retval` says: a property
    /// read's `get` hook and `__get` write it to the reading opline's
    /// `result` temporary, as `zend_std_read_property` hands them `rv`.
    pub(crate) fn serve_method_into(
        &mut self,
        retval: Retval,
        family: Family,
        method: Callee,
        arguments: &[Value],
        extra_named: &[(PhpString, Value)],
    ) -> Result<Value, Thrown> {
        self.serve_call(
            method,
            Carried::Nothing,
            retval,
            family,
            arguments,
            extra_named,
        )
    }

    /// `zend_call_known_instance_method` (`Z/Zend/zend_API.h:901`): the
    /// method on `object`, `object`'s class the called scope, as
    /// `zend_call_method` and the magic methods call it
    /// ([`Family::Known`]).
    pub(crate) fn serve_known_method(
        &mut self,
        function: Function,
        object: PhpObject,
        called_scope: ClassId,
        arguments: &[Value],
    ) -> Result<Value, Thrown> {
        self.serve_method_into(
            Retval::Native,
            Family::Known,
            Callee::method(function, Some(object), called_scope),
            arguments,
            &[],
        )
    }

    /// `zend_call_known_instance_method` over
    /// `zend_get_property_hook_trampoline(prop_info, kind, name)`, which
    /// `ReflectionProperty::getRawValue` and `setRawValue` call over a
    /// property with a hook of `kind`: the trampoline named `$name::get`
    /// or `$name::set` and scoped to `scope`, `prop_info->ce`, served on
    /// `object` with no hold of the call's own, so the read or write it
    /// makes finds it running and reaches the backing store, and what
    /// it throws has the trampoline's frame on its trace.
    pub(crate) fn serve_hook_trampoline(
        &mut self,
        object: PhpObject,
        scope: ClassId,
        name: &[u8],
        kind: HookKind,
        arguments: &[Value],
    ) -> Result<Value, Thrown> {
        let function = self.parent_hook_trampoline(kind);
        let carried = Carried::HookTrampoline(name, kind);
        self.serve_call(
            Callee::method(function, Some(object), scope),
            carried,
            Retval::Native,
            Family::Known,
            arguments,
            &[],
        )
    }

    /// The trampoline's record on `call`, given up where the call has no
    /// annex to hold it.
    pub(crate) fn carry_trampoline(&mut self, call: FrameRef, built: exec::HookTrampoline) {
        match self.annex_mut(call) {
            Some(annex) => annex.trampoline = Some(built),
            None => self.release(Value::string(built.name)),
        }
    }

    /// `zend_call_known_fcc` over a `Closure`, which is what
    /// `Closure::__invoke` serves: the payload's function on the
    /// payload's receiver and scope, with the closure itself on the
    /// call so the body's `BIND_STATIC` prologue finds the variables it
    /// bound (`ZEND_CALL_CLOSURE`). The closure is one more owner for
    /// the span of the call, taken where `zend_call_function` takes it,
    /// once the arguments are placed ([`Request::serve_call`]), so a call
    /// a handler's throw stops before it is made takes none and the
    /// closure is freed where its last owner lets go of it:
    ///
    /// ```php
    /// class D { function __destruct() { echo "d\n"; } }
    /// set_error_handler(function () { throw new Exception('x'); });
    /// $d = new D;
    /// $f = function (array &$r) use ($d) {};
    /// unset($d);
    /// try { call_user_func($f, 'v'); } catch (Exception $e) { echo "caught\n"; }
    /// unset($f);
    /// echo "end\n";
    /// ```
    /// ```expect
    /// caught
    /// d
    /// end
    /// ```
    pub(crate) fn serve_closure(
        &mut self,
        object: PhpObject,
        family: Family,
        arguments: &[Value],
        extra_named: &[(PhpString, Value)],
    ) -> Result<Value, Thrown> {
        let Some(closure) = object.closure(self).cloned() else {
            return Err(crate::api::raising::error(
                self,
                "Closure object cannot have properties",
            ));
        };
        self.serve_call(
            Callee {
                function: closure.function,
                this: closure.this,
                called_scope: closure.called_scope,
            },
            Carried::Closure(object),
            Retval::Native,
            family,
            arguments,
            extra_named,
        )
    }

    /// `Closure::call`'s serving (`Z/Zend/zend_closures.c:190-231`): the
    /// closure's function on `this`, in `scope`, with the closure itself
    /// on the call so the body's `BIND_STATIC` prologue finds what it
    /// bound. php serves a copy of the function whose scope is `scope`
    /// and puts no object in the store for it; here the closure's own
    /// payload carries the scope for the length of the call and has its
    /// own back after, so no object id is spent where php spends none.
    /// The copy reads a runtime cache of its own for the call where the
    /// scope is not the closure's or the closure has one of its own, as
    /// php's `emalloc`s one beside the copy and frees it after, since the
    /// slots hold what the scope decides. A generator's frame outlives
    /// the call, so php serves it through a closure of its own instead,
    /// which `Closure::call` makes ([`crate::classes::ClosureObject`]).
    pub(crate) fn serve_closure_as(
        &mut self,
        object: PhpObject,
        this: PhpObject,
        scope: ClassId,
        arguments: &[Value],
        extra_named: &[(PhpString, Value)],
    ) -> Result<Value, Thrown> {
        use crate::classes::{ClosureCache, ClosureObject};
        let Some(closure) = object.closure(self).cloned() else {
            return Ok(Value::null());
        };
        let room = self
            .user_function(closure.function)
            .map(|user| user.shape.cache_size.room);
        let own_cache = match room.filter(|_| {
            object.native::<ClosureObject>(self).is_some_and(|payload| {
                closure.scope != Some(scope) || payload.cache != ClosureCache::Function
            })
        }) {
            Some(room) => Some(self.own_cache(room)?),
            None => None,
        };
        let held = object.native_mut::<ClosureObject>(self).map(|payload| {
            let cache = match own_cache {
                Some(at) => std::mem::replace(&mut payload.cache, ClosureCache::Own(at)),
                None => payload.cache,
            };
            (payload.info.scope.replace(scope), cache)
        });
        let served = self.serve_call(
            Callee::method(closure.function, Some(this), scope),
            Carried::Closure(object),
            Retval::Native,
            Family::Fcall,
            arguments,
            extra_named,
        );
        if let Some((scope, cache)) = held
            && let Some(payload) = object.native_mut::<ClosureObject>(self)
        {
            payload.info.scope = scope;
            payload.cache = cache;
        }
        if let Some(at) = own_cache {
            self.give_back_cache(at);
        }
        served
    }

    /// `ZEND_ACC_USES_THIS`, which `zend_compile` sets on an op array
    /// that reads `$this`: a `FETCH_THIS`, an `ISSET_ISEMPTY_THIS`, or an
    /// object opcode whose object operand is the frame's own (`UNUSED`).
    pub(crate) fn closure_uses_this(&self, closure: &ClosureInfo) -> bool {
        use php_unit::{Opcode, OperandKind};
        let Some(code) = self.closure_code(closure.function) else {
            return false;
        };
        code.oplines.iter().any(|op| {
            matches!(op.opcode, Opcode::FetchThis | Opcode::IssetIsemptyThis)
                || (op.op1_type == OperandKind::Unused
                    && matches!(
                        op.opcode,
                        Opcode::FetchObjR
                            | Opcode::FetchObjW
                            | Opcode::FetchObjRw
                            | Opcode::FetchObjIs
                            | Opcode::FetchObjUnset
                            | Opcode::FetchObjFuncArg
                            | Opcode::AssignObj
                            | Opcode::AssignObjOp
                            | Opcode::AssignObjRef
                            | Opcode::PreIncObj
                            | Opcode::PreDecObj
                            | Opcode::PostIncObj
                            | Opcode::PostDecObj
                            | Opcode::UnsetObj
                            | Opcode::IssetIsemptyPropObj
                            | Opcode::InitMethodCall
                    ))
        })
    }

    /// The op array a user function serves, none for a native.
    fn closure_code(&self, function: Function) -> Option<&php_unit::OpArray> {
        let user = self.user_function(function)?;
        exec::body_of(&user.unit, user.body)
    }

    /// The live object the store holds under the id `spl_object_id`
    /// reports, none for an id no live object has.
    pub(crate) fn stored_object(&self, id: u32) -> Option<PhpObject> {
        self.heap.stored(php_heap::ObjectId(id)).map(PhpObject::of)
    }

    /// `ce->type == ZEND_INTERNAL_CLASS`: a class the engine or an
    /// extension declared, and not one a program did.
    pub(crate) fn class_is_internal(&self, class: ClassId) -> bool {
        self.native_class_at(class).is_some()
    }

    /// The `Closure` whose body is the first user frame standing, what
    /// `Closure::getCurrent` reads off `EX(prev_execute_data)`: none
    /// where that frame serves no closure, or a closure made of a named
    /// function (`ZEND_ACC_FAKE_CLOSURE`).
    pub(crate) fn calling_closure(&self) -> Option<PhpObject> {
        for link in self.chain() {
            if let Link::Site(_) = link {
                return None;
            }
            if let Link::Frame(_, state) = link {
                let closure = state.closure()?;
                return closure
                    .closure(self)
                    .filter(|info| !info.fake)
                    .map(|_| closure);
            }
        }
        None
    }

    /// `zend_call_function` over a native function: [`Request::serve_method_made`]
    /// with no receiver and no called scope, what `Closure::__invoke`
    /// serves for a closure over a function.
    pub(crate) fn serve_function(
        &mut self,
        function: Function,
        family: Family,
        arguments: &[Value],
        extra_named: &[(PhpString, Value)],
    ) -> Result<Value, Thrown> {
        self.serve_call(
            Callee {
                function,
                this: None,
                called_scope: None,
            },
            Carried::Nothing,
            Retval::Native,
            family,
            arguments,
            extra_named,
        )
    }

    /// `zend_call_function` (`Z/Zend/zend_execute_API.c:806`), which every
    /// `serve_*` ends in: the callee pushed on a call of its own with the
    /// arguments as its walk of `fci->params` places them. The call
    /// allocates nothing the request's memory counts beside the frame,
    /// so the first `__toString` a native serves costs what php's does:
    ///
    /// ```php
    /// class Foo { function __toString(): string { return 'Foo'; } }
    /// $f = new Foo;
    /// $s = memory_get_usage(); md5($f); array_map(fn($x) => $x, [1]);
    /// var_dump(memory_get_usage() - $s);
    /// ```
    /// ```expect
    /// int(0)
    /// ```
    fn serve_call(
        &mut self,
        target: Callee,
        carried: Carried<'_>,
        retval: Retval,
        family: Family,
        arguments: &[Value],
        extra_named: &[(PhpString, Value)],
    ) -> Result<Value, Thrown> {
        // `if (ZEND_HAS_OOM()) { zend_handle_delayed_oom(); return FAILURE; }`
        // (`Z/Zend/zend_execute_API.c:838` at 8.5.12): no call once the
        // memory limit was met, its fatal raised here if no safepoint has.
        if self.has_oom() {
            self.handle_delayed_oom();
            return self.answer_refusal(family, target.function);
        }
        // `EG(fake_scope) = NULL` for the call and put back after it: a
        // destructor or a `__toString` a scoped write serves stands in its
        // own scope.
        self.in_fake_scope(None, |req| {
            req.serve_call_body(target, carried, retval, arguments, extra_named)
        })
    }

    /// `zend_call_function`'s `FAILURE` once the memory limit was met, as
    /// the caller's [`Family`] answers it: no value for its own callers;
    /// for `zend_call_known_function`'s, the throwable or exit pending,
    /// or else `E_CORE_ERROR` `Couldn't execute method`, which ends the
    /// request where nothing catches it, as phpdbg's `ev` does.
    fn answer_refusal(&mut self, family: Family, function: Function) -> Result<Value, Thrown> {
        match family {
            Family::Fcall => Ok(Value::null()),
            Family::Known => {
                crate::api::raising::pending(self)?;
                let name = self.qualified_name_of(function);
                let name = String::from_utf8_lossy(&name);
                self.end_in_core_error(&format!("Couldn't execute method {name}"));
                Err(Thrown::of(None))
            }
        }
    }

    /// A native function served where php calls its C directly rather
    /// than through `zend_call_function`, as phpdbg's `ev` calls
    /// `zend_print_zval_r`: [`Request::serve_function`] past the memory
    /// limit's refusal of every call once it was met, so what such a
    /// call renders is rendered for the rest of the request.
    pub(crate) fn serve_function_from_c(
        &mut self,
        function: Function,
        arguments: &[Value],
    ) -> Result<Value, Thrown> {
        self.in_fake_scope(None, |req| {
            let target = Callee {
                function,
                this: None,
                called_scope: None,
            };
            req.serve_call_body(target, Carried::Nothing, Retval::Native, arguments, &[])
        })
    }

    /// [`Request::serve_call`] with no scope a write named standing.
    ///
    /// No call while anything stands in php's `EG(exception)`, an `exit`'s
    /// unwind exit among it, as `zend_call_function` answers `SUCCESS`
    /// at once then (`Z/Zend/zend_execute_API.c:843`): a native that goes
    /// on past a handler's `exit` calls nothing after it.
    ///
    /// ```php
    /// class U { function __toString(): string { echo "U called\n"; return "u"; } }
    /// set_error_handler(function ($no, $str) { echo "handler: $str\n"; exit("exit\n"); });
    /// echo implode(",", [[1], new U]), "\n";
    /// ```
    /// ```expect
    /// handler: Array to string conversion
    /// exit
    /// ```
    fn serve_call_body(
        &mut self,
        target: Callee,
        carried: Carried<'_>,
        retval: Retval,
        arguments: &[Value],
        extra_named: &[(PhpString, Value)],
    ) -> Result<Value, Thrown> {
        let Callee {
            function,
            this,
            called_scope,
        } = target;
        let closure = match carried {
            Carried::Closure(closure) => Some(closure),
            Carried::Nothing | Carried::HookTrampoline(..) => None,
        };
        // `if (EG(exception)) return SUCCESS;`: no call while a throwable
        // or an exit stands.
        if self.raised.is_some() {
            return Ok(Value::null());
        }
        // `EG(current_execute_data)` as the call finds it, which a user
        // function it reaches names as its caller.
        let caller = self
            .current_user_frame()
            .map_or(exec::Caller::Outside, exec::Caller::Loop);
        // `zend_call_function`: the deprecation before the arguments are
        // placed, and no call where a handler threw. php raises it with
        // the call pushed but not yet `EG(current_execute_data)`, so a
        // handler's backtrace and its file and line are the caller's;
        // here a pushed call is on the chain a backtrace reads, so the
        // deprecation and the warnings below are raised before the push.
        if self.function_flags(function).deprecated {
            crate::attributes::deprecated_function(self, function)?;
        }
        let wrapped = self.wrapped_positions(function, arguments);
        let prepared = if wrapped.is_empty() {
            None
        } else {
            Some(self.prepare_arguments(function, arguments, &wrapped)?)
        };
        for (extra, value) in extra_named {
            self.addref(&Value::string(*extra));
            self.addref(value);
        }
        // `zend_call_function`: `ZEND_CALL_HAS_THIS` and no
        // `ZEND_CALL_RELEASE_THIS`, the caller keeping the object.
        let receiver = this.map(exec::Receiver::lent);
        // `ZEND_CALL_MAY_HAVE_UNDEF`: a named argument skipped a slot.
        let may_have_undef = arguments
            .iter()
            .any(|argument| matches!(argument.as_ref(), ValueRef::Undef));
        // `zend_call_function` names the callee by
        // `func->common.function_name`, which the function carries; no
        // constant spelled it, so the call interns no name into the
        // request's memory. A hook trampoline's name is the one php builds
        // with the trampoline, laid below once the call stands.
        let extra_named = exec::ExtraNamed::laid(self, extra_named.to_vec())?;
        // `GC_ADDREF(ZEND_CLOSURE_OBJECT(func))` with `ZEND_CALL_CLOSURE`,
        // once the arguments are placed: the call's own owner of the
        // closure, which its end gives up.
        if let Some(closure) = closure {
            self.addref(&closure.as_value());
        }
        // `zend_vm_stack_push_call_frame`, the call served from here
        // (`EG(current_execute_data)` is it while an undefined argument's
        // default is found).
        let mut header =
            exec::Header::call(function, u32::try_from(arguments.len()).unwrap_or(u32::MAX));
        header.set_receiver(receiver);
        header.set_called_scope(called_scope);
        header.closure = closure;
        header.info.set_serving();
        let call = match self.push_call(header) {
            Ok(call) => call,
            Err(fatal) => {
                for argument in prepared.into_iter().flatten() {
                    self.release(argument);
                }
                self.give_up_unpushed(extra_named, receiver, closure);
                return self.fatal_of(Err(fatal));
            }
        };
        if extra_named.carries_any()
            && let Some(annex) = self.annex_mut(call)
        {
            annex.extra_named = extra_named;
        }
        self.place_arguments(call, function, arguments, prepared);
        // The trampoline's record, made once its call stands, where php
        // makes it before the call (`zend_get_property_hook_trampoline`):
        // only the memory limit could tell the two orders apart, and its
        // fatal ends the request either way.
        if let Carried::HookTrampoline(property, kind) = carried {
            match self.hook_trampoline(property, kind) {
                Ok(built) => self.carry_trampoline(call, built),
                Err(thrown) => {
                    self.pop_call();
                    return Err(thrown);
                }
            }
        }
        if may_have_undef && let Err(thrown) = crate::api::call::handle_undef_args(self, call) {
            self.pop_call();
            return Err(thrown);
        }
        // A user function is no trampoline, so it is served as itself;
        // a trampoline hands its call to the magic method it stands for.
        let user = self.user_function(function).is_some();
        let function = if user {
            function
        } else {
            match crate::trampoline::hand_to_magic(self, function) {
                Ok(function) => function,
                Err(thrown) => {
                    self.pop_call();
                    return Err(thrown);
                }
            }
        };
        if user || self.user_function(function).is_some() {
            let served = self.serve_user(call, caller, retval);
            return self.checked_after_call(served);
        }
        // `zend_call_function` ends an observed native with `fci->retval`
        // whether it threw or not (`Z/Zend/zend_execute_API.c:1039`).
        let served = self.serve_native(function, OnThrow::ReturnValue);
        self.pop_call();
        self.checked_after_call(served)
    }

    /// `zend_call_function`'s walk of `fci->params`
    /// (`Z/Zend/zend_execute_API.c:898-944`) where an argument is to be
    /// wrapped: each argument in turn followed where its parameter takes
    /// a value ("don't separate references for __call" but for a
    /// trampoline), one owner taken, and each position of `wrapped`
    /// warned of (`zend_param_must_be_ref`) and given a reference of its
    /// own, `Z_TRY_ADDREF_P(arg); ZVAL_NEW_REF(param, arg)`, before the
    /// next is reached. So the second argument's warning is raised with
    /// the first one's cell made, 32 bytes, as php raises it:
    ///
    /// ```php
    /// function f(&$a, &$b) { return 'f'; }
    /// $seen = array_fill(0, 2, 0); $i = 0;
    /// set_error_handler(function () use (&$seen, &$i) { $seen[$i++] = memory_get_usage(); return true; });
    /// array_map('f', [1], [2]);
    /// echo $seen[1] - $seen[0], "\n";
    /// ```
    /// ```expect
    /// 32
    /// ```
    ///
    /// A warning a handler turned into a throw stops the walk there, as
    /// php's `cleanup_args` does, and a cell the memory limit refuses is
    /// the fatal, each with what was taken before it given up.
    fn prepare_arguments(
        &mut self,
        function: Function,
        arguments: &[Value],
        wrapped: &[usize],
    ) -> Result<Vec<Value>, Thrown> {
        let mut prepared = Vec::with_capacity(arguments.len());
        let walked = self.walk_wrapped(function, arguments, wrapped, &mut prepared);
        if walked.is_err() {
            for argument in prepared.drain(..) {
                self.release(argument);
            }
        }
        walked.map(|()| prepared)
    }

    /// [`Request::prepare_arguments`]'s walk, each argument it took pushed
    /// onto `prepared`.
    fn walk_wrapped(
        &mut self,
        function: Function,
        arguments: &[Value],
        wrapped: &[usize],
        prepared: &mut Vec<Value>,
    ) -> Result<(), Thrown> {
        for (at, argument) in arguments.iter().enumerate() {
            let mut argument = self.followed(function, at, *argument);
            let wrap = wrapped.contains(&at);
            if wrap {
                self.param_must_be_ref(function, u32::try_from(at + 1).unwrap_or(u32::MAX));
                if self.is_pending() {
                    return Err(Thrown::of(self.take_pending_view()));
                }
            }
            self.addref(&argument);
            if wrap {
                // The owner just taken is the new cell's, and the call
                // holds the cell.
                match self.heap.reference(argument.into_zval()) {
                    Ok(made) => argument = PhpRef::of(made).as_value(),
                    Err(fatal) => {
                        self.release(argument);
                        return self.fatal_of(Err(fatal));
                    }
                }
            }
            prepared.push(argument);
        }
        Ok(())
    }

    /// `argument` at `at` as `zend_call_function` copies it: followed
    /// where it is a reference handed to a parameter taking a value,
    /// "don't separate references for __call" but for a trampoline,
    /// which takes it as it is.
    fn followed(&self, function: Function, at: usize, argument: Value) -> Value {
        if !self.via_trampoline(function)
            && argument.as_php_ref().is_some()
            && !self
                .arg_send_mode(function, u32::try_from(at + 1).unwrap_or(u32::MAX))
                .0
        {
            return argument.deref();
        }
        argument
    }

    /// The arguments copied into the slots of the call `call`, which
    /// stands: those [`Request::prepare_arguments`] made, moved, where an
    /// argument was wrapped; otherwise each followed ([`Request::followed`])
    /// and copied, `ZVAL_COPY(param, arg)`. The slots are the frame's, so
    /// a callback's arguments are copied once and no list of them is made
    /// beside it; a user function that takes nothing by reference, the
    /// callback nearly every native serves, finds the frame's slots once.
    fn place_arguments(
        &mut self,
        call: FrameRef,
        function: Function,
        arguments: &[Value],
        prepared: Option<Vec<Value>>,
    ) {
        if let Some(prepared) = prepared {
            if let Some(frame) = self.vm.stack.frame_mut(call) {
                for (slot, argument) in frame.slots_mut().iter_mut().zip(prepared) {
                    *slot = argument.into_zval();
                }
            }
            return;
        }
        if self.takes_nothing_by_ref(function) {
            let heap = &self.heap;
            if let Some(frame) = self.vm.stack.frame_mut(call) {
                for (slot, argument) in frame.slots_mut().iter_mut().zip(arguments) {
                    let argument = argument.deref().to_zval();
                    heap.addref(argument);
                    *slot = argument;
                }
            }
            return;
        }
        for (at, argument) in arguments.iter().enumerate() {
            let argument = self.followed(function, at, *argument);
            self.addref(&argument);
            let slot =
                (self.vm.stack.frame_mut(call)).and_then(|frame| frame.slots_mut().get_mut(at));
            if let Some(slot) = slot {
                *slot = argument.to_zval();
            }
        }
    }

    /// What [`Request::serve_call_body`] took for a call whose frame
    /// `zend_vm_stack_push_call_frame` refused, given back: the extra
    /// named arguments, the receiver and the closure; the arguments a
    /// wrap prepared are given up by the caller, and the others are
    /// placed only once the call stands ([`Request::place_arguments`]).
    fn give_up_unpushed(
        &mut self,
        extra_named: exec::ExtraNamed,
        receiver: Option<exec::Receiver>,
        closure: Option<PhpObject>,
    ) {
        self.release_named(extra_named);
        if let Some(receiver) = receiver {
            self.give_up_receiver(receiver);
        }
        if let Some(closure) = closure {
            self.release(closure.as_value());
        }
    }

    /// `zend_call_function`'s check after its callee returns, a native
    /// or, from php-8.5.12, a user function
    /// (`Z/Zend/zend_execute_API.c:1081-1091`): php tests the interrupt
    /// while running user code but not inside a native, so a native a
    /// native called (`array_map('time_nanosleep', ..)`) is where an
    /// interrupt raised during it is taken, and a user callback's
    /// `return` is no site of its own, so an interrupt raised after the
    /// callback's last check is taken here, before the native that
    /// called it goes on. The timer's is the fatal;
    /// otherwise the extensions' hooks, and what they throw is what the
    /// call hands back. A native that threw leaves the interrupt for the
    /// next site, as the loop's own check after a call does.
    ///
    /// The signal here is sent by a forked child, and its ordering is
    /// fixed by construction: the child sends it only once it has read a
    /// byte that `fwrite`, called by `iterator_apply`, wrote, so the
    /// signal cannot arrive before the parent is inside the outer
    /// native; and `iterator_apply` goes on calling `fwrite` over an
    /// endless iterator until a check after one of those calls takes
    /// the interrupt, so it cannot be missed whichever thread of the
    /// process runs the signal's handler, or how late. Nothing but a
    /// check after a native a native called ends the loop.
    ///
    /// ```php
    /// pcntl_async_signals(true);
    /// pcntl_signal(SIGVTALRM, function () { throw new Exception("thrown by the handler"); });
    /// [$r, $w] = stream_socket_pair(STREAM_PF_UNIX, STREAM_SOCK_STREAM, STREAM_IPPROTO_IP);
    /// $pid = pcntl_fork();
    /// if ($pid === 0) { fclose($w); fread($r, 1); posix_kill(posix_getppid(), SIGVTALRM); stream_get_contents($r); exit(0); }
    /// fclose($r);
    /// $endless = new InfiniteIterator(new ArrayIterator([0]));
    /// try { iterator_apply($endless, 'fwrite', [$w, 'x']); echo "unreached\n"; }
    /// catch (Exception $e) { echo $e->getMessage(), "\n"; }
    /// fclose($w);
    /// pcntl_waitpid($pid, $status);
    /// ```
    /// ```expect process
    /// thrown by the handler
    /// ```
    ///
    /// Here pcntl blocks signals while it serves `SIGUSR1`, so the
    /// `SIGUSR2` its handler sends lands as that dispatch ends, after the
    /// callback's last check; the check after the callback returns serves
    /// it before `usort` raises its deprecation for the `bool` the
    /// callback gave back:
    ///
    /// ```php
    /// pcntl_async_signals(true);
    /// pcntl_signal(SIGUSR1, function () { echo "usr1\n"; posix_kill(posix_getpid(), SIGUSR2); });
    /// pcntl_signal(SIGUSR2, function () { echo "usr2\n"; });
    /// $a = [2, 1];
    /// usort($a, function ($x, $y) { posix_kill(posix_getpid(), SIGUSR1); return $x > $y; });
    /// echo "after\n";
    /// ```
    /// ```expect
    /// usr1
    /// usr2
    ///
    /// Deprecated: usort(): Returning bool from comparison function is deprecated, return an integer less than, equal to, or greater than zero in %s on line 6
    /// after
    /// ```
    #[inline(always)] // one load and a branch on every callback's return
    fn checked_after_call(&mut self, served: Result<Value, Thrown>) -> Result<Value, Thrown> {
        if !self.limits().interrupt().raised().load(Ordering::Relaxed) {
            return served;
        }
        self.interrupted_after_call(served)
    }

    /// [`Request::checked_after_call`] past its load, where the interrupt
    /// is raised: what the callee served handed on as it stands where it
    /// threw, and otherwise the check's answer over it.
    #[cold]
    #[inline(never)]
    fn interrupted_after_call(&mut self, served: Result<Value, Thrown>) -> Result<Value, Thrown> {
        let Ok(value) = served else {
            return served;
        };
        match crate::interrupt::interrupted(self) {
            Ok(false) => Ok(value),
            Ok(true) => {
                self.release(value);
                Err(Thrown::of(self.take_pending_view()))
            }
            Err(fatal) => {
                self.release(value);
                self.fatal_of(Err(fatal))
            }
        }
    }

    /// The positions of `arguments` that `zend_call_function`'s walk of
    /// `fci->params` (`Z/Zend/zend_execute_API.c:898-944`) wraps in a
    /// reference of their own: an argument that is no reference, handed
    /// to a parameter `ARG_SHOULD_BE_SENT_BY_REF` and not
    /// `ARG_MAY_BE_SENT_BY_REF`, which is `zend_param_must_be_ref`'s
    /// warning as the walk reaches it ([`Request::place_arguments`]), and
    /// the call goes on with the value wrapped, so what the callee writes
    /// through it is lost; a warning a handler turned into a throw stops
    /// the call there, as php's `cleanup_args` does. The parameter is
    /// named where it is one of the declared ones,
    /// `get_function_arg_name`, and a variadic's is not.
    ///
    /// ```php
    /// function f(&$v) { $v = 1; return 'f'; }
    /// class C { static function m(&$v, ...$rest) { return 'm'; } }
    /// $x = 5;
    /// var_dump(call_user_func('f', $x), $x, array_map('f', [1]), call_user_func('C::m', 1, 2));
    /// ```
    /// ```expect
    /// Warning: f(): Argument #1 ($v) must be passed by reference, value given in %s on line 5
    ///
    /// Warning: f(): Argument #1 ($v) must be passed by reference, value given in %s on line 5
    ///
    /// Warning: C::m(): Argument #1 ($v) must be passed by reference, value given in %s on line 5
    /// string(1) "f"
    /// int(5)
    /// array(1) {
    ///   [0]=>
    ///   string(1) "f"
    /// }
    /// string(1) "m"
    /// ```
    fn wrapped_positions(&self, function: Function, arguments: &[Value]) -> Vec<usize> {
        if self.takes_nothing_by_ref(function) {
            return Vec::new();
        }
        let Some(decl) = self.symbols().function_decl(function) else {
            return Vec::new();
        };
        let variadic = decl.parameters.last().filter(|last| last.variadic);
        arguments
            .iter()
            .enumerate()
            .filter(|(_, argument)| {
                !matches!(argument.as_ref(), ValueRef::Undef | ValueRef::Reference(_))
            })
            .filter(|(at, _)| {
                decl.parameters
                    .get(*at)
                    .or(variadic)
                    .is_some_and(|parameter| parameter.by_ref && !parameter.prefer_ref)
            })
            .map(|(at, _)| at)
            .collect()
    }

    /// `ZEND_ARG_SEND_MODE` (`Z/Zend/zend_compile.h:1141`) over
    /// `function`: whether the parameter numbered `number` takes its
    /// argument by reference, and whether only where it can
    /// (`ZEND_SEND_PREFER_REF`), the variadic's for every argument past
    /// the declared ones. The shape's test is inline in the `SEND_*_EX`
    /// handlers that ask it, and the declaration is read out of line.
    #[inline(always)] // once per `SEND_*_EX`, in the loop's arms
    pub(crate) fn arg_send_mode(&self, function: Function, number: u32) -> (bool, bool) {
        if self.takes_nothing_by_ref(function) {
            return (false, false);
        }
        self.declared_send_mode(function, number)
    }

    /// Whether `function` is a user function none of whose parameters
    /// takes its argument by reference, as its shape says without its
    /// declaration read: no argument of a call to it is wrapped or
    /// followed but by value.
    #[inline(always)] // in the `SEND_*_EX` arms, as `arg_send_mode`
    fn takes_nothing_by_ref(&self, function: Function) -> bool {
        self.user_function(function)
            .is_some_and(|user| !user.shape.by_ref)
    }

    /// [`Request::arg_send_mode`] read off the declaration.
    #[inline(never)]
    fn declared_send_mode(&self, function: Function, number: u32) -> (bool, bool) {
        let Some(decl) = self.symbols().function_decl(function) else {
            return (false, false);
        };
        super::sent_parameter(decl, number).map_or((false, false), |parameter| {
            (parameter.by_ref, parameter.by_ref && parameter.prefer_ref)
        })
    }

    /// `fn_flags & ZEND_ACC_CALL_VIA_TRAMPOLINE`: `function` is the
    /// trampoline a call to a name the class serves through `__call` or
    /// `__callStatic` resolved to.
    pub(crate) fn via_trampoline(&self, function: Function) -> bool {
        self.trampoline_of(function)
            .is_some_and(|(_, served)| served == crate::trampoline::Served::Trampoline)
    }

    /// `zend_param_must_be_ref` (`Z/Zend/zend_execute.c`): the warning
    /// for an argument that is no reference handed to a parameter that
    /// takes one by reference and nothing else, naming the parameter
    /// where it is one of the declared ones (`get_function_arg_name`)
    /// and not where a variadic collects it. What `zend_call_function`,
    /// `SEND_USER` and `SEND_ARRAY` raise before they wrap the value in
    /// a reference of its own and go on.
    pub(crate) fn param_must_be_ref(&mut self, function: Function, number: u32) {
        let callee = self.qualified_name(function, None);
        let name = crate::api::raising::arg_name_suffix(self, Some(function), number);
        let message = bytes!(
            callee,
            format!("(): Argument #{number}{name} must be passed by reference, value given")
        );
        crate::api::raising::warning(self, &message);
    }

    /// `zend_call_function` over a user function, the branch that calls
    /// `zend_execute_ex` (design §5.6): the frame the call reached
    /// pushed as `DO_UCALL`'s `enter` pushes it, with no caller slot to
    /// return into, and the engine's loop entered on it once more,
    /// bound to the same tier as the serving that is already standing.
    /// Every frame the serving left behind is popped after it, its own
    /// last, since a throwable unwinds to here and not through the
    /// frames it passed.
    ///
    /// What the body returned is the caller's, one owner; what it left
    /// pending is the `Err`, the throwable's one owner with it, so a
    /// native reads a callback's raise the way it reads its own
    /// ([`Request::leave_pending`] puts it back). A body that returned
    /// and then threw, from a local's destructor at its end, has its
    /// value held ([`Request::hold_thrown_return`]) and given up where
    /// the opline that called the native unwinds, after that opline's
    /// operands, as php's caller holds the `retval`: so that value's
    /// destructor runs after the receiver's and before the `catch`, a
    /// property hook's and a callback's alike.
    ///
    /// ```php
    /// class V { function __destruct() { echo "V::__destruct\n"; } }
    /// class T { function __destruct() { throw new Exception("t"); } }
    /// class C { public $h { get { $l = new T; return new V; } } function __destruct() { echo "C::__destruct\n"; } }
    /// class M { function __get($n) { $l = new T; return new V; } function __destruct() { echo "M::__destruct\n"; } }
    /// function f() { $l = new T; return new V; }
    /// try { $x = (new C)->h; } catch (Exception $e) { echo "caught\n"; }
    /// try { $x = (new M)->m; } catch (Exception $e) { echo "caught\n"; }
    /// try { array_map('f', [1]); } catch (Exception $e) { echo "caught\n"; }
    /// echo "done\n";
    /// ```
    /// ```expect
    /// C::__destruct
    /// V::__destruct
    /// caught
    /// M::__destruct
    /// V::__destruct
    /// caught
    /// V::__destruct
    /// caught
    /// done
    /// ```
    ///
    /// What the body returned is its own while a destructor its leave
    /// serves runs a body of its own, which returns too: each serving has
    /// its own `retval`, as each `zend_call_function` has.
    ///
    /// ```php
    /// class D { function __destruct() { echo "D\n"; } }
    /// function f($x) { $l = new D; return [$x * 2]; }
    /// var_dump(array_map('f', [1]));
    /// ```
    /// ```expect
    /// D
    /// array(1) {
    ///   [0]=>
    ///   array(1) {
    ///     [0]=>
    ///     int(2)
    ///   }
    /// }
    /// ```
    ///
    /// A callback with a declared parameter type is held to it here as
    /// it is held to it under `DO_UCALL`, since one
    /// [`crate::exec::parameter_prologue`] serves both: a typed
    /// parameter reaches its `RECV` whatever the call filled, so the
    /// coercion a scalar argument carries outside `strict_types` is
    /// served and the `TypeError` for what does not convert is raised,
    /// with the boundary named `[internal function]` as
    /// `zend_trace_to_string` names a frame no opline called.
    ///
    /// ```php
    /// function f(int $a) { var_dump($a); }
    /// register_shutdown_function('f', "5");
    /// ```
    /// ```expect
    /// int(5)
    /// ```
    ///
    /// ```php
    /// function f(int $a) { var_dump($a); }
    /// register_shutdown_function('f', []);
    /// ```
    /// ```expect exit=255
    /// Fatal error: Uncaught TypeError: f(): Argument #1 ($a) must be of type int, array given in %s:2
    /// Stack trace:
    /// #0 [internal function]: f(Array)
    /// #1 {main}
    ///   thrown in %s on line 2
    /// ```
    ///
    /// A fatal the body raises is rendered while the body's frames still
    /// stand, as `php_error_cb` renders it under `zend_error` before
    /// anything unwinds, so the site it names is the body's own line and
    /// not the caller's.
    ///
    /// ```php
    /// set_time_limit(1);
    /// register_shutdown_function(function () {
    ///     for (;;) {}
    /// });
    /// ```
    /// ```expect exit=255 wall=1s
    /// Fatal error: Maximum execution time of 1 second exceeded in %s on line 4
    /// ```
    ///
    /// A native that serves user code while a fatal is in flight is in
    /// php's `zend_catch`, where `_zend_bailout` has landed: the call
    /// runs as any other, and the fatal is in flight again once it
    /// returns, the `zend_bailout()` php's catch blocks end with. The
    /// session's decode is the witness, in `ext/session`: its handler's
    /// `destroy` is served after the fatal a class declaration raised.
    fn serve_user(
        &mut self,
        call: FrameRef,
        caller: exec::Caller,
        retval: Retval,
    ) -> Result<Value, Thrown> {
        let caught = self.bailout;
        self.catch_bailout();
        // What an outer serving's entry frame returned stands while this
        // one runs, a destructor its leave serves among them, and is its
        // own again after: `returned` is one per serving, as each
        // `zend_call_function` has its own `retval`.
        let outer = self.take_returned();
        let served = self.serve_user_uncaught(call, caller, retval);
        let left = std::mem::replace(&mut self.returned, outer);
        if left.is_refcounted() && !self.bailout {
            self.release_thrown_value(left);
        }
        if caught {
            self.set_bailing_out(true);
        }
        served
    }

    fn serve_user_uncaught(
        &mut self,
        call: FrameRef,
        caller: exec::Caller,
        retval: Retval,
    ) -> Result<Value, Thrown> {
        let standing = call.0 as usize;
        let shape = self.call_shape(call).unwrap_or_default();
        let pushed = self.activate_user(call, shape, None, caller);
        let activated = match pushed {
            Ok(activated) => activated,
            Err(fatal) => {
                self.pop_frames_to(standing);
                return self.fatal_of(Err(fatal));
            }
        };
        // `enter`'s own prologue, so a body reached from here stands
        // where a body reached from `DO_UCALL` stands: every `RECV*`
        // php runs is run, which is where a declared parameter type is
        // checked and a scalar argument coerced, and the entry
        // safepoint is raised after them by the loop below.
        let frame = activated.frame;
        let raised = crate::exec::parameter_prologue(self, activated);
        let raised = match self.fatal_of(raised) {
            Ok(raised) => raised,
            Err(thrown) => {
                self.pop_frames_to(standing);
                return Err(thrown);
            }
        };
        if raised.is_some() {
            // The `RECV` that raised is the callee's, whose unwind ends
            // what it began (`unwind_from`) at `ZEND_HANDLE_EXCEPTION`;
            // here it is left at once.
            self.vm.stand_at_exception_op(frame, true);
            crate::observer::returned(self, frame, None);
            self.pop_frames_to(standing);
            return match self.take_pending_view() {
                Some(pending) => Err(Thrown::of(Some(pending))),
                None => Ok(Value::null()),
            };
        }
        let reenter = self.loops().reenter;
        let served = reenter(self, frame);
        // A fatal is rendered where it was raised, `php_error_cb` under
        // `zend_error`, so the frames that raised it still stand while
        // it is: the site a timeout names inside a callback, or inside a
        // fiber's body, is the callback's and not its caller's.
        let served = self.fatal_of(served);
        self.pop_frames_to(standing);
        served?;
        if self.raised.is_some() {
            // The body returned and then threw or exited, from a local's
            // destructor at its end: what it returned is php's `retval`,
            // where `retval` says it is.
            let returned = self.take_returned();
            self.thrown_return(caller, retval, returned);
        }
        match self.take_pending_view() {
            Some(pending) => Err(Thrown::of(Some(pending))),
            // The callback exited: what the native reads is php's
            // `EG(exception)` holding the unwind exit or the graceful
            // one, and the native returns with it standing, for the
            // unwinder to go on from the call opline; a fatal leaves the
            // native as it left the callback.
            None if self.is_pending() => Err(Thrown::of(None)),
            None => Ok(std::mem::take(&mut self.returned)),
        }
    }

    /// What a user callee returned before it threw, put where php's
    /// `retval` is ([`Retval`]): the `result` temporary of the opline the calling
    /// frame stands at where that is a property read's, `FETCH_OBJ_R`'s
    /// or `FETCH_OBJ_IS`'s, the read's `rv`, so `free_throwing_result`
    /// frees it after the opline's operands; given up at once otherwise,
    /// as a native gives up its own `retval` on `EG(exception)`. Out of
    /// line, since nothing reaches it but a throw.
    #[cold]
    #[inline(never)]
    fn thrown_return(&mut self, caller: exec::Caller, retval: Retval, value: Value) {
        let result = caller
            .frame()
            .filter(|_| retval == Retval::Result)
            .and_then(|frame| {
                let at = self.frame_at(frame)?;
                let op = exec::opline_at(at.code()?, at.opline())?;
                let reads = matches!(op.opcode, Opcode::FetchObjR | Opcode::FetchObjIs)
                    && matches!(op.out_type, OperandKind::Tmp | OperandKind::Var);
                reads.then_some((frame, op.out))
            });
        match result {
            Some((frame, out)) => self.replace_slot(frame, out, value),
            None => self.release(value),
        }
    }

    /// The entry the one serving the call replaced, the next down its
    /// native's chain of interceptions, and how far down that one
    /// stands; none where the entry serving is the native's own.
    pub(crate) fn replaced_entry(
        &self,
    ) -> Option<(crate::api::abi::Entry, usize, Option<ExtensionId>)> {
        let call = self.serving_call()?;
        let at = usize::try_from(call.function()?.0).ok()?;
        let beneath = call.annex().map_or(0, |annex| annex.beneath);
        let &(by, entry) = self.shared.replaced.get(at)?.get(beneath)?;
        Some((entry, beneath.saturating_add(1), by))
    }

    /// The call being served, served from `beneath` down its native's
    /// chain of interceptions.
    pub(crate) fn serve_beneath(&mut self, beneath: usize) {
        if let Some(call) = self.vm.serving_at()
            && let Some(annex) = self.annex_mut(call)
        {
            annex.beneath = beneath;
        }
    }

    /// The first argument of the call being served replaced by `value`,
    /// the one it held given up.
    pub(crate) fn replace_first_argument(&mut self, value: Value) {
        let slot = self
            .vm
            .serving_at()
            .filter(|&call| self.vm.at(call).is_some_and(|header| header.num_args > 0))
            .and_then(|call| self.vm.stack.frame_mut(call))
            .and_then(|frame| frame.slots_mut().first_mut());
        let old = match slot {
            Some(slot) => Value::of_zval(std::mem::replace(slot, value.to_zval())),
            None => value,
        };
        self.release(old);
    }

    /// The entry a `Function` names: one of the natives, or a native
    /// method numbered past them; none for a number nothing is linked
    /// under.
    fn entry_of(&self, function: Function) -> Option<crate::api::abi::Entry> {
        let function = self.served_native(function);
        if let Some((_, ClassNative::Enum(method))) = self.class_native_of(function) {
            return Some(method.entry());
        }
        if let Some((trampoline, served)) = self.trampoline_of(function) {
            use crate::trampoline::{Served, Via};
            return match (trampoline.via, served) {
                (Via::Invoke(_), Served::Trampoline) => {
                    Some(crate::classes::closure_invoke as crate::api::abi::Entry)
                }
                (Via::Magic(_), Served::ClosureCall) => {
                    Some(crate::trampoline::closure_call_magic as crate::api::abi::Entry)
                }
                (Via::Native(entry), _) => Some(entry),
                (Via::Magic(_), Served::Trampoline) | (Via::Invoke(_), Served::ClosureCall) => None,
            };
        }
        let at = usize::try_from(function.0).ok()?;
        match at.checked_sub(self.shared.functions.len()) {
            None => self.shared.entries.get(at).copied(),
            Some(method) => self.shared.methods.get(method).map(|m| m.entry),
        }
    }

    /// The entry of a native function, `fbc->internal_function.handler`:
    /// one read of the engine's table, and none for any other function,
    /// a native method, a class's native or a trampoline among them,
    /// which [`Request::serve_native`] finds.
    #[inline]
    pub(crate) fn native_entry(&self, function: Function) -> Option<crate::api::abi::Entry> {
        // One row per native function, so a number past them is none.
        let at = usize::try_from(function.0).ok()?;
        self.shared.entries.get(at).copied()
    }

    /// The native function no table carries whose declaration is named
    /// `name` ([`MethodAt::Unlisted`]).
    pub(crate) fn unlisted_function(&self, name: &[u8]) -> Option<Function> {
        let at = self.shared.methods.iter().position(|method| {
            matches!(method.at, MethodAt::Unlisted { at } if self
                .shared
                .trampolines
                .get(at as usize)
                .is_some_and(|decl| decl.function.name.0.as_ref() == name))
        })?;
        u32::try_from(self.shared.functions.len().checked_add(at)?)
            .ok()
            .map(Function)
    }

    /// The native method `function` is, if it is one: its class and its
    /// index among the class's declared methods.
    pub(super) fn native_method(&self, function: Function) -> Option<&NativeMethod> {
        let at = usize::try_from(function.0).ok()?;
        self.shared
            .methods
            .get(at.checked_sub(self.shared.functions.len())?)
    }

    /// The user function or method `function` is, if it is one: one of
    /// the request's own rows, or the published arena's, which are
    /// numbered past the natives and the native methods (design §5.14).
    #[inline]
    pub(crate) fn user_function(&self, function: Function) -> Option<&UserFunction> {
        user_in(&self.user_functions, &self.arena, function)
    }

    /// `fbc->common.fn_flags & (ZEND_ACC_DEPRECATED | ZEND_ACC_NODISCARD)`:
    /// what the callee's declaration raised where it was linked, none
    /// for an enum's own method and the pass function.
    pub(crate) fn function_flags(&self, function: Function) -> crate::attributes::Flags {
        // A user function is no class native, so it serves as itself.
        if let Some(user) = self.user_function(function) {
            return user.flags;
        }
        let function = self.served_native(function);
        if let Some(method) = self.native_method(function) {
            return method.flags;
        }
        // `fbc->common.fn_flags & (ZEND_ACC_DEPRECATED|ZEND_ACC_NODISCARD)`,
        // which `zend_get_call_trampoline_func` copies from the magic method.
        if let Some((trampoline, _)) = self.trampoline_of(function)
            && let crate::trampoline::Via::Magic(magic) = trampoline.via
        {
            return self.function_flags(magic);
        }
        usize::try_from(function.0)
            .ok()
            .and_then(|at| self.shared.function_flags.get(at))
            .copied()
            .unwrap_or_default()
    }

    /// The trampoline a function number names and which of its two
    /// functions it is; none for any other function.
    pub(crate) fn trampoline_of(
        &self,
        function: Function,
    ) -> Option<(
        &crate::trampoline::CallTrampoline,
        crate::trampoline::Served,
    )> {
        let (index, served) = crate::trampoline::decode(function)?;
        Some((self.calls.trampolines.get(index)?, served))
    }

    /// `fn_flags & (ZEND_ACC_CALL_VIA_TRAMPOLINE|ZEND_ACC_NEVER_CACHE)`:
    /// a callee no cache slot may hold, since the next lookup of the
    /// same name on the same class may answer another. A trampoline the
    /// request made, `__call`'s, a closure's `__invoke` or an
    /// extension's native, is one per name or per closure; and a method
    /// a class's `get_method` synthesises ([`MethodAt::Trampoline`]) is
    /// answered by the object and not by its class, as a method a `PDO`
    /// connection's connector lays on it is by that connection, which
    /// php marks `ZEND_ACC_NEVER_CACHE` (`Z/ext/pdo/pdo_dbh.c:1402`).
    pub(crate) fn never_cached(&self, function: Function) -> bool {
        self.trampoline_of(function).is_some()
            || self
                .native_method(function)
                .is_some_and(|method| matches!(method.at, MethodAt::Trampoline { .. }))
    }

    /// `zend_get_call_trampoline_func(ce, name, is_static)`
    /// (`Z/Zend/zend_object_handlers.c:1764`): the trampoline `class`'s
    /// `__call`, or its `__callStatic` where `is_static`, serves under
    /// `name`, the one the request already made for that method and
    /// name where it made one; none where the class has no such
    /// method. It is abstract, deprecated or `#[\NoDiscard]` where the
    /// magic method is, as php copies those flags onto it:
    ///
    /// ```php
    /// class C { #[\Deprecated("why")] function __call($n, $a) { return $n; } }
    /// interface I { public static function __callStatic($n, $a); }
    /// var_dump((new C)->anything());
    /// try { I::bar(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// Deprecated: Method C::anything() is deprecated, why in %s on line 4
    /// string(8) "anything"
    /// Cannot call abstract method I::bar()
    /// ```
    pub(crate) fn call_trampoline(
        &mut self,
        class: ClassId,
        name: &[u8],
        is_static: bool,
    ) -> Option<Function> {
        let magic = self.symbols().method_named(
            class,
            if is_static {
                b"__callstatic"
            } else {
                b"__call"
            },
        )?;
        self.trampoline_made(
            |made| made.is(magic, name),
            |req| {
                let of = req.method_decl(magic)?;
                Some(CallTrampoline::new(magic, name, is_static, of))
            },
        )
    }

    /// `zend_get_closure_invoke_method` (`Z/Zend/zend_closures.c:484`):
    /// the `__invoke` of `closure`, which carries the signature of the
    /// function it holds, the one the request already made for that
    /// function where it made one; none for an object that is no
    /// closure.
    pub(crate) fn invoke_trampoline(&mut self, closure: PhpObject) -> Option<Function> {
        let function = closure.closure(self)?.function;
        let via = crate::trampoline::Via::Invoke(function);
        self.trampoline_made(
            |made| made.via == via,
            |req| {
                Some(CallTrampoline::invoke(
                    function,
                    req.symbols().function_decl(function)?,
                ))
            },
        )
    }

    /// The number of the trampoline the request made that `held` picks,
    /// or of the one `make` makes, kept for the request so a number
    /// names one trampoline for its life; none where `make` makes none.
    fn trampoline_made(
        &mut self,
        held: impl Fn(&CallTrampoline) -> bool,
        make: impl FnOnce(&Request) -> Option<CallTrampoline>,
    ) -> Option<Function> {
        let at = if let Some(at) = self.calls.trampolines.iter().position(held) {
            at
        } else {
            let trampoline = make(self)?;
            self.calls.trampolines.push(trampoline);
            self.calls.trampolines.len().checked_sub(1)?
        };
        crate::trampoline::number(at, crate::trampoline::Served::Trampoline)
    }

    /// `get_static_method_fallback` (`Z/Zend/zend_object_handlers.c:2016`):
    /// where the name of a call on a class finds nothing `class` lets the scope
    /// call, the trampoline of `this`'s class's `__call` where the
    /// calling frame has a receiver that is an instance of `class`, and
    /// otherwise `class`'s `__callStatic`'s; none where neither serves.
    pub(crate) fn static_trampoline(
        &mut self,
        class: ClassId,
        name: &[u8],
        this: Option<PhpObject>,
    ) -> Option<Function> {
        if self.symbols().method_named(class, b"__call").is_some()
            && let Some(this) = this.filter(|this| self.symbols().instance_of(this.class(), class))
        {
            return self.call_trampoline(this.class(), name, false);
        }
        self.call_trampoline(class, name, true)
    }

    /// The trampoline `class` answers under `name`, any case, php's
    /// `ZEND_ACC_CALL_VIA_HANDLER` function: a method the class's
    /// `get_method` synthesises, which `ce->function_table` has no row
    /// for, so [`Symbols::method_named`] never finds it and no
    /// rendering lists it. `zend_get_closure_invoke_method` is the one
    /// the engine makes, `Closure::__invoke`.
    pub(crate) fn trampoline_named(&self, class: ClassId, name: &[u8]) -> Option<Function> {
        let table = self
            .shared
            .trampoline_tables
            .get(self.native_class_at(class)?)?;
        table.find(name).copied()
    }

    /// Serve the native the call frame on top of [`Vm`] names: its entry
    /// with the request, off which it reads its arguments (ADR-0002); the
    /// frame is the active function for the raisers' text while the entry
    /// runs, and stays on the stack until `DO_ICALL` pops it. `function`
    /// is the callee the `INIT_FCALL` resolved; one with no entry is the
    /// `Error` a function nothing is linked under raises, in the caller's
    /// spelling.
    ///
    /// An observer (design §4.3 seam 9) begins the call before the entry
    /// and ends it after, with what it returned, or, where it threw,
    /// with what `on_throw` says the path that called it hands
    /// ([`crate::observer::native_call`]).
    pub(crate) fn serve_native(
        &mut self,
        function: Function,
        on_throw: OnThrow,
    ) -> Result<Value, Thrown> {
        if function == Function::PASS {
            return Ok(Value::null());
        }
        let Some(entry) = self.entry_of(function) else {
            let name = self.declared_name(function);
            return Err(crate::api::raising::undefined_function(self, &name));
        };
        self.serve_entry(function, entry, on_throw)
    }

    /// [`Request::serve_native`] past the entry's lookup, for a caller
    /// that holds `function`'s entry already, as a frameless call's
    /// handler reads it off the frameless table (`ZEND_FLF_HANDLER`).
    ///
    /// What a user wrapper's method threw inside a stream op the native
    /// asked for, kept by the stream layer ([`Diagnostics::stream_thrown`])
    /// where php's leaves it in `EG(exception)`, is raised as the native
    /// returns: a throwable the native answers after it is chained onto
    /// it as `zend_throw_exception_internal` chains it, the kept one its
    /// `previous`, and an exit or a fatal gives the kept one up
    /// ([`Thrown::chain`]). `SimpleXMLElement` throws once libxml has
    /// read the empty document and closed its stream:
    ///
    /// ```php
    /// class W { public $context; function url_stat($p, $f) { return ['size' => 0]; } function stream_open($p, $m, $o, &$op) { return true; } function stream_read($n) { return ''; } function stream_eof() { return true; } function stream_close() { echo "close\n"; throw new Exception("close"); } }
    /// stream_wrapper_register('ww', 'W');
    /// try { new SimpleXMLElement('ww://x', LIBXML_NOERROR, true); }
    /// catch (Exception $e) { echo $e->getMessage(), " <- ", $e->getPrevious()?->getMessage(), "\n"; }
    /// ```
    /// ```expect
    /// close
    /// String could not be parsed as XML <- close
    /// ```
    ///
    /// [`Diagnostics::stream_thrown`]: super::Diagnostics::stream_thrown
    #[inline(always)] // design §5.8: every native call's, frameless or not
    pub(crate) fn serve_entry(
        &mut self,
        function: Function,
        entry: crate::api::abi::Entry,
        on_throw: OnThrow,
    ) -> Result<Value, Thrown> {
        let begun = crate::observer::native_call(self, function);
        #[cfg(debug_assertions)]
        let outer = self
            .function_origin(function)
            .map(|extension| self.heap.set_origin(extension.origin()));
        // The entry's owner is the handler layer's from here: the frame's
        // return slot holds the count.
        let served = entry(self).map(Owner::hand_on);
        #[cfg(debug_assertions)]
        if let Some(outer) = outer {
            self.heap.set_origin(outer);
        }
        let Some(deferred) = self.stream_thrown.take() else {
            crate::observer::native_returned(self, begun, &served, None, on_throw);
            return served;
        };
        if served.is_ok() {
            crate::observer::native_returned(self, begun, &served, Some(&deferred), on_throw);
        }
        match served {
            Ok(value) => {
                self.release(value);
                Err(deferred)
            }
            // Thrown over the kept one, chained as php chains it where
            // the native throws.
            Err(thrown) => {
                let served = Err(deferred.chain(self, thrown));
                crate::observer::native_returned(self, begun, &served, None, on_throw);
                served
            }
        }
    }

    /// Every frame above the depth `standing` popped.
    fn pop_frames_to(&mut self, standing: usize) {
        while self.vm.depth() > standing {
            self.pop_frame();
        }
    }

    /// `serve` served as code of `extension`, one of its lifecycle hooks:
    /// in a debug build, what the heap allocates meanwhile is tagged
    /// with the extension ([`php_heap::Heap::set_origin`]), so a leak it
    /// leaves names it, and the origin standing before is put back after.
    pub(crate) fn serve_for<T>(
        &mut self,
        extension: ExtensionId,
        serve: impl FnOnce(&mut Request) -> T,
    ) -> T {
        self.code_mut()
            .serve_supplied(Supplier::of(extension), serve)
    }

    /// The extension whose code a native is, for a debug build's heap to
    /// tag what it allocates while the native runs: php's
    /// `internal_function.module` ([`Extensions::function_extension`]);
    /// none for a closure's or a trampoline's entry that no extension
    /// declares, which allocates as its caller.
    #[cfg(debug_assertions)]
    fn function_origin(&self, function: Function) -> Option<ExtensionId> {
        let intercepted = usize::try_from(function.0)
            .ok()
            .and_then(|at| self.shared.intercepted_by.get(at).copied().flatten());
        intercepted.or_else(|| self.extensions().function_extension(function))
    }

    /// Record the natives this request enters from here on, for the
    /// witness runner (harness §4.5 condition 2, revision 58).
    pub(crate) fn trace_natives(&mut self) {
        self.calls.trace = Some(Vec::new());
    }

    /// One native entered, where the request records them; what the
    /// entry a macro expansion builds calls on its way in. Each native
    /// once per request, in the order the request first entered it: what
    /// the trace is read for is which natives a request entered, so a
    /// program that calls one in a loop records it once.
    #[cold]
    #[inline(never)]
    pub(crate) fn entered(&mut self, native: crate::trace::Entered) {
        if let Some(trace) = &mut self.calls.trace
            && !trace.contains(&native)
        {
            trace.push(native);
        }
    }

    /// Whether the natives this request enters are recorded
    /// ([`Request::trace_natives`]).
    #[inline]
    pub(crate) fn tracing_natives(&self) -> bool {
        self.calls.trace.is_some()
    }

    /// The natives recorded, given up at the request's end; none where
    /// nothing asked for them.
    pub(crate) fn take_trace(&mut self) -> Vec<crate::trace::Entered> {
        self.calls.trace.take().unwrap_or_default()
    }

    /// The default of the parameter `key` names, evaluated earlier in
    /// this request: a view, the request keeping the owner.
    pub(crate) fn default_argument(&self, key: (Function, u32)) -> Option<Value> {
        self.calls
            .defaults
            .iter()
            .find(|(at, _)| *at == key)
            .map(|(_, value)| *value)
    }

    /// Keep the owner of an evaluated default until the request ends, as
    /// php's `zend_get_default_from_internal_arg_info` evaluates one where
    /// the call left it absent.
    pub(crate) fn keep_default_argument(&mut self, key: (Function, u32), value: Value) {
        self.calls.defaults.push((key, value));
    }

    /// Give up the evaluated defaults, beside the functions' `static`
    /// variables.
    pub(crate) fn free_default_arguments(&mut self) {
        for (_, value) in std::mem::take(&mut self.calls.defaults) {
            self.release(value);
        }
    }

    /// The native trampoline of `entry` under `name`, carrying
    /// `reserved` where it is given; the one the request already made
    /// with the same where it made one.
    fn native_trampoline_as<T: std::any::Any + Copy + PartialEq + Send>(
        &mut self,
        name: &[u8],
        entry: crate::api::abi::Entry,
        is_instance: bool,
        reserved: Option<T>,
    ) -> Option<Function> {
        self.trampoline_made(
            |made| {
                matches!(made.via, crate::trampoline::Via::Native(made_entry)
                if std::ptr::fn_addr_eq(made_entry, entry))
                    && *made.decl.function.name.0 == *name
                    && made.decl.is_instance == is_instance
                    && match (&made.reserved, &reserved) {
                        (Some(made), Some(reserved)) => made.is(reserved),
                        (None, None) => true,
                        _ => false,
                    }
            },
            move |_| {
                let reserved = reserved.map(crate::trampoline::Reserved::new);
                Some(CallTrampoline::native(entry, name, is_instance, reserved))
            },
        )
    }

    /// What the native trampoline being served carries in `reserved[]`
    /// ([`Symbols::native_trampoline_reserving`]), where it is a `T`.
    pub(crate) fn active_reserved<T: std::any::Any + Copy>(&self) -> Option<T> {
        let function = self.active_native_function()?;
        let (made, _) = self.trampoline_of(function)?;
        made.reserved.as_ref()?.get::<T>()
    }

    /// Whether `function` is a native [`Symbols::native_trampoline`]
    /// made, php's `ZEND_ACC_CALL_VIA_TRAMPOLINE` on a function no
    /// magic method serves.
    pub(crate) fn via_native_trampoline(&self, function: Function) -> bool {
        self.trampoline_of(function)
            .is_some_and(|(made, _)| matches!(made.via, crate::trampoline::Via::Native(_)))
    }

    /// The function a `name(...)` node found earlier in this request,
    /// `ZEND_MAP_PTR_GET(fcc_ast->fptr)`.
    pub(crate) fn kept_callable(&self, unit: &php_unit::UnitCell, at: u32) -> Option<Function> {
        self.calls
            .function_callables
            .iter()
            .find(|(held, number, _)| held == unit && *number == at)
            .map(|(_, _, function)| *function)
    }

    /// `ZEND_MAP_PTR_SET(fcc_ast->fptr, fptr)`: the function the node
    /// found, kept for the rest of the request.
    pub(crate) fn keep_callable(&mut self, unit: &php_unit::UnitCell, at: u32, function: Function) {
        self.calls
            .function_callables
            .push((unit.clone(), at, function));
    }

    /// `zend_std_get_closure`: the class's `__invoke` with the object as
    /// its receiver and the class as its scope, and none for a class
    /// with no `__invoke`.
    pub(crate) fn invoke_closure(&mut self, object: PhpObject) -> Option<ClosureInfo> {
        let function = self.symbols().method_named(object.class(), b"__invoke")?;
        Some(ClosureInfo {
            function,
            this: Some(object),
            scope: Some(object.class()),
            called_scope: Some(object.class()),
            bound: None,
            fake: true,
        })
    }
}

impl Code<&Request> {
    /// The code this request is serving now, for a hook it supplies to
    /// be served as later ([`Code::serve_supplied`]): what an
    /// extension registers from its native or lifecycle hook, a stream
    /// wrapper or a filter, is called from wherever the program reaches
    /// it, and a debug build tags what it allocates there with the
    /// extension that supplied it, as it tags the extension's natives
    /// (#1993, #2113). A release build keeps nothing.
    #[must_use]
    pub fn supplier(self) -> Supplier {
        Supplier {
            #[cfg(debug_assertions)]
            origin: self.0.heap.origin(),
        }
    }
}

impl Code<&mut Request> {
    /// `serve` served as `supplier`'s code: in a debug build, what the
    /// heap allocates meanwhile is tagged with it, so a leak it leaves
    /// names it, and the origin standing before is put back after.
    pub fn serve_supplied<T>(self, supplier: Supplier, serve: impl FnOnce(&mut Request) -> T) -> T {
        #[cfg(debug_assertions)]
        let outer = self.0.heap.set_origin(supplier.origin);
        #[cfg(not(debug_assertions))]
        let _ = supplier;
        let served = serve(self.0);
        #[cfg(debug_assertions)]
        self.0.heap.set_origin(outer);
        served
    }

    /// `zend_fiber_switch_block()` before `f` and
    /// `zend_fiber_switch_unblock()` after it: while `f` is under way no
    /// fiber starts, suspends, resumes or is thrown into, each php's
    /// `FiberError`, `Cannot switch fibers in current execution context`,
    /// since a switch would leave `f` standing on a stack it does not
    /// return to. What a `ZEND_TICKS` round serves its functions under,
    /// and `pcntl_signal_dispatch` its handlers.
    pub fn with_fiber_switch_blocked<R>(self, f: impl FnOnce(&mut Request) -> R) -> R {
        self.0.fibers.block_switch();
        let out = f(self.0);
        self.0.fibers.unblock_switch();
        out
    }
}

impl Symbols<&Request> {
    /// Whether a closure's function is `ZEND_ACC_STATIC`: a closure
    /// written `static function`, or one made of a `static` method.
    ///
    /// ```php
    /// class F { static function s() {} function q() { return static function () {}; } }
    /// $r = fn ($c) => (new ReflectionFunction($c))->isStatic();
    /// var_dump($r(function () {}), $r(static function () {}), $r((new F)->q()), $r(Closure::fromCallable(['F', 's'])));
    /// ```
    /// ```expect
    /// bool(false)
    /// bool(true)
    /// bool(true)
    /// bool(true)
    /// ```
    #[must_use]
    pub fn closure_refuses_receiver(self, closure: &ClosureInfo) -> bool {
        if let Some(decl) = self.0.method_decl(closure.function) {
            return !decl.is_instance;
        }
        self.0
            .closure_code(closure.function)
            .is_some_and(|code| code.no_receiver)
    }

    /// The trampoline `class` answers under the lowered name, a method
    /// its `get_method` synthesises and its table has no row for: one
    /// an extension laid down with [`ExtensionBuilder::trampolines`],
    /// which is what a class's own `get_method` hands back for it.
    ///
    /// [`ExtensionBuilder::trampolines`]: crate::api::abi::ExtensionBuilder::trampolines
    #[must_use]
    pub fn trampoline(self, class: ClassId, lowered: &[u8]) -> Option<Function> {
        self.0.trampoline_named(class, lowered)
    }

    /// `ce->get_iterator` on a class that is no `Traversable`
    /// ([`ObjectHandlers::own_iterator`]): what Reflection calls
    /// iterable beside a `Traversable` class.
    ///
    /// [`ObjectHandlers::own_iterator`]: crate::api::abi::ObjectHandlers::own_iterator
    #[must_use]
    pub fn class_own_iterator(self, class: ClassId) -> bool {
        self.0.handlers_of(class).own_iterator()
    }
}

impl Symbols<&mut Request> {
    /// `zend_ffi_get_func`'s trampoline (`Z/ext/ffi/ffi.c:2951-2975`): a
    /// native named `name` whose body is `entry`, with no scope, every
    /// argument by value, made on a lookup by a class's
    /// [`ObjectHandlers::get_method`] or [`ObjectHandlers::get_closure`]
    /// for a name it learns only when a call names it; the one the
    /// request already made for that entry and name where it made one.
    /// The body reads its name off [`Frames::active_function_name`] and
    /// its arguments off [`Frames::argument`].
    ///
    /// [`ObjectHandlers::get_method`]: crate::api::abi::ObjectHandlers::get_method
    /// [`ObjectHandlers::get_closure`]: crate::api::abi::ObjectHandlers::get_closure
    pub fn native_trampoline(self, name: &[u8], entry: crate::api::abi::Entry) -> Option<Function> {
        self.0.native_trampoline_as::<()>(name, entry, true, None)
    }

    /// [`Symbols::native_trampoline`] carrying `reserved`, a value of
    /// the extension's own its body reads back with
    /// [`Frames::reserved`], as php's `internal_function.reserved[]`:
    /// what `zend_ffi_cdata_get_closure` makes (`Z/ext/ffi/ffi.c:2139`),
    /// the C function's type and address on the function, so the call
    /// holds no receiver and the `FFI\CData` the callee was read from is
    /// given up before the arguments are sent. One per entry, name and
    /// reserved value: a value equal to one the request made a trampoline
    /// with finds that one.
    ///
    /// [`Frames::reserved`]: crate::api::request::Frames::reserved
    pub fn native_trampoline_reserving<T: std::any::Any + Copy + PartialEq + Send>(
        self,
        name: &[u8],
        entry: crate::api::abi::Entry,
        reserved: T,
    ) -> Option<Function> {
        self.0
            .native_trampoline_as(name, entry, true, Some(reserved))
    }

    /// [`Symbols::native_trampoline`] for a call named on the class,
    /// `ZEND_ACC_STATIC` beside `ZEND_ACC_CALL_VIA_HANDLER`: what a
    /// class's [`ObjectHandlers::get_static_method`] answers for a name
    /// its table has no row for.
    ///
    /// [`ObjectHandlers::get_static_method`]: crate::api::abi::ObjectHandlers::get_static_method
    pub fn native_static_trampoline(
        self,
        name: &[u8],
        entry: crate::api::abi::Entry,
    ) -> Option<Function> {
        self.0.native_trampoline_as::<()>(name, entry, false, None)
    }
}

/// Whose code supplied a hook, as [`Code::supplier`] took it where
/// the hook was registered, for [`Code::serve_supplied`] to serve the
/// hook as: the extension a debug build tags what the hook allocates
/// with, and nothing in a release build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Supplier {
    #[cfg(debug_assertions)]
    origin: php_heap::Origin,
}

impl Supplier {
    /// The supplier an extension is.
    pub(crate) fn of(extension: ExtensionId) -> Supplier {
        #[cfg(not(debug_assertions))]
        let _ = extension;
        Supplier {
            #[cfg(debug_assertions)]
            origin: extension.origin(),
        }
    }
}

/// [`Request::user_function`] over the two tables it reads, for a caller
/// that holds the request's other fields at once.
#[inline]
pub(super) fn user_in<'a>(
    rows: &'a super::PinnedRows<UserFunction>,
    arena: &'a crate::arena::Arena,
    function: Function,
) -> Option<&'a UserFunction> {
    match function.local_at() {
        Some(at) => rows.get(at),
        None => arena.function(function),
    }
}
