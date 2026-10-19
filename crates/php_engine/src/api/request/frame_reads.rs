//! Frame reads for extensions (#1827, design §4.3 seam 9): a frame read
//! through a [`FrameView`], borrowed from the request, so nothing can
//! push, pop or switch a stack while it is held; and a frame kept past
//! that borrow as a [`FrameId`], checked against the stacks that stand
//! each time it is read again.
//!
//! A [`FrameRef`] is a position on one stack, and a position is not a
//! frame for long: a generator's frame is taken off the stack at each
//! yield and pushed where its next resumer stands, and a fiber switch
//! swaps the whole stack, so a position kept across either reads
//! whichever frame stands there now. The view is php's `execute_data`
//! for the span of a borrow, which a handler's [`Observed::frame`]
//! hands out (#1776's ruling: a borrowed, frame-focused view, no check
//! per read). The identity is php's `execute_data` pointer as an
//! extension keeps it, which names one frame however its stack moves:
//! a number the request gives a frame the first time one is asked of it
//! and never gives again, held in the frame's state, so it moves with
//! a generator's frame as php's pointer does. [`Request::frame_of`]
//! reads it where it stands on the chain of stacks php's
//! `prev_execute_data` walks, the running one and each resumer's below
//! it, and refuses it where it stands on none: a frame popped, a
//! suspended generator's, a suspended fiber's. A write or an evaluation
//! reaches the last two through what the generator or the fiber holds,
//! which [`FrameId`] names ([`Request::in_frame`]).
//!
//! A frame's identity is given lazily: a push writes the state's one
//! more word as none, and the number is given the first time an
//! extension asks.
//!
//! The walk php's `prev_execute_data` makes from a frame is
//! [`FrameView::walk`] (#2027): every frame of the chain, the user frames
//! [`FrameView::caller`] steps between and the frames php has where this
//! engine has none, a native's call, a suspended generator's frame a
//! resumption runs through, and a fiber's bottom frame, each a
//! [`ChainFrame`] read as a function and scope or a file.
//!
//! [`Observed::frame`]: crate::api::abi::Observed::frame

use std::cell::Cell;
use std::hash::{Hash, Hasher};
use std::num::NonZeroU64;
use std::ops::Range;

use php_heap::FrameRef;

use crate::api::call::Function;
use crate::api::values::{ClassId, Value};
use crate::exec::{self, FrameAt, Site, Vm};

use super::{Expand, Link, OpArrayView, OplineIdx, Request, Slots, links_of};

/// A frame kept past the borrow it was read in, php's `execute_data`
/// as an extension stores it: read again through [`Request::frame_of`],
/// which answers the frame it names or nothing, never another frame.
/// Two are equal where they name one frame.
#[derive(Debug, Clone, Copy)]
pub struct FrameId {
    /// Where the frame stood when the identity was read: where
    /// [`Request::frame_of`] looks first.
    at: FrameRef,
    /// The frame's identity, which [`exec::Header`] holds.
    id: Identity,
    /// The generator whose frame it is, no owner: where
    /// [`Request::in_frame`] looks for it while the generator is
    /// suspended and its frame on no stack.
    generator: Option<crate::api::values::PhpObject>,
    /// The coroutine whose stack the frame stood on, a `Fiber`'s or an
    /// extension's, no owner; none for the request's own stack: where
    /// [`Request::in_frame`] looks for it while the coroutine is
    /// suspended, as php reaches it through `fiber->execute_data`.
    coroutine: Option<crate::fiber::Context>,
}

/// The number a request gives one frame and no other, held in the
/// frame's state from the first time it is asked
/// ([`FrameView::id`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct Identity(NonZeroU64);

impl PartialEq for FrameId {
    fn eq(&self, other: &FrameId) -> bool {
        self.id == other.id
    }
}

impl Eq for FrameId {}

impl Hash for FrameId {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.id.hash(state);
    }
}

