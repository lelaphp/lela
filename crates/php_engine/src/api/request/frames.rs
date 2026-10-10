//! The frames of a request: the VM stack and the calls the VM
//! serves, a frame pushed for user code, a code frame or a native's
//! call, its slots read and written, the receivers it was lent, and the
//! frame left and popped; the fibers each with the stack that resumed
//! them, and the chain of frames and calls a backtrace walks. The stack
//! and the VM stay on the request, since every area reaches them.

use std::sync::Arc;

use php_heap::{Fatal, FrameRef, Slot, VmStack, Zval, ZvalRef};
use php_unit::{CacheSlot, Unit};

use crate::api::call::Function;
use crate::api::raising::Thrown;
use crate::api::values::{PhpObject, Value};
use crate::exec::{self, TableId, Vm};

use super::Request;

/// The links of one stack, innermost first: the frames `frames` numbers
/// and the calls `calls` numbers that are being served, each frame's
/// `calls_below` saying where among the calls it was pushed. A switch
/// ([`exec::Switch`]) links what stands above it to the frame it moved
/// `EG(current_execute_data)` to, so the frames and calls between are on
/// no chain while it stands, as php's `prev_execute_data` passes them.
pub(super) fn links_of<'a>(
    stack: &'a VmStack,
    vm: &'a Vm,
    frames: std::ops::Range<usize>,
    calls: std::ops::Range<usize>,
    expand: &dyn Fn(PhpObject) -> Vec<Link<'a>>,
) -> Vec<Link<'a>> {
    /// The walk up the stack, outermost first, reversed at the end.
    struct Walk<'a, 'e> {
        vm: &'a Vm,
        served: &'a [exec::CallState],
        end: usize,
        expand: &'e dyn Fn(PhpObject) -> Vec<Link<'a>>,
        links: Vec<Link<'a>>,
        /// The calls taken so far.
        taken: usize,
        /// Each frame walked past, with how many links stood when it was.
        ends: Vec<(usize, usize)>,
    }
    impl Walk<'_, '_> {
        /// The calls being served below `below`, from where the walk
        /// stands.
        fn take_below(&mut self, below: usize) {
            let below = below.min(self.end);
            while self.taken < below {
                if let Some(call) = self.served.get(self.taken)
                    && call.stage == exec::CallStage::Serving
                {
                    self.links.push(Link::Call(call));
                }
                self.taken += 1;
            }
        }

        /// What stands at `at` below the frame there: a placeholder,
        /// where a generator's frame stood, and the generators it
        /// expands into, outermost first here; a site; and a switch,
        /// which takes back off the walk every link above the frame it
        /// moved to.
        fn placed(&mut self, at: usize) {
            let vm = self.vm;
            for mark in vm.placeholders().iter().filter(|mark| mark.frame == at) {
                self.take_below(mark.calls);
                let expanded = (self.expand)(mark.generator);
                let generators = expanded.len();
                self.links.extend(expanded.into_iter().rev());
                self.links.push(Link::Fake(generators));
            }
            for site in vm.sites().iter().filter(|site| site.frame == at) {
                self.take_below(site.calls);
                self.links.push(Link::Site(site));
            }
            for switch in vm.switches().iter().filter(|switch| switch.frame == at) {
                self.take_below(switch.calls);
                let to = switch.to.0 as usize;
                if let Some(&(_, standing)) = self.ends.iter().rev().find(|(frame, _)| *frame == to)
                {
                    self.links.truncate(standing);
                }
            }
        }
    }
    let states = vm.frames();
    let mut walk = Walk {
        vm,
        served: vm.calls(),
        end: calls.end.min(vm.calls().len()),
        expand,
        links: Vec::new(),
        taken: calls.start,
        ends: Vec::new(),
    };
    for at in frames.clone() {
        walk.placed(at);
        let Some(state) = states.get(at) else {
            break;
        };
        walk.take_below(usize::try_from(state.calls_below).unwrap_or(usize::MAX));
        if let Some(generator) = state.generator {
            walk.links.extend(expand(generator).into_iter().rev());
        }
        if !state.leaving {
            walk.links.push(Link::Frame(Slots::Stack(stack, at), state));
        }
        walk.ends.push((at, walk.links.len()));
    }
    walk.placed(frames.end);
    walk.take_below(walk.end);
    let mut links = walk.links;
    links.reverse();
    links
}

