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
//! suspended generator's, a suspended fiber's.
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

use php_heap::{FrameRef, VmStack};

use crate::api::call::Function;
use crate::api::values::{ClassId, Value};
use crate::exec::{self, FrameState, Site, Vm};

use super::{Link, OpArrayView, OplineIdx, Request, Slots, links_of};

/// A frame kept past the borrow it was read in, php's `execute_data`
/// as an extension stores it: read again through [`Request::frame_of`],
/// which answers the frame it names or nothing, never another frame.
/// Two are equal where they name one frame.
#[derive(Debug, Clone, Copy)]
pub struct FrameId {
    /// Where the frame stood when the identity was read: where
    /// [`Request::frame_of`] looks first.
    at: FrameRef,
    /// The frame's identity, which [`FrameState`] holds.
    id: Identity,
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
            .and_then(|state| state.identity.get())
            .unwrap_or_else(|| {
                let id = self.req.frame_ids.give();
                if let Some(state) = state {
                    state.identity.set(Some(id));
                }
                id
            });
        FrameId { at: self.at, id }
    }

    /// The frame's state; a view is only made of a frame that stands, so
    /// none is this engine's error, read as an empty frame.
    fn state(&self) -> Option<&'a FrameState> {
        self.vm.at(self.at)
    }

    /// `EX(func)` where the frame serves a function or a method; `None`
    /// for an included, evaluated or main file's op array, which php
    /// names by its `filename` alone.
    #[must_use]
    pub fn function(&self) -> Option<Function> {
        self.state()?.function
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
        self.state().and_then(file_of).unwrap_or_default()
    }

    /// `EX(func)->op_array`: the op array the frame serves, a
    /// function's or an included, evaluated or main file's, read
    /// ([`OpArrayView`]).
    #[must_use]
    pub fn op_array(&self) -> Option<OpArrayView<'a>> {
        let state = self.state()?;
        exec::body_of(&state.unit, state.body).map(|code| OpArrayView::of(&state.unit, code))
    }

    /// `EX(opline)->lineno`: the line the frame stands on, the call's
    /// for a frame below the innermost; zero past its op array.
    #[must_use]
    pub fn line(&self) -> u32 {
        self.state().map_or(0, exec::line_of)
    }

    /// `EX(opline) - EX(func)->op_array.opcodes`: where in its op array
    /// the frame stands, the opline the loop dispatches next for the
    /// innermost frame and the call's for a frame below it.
    #[must_use]
    pub fn opline(&self) -> OplineIdx {
        OplineIdx(self.state().map_or(0, |state| state.opline))
    }

    /// `zend_get_opcode_name(EX(opline)->opcode)`: the opcode of the
    /// opline the frame stands on, as php names it, `ZEND_ECHO`, the
    /// call's for a frame below the innermost, as `zend_test`'s
    /// `observer.show_opcode` prints it from a begin or an end handler;
    /// empty past its op array.
    #[must_use]
    pub fn opcode(&self) -> &'static str {
        self.state()
            .and_then(|state| {
                exec::body_of(&state.unit, state.body)
                    .and_then(|code| exec::opline_at(code, state.opline))
            })
            .map_or("", |op| op.opcode.name())
    }

    /// `EX(prev_execute_data)` as far as user code goes: the user frame
    /// below this one, on its stack or, below a fiber's first frame, on
    /// the stack that resumed the fiber; none below the request's first.
    /// A frame being popped is passed over, as php has moved
    /// `EG(current_execute_data)` past it. The frames php links between
    /// two user frames are [`FrameView::walk`]'s.
    #[must_use]
    pub fn caller(&self) -> Option<FrameView<'a>> {
        let mut stack = self.stack;
        let mut vm = self.vm;
        let mut at = self.at.0 as usize;
        loop {
            if let Some(lower) = at.checked_sub(1) {
                at = lower;
            } else {
                stack += 1;
                vm = self.req.executions().nth(stack)?.1;
                at = vm.frames().len();
                continue;
            }
            let frame = FrameRef(u32::try_from(at).ok()?);
            if vm.at(frame).is_some_and(|state| !state.leaving) {
                return Some(FrameView {
                    req: self.req,
                    stack,
                    vm,
                    at: frame,
                });
            }
        }
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
        if let Some(generator) = state.and_then(|state| state.generator) {
            walk.extend(
                req.placeholder_links(generator)
                    .iter()
                    .map(|link| ChainFrame::of(req, self.stack, self.vm, link)),
            );
        }
        let calls = state.map_or(0, |state| {
            usize::try_from(state.calls_below).unwrap_or(usize::MAX)
        });
        if let Some((stack, _)) = req.executions().nth(self.stack) {
            walk.extend(req.stack_walk(
                self.stack,
                stack,
                self.vm,
                0..self.at.0 as usize,
                0..calls,
            ));
        }
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
    Kept(&'a FrameState),
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
fn file_of(state: &FrameState) -> Option<&[u8]> {
    exec::body_of(&state.unit, state.body).map(|code| code.filename.0.as_ref())
}

impl<'a> ChainFrame<'a> {
    /// The frame `link`, on the stack `stack` counts down the chain.
    fn of(req: &'a Request, stack: usize, vm: &'a Vm, link: &Link<'a>) -> ChainFrame<'a> {
        let read = match *link {
            Link::Frame(Slots::Stack(_, at), _) => Read::Stack(FrameView {
                req,
                stack,
                vm,
                at: FrameRef(u32::try_from(at).unwrap_or(u32::MAX)),
            }),
            Link::Frame(Slots::Kept(_), state) => Read::Kept(state),
            Link::Call(call) => Read::Native(call.function),
            Link::Site(site) => Read::Site(site),
        };
        ChainFrame { req, read }
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
            Read::Kept(state) => state.function,
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
        let names = |state: &FrameState| !state.leaving && state.identity.get() == Some(id.id);
        let found = self
            .executions()
            .enumerate()
            .find_map(|(stack, (_, vm))| {
                vm.at(id.at)
                    .is_some_and(names)
                    .then_some((stack, vm, id.at))
            })
            .or_else(|| {
                // A generator's frame pushed back where another resumer
                // stands.
                self.executions().enumerate().find_map(|(stack, (_, vm))| {
                    let at = vm.frames().iter().rposition(names)?;
                    Some((stack, vm, FrameRef(u32::try_from(at).ok()?)))
                })
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
    /// knows no such name, or is not one this reaches: popped,
    /// suspended, or below a fiber.
    pub fn frame_var(&mut self, frame: FrameId, name: &[u8]) -> Option<Value> {
        let at = self.running_frame_of(frame)?;
        if let Some(table) = self.vm.at(at).and_then(|state| state.symbol_table) {
            return self.symbol_value(table, name);
        }
        let cv = self
            .frame_var_names(at)
            .iter()
            .position(|held| held.as_slice() == name)?;
        Some(self.slot(at, u32::try_from(cv).ok()?))
    }

    /// `zend_rebuild_symbol_table()` over the frame `id` names, and each
    /// name it holds with its value, a view, in the table's order: the
    /// rows `get_defined_vars()` copies, `$_SERVER` and the other
    /// auto-global variables among them in the main frame's, as phpdbg's
    /// `info vars` walks them. `None`
    /// where the frame is not one this reaches.
    pub fn frame_vars(&mut self, frame: FrameId) -> Option<Vec<(Vec<u8>, Value)>> {
        let at = self.running_frame_of(frame)?;
        let table = self.rebuild_symbol_table(at);
        let entries = self.table_mut(table).map(|rows| rows.clone())?;
        Some(
            entries
                .into_iter()
                .map(|(name, symbol)| {
                    let value = self.symbol_read(table, symbol);
                    (name, value)
                })
                .collect(),
        )
    }

    /// Where the frame `id` names stands, where that is the running
    /// stack: the frames a write or an evaluation reaches
    /// ([`Request::set_frame_var`], [`Request::eval_in_frame`]).
    pub(super) fn running_frame_of(&self, id: FrameId) -> Option<FrameRef> {
        let view = self.frame_of(id)?;
        (view.stack == 0).then_some(view.at)
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
        self.executions().enumerate().find_map(|(stack, (_, vm))| {
            let at = vm.frames().iter().rposition(|state| !state.leaving)?;
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
        let served = vm.calls().len();
        let (frames, calls) = match vm.serving_call() {
            Some(call) if call.function == function => {
                let at = served.saturating_sub(1);
                // The frames pushed before the call: a frame pushed while
                // it is served counts it among the calls below.
                let frames = vm
                    .frames()
                    .iter()
                    .take_while(|state| {
                        usize::try_from(state.calls_below).is_ok_and(|below| below <= at)
                    })
                    .count();
                (frames, at)
            }
            _ => (vm.frames().len(), served),
        };
        walk.extend(self.stack_walk(0, &self.stack, vm, 0..frames, 0..calls));
        self.walk_below(0, &mut walk);
        walk.into_iter()
    }

    /// The frames `frames` numbers and the calls `calls` numbers on one
    /// stack, `stack` down the chain, read innermost first: the links a
    /// backtrace reads there ([`links_of`]).
    fn stack_walk<'a>(
        &'a self,
        stack: usize,
        on: &'a VmStack,
        vm: &'a Vm,
        frames: Range<usize>,
        calls: Range<usize>,
    ) -> impl Iterator<Item = ChainFrame<'a>> {
        links_of(on, vm, frames, calls, &|root| self.placeholder_links(root))
            .into_iter()
            .map(move |link| ChainFrame::of(self, stack, vm, &link))
    }

    /// The stacks below the one `from` counts down the chain, each
    /// under the bottom frame of the fiber it resumed: php links
    /// `fiber->stack_bottom` to the frame that resumed the fiber.
    fn walk_below<'a>(&'a self, from: usize, walk: &mut Vec<ChainFrame<'a>>) {
        for (stack, (on, vm)) in self.executions().enumerate().skip(from.saturating_add(1)) {
            walk.push(ChainFrame {
                req: self,
                read: Read::FiberBottom,
            });
            walk.extend(self.stack_walk(stack, on, vm, 0..vm.frames().len(), 0..vm.calls().len()));
        }
    }
}