/// One frame read while the request is borrowed: what it serves, where
/// it stands, the frame below it, and its [`FrameId`] to keep.
#[derive(Clone, Copy)]
pub struct FrameView<'a> {
    /// The request borrowed, so no stack moves while the view is held.
    req: &'a Request,
    /// The stack the frame stands on, counted down the chain
    /// [`Request::executions`] walks: the running one is zero, the
    /// resumer of the innermost fiber one.
    stack: usize,
    /// That stack's engine state.
    vm: &'a Vm,
    /// The frame's position on that stack.
    at: FrameRef,
}

impl std::fmt::Debug for FrameView<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FrameView")
            .field("stack", &self.stack)
            .field("at", &self.at)
            .finish_non_exhaustive()
    }
}

impl<'a> FrameView<'a> {
    /// The frame's identity, given now where it has none, to keep past
    /// this borrow.
    #[must_use]
    pub fn id(&self) -> FrameId {
        let state = self.state();
        let id = state
            .and_then(|state| state.header().identity.get())
            .unwrap_or_else(|| {
                let id = self.req.frame_ids.give();
                if let Some(state) = state {
                    state.header().identity.set(Some(id));
                }
                id
            });
        FrameId {
            at: self.at,
            id,
            generator: state.and_then(FrameAt::generator),
            coroutine: self.req.fibers.coroutine_on(self.stack),
        }
    }

    /// The frame's state; a view is only made of a frame that stands, so
    /// none is this engine's error, read as an empty frame.
    fn state(&self) -> Option<FrameAt<'a>> {
        self.req.frame_on(self.vm, self.at.0 as usize)
    }

    /// `EX(func)` where the frame serves a function or a method; `None`
    /// for an included, evaluated or main file's op array, which php
    /// names by its `filename` alone.
    #[must_use]
    pub fn function(&self) -> Option<Function> {
        self.state()?.function()
    }

    /// `EX(func)->common.scope`: the class a method is declared in, the
    /// one a closure is bound to, and the includer's for a file's frame.
    #[must_use]
    pub fn function_scope(&self) -> Option<ClassId> {
        self.req.scope_of(self.state()?)
    }

    /// `EX(func)->op_array.filename`: the file the op array was compiled
    /// from, `… : eval()'d code` for an evaluated string's.
    #[must_use]
    pub fn filename(&self) -> &'a [u8] {
        self.state().and_then(FrameAt::file).unwrap_or_default()
    }

    /// `EX(func)->op_array`: the op array the frame serves, a
    /// function's or an included, evaluated or main file's, read
    /// ([`OpArrayView`]).
    #[must_use]
    pub fn op_array(&self) -> Option<OpArrayView<'a>> {
        let (unit, body) = self.state()?.unit()?;
        exec::body_of(unit, body).map(|code| OpArrayView::of(unit, code))
    }

    /// `EX(opline)->lineno`: the line the frame stands on, the call's
    /// for a frame below the innermost; zero past its op array.
    #[must_use]
    pub fn line(&self) -> u32 {
        self.state().map_or(0, FrameAt::line)
    }

    /// `EX(opline) - EX(func)->op_array.opcodes`: where in its op array
    /// the frame stands, the opline the loop dispatches next for the
    /// innermost frame and the call's for a frame below it.
    #[must_use]
    pub fn opline(&self) -> OplineIdx {
        OplineIdx(self.state().map_or(0, FrameAt::opline))
    }

    /// `zend_get_opcode_name(EX(opline)->opcode)`: the opcode of the
    /// opline the frame stands on, as php names it, `ZEND_ECHO`, the
    /// call's for a frame below the innermost, as `zend_test`'s
    /// `observer.show_opcode` prints it from a begin or an end handler;
    /// `ZEND_HANDLE_EXCEPTION` while the unwinder answers a throwable in
    /// the frame, php's `EG(exception_op)`, which is what an end handler
    /// the uncaught arm calls reads;
    /// empty past its op array.
    #[must_use]
    pub fn opcode(&self) -> &'static str {
        if self.state().is_some_and(FrameAt::at_exception_op) {
            return "ZEND_HANDLE_EXCEPTION";
        }
        self.state()
            .and_then(|state| exec::opline_at(state.code()?, state.opline()))
            .map_or("", |op| op.opcode.name())
    }

    /// `EX(prev_execute_data)` as far as user code goes: the user frame
    /// below this one, on its stack or, below a fiber's first frame, on
    /// the stack that resumed the fiber; none below the request's first.
    /// Above a switch it is the frame the switch moved
    /// `EG(current_execute_data)` to ([`exec::Switch`]). A frame being
    /// popped is passed over, as php has moved `EG(current_execute_data)`
    /// past it. The frames php links between two user frames are
    /// [`FrameView::walk`]'s.
    #[must_use]
    pub fn caller(&self) -> Option<FrameView<'a>> {
        let mut stack = self.stack;
        let mut vm = self.vm;
        let mut above = self.at.0 as usize;
        let at = loop {
            if let Some(lower) = vm.below(above) {
                break lower;
            }
            stack += 1;
            vm = self.req.executions().nth(stack)?;
            above = vm.depth();
        };
        Some(FrameView {
            req: self.req,
            stack,
            vm,
            at: FrameRef(u32::try_from(at).ok()?),
        })
    }
}