/// One link of php's `prev_execute_data` chain as this engine holds it:
/// a user frame, with its place among the frames, or a call a native
/// is serving.
pub(super) enum Link<'a> {
    Frame(Slots<'a>, &'a exec::FrameState),
    Call(&'a exec::CallState),
    /// `zend_get_attribute_object`'s dummy frame ([`exec::Site`]): user
    /// code that serves no function, has no `$this` and no scope, and
    /// makes no trace row of its own.
    Site(&'a exec::Site),
    /// php's `execute_fake` placeholder where a delegating generator's
    /// frame stood ([`exec::Placeholder`]), the frame gone while what it
    /// held is released, so a destructor that release serves is called
    /// from it: no function and no row of its own, it stands for the
    /// kept frames of the generators the next links are, this many of
    /// them, and its `prev_execute_data` is the resumer below those.
    Fake(usize),
}

/// Where the slots of a frame on the chain are: on a VM stack at its
/// place, or kept by a suspended generator a running one was resumed
/// through, which php's `execute_fake` placeholder stands for until a
/// backtrace expands it.
#[derive(Clone, Copy)]
pub(super) enum Slots<'a> {
    Stack(&'a VmStack, usize),
    Kept(&'a php_heap::Frame),
}

impl<'a> Slots<'a> {
    pub(super) fn frame(self) -> Option<&'a php_heap::Frame> {
        match self {
            Slots::Stack(stack, at) => stack.frame(FrameRef(u32::try_from(at).unwrap_or(u32::MAX))),
            Slots::Kept(frame) => Some(frame),
        }
    }
}

impl Request {
    pub(crate) fn stack_mut(&mut self) -> &mut VmStack {
        &mut self.stack
    }

    /// [`VmStack::move_to_heap`] over the request's stack and heap.
    pub(crate) fn move_frame_to_heap(&mut self) -> Result<Option<php_heap::Frame>, Fatal> {
        self.stack.move_to_heap(&mut self.heap)
    }

    /// The first page of the VM stack the request runs on, out of its
    /// heap, where none stands: what `zend_fiber_execute` takes for a
    /// fiber's stack before the fiber's function is called.
    pub(crate) fn open_stack(&mut self) -> Result<(), Fatal> {
        self.stack.open(&mut self.heap)
    }

    /// The engine's half of the frames: what the loop and the handlers
    /// read (design §5.6).
    pub(crate) fn vm(&self) -> &Vm {
        &self.vm
    }

    pub(crate) fn vm_mut(&mut self) -> &mut Vm {
        &mut self.vm
    }

    /// The fibers running on this request (design §5.11).
    pub(crate) fn fibers(&self) -> &crate::fiber::Fibers {
        &self.fibers
    }

    pub(crate) fn fibers_mut(&mut self) -> &mut crate::fiber::Fibers {
        &mut self.fibers
    }

    /// The half of `zend_fiber_switch_context` that is the engine's:
    /// the execution state the request runs on and `other` change
    /// places, `zend_fiber_capture_vm_state` and
    /// `zend_fiber_restore_vm_state` as one swap. The VM stack carries
    /// the current frame and the entry slot with it, so neither is
    /// saved on its own; the chain of open observed frames goes with the
    /// stack its frames stand on, the swap php's observer makes at the
    /// switch's notice (`Z/Zend/zend_observer.c:423-424`).
    pub(crate) fn switch_execution(&mut self, other: &mut crate::fiber::Execution) {
        self.switch_stack(other);
        self.swap_error_reporting(&mut other.error_reporting);
        self.swap_stack_limit(&mut other.stack_limit);
        std::mem::swap(&mut self.fibers.active, &mut other.active);
    }

    /// The stack the request runs on and `other`'s change places, the
    /// frames' engine half and the chain of open observed frames with
    /// it; the execution left off the running chain keeps what it was
    /// lent ([`exec::Receiver`]).
    fn switch_stack(&mut self, other: &mut crate::fiber::Execution) {
        self.own_lent_receivers();
        std::mem::swap(&mut self.stack, &mut other.stack);
        std::mem::swap(&mut self.vm, &mut other.vm);
        self.swap_observing(&mut other.observing);
    }

    /// `f` served with the request running on the `stack`-th stack
    /// down the chain [`Request::executions`] walks, as a debugger
    /// serves a frame below a fiber with `EG(current_execute_data)`
    /// moved there: that stack is the running one for `f`, the fibers
    /// above it set aside, so the chain runs from it into the stacks
    /// below, and its frames are the ones a [`FrameRef`] names. The
    /// error-reporting level and the machine stack's limit stay the
    /// running fiber's, as php moves neither, and no fiber starts,
    /// suspends or resumes meanwhile, since `f` is served on the
    /// running fiber's machine stack. `None` where no stack stands
    /// there.
    pub(super) fn on_stack<R>(
        &mut self,
        stack: usize,
        f: impl FnOnce(&mut Request) -> R,
    ) -> Option<R> {
        if stack == 0 {
            return Some(f(self));
        }
        let mut other = std::mem::take(self.fibers.set_aside_above(stack)?);
        self.switch_stack(&mut other);
        let served = self.with_fiber_switch_blocked(f);
        self.switch_stack(&mut other);
        if let Some(slot) = self.fibers.set_aside_execution() {
            *slot = other;
        }
        self.fibers.put_back();
        Some(served)
    }

    /// The stacks php's one `prev_execute_data` chain runs through: the
    /// one the request is running on, then each resumer's below it, the
    /// request's own last.
    pub(super) fn executions(&self) -> impl Iterator<Item = (&VmStack, &Vm)> {
        std::iter::once((&self.stack, &self.vm)).chain(
            self.fibers
                .below()
                .map(|execution| (&execution.stack, &execution.vm)),
        )
    }

    /// The innermost user frame on the chain, which is where
    /// `zend_get_executed_filename` and `zend_get_executed_lineno` stop:
    /// on a fiber whose callable is a native, the resumer's.
    pub(super) fn innermost_frame(&self) -> Option<&exec::FrameState> {
        self.executions().find_map(|(_, vm)| vm.innermost())
    }

    /// The loop over a generator's frame, bound to the engine's tier
    /// ([`crate::tier::resume`]).
    pub(crate) fn resume_loop(&self) -> fn(&mut Request, FrameRef) -> Result<(), Fatal> {
        self.shared.loops.resume
    }

    /// Push a frame for `unit`'s op array: its shape on the VM stack and
    /// its engine state, which are pushed and popped together and share a
    /// `FrameRef`. A VM stack whose page the memory limit refuses is
    /// the fatal php raises there, and nothing is pushed. The frame is
    /// not begun: its observer begin is the caller's, which `execute`
    /// raises as `zend_execute` does.
    pub(crate) fn push_frame(&mut self, unit: Arc<Unit>) -> Result<FrameRef, Fatal> {
        let code = exec::code_of(&unit);
        let slots = usize::try_from(code.num_cvs.saturating_add(code.num_tmps)).unwrap_or(0);
        let frame = self.stack.push_frame(&mut self.heap, slots, 0)?;
        exec::init_runtime_cache(&mut self.map, code, exec::Body::Main);
        self.forget_main_statics(code.runtime_cache);
        // `zend_execute` gives the main op array's frame `EG(symbol_table)`
        // and `i_init_code_execute_data` attaches it, so every CV name
        // stands in the table, in the compiler's order, from the start.
        self.vm.push(exec::Activation {
            symbol_table: Some(TableId::GLOBAL),
            ..exec::Activation::of(unit, exec::Body::Main)
        });
        self.attach_symbol_table(frame);
        Ok(frame)
    }

    /// The frame for the main op array of a file an `INCLUDE_OR_EVAL`
    /// opened, `zend_vm_stack_push_call_frame` under
    /// `ZEND_CALL_NESTED_CODE | ZEND_CALL_HAS_SYMBOL_TABLE` and
    /// `i_init_code_execute_data` (`Z/Zend/zend_vm_def.h:6590`,
    /// `Z/Zend/zend_execute.c:4504`): the includer's receiver and called
    /// scope carried over, the receiver with one more owner since the pop
    /// gives one up, the includer's scope written onto the op array
    /// (`new_op_array->scope`), and the includer's symbol table shared,
    /// rebuilt for it first where it is a function frame with none
    /// ([`Request::rebuild_symbol_table`]); then the new frame attached
    /// to it, so the included code reads the includer's variables.
    /// `caller` is `prev_execute_data`, the frame whose opline is the
    /// `INCLUDE_OR_EVAL`, and `returns_to` its `VAR` where the value is
    /// read. The push comes first, as php's does, so a VM stack page the
    /// memory limit refuses is the fatal and nothing else is touched.
    pub(crate) fn push_code_frame(
        &mut self,
        unit: Arc<Unit>,
        caller: FrameRef,
        returns_to: Option<(FrameRef, u32)>,
    ) -> Result<FrameRef, Fatal> {
        let code = exec::code_of(&unit);
        let slots = usize::try_from(code.num_cvs.saturating_add(code.num_tmps)).unwrap_or(0);
        let frame = self.stack.push_frame(&mut self.heap, slots, 0)?;
        let (this, called_scope, scope) = self.vm.at(caller).map_or((None, None, None), |state| {
            (state.this(), state.called_scope, self.frame_scope(caller))
        });
        // `ZEND_CALL_HAS_THIS` carried from the including frame, whose
        // `$this` that frame keeps: no `ZEND_CALL_RELEASE_THIS`.
        let receiver = this.map(exec::Receiver::lent);
        let table = self.rebuild_symbol_table(caller);
        exec::init_runtime_cache(&mut self.map, code, exec::Body::Main);
        self.forget_main_statics(code.runtime_cache);
        self.vm.push(exec::Activation {
            receiver,
            called_scope,
            scope,
            symbol_table: Some(table),
            returns_to,
            caller: exec::Caller::Opline(caller),
            ..exec::Activation::of(unit, exec::Body::Main)
        });
        self.attach_symbol_table(frame);
        crate::observer::call(self, frame);
        Ok(frame)
    }

    /// The op array `frame` serves held on the request's heap as php's
    /// `INCLUDE_OR_EVAL` holds it ([`exec::HeapOpArray`]): `copy`, made
    /// before the frame was pushed, and the runtime cache
    /// `i_init_code_execute_data` lays down after the symbol table is
    /// attached, `cache_size` bytes. Both go when the frame is popped,
    /// whether the file returned or threw, and an `eval`'s as an
    /// include's:
    ///
    /// ```php
    /// $f = __DIR__ . '/' . bin2hex(random_bytes(8)) . '.php';
    /// file_put_contents($f, '<?php if ($t) throw new Exception("x"); $y = 2; return $y;'); touch($f, time() - 10); clearstatcache();
    /// $t = false; include $f; $t = true; try { include $f; } catch (Exception $e) {} unset($e);
    /// $m = memory_get_usage(); try { include $f; } catch (Exception $e) {} unset($e); echo memory_get_usage() - $m, "\n";
    /// $m = memory_get_usage(); $t = false; $r = include $f; echo memory_get_usage() - $m, "\n";
    /// $m = memory_get_usage(); $r = eval('$q = 1; return $q;'); echo memory_get_usage() - $m, "\n";
    /// unlink($f);
    /// ```
    /// ```expect regime=jit-off
    /// 0
    /// 0
    /// 0
    /// ```
    ///
    /// The `Err` is the memory limit, the copy held already.
    pub(crate) fn hold_op_array(
        &mut self,
        frame: FrameRef,
        copy: crate::api::values::Emalloc,
    ) -> Result<(), Thrown> {
        let size = self
            .vm
            .serving(frame)
            .and_then(|serving| serving.code().map(|code| code.cache_size))
            .unwrap_or(0);
        if let Some(state) = self.vm.at_mut(frame) {
            state.op_array = Some(exec::HeapOpArray {
                copy,
                run_time_cache: None,
            });
        }
        let cache =
            crate::api::values::Emalloc::new(self, usize::try_from(size).unwrap_or(usize::MAX))?;
        if let Some(held) = self
            .vm
            .at_mut(frame)
            .and_then(|state| state.op_array.as_mut())
        {
            held.run_time_cache = Some(cache);
        }
        Ok(())
    }

    /// `zend_leave_helper`'s arm for a nested code frame
    /// (`Z/Zend/zend_vm_def.h:3030`): the frame's CVs detached into the
    /// table it shares with its includer, the frame popped, and the
    /// includer, `EX(prev_execute_data)`, attached to the table again, so
    /// what the included file wrote to a variable of the includer's is in
    /// the includer's CV when the loop goes on there. The includer is the
    /// frame below for an include; for code evaluated in a frame further
    /// down, as a debugger evaluates in a paused frame
    /// ([`Request::eval_in_frame`]), it is that frame, and the frames
    /// above it are not attached. A function frame is popped alone, as
    /// `zend_leave_helper`'s other arms pop one.
    pub(crate) fn leave_frame(&mut self, frame: FrameRef) {
        let code_frame = self
            .vm
            .at(frame)
            .filter(|state| state.function.is_none())
            .map(|state| state.caller.frame());
        if code_frame.is_some() {
            self.detach_symbol_table(frame);
        }
        self.pop_frame();
        if let Some(includer) = code_frame {
            let includer = includer.unwrap_or_else(|| self.stack.current());
            self.attach_symbol_table(includer);
        }
    }

    /// `EX(func)`: the function `frame` serves, `None` for an included,
    /// evaluated or main file's op array, which is no function.
    pub(crate) fn frame_function(&self, frame: FrameRef) -> Option<Function> {
        self.vm.at(frame).and_then(|state| state.function)
    }

    /// `CACHED_PTR(num)`: what the runtime cache of the op array `frame`
    /// serves holds at `slot`, in this request's map table
    /// ([`exec::Vm::cached`]).
    pub(crate) fn cached(&self, frame: FrameRef, slot: CacheSlot) -> exec::Cached {
        self.vm.cached(&self.map, frame, slot)
    }

    /// `CACHE_PTR(num, ptr)`: `value` into the runtime cache of the op
    /// array `frame` serves at `slot`, in this request's map table
    /// ([`exec::Vm::prime`]), where every pass after reads it.
    pub(crate) fn prime(&mut self, frame: FrameRef, slot: CacheSlot, value: exec::Cached) {
        self.vm.prime(&mut self.map, frame, slot, value);
    }

    /// `CACHED_PTR(num) == ce ? CACHED_PTR(num + sizeof(void *)) : NULL`
    /// for a method call's pair (`Z/Zend/zend_vm_def.h:3670`, `:3778`):
    /// the callee [`exec::Cached::ClassAndCallee`] at `slot` holds where
    /// it holds `class` and was written from `frame`'s scope, and none
    /// otherwise, which is the pass that looks the method up.
    pub(crate) fn cached_callee(
        &self,
        frame: FrameRef,
        slot: CacheSlot,
        class: crate::api::values::ClassId,
    ) -> Option<Function> {
        match self.cached(frame, slot) {
            exec::Cached::ClassAndCallee {
                class: held,
                scope,
                function,
            } if held == class && scope == self.frame_scope(frame) => Some(function),
            _ => None,
        }
    }

    /// `CACHE_POLYMORPHIC_PTR(num, ce, fbc)` (`Z/Zend/zend_vm_def.h:3694`,
    /// `:3818`): `class` and the callee it answered into `slot`, with
    /// `frame`'s scope as the key [`Request::cached_callee`] reads back.
    pub(crate) fn prime_callee(
        &mut self,
        frame: FrameRef,
        slot: CacheSlot,
        class: crate::api::values::ClassId,
        function: Function,
    ) {
        let scope = self.frame_scope(frame);
        self.prime(
            frame,
            slot,
            exec::Cached::ClassAndCallee {
                class,
                scope,
                function,
            },
        );
    }

    /// Push a frame for the body of the user function `function`, the
    /// call that reached it served (`zend_vm_stack_push_call_frame` and
    /// `i_init_func_execute_data`, design §5.6): the CVs and the
    /// temporaries as slots, the arguments the call placed written into
    /// the first CVs, which are the parameters, the ones past the
    /// declared parameters kept beside the frame, and the receiver and
    /// the called scope carried over; `returns_to` is the caller's slot
    /// for what the frame returns and `caller` the frame whose opline
    /// pushed the call, `None` where a native did. The frame's room on
    /// the VM stack is php's `zend_vm_calc_used_stack`, the extra
    /// arguments' slots with it, and a page the memory limit refuses is
    /// the fatal php raises at the push; a function that is no user
    /// function is an internal error.
    pub(crate) fn push_user_frame(
        &mut self,
        function: Function,
        call: exec::CallState,
        returns_to: Option<(FrameRef, u32)>,
        caller: exec::Caller,
    ) -> Result<FrameRef, Fatal> {
        let no_body = || {
            Fatal::Error(String::from(
                "InternalError: a user function whose body the unit has not",
            ))
        };
        // The declared parameters, which is php's `common.num_args` and
        // excludes a variadic: an argument from the variadic's position
        // on is an extra one, where php leaves it on the stack for
        // `RECV_VARIADIC` to collect.
        let (unit, body, arity) = {
            let user = self.user_function(function).ok_or_else(no_body)?;
            (Arc::clone(&user.unit), user.body, user.arity)
        };
        let code = exec::body_of(&unit, body).ok_or_else(no_body)?;
        let slots = usize::try_from(code.num_cvs.saturating_add(code.num_tmps)).unwrap_or(0);
        let parameters = usize::try_from(arity.parameters).unwrap_or(usize::MAX);
        let entry = code.safepoints.first().map_or(0, |at| at.0);
        let num_args = u32::try_from(call.arguments.len()).unwrap_or(u32::MAX);
        let extra_args = call.arguments.len().saturating_sub(parameters);
        let frame = self.stack.push_frame(&mut self.heap, slots, extra_args)?;
        exec::init_runtime_cache(&mut self.map, code, body);
        // The parameters' CVs of the frame just pushed hold `UNDEF`, so
        // each argument is written over nothing, as `SEND_*` writes into
        // the callee's slot; the rest are the extra arguments.
        let mut arguments = call.arguments;
        let mut placing = arguments.drain(..);
        if let Some(slots) = self.stack.frame_mut(frame) {
            let cells = slots.slots_mut().iter_mut().take(parameters);
            for (cell, argument) in cells.zip(placing.by_ref()) {
                *cell = argument.to_zval();
            }
        }
        let extra_args: Vec<Value> = placing.collect();
        self.vm.keep_arguments(arguments);
        let opline =
            exec::skipped_recv(num_args, arity.parameters, arity.typed, entry).unwrap_or(0);
        self.vm.push(exec::Activation {
            function: Some(function),
            opline,
            receiver: call.receiver,
            called_scope: call.called_scope,
            closure: call.closure,
            num_args,
            extra_args,
            extra_named: call.extra_named,
            returns_to,
            caller,
            ..exec::Activation::of(unit, body)
        });
        if let Some(name) = call.name {
            self.release(Value::string(name));
        }
        for coerced in call.coerced {
            self.release(coerced);
        }
        crate::observer::call(self, frame);
        Ok(frame)
    }

    /// The frame off both stacks, every owner its slots held given up
    /// while the frame still stands, `i_free_compiled_variables` over
    /// the CVs, each a possible root of the cycle collector as php's
    /// `i_zval_ptr_dtor` makes it, and the temporaries after them as
    /// `FREE_OP` gives one up, since a temporary the compiler numbered
    /// for an opline's value and nothing read is still an owner here;
    /// then, in `zend_leave_helper`'s order, the extra named arguments,
    /// the extra arguments (`zend_vm_stack_free_extra_args`, each a
    /// possible root as `i_zval_ptr_dtor` makes it, so a cycle a
    /// collection in the callee found reachable through one is found
    /// again), the receiver (`ZEND_CALL_RELEASE_THIS`) and the closure, all of
    /// them while the frame still stands on both stacks, as php's
    /// `vm_stack_top` is only put back after them, so a destructor one
    /// of them serves pushes its frames above it; then
    /// `zend_vm_stack_free_call_frame`. The frame is off the chain a
    /// backtrace reads from the first release on, as php's
    /// `EG(current_execute_data)` is the caller's by then, so a
    /// destructor served from here is called from the caller's line:
    ///
    /// ```php
    /// class D { function __destruct() { debug_print_backtrace(); } }
    /// function f($x) { $d = new D; }
    /// f(new D);
    /// ```
    /// ```expect
    /// #0 %s(4): D->__destruct()
    /// #0 %s(4): D->__destruct()
    /// ```
    ///
    /// ```php
    /// function &foo() { $a = []; $a[] =& $a; return $a; }
    /// function bar() { var_dump(gc_collect_cycles()); }
    /// bar(foo());
    /// var_dump(gc_collect_cycles());
    /// ```
    /// ```expect
    /// int(0)
    /// int(1)
    /// ```
    ///
    /// A frame a `zend_bailout` leaves is not unwound in php: the
    /// `zend_try` that catches it puts `vm_stack_top` back, and what the
    /// frame held is held on until shutdown destructs the objects and the
    /// heap's end discards the rest. So none of it is given up here then:
    /// what the frame held stands for a shutdown function to see, and a
    /// client's call caught over its callback's fatal destructs what the
    /// callback held at the end (`ext/soap`'s witness).
    ///
    /// ```php
    /// function f() { $o = new stdClass; $GLOBALS['w'] = WeakReference::create($o); trigger_error("x", E_USER_ERROR); }
    /// register_shutdown_function(function () { var_dump($GLOBALS['w']->get() !== null); });
    /// f();
    /// ```
    /// ```expect exit=255
    /// Deprecated: Passing E_USER_ERROR to trigger_error() is deprecated since 8.4, throw an exception or call exit with a string message instead in %s on line 2
    ///
    /// Fatal error: x in %s on line 2
    /// bool(true)
    /// ```
    pub(crate) fn pop_frame(&mut self) {
        let frame = self.stack.current();
        if let Some(state) = self.vm.at_mut(frame) {
            state.leaving = true;
        }
        if !self.bailout {
            self.release_frame(frame);
        }
        // `destroy_op_array`, which frees the heap runtime cache, then
        // `efree_size(EX(func))`.
        let Some(mut popped) = self.vm.pop() else {
            self.stack.pop_frame(&mut self.heap);
            return;
        };
        if let Some(held) = popped.op_array.take() {
            if let Some(cache) = held.run_time_cache {
                cache.free(self);
            }
            held.copy.free(self);
        }
        self.stack.pop_frame(&mut self.heap);
        // An `eval`'s main op array is destroyed when its execution ends,
        // and the names its compile made with it.
        if popped.function.is_none() && popped.body == exec::Body::Main && !self.bailout {
            self.release_unit_names(Some(&popped.unit));
        }
    }

    /// What [`Request::pop_frame`] gives up of `frame`, in its order.
    fn release_frame(&mut self, frame: FrameRef) {
        let slots = self.stack.frame(frame).map_or(0, php_heap::Frame::len);
        let cvs = self
            .vm
            .at(frame)
            .and_then(|state| exec::body_of(&state.unit, state.body))
            .map_or(slots, |code| code.num_cvs as usize);
        let mut from = 0;
        while let Some((at, value)) = self.take_next_owner(frame, from) {
            if at < cvs {
                self.release(value);
            } else {
                self.release_nogc(value);
            }
            from = at.saturating_add(1);
        }
        self.free_rebuilt_table(frame);
        // A generator's frame leaves its closure to the generator
        // (`ZendGenerator::closure`), which keeps it until it is freed.
        let held = self.vm.at_mut(frame).map(|state| {
            (
                std::mem::take(&mut state.extra_named),
                std::mem::take(&mut state.extra_args),
                state.receiver.take(),
                state.closure.take().filter(|_| state.generator.is_none()),
            )
        });
        if let Some((named, extra_args, receiver, closure)) = held {
            self.release_named(named);
            // `zend_vm_stack_free_extra_args_ex`'s `i_zval_ptr_dtor`: an
            // extra argument that keeps owners after the release is a
            // possible root, so a cycle a collection in the callee found
            // reachable through it is found again.
            for extra in extra_args {
                self.release(extra);
            }
            if let Some(receiver) = receiver {
                self.give_up_receiver(receiver);
            }
            if let Some(closure) = closure {
                self.release(closure.as_value());
            }
        }
    }

    /// The first slot of `frame` from `from` on that holds an owner, and
    /// what it held, the slot left `UNDEF`: the walk of a frame being
    /// popped, which gives up each owner in slot order, the frame looked
    /// up again after each release since the release may serve PHP
    /// code. A slot that owns nothing is passed over in the same lookup
    /// and left as it stands, as `i_free_compiled_variables` passes over
    /// what `Z_REFCOUNTED_P` refuses.
    fn take_next_owner(&mut self, frame: FrameRef, from: usize) -> Option<(usize, Value)> {
        let slots = self.stack.frame_mut(frame)?.slots_mut();
        let (at, cell) = slots
            .iter_mut()
            .enumerate()
            .skip(from)
            .find(|(_, cell)| cell.is_refcounted())?;
        Some((at, Value::of_zval(std::mem::replace(cell, Zval::UNDEF))))
    }

    /// The value in a slot of `frame`, a view of it: `ZVAL_COPY_VALUE`,
    /// no owner taken, off the heap's frame (design §5.6). A slot past
    /// the end reads as `UNDEF`, as an uninitialised zval reads.
    pub(crate) fn slot(&self, frame: FrameRef, at: u32) -> Value {
        self.stack
            .frame(frame)
            .map_or(Value::undef(), |f| Value::of_zval(f.slot(Slot(at))))
    }

    /// Every slot of `frame` in place ([`php_heap::Frame::slots_mut`]),
    /// the CVs and then the temporaries: what a handler's inline path
    /// addresses its operands and its temporary through, one lookup of
    /// the frame for all of them (design §5.8). Empty for a frame that
    /// is not live, where every operand reads as `UNDEF` and the inline
    /// path stands aside for the kernel.
    pub(crate) fn slots_mut(&mut self, frame: FrameRef) -> &mut [Zval] {
        self.stack
            .frame_mut(frame)
            .map_or(&mut [], php_heap::Frame::slots_mut)
    }

    /// The value out of a slot, the slot left `UNDEF`: what the slot
    /// owned is the caller's now, which is how a temporary is consumed.
    pub(crate) fn take_slot(&mut self, frame: FrameRef, at: u32) -> Value {
        let Some(f) = self.stack.frame_mut(frame) else {
            return Value::undef();
        };
        let z = f.slot(Slot(at));
        f.set(Slot(at), Zval::UNDEF);
        Value::of_zval(z)
    }

    /// `value` into a slot, the slot owning it from here, and what the
    /// slot held handed back for the caller to release; a slot past the
    /// end takes nothing and hands `value` back.
    ///
    /// php writes an opline's `TMP` with `ZVAL_COPY_VALUE` and never
    /// releases what stood there, because a live temporary is never
    /// written over: the handler that reads one frees it or moves it out.
    /// Here the write hands the old value back to be released, so a `TMP`
    /// whose owner a handler moved without emptying the slot is released
    /// a second time by the next write (#723). A debug build stops that
    /// write: the `TMP` the opline the frame stands at writes, found
    /// holding an owner, is recorded for the loop's next sensing point
    /// ([`Request::sense_dirty_temporary`]). A scalar left there is no owner and no second release, and
    /// php writes over one itself: `JMPZ_EX` and `JMPNZ_EX` write the `TMP`
    /// the `BOOL` after them writes again.
    pub(crate) fn set_slot(&mut self, frame: FrameRef, at: u32, value: Value) -> Value {
        let Some(f) = self
            .stack
            .frame_mut(frame)
            .filter(|f| (at as usize) < f.len())
        else {
            return value;
        };
        let old = f.slot(Slot(at));
        f.set(Slot(at), value.to_zval());
        #[cfg(debug_assertions)]
        if old.is_refcounted() {
            self.sense_dirty_temporary(frame, at, old.kind());
        }
        Value::of_zval(old)
    }

    /// [`Request::set_slot`] and what the slot held released after the
    /// write, the `zval_ptr_dtor` php's assignment over a slot ends with.
    pub(crate) fn replace_slot(&mut self, frame: FrameRef, at: u32, value: Value) {
        let previous = self.set_slot(frame, at, value);
        self.release(previous);
    }

    /// The zval in a slot of `frame` in place, php's `EX_VAR(var)`
    /// (design §5.6): what a `FETCH_*_W` points its `VAR` at, and what
    /// the `ASSIGN*` over that `VAR` reads through when an `IS_INDIRECT`
    /// stands there (`_get_zval_ptr_ptr_var`). None for a slot the
    /// frame does not have.
    pub(crate) fn slot_ptr(&mut self, frame: FrameRef, at: u32) -> Option<ZvalRef> {
        self.stack.frame_mut(frame)?.slot_ptr(Slot(at))
    }

    /// A call's or a frame's receiver given up: the owner an owned one
    /// holds released, which may free the object, and the pin a lent one
    /// holds let go, which gives back the bytes of an object freed while
    /// it stood.
    pub(crate) fn give_up_receiver(&mut self, receiver: exec::Receiver) {
        match receiver {
            exec::Receiver::Owned(object) => self.release(object.as_value()),
            exec::Receiver::Lent(object) => self.heap.unpin(object.heap()),
        }
    }

    /// A lent receiver made an owned one, what a frame that leaves the
    /// running chain does with what it was lent: one owner taken, then
    /// the pin let go.
    pub(crate) fn own_receiver(&mut self, object: PhpObject) {
        self.addref(&object.as_value());
        self.heap.unpin(object.heap());
    }

    /// Every lent receiver of the running execution made an owned one,
    /// what a fiber switch does to the execution it leaves.
    pub(crate) fn own_lent_receivers(&mut self) {
        for held in self.vm.own_lent() {
            self.own_receiver(held);
        }
    }

    /// The served call off the stack, `zend_vm_stack_free_args` over what
    /// the `SEND_*` oplines placed and `zend_vm_stack_free_call_frame`
    /// after it. A native's argument is given up as php's `nogc` gives it
    /// up and is no possible root of the cycle collector, where a user
    /// function's, a CV of its frame, is one when the frame is left.
    ///
    /// ```php
    /// $o = new stdClass;
    /// var_dump(is_object($o), gc_status()["roots"]);
    /// function f($x) {}
    /// f($o);
    /// var_dump(gc_status()["roots"]);
    /// ```
    /// ```expect
    /// bool(true)
    /// int(0)
    /// int(1)
    /// ```
    pub(crate) fn pop_call(&mut self) {
        if let Some(call) = self.vm.pop_call() {
            // `zend_vm_stack_free_args`, whose `zval_ptr_dtor_nogc` makes
            // no argument a possible root; the receiver and the closure
            // after them are `OBJ_RELEASE`, which does.
            for value in call.coerced {
                self.release_nogc(value);
            }
            let mut arguments = call.arguments;
            for value in arguments.drain(..) {
                self.release_nogc(value);
            }
            self.vm.keep_arguments(arguments);
            if let Some(name) = call.name {
                self.release(Value::string(name));
            }
            if let Some(receiver) = call.receiver {
                self.give_up_receiver(receiver);
            }
            if let Some(closure) = call.closure {
                self.release(closure.as_value());
            }
            self.release_named(call.extra_named);
        }
    }

    /// `zend_free_extra_named_params`: each name and value of a call's
    /// or a frame's extra named arguments given up.
    /// A call or a frame that carries none, nearly every one, is told
    /// apart where the caller stands and gives up nothing.
    #[inline]
    pub(crate) fn release_named(&mut self, named: exec::ExtraNamed) {
        if named.carries_any() {
            self.release_named_rows(named);
        }
    }

    /// [`Request::release_named`] over what carries a row or a table.
    #[inline(never)]
    fn release_named_rows(&mut self, named: exec::ExtraNamed) {
        for (name, value) in named.free(self) {
            self.release(Value::string(name));
            self.release(value);
        }
    }

    /// `EG(current_execute_data)`: whether a frame or a native's call
    /// stands, or the bottom frame of a coroutine's stack, a
    /// `Fiber`'s or an extension's, which is every release inside a
    /// serving and none of the
    /// releases `php_request_shutdown` makes after the main frame popped.
    /// A frame being popped is not one: `zend_leave_helper` has made its
    /// caller current before it frees the CVs (`Z/Zend/zend_vm_def.h:2970`),
    /// so a destructor a top frame's last release serves at shutdown
    /// finds nothing executing, and what it raises is the uncaught fatal
    /// `zend_call_function`'s tail renders there, the throwable the
    /// frame was left with never chained under it.
    ///
    /// ```php
    /// class C {
    ///     public function __destruct() {
    ///         static $n = 0;
    ///         if (++$n === 1) { $c = new C; }
    ///         throw new Exception("from destructor $n");
    ///     }
    /// }
    /// $c = new C;
    /// ```
    /// ```expect exit=255
    /// Fatal error: Uncaught Exception: from destructor 2 in %s:6
    /// Stack trace:
    /// #0 [internal function]: C->__destruct()
    /// #1 {main}
    ///   thrown in %s on line 6
    /// ```
    pub fn executing(&self) -> bool {
        self.fibers.on_context_stack()
            || self.vm.frames().iter().any(|frame| !frame.leaving)
            || self.vm.serving_call().is_some()
    }
}
