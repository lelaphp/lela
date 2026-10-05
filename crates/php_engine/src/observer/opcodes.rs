//! Observation of a declared set of opcodes (design §7.1, revision 83,
//! as #1780 ruled): php's `zend_set_user_opcode_handler` with a handler
//! that answers `ZEND_USER_OPCODE_DISPATCH`, and nothing else of it. An
//! extension declares, as the engine starts, which opcodes it observes
//! ([`ExtensionBuilder::opcodes`]); the loop tells it of each opline of
//! one of them before the opcode's handler runs, and the handler always
//! runs. No extension replaces a handler: every semantic lives in one
//! function called from one loop.
//!
//! php resolves an opline's handler as the opline is compiled, and an
//! opcode with a user handler resolves to `ZEND_USER_OPCODE`'s
//! (`zend_vm_set_opcode_handler`, `Z/Zend/zend_execute.c:5913-5926`),
//! whose handler calls the user's and then dispatches the opline's own
//! (`Z/Zend/zend_vm_def.h:8237-8268`). So it is told of every opline the
//! VM dispatches and of no other: not of an `OP_DATA` its predecessor
//! reads, not of a `JMPZ` a smart branch steps over, not of the `RECV`
//! of a passed argument a function with no declared type skips, and not
//! of the `exception_op` the unwinder serves, whose handler
//! `zend_startup` resolved before any `MINIT` (`Z/Zend/zend.c:1064`).
//! The loop tells it where it dispatches ([`Declared::before`]); a
//! stepping host's [`crate::tier::step`] where it does, and a frame's
//! parameter prologue at each `RECV*` it serves, which php's VM
//! dispatches ([`tell_before`]); so each of those is php's set.
//!
//! The declared set is the engine's, fixed as it starts
//! ([`ObservedOpcodes`]): a unit is shared by every engine and holds no
//! trace of it. A serving reads it once, as its loop is entered, and
//! each opline pays one test of what it read; with none declared that
//! is a word that is zero.
//!
//! [`ExtensionBuilder::opcodes`]: crate::api::abi::ExtensionBuilder::opcodes

use php_heap::FrameRef;
use php_unit::Opcode;

use crate::api::abi::Start;
use crate::api::request::Request;
use crate::supplied::Chain;

use super::{Observable, Observed};

/// A set of opcodes, each named as php names it, `ZEND_ECHO`: what an
/// extension declares it observes ([`ExtensionBuilder::opcodes`]).
///
/// [`ExtensionBuilder::opcodes`]: crate::api::abi::ExtensionBuilder::opcodes
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Opcodes([u64; 4]);

impl Opcodes {
    /// The opcode `name` names added, php's `zend_get_opcode_id`
    /// (`Z/Zend/zend_vm_opcodes.c:465-474`): the opcode of the lowest
    /// number whose name opens with `name`, so `ZEND_BEGIN` is
    /// `ZEND_BEGIN_SILENCE`; `false`, and nothing added, where no
    /// opcode's does, for which php's `zend_test` warns
    /// `Invalid opcode name`. php's search stops one short of the length
    /// of its table of names, so its last opcode,
    /// `ZEND_DECLARE_ATTRIBUTED_CONST`, is never found.
    pub fn insert(&mut self, name: &[u8]) -> bool {
        let last = (0..=u8::MAX).rev().find_map(Opcode::from_u8);
        let found = (0..=u8::MAX)
            .filter_map(Opcode::from_u8)
            .take_while(|opcode| Some(*opcode) != last)
            .find(|opcode| opcode.name().as_bytes().starts_with(name));
        let Some(opcode) = found else {
            return false;
        };
        let (word, bit) = Self::place(opcode);
        if let Some(word) = self.0.get_mut(word) {
            *word |= bit;
        }
        true
    }

    /// Whether no opcode is in the set.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0 == [0; 4]
    }

    /// Whether `opcode` is in the set.
    #[inline(always)] // the loop's test of an opline: one word and one bit, never a call
    #[allow(clippy::inline_always)]
    pub(crate) fn contains(&self, opcode: Opcode) -> bool {
        let (word, bit) = Self::place(opcode);
        self.0.get(word).is_some_and(|word| word & bit != 0)
    }

    /// Every opcode of either set.
    fn union(self, other: Opcodes) -> Opcodes {
        let mut words = self.0;
        for (word, other) in words.iter_mut().zip(other.0) {
            *word |= other;
        }
        Opcodes(words)
    }

    /// The word and the bit `opcode`'s number stands at.
    #[inline(always)] // as `contains`
    #[allow(clippy::inline_always)]
    fn place(opcode: Opcode) -> (usize, u64) {
        let n = opcode.num();
        (usize::from(n >> 6), 1 << (n & 63))
    }
}