impl<'a> FrameView<'a> {
    /// php's `prev_execute_data` walk from this frame: the frame itself,
    /// then every frame below it, innermost first, as
    /// `observer_show_init_backtrace` walks it. Where
    /// [`FrameView::caller`] steps from user frame to user frame, the
    /// walk meets every frame php links: a native's call between two
    /// user frames, the frames of the generators a resumption runs
    /// through, a delegating generator's below the one it delegates to,
    /// and below a fiber's first frame the fiber's bottom frame, then
    /// the stack that resumed it.
    #[must_use]
    pub fn walk(&self) -> std::vec::IntoIter<ChainFrame<'a>> {
        let req = self.req;
        let mut walk = vec![ChainFrame {
            req,
            read: Read::Stack(*self),
        }];
        let state = self.state();
        // The generators the frame's own generator runs through stand
        // below it, ahead of the calls and frames it was pushed over.
        if let Some(generator) = state.and_then(FrameAt::generator) {
            walk.extend(
                req.placeholder_links(generator)
                    .iter()
                    .filter_map(|link| ChainFrame::of(req, self.stack, self.vm, link)),
            );
        }
        walk.extend(req.stack_walk(self.stack, self.vm, 0..self.at.0 as usize));
        req.walk_below(self.stack, &mut walk);
        walk.into_iter()
    }
}

/// One frame of php's `prev_execute_data` chain, read while the request
/// is borrowed ([`FrameView::walk`]): a user frame standing on a stack,
/// which [`ChainFrame::frame`] reads whole, or a frame php has where this
/// engine keeps none on a stack: a native's call, a suspended
/// generator's frame a resumption runs through (php's `execute_fake`
/// placeholder expanded, `zend_generator_check_placeholder_frame`), an
/// attribute's dummy frame, and a fiber's bottom frame, whose
/// `zend_fiber_function` names no function and no file.
#[derive(Clone, Copy)]
pub struct ChainFrame<'a> {
    req: &'a Request,
    read: Read<'a>,
}

/// What a [`ChainFrame`] is read off.
#[derive(Clone, Copy)]
enum Read<'a> {
    /// A user frame on a stack.
    Stack(FrameView<'a>),
    /// A suspended generator's frame, kept on its object.
    Kept(FrameAt<'a>),
    /// A native's call being served.
    Native(Function),
    /// `zend_get_attribute_object`'s dummy frame.
    Site(&'a Site),
    /// `fiber->stack_bottom`, below a fiber's first frame.
    FiberBottom,
}

impl std::fmt::Debug for ChainFrame<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let mut debug = f.debug_tuple("ChainFrame");
        match &self.read {
            Read::Stack(view) => debug.field(view),
            Read::Kept(_) => debug.field(&"kept"),
            Read::Native(function) => debug.field(function),
            Read::Site(_) => debug.field(&"site"),
            Read::FiberBottom => debug.field(&"fiber bottom"),
        };
        debug.finish()
    }
}

