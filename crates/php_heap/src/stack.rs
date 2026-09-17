//! The VM stack (design §5.6): frames of sixteen-byte slots that never
//! move while they live, and the entry slot the loop reads at every
//! `RETURN*` to know whether the frame returning is the entry frame of
//! the innermost serving (`jit-design.md` §2.2).

use std::ptr::NonNull;

use crate::{Zval, ZvalRef};

/// A slot index within one frame: a CV, a temporary or an extra argument.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Slot(pub u32);

/// A frame's position on the stack; frames never move, so the position
/// is the identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct FrameRef(pub u32);

/// One call frame: its header fields live with the engine's `Func`; here
/// are its slots, typed and bounds-checked. A slot past the end reads as
/// `UNDEF` and a write to it is dropped, as an uninitialised zval reads.
#[derive(Debug, Default)]
pub struct Frame {
    slots: Box<[Zval]>,
}

impl Frame {
    /// A frame with `slots` slots, all `UNDEF`.
    #[must_use]
    pub fn new(slots: usize) -> Frame {
        Frame {
            slots: vec![Zval::UNDEF; slots].into_boxed_slice(),
        }
    }

    /// How many slots the frame has: the CVs, the temporaries and the
    /// extra arguments, which is what `i_free_compiled_variables` and
    /// the temporaries' release walk when the frame leaves.
    #[must_use]
    pub fn len(&self) -> usize {
        self.slots.len()
    }

    /// Whether the frame has no slots at all.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// The zval in `slot`, copied.
    #[must_use]
    pub fn slot(&self, slot: Slot) -> Zval {
        self.slots
            .get(slot.0 as usize)
            .copied()
            .unwrap_or(Zval::UNDEF)
    }

    /// Write `z` into `slot`.
    pub fn set(&mut self, slot: Slot, z: Zval) {
        if let Some(cell) = self.slots.get_mut(slot.0 as usize) {
            *cell = z;
        }
    }

    /// The zval in `slot` in place, php's `EX_VAR(var)`: what a
    /// `FETCH_*_W` points its `VAR` at and what the `ASSIGN*` over that
    /// `VAR` reads through (design §5.6). The slots never move while
    /// the frame lives, so the name holds as long as the frame does.
    /// None for a slot the frame does not have.
    pub fn slot_ptr(&mut self, slot: Slot) -> Option<ZvalRef> {
        self.slots
            .get_mut(slot.0 as usize)
            .map(|cell| ZvalRef::new(NonNull::from(cell)))
    }
}

/// The stack of one request, or of one fiber: pages of slots and the
/// entry slot.
#[derive(Debug, Default)]
pub struct VmStack {
    frames: Vec<Frame>,
    entries: Vec<FrameRef>,
}

impl VmStack {
    /// An empty stack.
    #[must_use]
    pub fn new() -> VmStack {
        VmStack::default()
    }

    /// Push a frame with `slots` slots and return its position.
    pub fn push_frame(&mut self, slots: usize) -> FrameRef {
        let at = u32::try_from(self.frames.len()).unwrap_or(u32::MAX);
        self.frames.push(Frame::new(slots));
        FrameRef(at)
    }

    /// Pop the frame the loop is executing.
    pub fn pop_frame(&mut self) {
        self.frames.pop();
    }

    /// The frame at `f`, if it is live.
    #[must_use]
    pub fn frame(&self, f: FrameRef) -> Option<&Frame> {
        self.frames.get(f.0 as usize)
    }

    /// The frame at `f`, mutably, if it is live.
    pub fn frame_mut(&mut self, f: FrameRef) -> Option<&mut Frame> {
        self.frames.get_mut(f.0 as usize)
    }

    /// The frame the loop is executing: the top of the stack.
    #[must_use]
    pub fn current(&self) -> FrameRef {
        FrameRef(u32::try_from(self.frames.len().saturating_sub(1)).unwrap_or(u32::MAX))
    }

    /// The entry frame of the innermost serving on this stack; the
    /// current frame when nothing has pushed an entry.
    #[must_use]
    pub fn entry(&self) -> FrameRef {
        self.entries
            .last()
            .copied()
            .unwrap_or_else(|| self.current())
    }

    /// Pushed by the loop on entry, and popped by [`VmStack::pop_entry`]
    /// when that serving ends, on the fatal path as on the normal one,
    /// since shutdown functions and destructors serve again after a
    /// fatal. The guard is the receipt: the loop holds the request and
    /// not the stack while it dispatches, so the pairing is a value it
    /// carries rather than a borrow it keeps.
    #[must_use = "the entry slot is popped by VmStack::pop_entry, which takes this"]
    pub fn push_entry(&mut self, f: FrameRef) -> EntryGuard {
        self.entries.push(f);
        EntryGuard {
            at: self.entries.len(),
        }
    }

    /// Pop the entry slot the guard is the receipt for, and any an inner
    /// serving left standing, so a serving that ended without popping its
    /// own cannot leave one behind.
    // Taking the receipt by value is the point: one serving pops its own
    // entry slot once, and a `Copy` guard could pop it twice.
    #[allow(clippy::needless_pass_by_value)]
    pub fn pop_entry(&mut self, guard: EntryGuard) {
        let EntryGuard { at } = guard;
        self.entries.truncate(at.saturating_sub(1));
    }
}

/// What [`VmStack::push_entry`] hands back, and what
/// [`VmStack::pop_entry`] takes: one serving's claim on the entry slot.
#[derive(Debug)]
pub struct EntryGuard {
    /// The depth of the entry slot stack with this serving's frame on it.
    at: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_entry_slot_pops_with_its_guard() {
        let mut stack = VmStack::new();
        let outer = stack.push_frame(2);
        let guard = stack.push_entry(outer);
        let inner = stack.push_frame(1);
        assert_eq!(stack.entry(), outer);
        assert_eq!(stack.current(), inner);
        stack.pop_frame();
        stack.pop_entry(guard);
        assert_eq!(stack.entry(), outer);
        if let Some(f) = stack.frame_mut(outer) {
            f.set(Slot(1), Zval::long(9));
        }
        assert_eq!(
            stack.frame(outer).map(|f| f.slot(Slot(1))),
            Some(Zval::long(9))
        );
        assert_eq!(
            stack.frame(outer).map(|f| f.slot(Slot(5))),
            Some(Zval::UNDEF)
        );
    }
}
