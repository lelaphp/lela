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
//! A frame's identity is given lazily, so a call pays nothing for it
//! until an extension asks.
//!
//! [`Observed::frame`]: crate::api::abi::Observed::frame

use std::cell::Cell;
use std::hash::{Hash, Hasher};
use std::num::NonZeroU64;

use php_heap::FrameRef;

use crate::api::call::Function;
use crate::api::values::ClassId;
use crate::exec::{self, FrameState, Vm};

use super::Request;

/// A frame kept past the borrow it was read in, php's `execute_data`
/// as an extension stores it: read again through [`Request::frame_of`],
/// which answers the frame it names or nothing, never another frame.
/// Two are equal where they name one frame.
#[derive(Debug, Clone, Copy)]
pub struct FrameId {
    /// Where the frame stood when the identity was read: where
    /// [`Request::frame_of`] looks first.
    at: FrameRef,
    id: NonZeroU64,
}

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
    req: &'a Request,
    /// The stack the frame stands on, counted down the chain: the
    /// running one is zero, the resumer of the innermost fiber one.
    below: usize,
    vm: &'a Vm,
    at: FrameRef,
}

impl std::fmt::Debug for FrameView<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FrameView")
            .field("below", &self.below)
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
        self.state()
            .and_then(|state| exec::body_of(&state.unit, state.body))
            .map(|code| code.filename.0.as_ref())
            .unwrap_or_default()
    }

    /// `EX(opline)->lineno`: the line the frame stands on, the call's
    /// for a frame below the innermost; zero past its op array.
    #[must_use]
    pub fn line(&self) -> u32 {
        self.state().map_or(0, exec::line_of)
    }

    /// `EX(prev_execute_data)` as far as user code goes: the user frame
    /// below this one, on its stack or, below a fiber's first frame, on
    /// the stack that resumed the fiber; none below the request's first.
    /// A frame being popped is passed over, as php has moved
    /// `EG(current_execute_data)` past it.
    #[must_use]
    pub fn caller(&self) -> Option<FrameView<'a>> {
        let mut below = self.below;
        let mut vm = self.vm;
        let mut at = self.at.0 as usize;
        loop {
            if let Some(lower) = at.checked_sub(1) {
                at = lower;
            } else {
                below += 1;
                vm = self.req.executions().nth(below)?.1;
                at = vm.frames().len();
                continue;
            }
            let frame = FrameRef(u32::try_from(at).ok()?);
            if vm.at(frame).is_some_and(|state| !state.leaving) {
                return Some(FrameView {
                    req: self.req,
                    below,
                    vm,
                    at: frame,
                });
            }
        }
    }
}

/// The identities the request has given, the last one's number.
#[derive(Debug, Default)]
pub(super) struct FrameIds(Cell<u64>);

impl FrameIds {
    /// A number no frame of the request was given before.
    fn give(&self) -> NonZeroU64 {
        let next = self.0.get().saturating_add(1);
        self.0.set(next);
        NonZeroU64::new(next).unwrap_or(NonZeroU64::MAX)
    }
}

impl Request {
    /// The frame `id` names, where it stands on the running stack or a
    /// resumer's below it; `None` where it stands on none, so a frame
    /// that has left them is refused and never read as the frame at its
    /// old place.
    #[must_use]
    pub fn frame_of(&self, id: FrameId) -> Option<FrameView<'_>> {
        let names = |state: &FrameState| !state.leaving && state.identity.get() == Some(id.id);
        let found = self
            .executions()
            .enumerate()
            .find_map(|(below, (_, vm))| {
                vm.at(id.at)
                    .is_some_and(names)
                    .then_some((below, vm, id.at))
            })
            .or_else(|| {
                // A generator's frame pushed back where another resumer
                // stands.
                self.executions().enumerate().find_map(|(below, (_, vm))| {
                    let at = vm.frames().iter().rposition(names)?;
                    Some((below, vm, FrameRef(u32::try_from(at).ok()?)))
                })
            });
        let (below, vm, at) = found?;
        Some(FrameView {
            req: self,
            below,
            vm,
            at,
        })
    }

    /// `frame` on the running stack, read: what an observer's handler is
    /// handed ([`crate::observer`]).
    pub(crate) fn frame_view(&self, frame: FrameRef) -> FrameView<'_> {
        FrameView {
            req: self,
            below: 0,
            vm: &self.vm,
            at: frame,
        }
    }
}