/// `EX(func)->op_array.filename` of the op array a frame's state serves.
fn file_of(state: FrameAt<'_>) -> Option<&[u8]> {
    state.file()
}

impl<'a> ChainFrame<'a> {
    /// The frame `link`, on the stack `stack` counts down the chain;
    /// none for php's `execute_fake`, which a walk meets as the frames
    /// it expands into (`zend_generator_check_placeholder_frame`).
    fn of(req: &'a Request, stack: usize, vm: &'a Vm, link: &Link<'a>) -> Option<ChainFrame<'a>> {
        let read = match *link {
            Link::Frame(Slots::Stack(_, at), _) => Read::Stack(FrameView {
                req,
                stack,
                vm,
                at: FrameRef(u32::try_from(at).unwrap_or(u32::MAX)),
            }),
            Link::Frame(Slots::Kept, state) => Read::Kept(state),
            Link::Call(call) => Read::Native(call.callee()),
            Link::Site(site) => Read::Site(site),
            Link::Fake(_) => return None,
        };
        Some(ChainFrame { req, read })
    }

    /// The frame read whole, where it is a user frame standing on a
    /// stack: its line, its op array and its identity to keep. `None`
    /// for every other frame of the chain, which no stack holds.
    #[must_use]
    pub fn frame(&self) -> Option<FrameView<'a>> {
        match self.read {
            Read::Stack(view) => Some(view),
            Read::Kept(_) | Read::Native(_) | Read::Site(_) | Read::FiberBottom => None,
        }
    }

    /// `EX(func)` where it names a function: a user function or method,
    /// a closure or a native; `None` for an included, evaluated or main
    /// file's op array, an attribute's dummy frame and a fiber's bottom
    /// frame, whose `function_name` is `NULL`.
    #[must_use]
    pub fn function(&self) -> Option<Function> {
        match self.read {
            Read::Stack(view) => view.function(),
            Read::Kept(state) => state.function(),
            Read::Native(function) => Some(function),
            Read::Site(_) | Read::FiberBottom => None,
        }
    }

    /// `EX(func)->common.scope`: the class a method, native or not, is
    /// declared in, the one a closure is bound to, and the includer's
    /// for a file's frame.
    #[must_use]
    pub fn function_scope(&self) -> Option<ClassId> {
        match self.read {
            Read::Stack(view) => view.function_scope(),
            Read::Kept(state) => self.req.scope_of(state),
            Read::Native(function) => self.req.method_class(function),
            Read::Site(_) | Read::FiberBottom => None,
        }
    }

    /// `EX(func)->op_array.filename` where `EX(func)` is user code: the
    /// file a frame's op array was compiled from, an attribute's site;
    /// `None` for a native's call and a fiber's bottom frame, which php
    /// writes as `[no active file]`.
    #[must_use]
    pub fn filename(&self) -> Option<&'a [u8]> {
        match self.read {
            Read::Stack(view) => view.state().and_then(file_of),
            Read::Kept(state) => file_of(state),
            Read::Site(site) => Some(&site.file),
            Read::Native(_) | Read::FiberBottom => None,
        }
    }
}

/// How many identities the request has given.
#[derive(Debug, Default)]
pub(super) struct FrameIds(Cell<u64>);

impl FrameIds {
    /// A number no frame of the request was given before: one past the
    /// count, which no request lives long enough to exhaust.
    fn give(&self) -> Identity {
        let next = NonZeroU64::MIN.saturating_add(self.0.get());
        self.0.set(next.get());
        Identity(next)
    }
}

impl FrameId {
    /// Whether the frame stands on `vm` at the place it was kept at.
    fn kept_at(&self, vm: &Vm) -> bool {
        vm.at(self.at).is_some_and(|state| self.names(state))
    }

