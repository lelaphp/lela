//! The frames of a request: the VM stack and the calls the VM
//! serves, a frame pushed for user code, a code frame or a native's
//! call, its slots read and written, the receivers it was lent, and the
//! frame left and popped; the fibers each with the stack that resumed
//! them, and the chain of frames and calls a backtrace walks. The stack
//! and the VM stay on the request, since every area reaches them.

use php_heap::{Fatal, FrameRef, Slot, Zval, ZvalRef, ZvalType};
use php_unit::{CacheSlot, PublishedUnit, Unit};

use crate::api::call::Function;
use crate::api::raising::Thrown;
use crate::api::values::{PhpObject, Value};
use crate::exec::{self, TableId, Vm};

use super::Request;

/// The links of one stack, innermost first: the records `records`
/// numbers that are on php's chain, every user frame and every native's
/// call being served, a call still being built and a frame being popped
/// passed over. A switch ([`exec::Switch`]) links what stands above it
/// to the frame it moved `EG(current_execute_data)` to, so the records
/// between are on no chain while it stands, as php's
/// `prev_execute_data` passes them.
///
/// The walk is read as php walks `prev_execute_data`, from the
/// innermost record down and only as far as its reader goes, so a
/// reader that stops at the first link it needs, the executing scope
/// among them (`zend_get_executed_scope`), costs what that link costs
/// whatever the depth of the stack.
pub(super) fn links_of<'a>(
    req: &'a Request,
    vm: &'a Vm,
    records: std::ops::Range<usize>,
    expand: Expand,
) -> Links<'a> {
    let end = records.end.min(vm.depth());
    Links {
        req,
        vm,
        start: records.start,
        next: if records.start <= end {
            Step::Placed(end)
        } else {
            Step::Done
        },
        expand,
        pending: Vec::new(),
    }
}

/// What a generator's frame on the walk, or a placeholder where one
/// stood, expands into: the kept frames of the generators it runs
/// through ([`Request::placeholder_links`]), or nothing, where the
/// reader put those frames on the stack itself
/// ([`crate::generator::with_frames`]).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum Expand {
    Generators,
    Nothing,
}

/// [`links_of`]'s walk: the place it reads next, counting down, and the
/// links of the place it read that it has not handed out yet.
pub(super) struct Links<'a> {
    req: &'a Request,
    vm: &'a Vm,
    start: usize,
    next: Step,
    expand: Expand,
    /// The links of the last place read, outermost first, handed out
    /// from the end.
    pending: Vec<Link<'a>>,
}

/// One place of a stack's walk: what stands at a place below the
/// record there ([`Step::Placed`]), placeholders, sites and switches;
/// or the record itself.
#[derive(Clone, Copy)]
enum Step {
    Placed(usize),
    Record(usize),
    Done,
}

impl<'a> Links<'a> {
    fn expanded(&self, generator: PhpObject) -> Vec<Link<'a>> {
        match self.expand {
            Expand::Generators => self.req.placeholder_links(generator),
            Expand::Nothing => Vec::new(),
        }
    }

    /// What stands at `at` below the record there, outermost first into
    /// `pending`: a placeholder, where a generator's frame stood, after
    /// the generators it expands into; then a site.
    fn place(&mut self, at: usize) {
        let vm = self.vm;
        for mark in vm.placeholders().iter().filter(|mark| mark.frame == at) {
            let expanded = self.expanded(mark.generator);
            let generators = expanded.len();
            self.pending.extend(expanded.into_iter().rev());
            self.pending.push(Link::Fake(generators));
        }
        for site in vm.sites().iter().filter(|site| site.frame == at) {
            self.pending.push(Link::Site(site));
        }
    }

    /// The frame a switch standing at `at` moved the chain to, where the
    /// walk reaches it below: the lowest, where several stand there.
    fn switched(&self, at: usize) -> Option<usize> {
        self.vm
            .switches()
            .iter()
            .filter(|switch| switch.frame == at)
            .map(|switch| switch.to.0 as usize)
            .filter(|to| (self.start..at).contains(to))
            .min()
    }
}

impl<'a> Iterator for Links<'a> {
    type Item = Link<'a>;

    fn next(&mut self) -> Option<Link<'a>> {
        loop {
            if let Some(link) = self.pending.pop() {
                return Some(link);
            }
            match self.next {
                Step::Done => return None,
                Step::Placed(at) => {
                    if let Some(to) = self.switched(at) {
                        self.next = Step::Record(to);
                        continue;
                    }
                    self.next = match at.checked_sub(1) {
                        Some(below) if below >= self.start => Step::Record(below),
                        _ => Step::Done,
                    };
                    self.place(at);
                }
                Step::Record(at) => {
                    self.next = Step::Placed(at);
                    let Some(frame) = u32::try_from(at)
                        .ok()
                        .and_then(|at| self.vm.stack.frame(FrameRef(at)))
                    else {
                        continue;
                    };
                    let state = exec::FrameAt {
                        req: self.req,
                        frame,
                    };
                    let info = state.info();
                    if info.serving_native() && !info.leaving() {
                        return Some(Link::Call(state));
                    }
                    if !info.user() {
                        continue;
                    }
                    let link =
                        (!info.leaving()).then_some(Link::Frame(Slots::Stack(self.vm, at), state));
                    match state
                        .generator()
                        .filter(|_| self.expand == Expand::Generators)
                    {
                        None => {
                            if link.is_some() {
                                return link;
                            }
                        }
                        Some(generator) => {
                            let expanded = self.expanded(generator);
                            self.pending.extend(expanded.into_iter().rev());
                            self.pending.extend(link);
                        }
                    }
                }
            }
        }
    }
}