/// What an extension observing opcodes is told of an opline, before
/// the opcode's handler runs: the frame standing at it, as
/// [`Observed::frame`] reads it, and the opcode's name as php names
/// it, `zend_get_opcode_name(EX(opline)->opcode)`. Nothing it does
/// replaces the handler, which runs after it whatever it did.
pub type OpcodeHandler = fn(&mut Observed<'_>, &'static str);

/// What an extension's engine start answers of the opcodes it observes,
/// handed what its `MINIT` is ([`Start`]).
pub type OpcodesDeclared = fn(&Start) -> Opcodes;

/// One extension's opcode observer as its builder took it: what it
/// declares and the handler it is told through.
#[derive(Clone, Copy)]
pub(crate) struct OpcodeObserver {
    pub(crate) declared: OpcodesDeclared,
    pub(crate) handler: OpcodeHandler,
}

/// The engine's opcode observers, fixed as it starts: each declaring
/// extension's set beside its handler, the later supplier's first, and
/// every opcode any of them declared.
#[derive(Debug, Default)]
pub(crate) struct ObservedOpcodes {
    any: Opcodes,
    observing: bool,
    observers: Box<[(Opcodes, OpcodeHandler)]>,
}

impl ObservedOpcodes {
    /// Every observer's declaration asked of `start`, as php's
    /// `MINIT`s call `zend_set_user_opcode_handler`. Each is told from
    /// the top of `chain` down: php keeps one handler per opcode, and an
    /// extension that sets one over another's saves it and calls it after
    /// its own work, as xdebug's `xdebug_set_opcode_handler` does, so the
    /// later supplier is told first. One that declares nothing is not
    /// kept.
    pub(crate) fn start(chain: &Chain<OpcodeObserver>, start: &Start) -> ObservedOpcodes {
        let observers: Box<[_]> = chain
            .top_down()
            .map(|observer| ((observer.declared)(start), observer.handler))
            .filter(|(declared, _)| !declared.is_empty())
            .collect();
        let any = observers
            .iter()
            .fold(Opcodes::default(), |any, (declared, _)| {
                any.union(*declared)
            });
        ObservedOpcodes {
            any,
            observing: !any.is_empty(),
            observers,
        }
    }

    /// Whether no extension observes any opcode.
    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.any.is_empty() && self.observers.is_empty()
    }
}

/// What one serving read of the declared set as its loop was entered:
/// the set, and whether it holds any opcode, which is the one test an
/// opline pays while it holds none.
#[derive(Clone, Copy)]
pub(crate) struct Declared {
    observing: bool,
    opcodes: Opcodes,
}

impl Declared {
    /// The engine's set, read once per serving.
    #[inline(always)] // read at the loop's entry, outside the loop
    #[allow(clippy::inline_always)]
    pub(crate) fn of(req: &Request) -> Declared {
        Declared {
            observing: req.shared.opcodes.observing,
            opcodes: req.shared.opcodes.any,
        }
    }

    /// The loop's home: the observers of `opcode` told of the opline
    /// `frame` stands at, before its handler runs. One test of a word
    /// while none is declared.
    #[inline(always)] // the loop pays a test and a branch, never a call
    #[allow(clippy::inline_always)]
    pub(crate) fn before(self, req: &mut Request, frame: FrameRef, opcode: Opcode) {
        if self.observing && self.opcodes.contains(opcode) {
            tell(req, frame, opcode);
        }
    }
}

/// [`Declared::before`] where one opline is dispatched outside the
/// loop: by a stepping host's [`crate::tier::step`], and by a frame's
/// parameter prologue, which serves the `RECV*` php's VM dispatches
/// ([`crate::exec::parameter_prologue`]). One load and one branch while
/// none is declared.
#[inline(always)] // as `Declared::before`
#[allow(clippy::inline_always)]
pub(crate) fn tell_before(req: &mut Request, frame: FrameRef, opcode: Opcode) {
    Declared::of(req).before(req, frame, opcode);
}

/// php's `ZEND_USER_OPCODE` handler up to its dispatch: every observer
/// that declared `opcode` told of the opline, the later supplier's first.
#[cold]
#[inline(never)]
fn tell(req: &mut Request, frame: FrameRef, opcode: Opcode) {
    let shared = std::sync::Arc::clone(&req.shared);
    let at = Observable::Code(frame);
    for (declared, handler) in &shared.opcodes.observers {
        if declared.contains(opcode) {
            handler(&mut Observed { req, at }, opcode.name());
        }
    }
}