    /// Where the frame stands on `vm`, looked for over every frame, the
    /// innermost first: a generator's pushed back elsewhere.
    fn moved_on(&self, vm: &Vm) -> Option<FrameRef> {
        (0..vm.depth()).rev().find_map(|at| {
            let at = FrameRef(u32::try_from(at).ok()?);
            vm.at(at)
                .is_some_and(|state| self.names(state))
                .then_some(at)
        })
    }

    /// Where the frame stands on `vm`, its kept place first.
    fn place_on(&self, vm: &Vm) -> Option<FrameRef> {
        if self.kept_at(vm) {
            return Some(self.at);
        }
        self.moved_on(vm)
    }

    /// Whether `state` is the frame's, and not one being popped.
    fn names(&self, state: &exec::Header) -> bool {
        !state.info.leaving() && state.identity.get() == Some(self.id)
    }
}

impl Request {
    /// The frame `id` names, where it stands on the running stack or a
    /// resumer's below it; `None` where it stands on none, so a frame
    /// that has left them is refused and never read as the frame at its
    /// old place. The position it was kept at is looked at first, on
    /// each stack; past that every frame of every stack is, so a
    /// generator's frame pushed back elsewhere is found and a refusal
    /// walks the whole chain.
    #[must_use]
    pub fn frame_of(&self, id: FrameId) -> Option<FrameView<'_>> {
        let found = self
            .executions()
            .enumerate()
            .find_map(|(stack, vm)| id.kept_at(vm).then_some((stack, vm, id.at)))
            .or_else(|| {
                // A generator's frame pushed back where another resumer
                // stands.
                self.executions()
                    .enumerate()
                    .find_map(|(stack, vm)| Some((stack, vm, id.moved_on(vm)?)))
            });
        let (stack, vm, at) = found?;
        Some(FrameView {
            req: self,
            stack,
            vm,
            at,
        })
    }

    /// The variable `name` of the frame `id` names, a view, as php's
    /// debuggers read one between two oplines: what the frame's symbol
    /// table holds under the name where it has one, else the compiled
    /// variable of that name, so no table is built for the read; `UNDEF`
    /// where the name is one and holds nothing. `None` where the frame
    /// knows no such name, or is not one this reaches
    /// ([`Request::in_frame`]).
    pub fn frame_var(&mut self, frame: FrameId, name: &[u8]) -> Option<Value> {
        self.in_frame(frame, |req, at| {
            if let Some(table) = req.frame_at(at).and_then(FrameAt::symbol_table) {
                return req.symbol_value(table, name);
            }
            let cv = req.frame_var_at(at, name)?;
            Some(req.slot(at, cv))
        })
        .flatten()
    }

    /// `zend_rebuild_symbol_table()` over the frame `id` names, and each
    /// name it holds with its value, a view, in the table's order: the
    /// rows `get_defined_vars()` copies, `$_SERVER` and the other
    /// auto-global variables among them in the main frame's, as phpdbg's
    /// `info vars` walks them. `None`
    /// where the frame is not one this reaches.
    pub fn frame_vars(&mut self, frame: FrameId) -> Option<Vec<(Vec<u8>, Value)>> {
        self.in_frame(frame, |req, at| {
            let table = req.rebuild_symbol_table(at);
            let entries = req.table_mut(table).map(|rows| rows.clone())?;
            Some(
                entries
                    .into_iter()
                    .map(|(name, symbol)| {
                        let value = req.symbol_read(table, symbol);
                        (name.as_bytes().to_vec(), value)
                    })
                    .collect(),
            )
        })
        .flatten()
    }

    /// `f` served at the frame `id` names, with the request running on
    /// the stack it stands on, where it stands at its place: the frames
    /// a read, a write or an evaluation reaches
    /// ([`Request::set_frame_var`], [`Request::eval_in_frame`]). That
    /// is a frame on the running stack; one below a fiber, on the stack
    /// that resumed it, which the request runs on for `f`
    /// ([`Request::on_stack`]); and a suspended generator's, kept as a
    /// [`FrameId`] while it ran, which stands on top of the running
    /// stack for `f` as a resumption stands it and is suspended again
    /// after ([`crate::generator::with_kept_frame`]). And a suspended
    /// fiber's frame, on the stack the fiber holds, which the request runs
    /// on for `f` with the chain ending at the fiber's bottom frame; and a
    /// frame of a fiber a suspend passed through, waiting, whose chain
    /// runs on into its resumer's held stack ([`Request::on_held_stack`]).
    /// `None` for a frame popped.
    pub(super) fn in_frame<R>(
        &mut self,
        id: FrameId,
        f: impl FnOnce(&mut Request, FrameRef) -> R,
    ) -> Option<R> {
        if let Some((stack, at)) = self.frame_of(id).map(|view| (view.stack, view.at)) {
            return self.on_stack(stack, |req| f(req, at));
        }
        if let Some(coroutine) = id.coroutine
            && let Some(at) = coroutine
                .coroutine(self)
                .and_then(|held| id.place_on(&held.held()?.vm))
        {
            return self.on_held_stack(coroutine, |req| f(req, at));
        }
        crate::generator::with_kept_frame(self, id.generator?, id.id, f)
    }

    /// `frame` on the running stack, read: what an observer's handler is
    /// handed ([`crate::observer`]).
    pub(crate) fn frame_view(&self, frame: FrameRef) -> FrameView<'_> {
        FrameView {
            req: self,
            stack: 0,
            vm: &self.vm,
            at: frame,
        }
    }

    /// The innermost user frame on the chain, not being popped: where a
    /// native's call stands, read ([`crate::observer::Observed::caller`]),
    /// on a fiber whose callable is a native the resumer's.
    pub(crate) fn innermost_frame_view(&self) -> Option<FrameView<'_>> {
        self.executions().enumerate().find_map(|(stack, vm)| {
            let at = vm.innermost_at()?;
            Some(FrameView {
                req: self,
                stack,
                vm,
                at: FrameRef(u32::try_from(at).ok()?),
            })
        })
    }

    /// The walk from a native's call to `function` ([`FrameView::walk`]):
    /// the call, then the frames and calls below it. The native being
    /// served is the call on top of the running stack, which every path
    /// to a native's entry serves it from, told by its function since a
    /// call holds no number of the observer's; were it not, the call is
    /// read alone above the whole chain.
    pub(crate) fn native_walk(&self, function: Function) -> std::vec::IntoIter<ChainFrame<'_>> {
        let mut walk = vec![ChainFrame {
            req: self,
            read: Read::Native(function),
        }];
        let vm = &self.vm;
        // The records pushed before the call, which stand below it.
        let below = match self.serving_call() {
            Some(call) if call.function() == Some(function) => {
                vm.serving_at().map_or(0, |at| at.0 as usize)
            }
            _ => vm.depth(),
        };
        walk.extend(self.stack_walk(0, vm, 0..below));
        self.walk_below(0, &mut walk);
        walk.into_iter()
    }

    /// The frames `frames` numbers and the calls `calls` numbers on one
    /// stack, `stack` down the chain, read innermost first: the links a
    /// backtrace reads there ([`links_of`]).
    fn stack_walk<'a>(
        &'a self,
        stack: usize,
        vm: &'a Vm,
        records: Range<usize>,
    ) -> impl Iterator<Item = ChainFrame<'a>> {
        links_of(self, vm, records, Expand::Generators)
            .filter_map(move |link| ChainFrame::of(self, stack, vm, &link))
    }

    /// The stacks below the one `from` counts down the chain, each
    /// under the bottom frame of the fiber it resumed: php links
    /// `fiber->stack_bottom` to the frame that resumed the fiber.
    fn walk_below<'a>(&'a self, from: usize, walk: &mut Vec<ChainFrame<'a>>) {
        for (stack, vm) in self.executions().enumerate().skip(from.saturating_add(1)) {
            walk.push(ChainFrame {
                req: self,
                read: Read::FiberBottom,
            });
            walk.extend(self.stack_walk(stack, vm, 0..vm.depth()));
        }
    }
}