/// One link of php's `prev_execute_data` chain as this engine holds it:
/// a user frame, with its place among the frames, or a call a native
/// is serving.
pub(super) enum Link<'a> {
    Frame(Slots<'a>, exec::FrameAt<'a>),
    Call(exec::FrameAt<'a>),
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

/// Where a frame on the chain is: on its VM stack at its place, or kept
/// by a suspended generator a running one was resumed through, which
/// php's `execute_fake` placeholder stands for until a backtrace expands
/// it.
#[derive(Clone, Copy)]
pub(super) enum Slots<'a> {
    Stack(&'a Vm, usize),
    Kept,
}

impl Request {
    pub(crate) fn stack_mut(&mut self) -> &mut crate::exec::Stack {
        &mut self.vm.stack
    }

    /// What the frames own beyond their headers ([`exec::Annex`]).
    pub(crate) fn annexes(&self) -> &exec::Annexes {
        &self.annexes
    }

    pub(crate) fn annexes_mut(&mut self) -> &mut exec::Annexes {
        &mut self.annexes
    }

    /// `frame` of the running stack read where it stands; none where no
    /// frame stands there.
    pub(crate) fn frame_at(&self, frame: FrameRef) -> Option<exec::FrameAt<'_>> {
        let frame = self.vm.stack.frame(frame)?;
        Some(exec::FrameAt { req: self, frame })
    }

    /// The frame at `at` of `vm`, a running or a resumer's stack, read
    /// where it stands.
    pub(super) fn frame_on<'a>(&'a self, vm: &'a Vm, at: usize) -> Option<exec::FrameAt<'a>> {
        let frame = vm.stack.frame(FrameRef(u32::try_from(at).ok()?))?;
        Some(exec::FrameAt { req: self, frame })
    }

    /// A frame no stack holds, a suspended generator's, read where it is
    /// kept.
    pub(crate) fn kept_frame<'a>(&'a self, frame: &'a exec::HeapFrame) -> exec::FrameAt<'a> {
        exec::FrameAt { req: self, frame }
    }

    /// The unit `header`'s frame serves and which of its op arrays: a
    /// function's through the function table, which holds the unit, a
    /// file's through the frame's [`exec::Annex`].
    pub(crate) fn unit_of(&self, header: &exec::Header) -> Option<(&PublishedUnit, exec::Body)> {
        if let Some(function) = header.function {
            let user = self.user_function(function)?;
            Some((&user.unit, user.body))
        } else {
            let unit = self.annexes.get(header.annex?)?.unit.as_ref()?;
            Some((unit, exec::Body::Main))
        }
    }

    /// The unit and the op array being served in `frame`, the unit
    /// shared rather than copied, so the loop can read its oplines while
    /// the request is borrowed.
    pub(crate) fn serving(&self, frame: FrameRef) -> Option<exec::Serving> {
        let (unit, body) = self.unit_of(self.vm.at(frame)?)?;
        Some(exec::Serving {
            unit: PublishedUnit::clone(unit),
            body,
        })
    }

    /// The [`exec::Annex`] of `frame`, made where it has none.
    pub(crate) fn annex_mut(&mut self, frame: FrameRef) -> Option<&mut exec::Annex> {
        let id = if let Some(id) = self.vm.at(frame)?.annex {
            id
        } else {
            let id = self.annexes.insert(exec::Annex::default());
            if let Some(header) = self.vm.at_mut(frame) {
                header.annex = Some(id);
            }
            id
        };
        self.annexes.get_mut(id)
    }

    /// The [`exec::Annex`] of `frame`, where it has one.
    pub(crate) fn annex_of(&self, frame: FrameRef) -> Option<&exec::Annex> {
        self.annexes.get(self.vm.at(frame)?.annex?)
    }

    /// The fast-call cell of `frame`'s temporary `var`
    /// ([`exec::Annex::fast_call`]).
    pub(crate) fn fast_call(&mut self, frame: FrameRef, var: u32) -> Option<&mut exec::FastCall> {
        self.annex_mut(frame)?.fast_call(var)
    }

    /// A frame's [`exec::Annex`] given up with the frame: its place
    /// given back, and what it owns handed to the caller.
    pub(crate) fn take_annex(&mut self, header: &exec::Header) -> Option<exec::Annex> {
        self.annexes.take(header.annex?)
    }

    /// A generator's frame back on the stack with the calls it was
    /// building above it ([`Vm::resume`]).
    pub(crate) fn resume_frame(
        &mut self,
        frame: crate::exec::HeapFrame,
        calls: Vec<crate::exec::HeapFrame>,
    ) -> Result<FrameRef, Fatal> {
        self.vm.resume(&mut self.heap, frame, calls)
    }

    /// The room `room` finds in the payload `object` carries as `T`
    /// linked as the innermost on the running stack, so the next frame
    /// pushed links to it ([`php_heap::VmStack::link_room`]); nothing
    /// where the object carries no such payload. The room stays linked
    /// until [`Request::unlink_room`].
    pub(crate) fn link_room<T: std::any::Any + Send>(
        &mut self,
        object: PhpObject,
        room: fn(&mut T) -> &mut exec::FakeRoom,
    ) {
        if let Some(payload) = self.heap.native_mut::<T>(object.heap()) {
            self.vm.stack.link_room(room(payload));
        }
    }

    /// The running stack's word moved back to what the room `room` finds
    /// in `object`'s payload links, once the frame linked above it has
    /// left ([`php_heap::VmStack::unlink_room`]).
    pub(crate) fn unlink_room<T: std::any::Any + Send>(
        &mut self,
        object: PhpObject,
        room: fn(&T) -> Option<&exec::FakeRoom>,
    ) {
        if let Some(room) = self.heap.native::<T>(object.heap()).and_then(room) {
            self.vm.stack.unlink_room(room);
        }
    }

    /// A call a generator froze, given up as `cleanup_unfinished_calls`
    /// gives up one standing: back on the stack and popped.
    pub(crate) fn release_frozen_call(&mut self, call: crate::exec::HeapFrame) {
        if self.vm.stack.restore_frame(&mut self.heap, call).is_ok() {
            self.pop_call();
        }
    }

    /// The innermost frame off the stack, moved to an allocation of its
    /// own, with the calls it was building ([`Vm::suspend`]).
    pub(crate) fn suspend_frame(
        &mut self,
    ) -> Result<Option<(crate::exec::HeapFrame, Vec<crate::exec::HeapFrame>)>, Fatal> {
        self.vm.suspend(&mut self.heap)
    }

    /// The first page of the VM stack the request runs on, out of its
    /// heap, where none stands: what `zend_fiber_execute` takes for a
    /// fiber's stack before the fiber's function is called.
    pub(crate) fn open_stack(&mut self) -> Result<(), Fatal> {
        self.vm.stack.open(&mut self.heap)
    }

    /// Where the chain of the frames another process reads starts, the
    /// address of the running stack's word (design §5.6, revision 59): a
    /// fiber's stack changes places with the request's at a switch, so
    /// the word at this address is always the running one's.
    pub(crate) fn innermost(&self) -> crate::readable::Innermost {
        crate::readable::Innermost::of(self.vm.stack.link_word())
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

    /// `f` served with the request running on the stack of `coroutine`,
    /// held off the running chain, as a debugger serves a suspended
    /// fiber's frame with `EG(current_execute_data)` moved into
    /// `fiber->execute_data`'s chain: every running coroutine set aside,
    /// so the chain runs from that stack, and on through the held stacks
    /// below it while a suspend that passed through leaves each waiting
    /// on the one under it, to the bottom frame of the last, where
    /// `zend_fiber_suspend` cut `prev_execute_data`
    /// ([`crate::fiber::take_chain`]). The request's own stack is kept
    /// where [`crate::fiber::Fibers::request_stack`] finds it, so
    /// `EG(symbol_table)`'s CVs are read where they stand, and the serve
    /// holds one owner of each coroutine's object on the chain, so nothing
    /// `f` does destroys one meanwhile. As in [`Request::on_stack`], the
    /// error-reporting level and the machine stack's limit stay the
    /// running fiber's, and no fiber starts, suspends or resumes
    /// meanwhile. `None` where the coroutine holds no execution.
    pub(super) fn on_held_stack<R>(
        &mut self,
        coroutine: crate::fiber::Context,
        f: impl FnOnce(&mut Request) -> R,
    ) -> Option<R> {
        let (mut execution, below) = crate::fiber::take_chain(self, coroutine)?;
        let objects: Vec<Value> = std::iter::once(coroutine)
            .chain(below.iter().map(|held| held.coroutine))
            .map(|held| held.object().as_value())
            .collect();
        for object in &objects {
            self.addref(object);
        }
        self.switch_stack(&mut execution);
        self.fibers.set_aside_for(coroutine, execution, below);
        let served = self.with_fiber_switch_blocked(f);
        if let Some((coroutine, mut execution, below)) = self.fibers.take_back() {
            self.switch_stack(&mut execution);
            let held = std::iter::once(crate::fiber::HeldStack {
                coroutine,
                execution,
            });
            for held in held.chain(below) {
                if let Some(coroutine) = held.coroutine.coroutine(self) {
                    coroutine.hold(held.execution);
                }
            }
        }
        for object in objects {
            self.release(object);
        }
        Some(served)
    }

    /// The stacks php's one `prev_execute_data` chain runs through: the
    /// one the request is running on, then each resumer's below it, the
    /// request's own last.
    pub(super) fn executions(&self) -> impl Iterator<Item = &Vm> {
        std::iter::once(&self.vm).chain(self.fibers.below().map(|execution| &execution.vm))
    }

    /// The innermost user frame on the chain, which is where
    /// `zend_get_executed_filename` and `zend_get_executed_lineno` stop:
    /// on a fiber whose callable is a native, the resumer's.
    pub(super) fn innermost_frame(&self) -> Option<exec::FrameAt<'_>> {
        self.executions()
            .find_map(|vm| self.frame_on(vm, vm.innermost_at()?))
    }

    /// The loop over a generator's frame, bound to the engine's tier
    /// ([`crate::tier::resume`]).
    pub(crate) fn resume_loop(&self) -> fn(&mut Request, FrameRef) -> Result<(), Fatal> {
        self.shared.loops.resume
    }

    /// A unit compiled for this request alone, a source the host handed
    /// over, an `eval`'s string or a file no cache keeps, shared from
    /// here on: its op arrays take runtime caches of the request's own,
    /// as php gives them caches on the heap, and none of the engine's
    /// offsets ([`exec::RuntimeCaches::take`]).
    pub(crate) fn take_unit(&mut self, unit: Unit) -> PublishedUnit {
        self.map.take(unit)
    }

    /// Push a frame for `unit`'s op array: its shape on the VM stack and
    /// its engine state, which are pushed and popped together and share a
    /// `FrameRef`. A VM stack whose page the memory limit refuses is
    /// the fatal php raises there, and nothing is pushed. The frame is
    /// not begun: its observer begin is the caller's, which `execute`
    /// raises as `zend_execute` does.
    pub(crate) fn push_frame(&mut self, unit: PublishedUnit) -> Result<FrameRef, Fatal> {
        let code = exec::code_of(&unit);
        let slots = usize::try_from(code.num_cvs.saturating_add(code.num_tmps)).unwrap_or(0);
        let mut header = exec::Header::of(None, code);
        // `zend_execute`'s `ZEND_CALL_TOP_CODE`.
        header.info.set_top_code(true);
        exec::init_runtime_cache(&mut self.map, &mut self.heap, code, exec::Body::Main)?;
        self.forget_main_statics(code.runtime_cache);
        // `zend_execute` gives the main op array's frame `EG(symbol_table)`
        // and `i_init_code_execute_data` attaches it, so every CV name
        // stands in the table, in the compiler's order, from the start.
        let frame = self.push_code_record(
            slots,
            header,
            exec::Annex {
                unit: Some(unit),
                symbol_table: Some(TableId::GLOBAL),
                ..exec::Annex::default()
            },
        )?;
        self.attach_symbol_table(frame);
        Ok(frame)
    }

    /// A file's frame pushed with its header and its [`exec::Annex`],
    /// the extra given back where the push is refused.
    fn push_code_record(
        &mut self,
        slots: usize,
        mut header: exec::Header,
        extra: exec::Annex,
    ) -> Result<FrameRef, Fatal> {
        let mut extra = extra;
        if let Some(unit) = &extra.unit {
            let record = Box::new(exec::code_record(unit));
            header.callee = record.at();
            extra.record = Some(record);
        }
        let id = self.annexes.insert(extra);
        header.annex = Some(id);
        let pushed = self.vm.push(&mut self.heap, slots, 0, header);
        if pushed.is_err() {
            self.annexes.take(id);
        }
        pushed
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
        unit: PublishedUnit,
        caller: FrameRef,
        returns_to: Option<(FrameRef, u32)>,
    ) -> Result<FrameRef, Fatal> {
        let code = exec::code_of(&unit);
        let slots = usize::try_from(code.num_cvs.saturating_add(code.num_tmps)).unwrap_or(0);
        let mut header = exec::Header::of(None, code);
        let record = Box::new(exec::code_record(&unit));
        header.callee = record.at();
        let reserved = self.vm.push(&mut self.heap, slots, 0, header.clone())?;
        let (this, called_scope, scope) = self.vm.at(caller).map_or((None, None, None), |state| {
            (state.this(), state.called_scope(), self.frame_scope(caller))
        });
        // `ZEND_CALL_HAS_THIS` carried from the including frame, whose
        // `$this` that frame keeps: no `ZEND_CALL_RELEASE_THIS`.
        header.set_receiver(this.map(exec::Receiver::lent));
        header.set_called_scope(called_scope);
        header.set_returns_to(returns_to.map(|(_, slot)| slot));
        header.set_caller(exec::Caller::Opline(caller));
        let table = self.rebuild_symbol_table(caller);
        exec::init_runtime_cache(&mut self.map, &mut self.heap, code, exec::Body::Main)?;
        self.forget_main_statics(code.runtime_cache);
        header.annex = Some(self.annexes.insert(exec::Annex {
            unit: Some(unit),
            record: Some(record),
            scope,
            symbol_table: Some(table),
            ..exec::Annex::default()
        }));
        if let Some(laid) = self.vm.at_mut(reserved) {
            *laid = header;
        }
        let frame = reserved;
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
            .frame_at(frame)
            .and_then(|state| state.code().map(|code| code.cache_size))
            .unwrap_or(0);
        if let Some(extra) = self.annex_mut(frame) {
            extra.op_array = Some(exec::HeapOpArray {
                copy,
                run_time_cache: None,
            });
        }
        let cache =
            crate::api::values::Emalloc::new(self, usize::try_from(size).unwrap_or(usize::MAX))?;
        if let Some(held) = self
            .annex_mut(frame)
            .and_then(|extra| extra.op_array.as_mut())
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
            .map(|state| state.caller().frame());
        if code_frame.is_some() {
            self.detach_symbol_table(frame);
        }
        self.pop_frame();
        if let Some(includer) = code_frame
            && let Some(includer) = includer.or_else(|| self.innermost_user_frame())
        {
            self.attach_symbol_table(includer);
        }
    }

    /// How `frame`, the top of the stack, returns `returned` with no PHP
    /// code served, `zend_leave_helper`'s first arm with
    /// `i_free_compiled_variables` giving up only strings: a user
    /// function's frame its caller pushed, with no annex, no closure and
    /// no argument past its parameters, whose receiver, where it has
    /// one, is lent or owned with another owner standing and nothing for
    /// the collector to take ([`php_heap::Heap::releases_quietly`]), none of whose
    /// slots but `moved`, which the caller has taken the value out of,
    /// owns anything but a string, returning into a caller's slot that
    /// owns nothing, or, where the call reads no value, returning a
    /// value that owns nothing. A string given up serves no destructor
    /// and is offered to no collector, so such a frame is left by
    /// [`Request::leave_plainly`] and [`Request::pop_frame`] alike, and
    /// nothing is pending after it. The frame and its caller are each
    /// read once. None for every other frame.
    #[inline(always)] // once per return, in the loop's RETURN arms
    #[allow(clippy::inline_always)]
    pub(crate) fn leaves_plainly(
        &self,
        frame: FrameRef,
        moved: Option<usize>,
        returned: Zval,
    ) -> Option<PlainLeave> {
        let (top, laid) = self.vm.stack.top()?;
        if top != frame {
            return None;
        }
        let header = laid.header();
        let receiver = header.receiver();
        let plain = header.function.is_some()
            && header.annex.is_none()
            && (receiver.is_none() || quiet_receiver(receiver))
            && header.closure.is_none()
            && !header.info.call()
            && !header.info.extra_args();
        let caller = header.caller().frame().filter(|_| plain)?;
        let returns_to = header.returns_to();
        // Every slot's refcounted bit folded with no branch per slot,
        // where php's `i_free_compiled_variables` tests each CV: a frame
        // none of whose slots owns anything, nearly every one, is
        // answered there, and only one that owns something is walked
        // for what.
        let owners = laid
            .slots()
            .iter()
            .fold(false, |any, z| any | z.is_refcounted());
        let strings = owners && owns_strings_alone(laid.slots(), moved)?;
        let taken = match returns_to {
            Some(slot) => self
                .vm
                .stack
                .frame(caller)?
                .slots()
                .get(slot as usize)?
                .is_refcounted(),
            None => returned.is_refcounted(),
        };
        if taken {
            return None;
        }
        Some(PlainLeave {
            caller,
            returns_to,
            moved,
            strings,
            receiver,
        })
    }

    /// The top frame left, as [`Request::leaves_plainly`] found it: each
    /// string its slots but the moved one own given up, in slot order as
    /// `i_free_compiled_variables` gives them up, the receiver after
    /// them (`OBJ_RELEASE` under `ZEND_CALL_RELEASE_THIS`, a lent one's
    /// pin let go), then `zend_vm_stack_free_call_frame`; `returned` written into the
    /// caller's slot where the call reads it, and the caller on past its
    /// call opline, `LOAD_NEXT_OPLINE()`, the caller read once for both.
    #[inline(always)] // as `leaves_plainly`
    #[allow(clippy::inline_always)]
    pub(crate) fn leave_plainly(&mut self, leave: PlainLeave, returned: Zval) {
        if leave.strings {
            self.give_up_strings(leave.moved);
        }
        match leave.receiver {
            // `OBJ_RELEASE` of an object another owner of stands and the
            // collector has no use for: the decrement alone.
            Some(exec::Receiver::Owned(object))
                if php_heap::Heap::release_quietly(object.as_value().to_zval()) => {}
            Some(receiver) => self.give_up_receiver(receiver),
            None => {}
        }
        self.vm.stack.pop_frame(&mut self.heap);
        if let Some(caller) = self.vm.stack.frame_mut(leave.caller) {
            if let Some(slot) = leave.returns_to
                && let Some(into) = caller.slots_mut().get_mut(slot as usize)
            {
                *into = returned;
            }
            let header = caller.header_mut();
            header.opline = header.opline.saturating_add(1);
        }
    }

    /// The strings the top frame's slots own, but the one `moved` out
    /// of, given up in slot order ([`Request::leave_plainly`]).
    #[inline(never)]
    fn give_up_strings(&mut self, moved: Option<usize>) {
        let frame = self.vm.stack.current();
        let mut from = 0;
        while let Some((at, value)) = self.take_next_owner(frame, from, usize::MAX) {
            if Some(at) != moved {
                self.release_nogc(value);
            }
            from = at.saturating_add(1);
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
    #[inline]
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
    /// the callee [`exec::Cached::ClassAndCallee`] at `slot` holds, with
    /// whether it takes `$this`, where it holds `class` and was written
    /// from `frame`'s scope: a frame of the function that wrote it, no
    /// closure's, has that scope without asking it. None otherwise,
    /// which is the pass that looks the method up.
    #[inline]
    pub(crate) fn cached_callee(
        &self,
        frame: FrameRef,
        slot: CacheSlot,
        class: crate::api::values::ClassId,
    ) -> Option<(Function, bool)> {
        let state = self.frame_at(frame)?;
        let header = state.header();
        let exec::Cached::ClassAndCallee {
            class: held,
            scope,
            from,
            function,
            instance,
        } = self.map.get(header.runtime_cache())?.get(slot)
        else {
            return None;
        };
        let same = held == class
            && (from.is_some() && header.closure.is_none() && header.function == from
                || scope == self.scope_of(state));
        same.then_some((function, instance))
    }

    /// `CACHE_POLYMORPHIC_PTR(num, ce, fbc)` (`Z/Zend/zend_vm_def.h:3694`,
    /// `:3818`): `class` and the callee it answered into `slot`, with
    /// whether the callee takes `$this` ([`Request::takes_this`]) and
    /// `frame`'s scope as the key [`Request::cached_callee`] reads back.
    pub(crate) fn prime_callee(
        &mut self,
        frame: FrameRef,
        slot: CacheSlot,
        class: crate::api::values::ClassId,
        (function, instance): (Function, bool),
    ) {
        let scope = self.frame_scope(frame);
        let from = self
            .vm
            .at(frame)
            .filter(|header| header.closure.is_none())
            .and_then(|header| header.function);
        self.prime(
            frame,
            slot,
            exec::Cached::ClassAndCallee {
                class,
                scope,
                from,
                function,
                instance,
            },
        );
    }

    /// `zobj->ce == CACHED_PTR_EX(cache_slot)` and the offset beside it
    /// (`ZEND_FETCH_OBJ_R`, `ZEND_ASSIGN_OBJ`): the declared slot of
    /// objects of `class` the property fetch at `slot` placed on an
    /// earlier pass, and whether it is typed, where `frame` is one the
    /// placement holds for ([`exec::Cached::Property`]) and no write
    /// names its own scope; none otherwise, and the fetch places the
    /// name.
    pub(crate) fn cached_property(
        &self,
        frame: FrameRef,
        slot: CacheSlot,
        class: crate::api::values::ClassId,
    ) -> Option<(u32, bool)> {
        let header = self.vm.at(frame)?;
        let exec::Cached::Property {
            class: held,
            function,
            scope,
            slot,
            typed,
        } = self.map.get(header.runtime_cache())?.get(slot)
        else {
            return None;
        };
        let same = held == class
            && header.closure.is_none()
            && header.function == function
            && !self.fakes_scope()
            && (function.is_some() || self.frame_scope(frame) == scope);
        same.then_some((slot, typed))
    }

    /// `zend_get_property_offset`'s `CACHE_POLYMORPHIC_PTR_EX` for the
    /// fetch at `slot` in `frame`: the slot `name` is placed in on
    /// objects of `object`'s class written into the runtime cache,
    /// where [`Request::cacheable_slot`] answers one and the scope it
    /// was placed from is the frame's own. A closure's frame is never
    /// written for, since one op array serves closures bound to many
    /// scopes.
    pub(crate) fn prime_property(
        &mut self,
        frame: FrameRef,
        slot: CacheSlot,
        object: PhpObject,
        name: &[u8],
    ) {
        let Some(header) = self.vm.at(frame).filter(|header| header.closure.is_none()) else {
            return;
        };
        let function = header.function;
        let scope = self.frame_scope(frame);
        if self.property_scope() != scope {
            return;
        }
        let Some((at, typed)) = self.cacheable_slot(object, name) else {
            return;
        };
        let cached = exec::Cached::Property {
            class: object.class(),
            function,
            scope,
            slot: at,
            typed,
        };
        self.prime(frame, slot, cached);
    }

    /// `!(fbc->common.fn_flags & ZEND_ACC_STATIC)`: whether the method
    /// `function` takes `$this`, read off its declaration, what a
    /// method call's lookup asks once and the cache slot then holds
    /// beside the callee ([`exec::Cached::ClassAndCallee`]).
    pub(crate) fn takes_this(&self, function: Function) -> bool {
        self.method_decl(function)
            .is_some_and(|decl| decl.is_instance)
    }

    /// The room on the VM stack a call to `function` takes when its
    /// `INIT_*` says `count` arguments are passed, past the header:
    /// `zend_vm_calc_used_stack`, the user function's CVs and
    /// temporaries and the arguments past its declared parameters, a
    /// native's arguments alone, and for `__call`'s trampoline the
    /// arguments and the magic method's CVs and temporaries, at least
    /// two, as `zend_get_call_trampoline_func` sizes the frame it
    /// reuses.
    /// With the room, where the callee's record stands, which the
    /// call's header names ([`Request::callee_at`]); none for a callee
    /// no table holds one for, whose push makes it.
    #[inline]
    pub(crate) fn call_room(
        &self,
        function: Function,
        count: u32,
    ) -> (usize, crate::readable::CalleeAt) {
        match self.user_function(function) {
            Some(user) => (
                usize::try_from(user.shape.room(count)).unwrap_or(usize::MAX),
                user.callee_at(),
            ),
            None => self.call_room_past_user(function, count),
        }
    }

    /// [`Request::call_room`] of a callee that is no user function: a
    /// native's or a trampoline's.
    #[inline(never)]
    fn call_room_past_user(
        &self,
        function: Function,
        count: u32,
    ) -> (usize, crate::readable::CalleeAt) {
        let callee = self.shared.native_callee(function).unwrap_or_default();
        let shape = |function: Function| Some(self.user_function(function)?.shape);
        let room = if let Some(magic) = self
            .trampoline_of(function)
            .and_then(|(trampoline, _)| trampoline.magic())
            && let Some(magic) = shape(magic)
        {
            count.saturating_add(magic.slots.max(2))
        } else {
            count
        };
        (usize::try_from(room).unwrap_or(usize::MAX), callee)
    }

    /// `zend_vm_stack_push_call_frame`: a call to `function` pushed with
    /// its room ([`Request::call_room`]) for the `SEND_*` oplines to
    /// write its arguments into, `header` laid down whole; a page the
    /// memory limit refuses is the fatal php raises there.
    pub(crate) fn push_call(&mut self, mut header: exec::Header) -> Result<FrameRef, Fatal> {
        let (_, room, callee) = self.callee_room(&header)?;
        header.callee = callee;
        self.push_room(room, header)
    }

    /// The call of a frameless native, `FRAMELESS_ICALL_*`'s: php's full
    /// call where the native is observed
    /// ([`crate::observer::frameless_observed`]), on the VM stack as
    /// [`Request::push_call`] pushes it, and any other off the stack's
    /// pages ([`php_heap::VmStack::push_frameless`]), since php serves it
    /// over no frame and it takes no room php's call does not.
    /// A frameless native's call takes one slot per operand, as
    /// [`Request::call_room`] sizes any native's call.
    pub(crate) fn push_frameless(&mut self, header: exec::Header) -> Result<FrameRef, Fatal> {
        let Some(function) = header.function else {
            return Err(Fatal::error("InternalError: a call pushed with no callee"));
        };
        let room = header.num_args as usize;
        if crate::observer::frameless_observed(self, function) {
            return self.push_room(room, header);
        }
        self.vm.push_frameless(room, header)
    }

    /// The frameless call on top popped ([`Request::push_frameless`]),
    /// its operands given up as php's `FREE_OP*` after the frameless
    /// handler frees them, `zval_ptr_dtor_nogc`, once the call is on no
    /// chain. A frameless call is pushed with no receiver and no
    /// closure, and an annex only where an argument's coercion kept a
    /// value there; a record carrying any of them is popped as any call
    /// is ([`Request::pop_call`]).
    pub(crate) fn pop_frameless(&mut self) {
        /// `ZEND_FRAMELESS_ICALL_3`'s, the most operands a frameless
        /// call passes.
        const OPERANDS: usize = 3;
        let call = self.vm.stack.current();
        let Some(header) = self.vm.at_mut(call) else {
            return;
        };
        if header.annex.is_some() || header.closure.is_some() || header.receiver().is_some() {
            return self.pop_call();
        }
        header.info.set_leaving(true);
        let mut operands = [php_heap::Zval::UNDEF; OPERANDS];
        if let Some(frame) = self.vm.stack.frame_mut(call) {
            for (operand, slot) in operands.iter_mut().zip(frame.slots_mut()) {
                *operand = std::mem::replace(slot, php_heap::Zval::UNDEF);
            }
        }
        for operand in operands {
            self.release_zval_nogc(operand);
        }
        self.vm.stack.pop_frame(&mut self.heap);
    }

    /// The callee `header` names and the room its call takes
    /// ([`Request::call_room`]); a header with no callee is the engine's
    /// error.
    fn callee_room(
        &self,
        header: &exec::Header,
    ) -> Result<(Function, usize, crate::readable::CalleeAt), Fatal> {
        let Some(function) = header.function else {
            return Err(Fatal::error("InternalError: a call pushed with no callee"));
        };
        let (room, callee) = self.call_room(function, header.num_args);
        Ok((function, room, callee))
    }

    /// A call pushed with `room` slots past its header, which the caller
    /// sized for its callee ([`Request::call_room`]), `header` laid
    /// down whole; the fatal is the memory limit, as [`Request::push_call`]'s.
    #[inline]
    pub(crate) fn push_room(
        &mut self,
        room: usize,
        mut header: exec::Header,
    ) -> Result<FrameRef, Fatal> {
        if header.callee.is_none() {
            self.lay_callee(&mut header);
        }
        self.vm.push(&mut self.heap, room, 0, header)
    }

    /// The record of the call `header` lays down written into it,
    /// where its push was handed none ([`Request::callee_at`]).
    #[cold]
    #[inline(never)]
    fn lay_callee(&mut self, header: &mut exec::Header) {
        if let Some(function) = header.function {
            header.callee = self.callee_at(function);
        }
    }

    /// Where the record of `function` stands, which a frame serving
    /// it names (design §5.6, revision 59): a native's or a native
    /// method's in the engine's table, a user function's on its row, and
    /// anything else's, a class's native, a trampoline, made the first
    /// time a frame serves it and held for the request. `zend_pass_function`
    /// has none, as php's has no name.
    pub(crate) fn callee_at(&mut self, function: Function) -> crate::readable::CalleeAt {
        if let Some(native) = self.shared.native_callee(function) {
            return native;
        }
        if let Some(user) = self.user_function(function) {
            return user.callee_at();
        }
        self.callee_past_tables(function)
    }

    #[cold]
    #[inline(never)]
    fn callee_past_tables(&mut self, function: Function) -> crate::readable::CalleeAt {
        if function == Function::PASS {
            return crate::readable::CalleeAt::default();
        }
        if let Some(named) = self.named_callees.get(&function) {
            return named.at();
        }
        let name = self
            .function_decl(function)
            .map(|decl| decl.name.0.to_vec())
            .unwrap_or_default();
        let named = Box::new(crate::readable::Named::native(name));
        let at = named.at();
        self.named_callees.insert(function, named);
        at
    }

    /// The call `call`, on top of the stack, grown by `more` slots
    /// ([`php_heap::VmStack::grow_top`]).
    pub(crate) fn grow_call(&mut self, call: FrameRef, more: usize) -> Result<(), Fatal> {
        self.vm.stack.grow_top(&mut self.heap, call, more)
    }

    /// The [`FrameShape`](crate::api::request::FrameShape) of the user
    /// function `call` names; none for a call to anything else.
    pub(crate) fn call_shape(&self, call: FrameRef) -> Option<crate::api::request::FrameShape> {
        Some(self.user_function(self.vm.at(call)?.function?)?.shape)
    }

    /// The innermost user frame of the running stack ([`Vm::innermost_at`]).
    pub(crate) fn innermost_user_frame(&self) -> Option<FrameRef> {
        Some(FrameRef(u32::try_from(self.vm.innermost_at()?).ok()?))
    }

    /// Where `frame` returns: the frame it returns into
    /// ([`Request::returns_into`]) and the slot of it that takes what it
    /// returns, where the call wants the value, read off one header.
    pub(crate) fn returning(&self, frame: FrameRef) -> (FrameRef, Option<(FrameRef, u32)>) {
        let named = self
            .vm
            .at(frame)
            .map(|header| (header.caller().frame(), header.returns_to()));
        match named {
            Some((Some(caller), returns_to)) => (caller, returns_to.map(|slot| (caller, slot))),
            _ => (self.returns_into(frame), None),
        }
    }

    /// The frame `frame` returns to: the caller its push named, or the
    /// innermost user frame below it where it named none.
    pub(crate) fn returns_into(&self, frame: FrameRef) -> FrameRef {
        self.vm
            .at(frame)
            .and_then(|header| header.caller().frame())
            .or_else(|| {
                let below = self.vm.below(frame.0 as usize)?;
                Some(FrameRef(u32::try_from(below).ok()?))
            })
            .unwrap_or(frame)
    }

    /// The call the `SEND_*` oplines are placing arguments into
    /// ([`Vm::building_at`]).
    pub(crate) fn building_call(&self) -> Option<FrameRef> {
        self.vm.building_at()
    }

    /// The callee of the call being built, which a `SEND_*_EX` reads the
    /// parameter's by-reference bit off (`ARG_MUST_BE_SENT_BY_REF`).
    pub(crate) fn building_call_function(&self) -> Option<Function> {
        self.vm.at(self.building_call()?)?.function
    }

    /// The call being served where it is a native's ([`Vm::serving_at`]):
    /// the record on top of the stack where no switch stands and it is
    /// one, read once, which is what a native's entry reads per
    /// argument it holds; the walk otherwise.
    #[inline]
    pub(crate) fn serving_call(&self) -> Option<exec::FrameAt<'_>> {
        if let Some(frame) = self.vm.serving_top() {
            return Some(exec::FrameAt { req: self, frame });
        }
        self.frame_at(self.vm.serving_at()?)
    }

    /// `DO_*CALL` making the call it serves the current one,
    /// `EG(current_execute_data) = call`: the call and its callee, with
    /// whether the opline's `result` operand is read. Nothing under a
    /// `DO_*CALL` is none.
    pub(crate) fn activate_call(&mut self, return_used: bool) -> Option<(FrameRef, Function)> {
        let call = self.building_call()?;
        let header = self.vm.at_mut(call)?;
        header.info.set_serving();
        header.info.set_return_used(return_used);
        Some((call, header.function?))
    }

    /// `DO_UCALL`, and `DO_FCALL` or `DO_FCALL_BY_NAME` where `flagged`,
    /// over a call that `i_init_func_execute_data` makes a frame of by
    /// writing words alone: the call on top of the stack and being
    /// built, to a user function passed no more arguments than it
    /// declares, in the room its `INIT` laid for it, whose runtime cache
    /// an earlier activation laid down, and which, where `flagged`,
    /// raises neither a deprecation nor a discarded value's warning
    /// ([`Request::function_flags`]). The call is made the callee's
    /// frame as [`Request::activate_call`] and [`Request::activate_user`]
    /// make it, the top of the stack read once, and its observer begin
    /// raised; `returns_to` is the slot of `caller`'s that takes what it
    /// returns, none where the call opline reads nothing. None, with
    /// nothing touched, for every other call, which those two serve.
    #[inline(always)] // once per call, in the loop's DO_*CALL arms
    #[allow(clippy::inline_always)]
    pub(crate) fn enter_call(
        &mut self,
        returns_to: Option<u32>,
        caller: FrameRef,
        flagged: bool,
    ) -> Option<FrameRef> {
        let (call, top) = self.vm.stack.top()?;
        let header = top.header();
        if !header.info.building() {
            return None;
        }
        let function = header.function?;
        let passed = header.num_args;
        let len = top.len();
        let own = self.closure_own_cache(header.closure);
        let user = self.user_function(function)?;
        let shape = user.shape;
        let cache = own.unwrap_or(shape.cache);
        let refused =
            flagged && (user.flags.deprecated || user.flags.no_discard && returns_to.is_none());
        if refused
            || passed > shape.parameters
            || len < shape.slots as usize
            || !self.map.laid(cache)
        {
            return None;
        }
        let opline =
            exec::skipped_recv(passed, shape.parameters, shape.typed, shape.entry).unwrap_or(0);
        let (_, top) = self.vm.stack.top_mut()?;
        let laid = top.header_mut();
        laid.info.set_serving();
        laid.info.set_return_used(returns_to.is_some());
        laid.activate(cache, opline, false);
        laid.set_returns_to(returns_to);
        laid.set_caller(exec::Caller::Opline(caller));
        crate::observer::call(self, call);
        Some(call)
    }

    /// The call `call` made the frame of the user function it names,
    /// `i_init_func_execute_data` (design §5.6): the arguments past the
    /// declared parameters moved past the CVs and the temporaries, the
    /// slots they leave `UNDEF` (`zend_copy_extra_args`), the frame
    /// grown first where its room is short of that, as a call laid down
    /// for a trampoline is; the opline it stands at by the prologue's
    /// skip, its runtime cache laid down (`zend_init_func_run_time_cache`);
    /// `returns_to` the slot of `caller`'s that takes what it returns.
    /// A function that is no user function is an internal error.
    pub(crate) fn activate_user(
        &mut self,
        call: FrameRef,
        shape: crate::api::request::FrameShape,
        returns_to: Option<u32>,
        caller: exec::Caller,
    ) -> Result<exec::Activated, Fatal> {
        let (num_args, len, closure) = self.vm.stack.frame(call).map_or((0, 0, None), |frame| {
            let header = frame.header();
            (header.num_args, frame.len(), header.closure)
        });
        // A closure with a cache of its own reads it in place of its
        // function's (`zend_create_closure_ex`).
        let own_cache = self.closure_own_cache(closure);
        let cache = own_cache.unwrap_or(shape.cache);
        let own_cache = own_cache.is_some();
        // The declared parameters, which is php's `common.num_args` and
        // excludes a variadic: an argument from the variadic's position
        // on is an extra one, where php leaves it past the temporaries
        // for `RECV_VARIADIC` to collect.
        let own = shape.slots as usize;
        let parameters = shape.parameters as usize;
        let passed = num_args as usize;
        let extra = passed.saturating_sub(parameters);
        let opline =
            exec::skipped_recv(num_args, shape.parameters, shape.typed, shape.entry).unwrap_or(0);
        if len < own + extra {
            self.vm
                .stack
                .grow_top(&mut self.heap, call, own + extra - len)?;
        }
        if let Some(frame) = self.vm.stack.frame_mut(call) {
            let laid = frame.header_mut();
            laid.activate(cache, opline, extra > 0);
            laid.set_returns_to(returns_to);
            laid.set_caller(caller);
            let slots = frame.slots_mut();
            if extra > 0 && passed <= slots.len() && own + extra <= slots.len() {
                slots.copy_within(parameters..passed, own);
                if let Some(left) = slots.get_mut(parameters..passed.min(own)) {
                    left.fill(Zval::UNDEF);
                }
            }
        }
        if own_cache {
            exec::lay_own_cache(&mut self.map, cache, shape.cache_size);
        } else {
            exec::lay_runtime_cache(&mut self.map, &mut self.heap, cache, shape.cache_size)?;
        }
        crate::observer::call(self, call);
        Ok(exec::Activated {
            frame: call,
            prologue: opline < shape.entry,
        })
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
        let frame = self.vm.stack.current();
        let Some(header) = self.vm.at_mut(frame) else {
            return;
        };
        if header.info.call() {
            self.pop_call();
            return;
        }
        // `EG(current_execute_data) = EX(prev_execute_data)`: off the
        // chain from here, its receiver and closure taken off the header
        // for the release.
        header.info.set_leaving(true);
        let receiver = header.receiver();
        header.set_receiver(None);
        let closure = header.closure.take();
        let (function, annex, num_args) = (header.function, header.annex, header.num_args);
        if !self.bailout {
            self.release_frame(frame, function, annex, num_args, receiver, closure);
        }
        let annex = annex.and_then(|id| self.annexes.take(id));
        self.vm.stack.pop_frame(&mut self.heap);
        if let Some(annex) = annex {
            // An `eval`'s main op array is destroyed when its execution
            // ends, and the names its compile made with it.
            self.give_up_annex(annex, function.is_none() && !self.bailout);
        }
    }

    /// What a frame's [`exec::Annex`] holds given up as the frame leaves
    /// the stack: `destroy_op_array`, which frees the heap runtime cache,
    /// then `efree_size(EX(func))`, and, where `names`, the names the
    /// compile of the unit it served made.
    fn give_up_annex(&mut self, mut annex: exec::Annex, names: bool) {
        if let Some(held) = annex.op_array.take() {
            if let Some(cache) = held.run_time_cache {
                cache.free(self);
            }
            held.copy.free(self);
        }
        if names {
            self.release_unit_names(annex.unit.as_ref());
        }
    }

    /// The frames a stack let go of without the engine's pop, the newest
    /// first ([`php_heap::VmStack::close`], [`php_heap::VmStack::put_back`]):
    /// a fiber's that ended, the request's at its end, and what a serving
    /// with nothing executing left over the frames it set aside. What
    /// each frame's annex holds is given up as a pop under a fatal gives
    /// it up, its slab entry with it; nothing its slots held is, as
    /// php's `zend_vm_stack_destroy` releases nothing a frame held. Every
    /// serving pops what it pushed, a fatal's way out included, so these
    /// are none unless a serving left its frames.
    pub(crate) fn drop_frames(&mut self, dropped: Vec<exec::Header>) {
        for header in dropped {
            if let Some(annex) = header.annex.and_then(|id| self.annexes.take(id)) {
                self.give_up_annex(annex, false);
            }
        }
    }

    /// What [`Request::pop_frame`] gives up of `frame`, in its order:
    /// the CVs and the temporaries, the rebuilt table and the extra named
    /// arguments where the frame has an annex, the extra arguments past
    /// the temporaries where it was passed more than it declares, the
    /// receiver and the closure. A frame with no annex and no extra
    /// argument, nearly every one, walks its own slots and nothing else.
    fn release_frame(
        &mut self,
        frame: FrameRef,
        function: Option<Function>,
        annex: Option<exec::AnnexId>,
        num_args: u32,
        receiver: Option<exec::Receiver>,
        closure: Option<PhpObject>,
    ) {
        let slots = self
            .vm
            .stack
            .frame(frame)
            .map_or(0, crate::exec::HeapFrame::len);
        let (cvs, own, parameters) = match function.and_then(|f| self.user_function(f)) {
            Some(user) => (
                user.shape.cvs as usize,
                user.shape.slots as usize,
                user.shape.parameters,
            ),
            None => self.frame_at(frame).and_then(exec::FrameAt::code).map_or(
                (slots, slots, 0),
                |code| {
                    (
                        code.num_cvs as usize,
                        code.num_cvs.saturating_add(code.num_tmps) as usize,
                        0,
                    )
                },
            ),
        };
        let mut from = 0;
        while let Some((at, value)) = self.take_next_owner(frame, from, own) {
            if at < cvs {
                self.release(value);
            } else {
                self.release_nogc(value);
            }
            from = at.saturating_add(1);
        }
        // A generator's frame leaves its closure to the generator
        // (`ZendGenerator::closure`), which keeps it until it is freed.
        let mut generator = None;
        if let Some(id) = annex {
            self.free_rebuilt_table(frame);
            let named = self.annexes.get_mut(id).map(|annex| {
                generator = annex.generator;
                std::mem::take(&mut annex.extra_named)
            });
            if let Some(named) = named {
                self.release_named(named);
            }
        }
        // `zend_vm_stack_free_extra_args_ex`'s `i_zval_ptr_dtor`: an
        // extra argument, past the CVs and the temporaries, that keeps
        // owners after the release is a possible root, so a cycle a
        // collection in the callee found reachable through it is found
        // again.
        if num_args > parameters || function.is_none() {
            let mut from = own;
            while let Some((at, value)) = self.take_next_owner(frame, from, usize::MAX) {
                self.release(value);
                from = at.saturating_add(1);
            }
        }
        if let Some(receiver) = receiver {
            self.give_up_receiver(receiver);
        }
        if let Some(closure) = closure.filter(|_| generator.is_none()) {
            self.release(closure.as_value());
        }
    }

    /// The first slot of `frame` from `from` on that holds an owner, and
    /// what it held, the slot left `UNDEF`: the walk of a frame being
    /// popped, which gives up each owner in slot order, the frame looked
    /// up again after each release since the release may serve PHP
    /// code. A slot that owns nothing is passed over in the same lookup
    /// and left as it stands, as `i_free_compiled_variables` passes over
    /// what `Z_REFCOUNTED_P` refuses.
    fn take_next_owner(
        &mut self,
        frame: FrameRef,
        from: usize,
        end: usize,
    ) -> Option<(usize, Value)> {
        let slots = self.vm.stack.frame_mut(frame)?.slots_mut();
        let end = end.min(slots.len());
        let (at, cell) = slots
            .get_mut(..end)?
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
        self.vm
            .stack
            .frame(frame)
            .map_or(Value::undef(), |f| Value::of_zval(f.slot(Slot(at))))
    }

    /// Every slot of `frame` in place ([`crate::exec::HeapFrame::slots_mut`]),
    /// the CVs and then the temporaries: what a handler's inline path
    /// addresses its operands and its temporary through, one lookup of
    /// the frame for all of them (design §5.8). Empty for a frame that
    /// is not live, where every operand reads as `UNDEF` and the inline
    /// path stands aside for the kernel.
    ///
    /// The lookup is the frame table's bounds check on `frame`, the
    /// table's base and the frame's slot base and count: eight
    /// instructions of an inline `JMPZ` over a CV, before the operand's
    /// own bounds check, where php indexes off `execute_data` in a
    /// register (#2516). A frame cursor the loop keeps valid across
    /// handlers would be an `unsafe` contract across the heap seam,
    /// which #2540 ruled out for this term.
    pub(crate) fn slots_mut(&mut self, frame: FrameRef) -> &mut [Zval] {
        self.vm
            .stack
            .frame_mut(frame)
            .map_or(&mut [], crate::exec::HeapFrame::slots_mut)
    }

    /// The value out of a slot, the slot left `UNDEF`: what the slot
    /// owned is the caller's now, which is how a temporary is consumed.
    pub(crate) fn take_slot(&mut self, frame: FrameRef, at: u32) -> Value {
        let Some(f) = self.vm.stack.frame_mut(frame) else {
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
            .vm
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

    /// `value` into a slot of `frame` and what it held handed back, with
    /// no sensing of a temporary written over: what a coercion writes
    /// over the argument it coerced. A slot past the end takes nothing
    /// and hands `value` back.
    pub(crate) fn swap_slot(&mut self, frame: FrameRef, at: u32, value: Value) -> Value {
        match self
            .vm
            .stack
            .frame_mut(frame)
            .and_then(|f| f.slots_mut().get_mut(at as usize))
        {
            Some(slot) => Value::of_zval(std::mem::replace(slot, value.to_zval())),
            None => value,
        }
    }

    /// [`Request::set_slot`] and what the slot held released after the
    /// write, the `zval_ptr_dtor` php's assignment over a slot ends with.
    pub(crate) fn replace_slot(&mut self, frame: FrameRef, at: u32, value: Value) {
        let Some(slot) = self
            .vm
            .stack
            .frame_mut(frame)
            .and_then(|f| f.slots_mut().get_mut(at as usize))
        else {
            return self.release(value);
        };
        let old = std::mem::replace(slot, value.to_zval());
        if old.is_refcounted() {
            #[cfg(debug_assertions)]
            self.sense_dirty_temporary(frame, at, old.kind());
            php_heap::Heap::release(&mut super::Hooks(self), old);
        }
    }

    /// The zval in a slot of `frame` in place, php's `EX_VAR(var)`
    /// (design §5.6): what a `FETCH_*_W` points its `VAR` at, and what
    /// the `ASSIGN*` over that `VAR` reads through when an `IS_INDIRECT`
    /// stands there (`_get_zval_ptr_ptr_var`). None for a slot the
    /// frame does not have.
    pub(crate) fn slot_ptr(&mut self, frame: FrameRef, at: u32) -> Option<ZvalRef> {
        self.vm.stack.frame_mut(frame)?.slot_ptr(Slot(at))
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
        let call = self.vm.stack.current();
        // `EG(current_execute_data) = execute_data` before the arguments
        // are freed: the call is on no chain from here.
        let Some(header) = self.vm.at_mut(call) else {
            return;
        };
        header.info.set_leaving(true);
        let receiver = header.receiver();
        header.set_receiver(None);
        let closure = header.closure.take();
        let annex = header.annex.take();
        let annex = annex
            .and_then(|id| self.annexes.take(id))
            .unwrap_or_default();
        // `zend_vm_stack_free_args`, whose `zval_ptr_dtor_nogc` makes no
        // argument a possible root; the receiver and the closure after
        // them are `OBJ_RELEASE`, which does.
        for value in annex.coerced {
            self.release_nogc(value);
        }
        let mut from = 0;
        while let Some((at, value)) = self.take_next_owner(call, from, usize::MAX) {
            self.release_nogc(value);
            from = at.saturating_add(1);
        }
        if let Some(name) = annex.name {
            self.release(Value::string(name));
        }
        if let Some(receiver) = receiver {
            self.give_up_receiver(receiver);
        }
        if let Some(closure) = closure {
            self.release(closure.as_value());
        }
        self.release_named(annex.extra_named);
        self.vm.stack.pop_frame(&mut self.heap);
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
            || (0..self.vm.depth()).any(|at| {
                self.frame_on(&self.vm, at).is_some_and(|frame| {
                    let info = frame.info();
                    !info.leaving() && !info.building()
                })
            })
    }
}

/// Whether a frame's receiver is given up with no PHP code served: lent,
/// or owned with another owner standing and nothing for the collector
/// to take ([`php_heap::Heap::releases_quietly`]).
#[inline(never)]
fn quiet_receiver(receiver: Option<exec::Receiver>) -> bool {
    match receiver {
        None | Some(exec::Receiver::Lent(_)) => true,
        Some(exec::Receiver::Owned(object)) => {
            php_heap::Heap::releases_quietly(object.as_value().to_zval())
        }
    }
}

/// What the slots of a frame that owns something own, but the one
/// `moved` out of: whether any owns a string, where nothing but
/// strings is owned; none where any owns anything else, which
/// [`Request::leaves_plainly`] refuses.
#[inline(never)]
fn owns_strings_alone(slots: &[Zval], moved: Option<usize>) -> Option<bool> {
    let mut strings = false;
    for (at, z) in slots.iter().enumerate() {
        if !z.is_refcounted() || Some(at) == moved {
            continue;
        }
        if z.kind() != ZvalType::String {
            return None;
        }
        strings = true;
    }
    Some(strings)
}

/// A frame [`Request::leaves_plainly`] found returns with no PHP code
/// served: the frame it returns into and the slot of it that takes the
/// value, where the call reads one, the slot the returned value was
/// moved out of, whether any other slot owns a string to give up, and
/// the receiver the frame gives up, where it has one.
#[derive(Clone, Copy)]
pub(crate) struct PlainLeave {
    pub(crate) caller: FrameRef,
    pub(crate) returns_to: Option<u32>,
    moved: Option<usize>,
    strings: bool,
    receiver: Option<exec::Receiver>,
}

/// The runtime caches a closure reads, as `zend_create_closure_ex`
/// chooses them ([`crate::classes::closure_cache`]) and an activation
/// finds them.
impl Request {
    /// The cache a frame serving `closure` reads in place of its
    /// function's: the closure's own, where it has one
    /// (`ZEND_MAP_PTR_INIT(closure->func.op_array.run_time_cache, ptr)`);
    /// none for a frame with no closure and a closure sharing its
    /// function's.
    pub(crate) fn closure_own_cache(&self, closure: Option<PhpObject>) -> Option<exec::CacheAt> {
        match closure?
            .native::<crate::classes::ClosureObject>(self)?
            .cache
        {
            crate::classes::ClosureCache::Own(at) => Some(at),
            crate::classes::ClosureCache::Function => None,
        }
    }

    /// A cache of the request's own for a closure, its `cache_size`
    /// bytes `emalloc`ed as `zend_create_closure_ex` allocates them
    /// (`Z/Zend/zend_closures.c:813`) and its slots laid down at its
    /// first call ([`exec::RuntimeCaches::own_cache`]). A closure bound
    /// to another scope takes sixteen bytes more than one bound to its
    /// own, the cache of its two calls, and gives them back with it:
    ///
    /// ```php
    /// class A {} function g() {}
    /// $c = function () { g(); g(); };
    /// $m = memory_get_usage(); $b = Closure::bind($c, null, A::class); echo memory_get_usage() - $m, " ";
    /// $m = memory_get_usage(); $d = Closure::bind($c, null, null); echo memory_get_usage() - $m, " ";
    /// $m = memory_get_usage(); unset($b); echo memory_get_usage() - $m, "\n";
    /// ```
    /// ```expect
    /// 400 384 -400
    /// ```
    ///
    /// The `Err` is the memory limit's fatal.
    pub(crate) fn own_cache(&mut self, cache_size: u32) -> Result<exec::CacheAt, Thrown> {
        let room = self.heap.block(cache_size as usize);
        let room = self.fatal_of(room)?;
        Ok(self.map.own_cache(Some(room)))
    }

    /// A closure's own cache given back with it, its room to the heap.
    pub(crate) fn give_back_cache(&mut self, at: exec::CacheAt) {
        self.map.give_back(&mut self.heap, at);
    }

    /// Whether the cache at `at` is laid down, `RUN_TIME_CACHE` not
    /// `NULL`.
    pub(crate) fn cache_laid(&self, at: exec::CacheAt) -> bool {
        self.map.laid(at)
    }

    /// The scope the cache at `at` was laid down for by the first closure
    /// made over its op array ([`exec::RuntimeCache::laid_for`]).
    pub(crate) fn laid_for(&self, at: exec::CacheAt) -> Option<crate::api::values::ClassId> {
        self.map.get(at).and_then(|cache| cache.laid_for)
    }

    /// A real closure's op array's cache laid down by the first closure
    /// made over it, for the scope that closure is made in.
    pub(crate) fn lay_closure_cache(
        &mut self,
        at: exec::CacheAt,
        cache_size: u32,
        scope: Option<crate::api::values::ClassId>,
    ) -> Result<(), Thrown> {
        let laid = exec::lay_runtime_cache(&mut self.map, &mut self.heap, at, cache_size);
        self.fatal_of(laid)?;
        self.map.map(at).laid_for = scope;
        Ok(())
    }
}
