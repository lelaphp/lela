//! The VM stack (design §5.6): frames of sixteen-byte slots that never
//! move while they live, and the entry slot the loop reads at every
//! `RETURN*` to know whether the frame returning is the entry frame of
//! the innermost serving (`jit-design.md` §2.2).

use crate::Zval;

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

    /// Pushed by the loop on entry; the guard pops on drop, so an `Err`
    /// pops too, since shutdown functions and destructors serve again
    /// after a fatal.
    pub fn push_entry(&mut self, f: FrameRef) -> EntryGuard<'_> {
        self.entries.push(f);
        EntryGuard { stack: self }
    }
}

/// What [`VmStack::push_entry`] hands back: the entry slot is popped when
/// this drops.
#[derive(Debug)]
pub struct EntryGuard<'a> {
    stack: &'a mut VmStack,
}

impl EntryGuard<'_> {
    /// The stack the guard holds, for the loop that owns both.
    pub fn stack(&mut self) -> &mut VmStack {
        self.stack
    }
}

impl Drop for EntryGuard<'_> {
    fn drop(&mut self) {
        self.stack.entries.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_entry_slot_pops_with_its_guard() {
        let mut stack = VmStack::new();
        let outer = stack.push_frame(2);
        {
            let mut guard = stack.push_entry(outer);
            let inner = guard.stack().push_frame(1);
            assert_eq!(guard.stack().entry(), outer);
            assert_eq!(guard.stack().current(), inner);
            guard.stack().pop_frame();
        }
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
