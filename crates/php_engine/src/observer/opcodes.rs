//! Observation of a declared set of opcodes (design §7.1, revisions 83
//! and 107, as #1780 and #2032 ruled): php's
//! `zend_set_user_opcode_handler` with a handler that answers
//! `ZEND_USER_OPCODE_DISPATCH`, and nothing else of it. An extension
//! declares, as the engine starts, which opcodes it observes
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
//! The loop tells it where it dispatches ([`Telling::before`]); a
//! stepping host's [`crate::tier::step`] where it does, and a frame's
//! parameter prologue at each `RECV*` it serves, which php's VM
//! dispatches ([`tell_before`]); so each of those is php's set.
//!
//! The declared set is the engine's, fixed as it starts
//! ([`ObservedOpcodes`]): a unit is shared by every engine and holds no
//! trace of it. So the engine chooses its loop there, as php chooses an
//! opline's handler when it compiles it: the loop is generic over
//! [`Telling`], and the engine binds [`Declared`]'s instantiation where
//! an extension declared an opcode and [`Untold`]'s, which has no test,
//! where none did (design §5.8). The told loop reads the set once, as
//! it is entered, and each opline pays one test of what it read.
//!
//! [`ExtensionBuilder::opcodes`]: crate::api::abi::ExtensionBuilder::opcodes

use crate::api::request::ExtensionId;
use php_heap::FrameRef;
use php_unit::Opcode;

use crate::api::abi::{Beneath, Start, chain};
use crate::api::request::Request;
use crate::supplied::Chain;
use crate::supplied::chain::{OpcodeArgs, OpcodeLink};

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
/// [`Observed::frame`] reads it, and the one beneath, which names the
/// opcode as php names it, `zend_get_opcode_name(EX(opline)->opcode)`
/// ([`Beneath::opcode`]), and which the handler serves where it will.
/// Nothing it does replaces the opcode's handler, which runs after it
/// whatever it did.
pub type OpcodeHandler = for<'a, 'b> fn(&mut Observed<'a>, Beneath<'b, chain::Opcode>);

/// What an extension's engine start answers of the opcodes it observes,
/// handed what its `MINIT` is ([`Start`]).
pub type OpcodesDeclared = fn(&Start) -> Opcodes;

/// One extension's opcode observer as its builder took it: what it
/// declares and the handler it is told through.
#[derive(Debug, Clone, Copy)]
pub(crate) struct OpcodeObserver {
    pub(crate) declared: OpcodesDeclared,
    pub(crate) handler: OpcodeHandler,
}

/// `zend_get_user_opcode_handler(opcode) != NULL` as the tier's start
/// reads it, once every extension's start has declared what it observes,
/// as `zend_jit_check_support` reads it after every `MINIT`
/// (`Z/ext/opcache/jit/zend_jit.c:3743-3756`): whether a declaration of
/// the bundle's, asked of `start`, names `opcode`. A declaration is a
/// plain function of the start it is handed, and an extension keeps no
/// state it could change (ADR-0005), so asking it again answers what
/// the engine fixes as it starts ([`ObservedOpcodes::start`]).
#[must_use]
pub fn observes(start: &Start, opcode: Opcode) -> bool {
    (start.opcodes.iter()).any(|observer| (observer.declared)(start).contains(opcode))
}

/// The engine's opcode observers, fixed as it starts: the links of the
/// chain, each declaring extension's set beside its handler, the first
/// supplier's first, and every opcode any of them declared.
#[derive(Debug, Default)]
pub(crate) struct ObservedOpcodes {
    any: Opcodes,
    links: Box<[(ExtensionId, OpcodeLink)]>,
}

impl ObservedOpcodes {
    /// Every observer's declaration asked of `start`, as php's
    /// `MINIT`s call `zend_set_user_opcode_handler`, each the chain's
    /// link in the order they were installed. One that declares nothing
    /// is not kept.
    pub(crate) fn start(chain: &Chain<OpcodeObserver>, start: &Start) -> ObservedOpcodes {
        let links: Box<[_]> = chain
            .links()
            .iter()
            .map(|&(by, observer)| {
                let link = OpcodeLink {
                    declared: (observer.declared)(start),
                    handler: observer.handler,
                };
                (by, link)
            })
            .filter(|(_, link)| !link.declared.is_empty())
            .collect();
        let any = links.iter().fold(Opcodes::default(), |any, (_, link)| {
            any.union(link.declared)
        });
        ObservedOpcodes { any, links }
    }

    /// Whether any extension declared an opcode, which chooses the loop
    /// the engine binds ([`crate::tier::Loops::bound`]).
    pub(crate) fn declares_any(&self) -> bool {
        !self.any.is_empty()
    }
}

/// Whether a loop tells the observers of the declared opcodes, a type
/// the loop is generic over (design §5.8, §7.1): the engine binds one
/// instantiation as it starts, [`Declared`]'s where an extension
/// declared an opcode and [`Untold`]'s where none did.
pub(crate) trait Telling: Copy + 'static {
    /// What a serving reads as its loop is entered.
    fn of(req: &Request) -> Self;

    /// The loop's home: the observers of `opcode` told of the opline
    /// `frame` stands at, `at`, before its handler runs. The loop holds
    /// the position in its own hand ([`crate::tier::Next`]), so a loop
    /// that tells writes it to the frame first, where an observer reads
    /// it.
    fn before(self, req: &mut Request, frame: FrameRef, at: u32, opcode: Opcode);

    /// Whether the observers of `opcode` are told of it, which oplines
    /// served past the loop's home ask first.
    fn tells(self, opcode: Opcode) -> bool;
}

/// The loop of an engine where no extension declared an opcode: it
/// reads nothing and tests nothing, as php's VM, whose handlers are
/// resolved when the opline is compiled, tests nothing for an opcode
/// with no user handler.
#[derive(Clone, Copy)]
pub(crate) struct Untold;

impl Telling for Untold {
    #[inline(always)] // nothing, so the loop has nothing of it
    #[allow(clippy::inline_always)]
    fn of(_: &Request) -> Untold {
        Untold
    }

    #[inline(always)] // as `of`
    #[allow(clippy::inline_always)]
    fn before(self, _: &mut Request, _: FrameRef, _: u32, _: Opcode) {}

    #[inline(always)] // as `of`
    #[allow(clippy::inline_always)]
    fn tells(self, _: Opcode) -> bool {
        false
    }
}

/// The loop of an engine where an extension declared an opcode: the
/// engine's set, read once per serving, and one test of it per opline.
/// [`tell_before`] reads it too, for an opline served outside the loop,
/// whichever loop the engine bound.
#[derive(Clone, Copy)]
pub(crate) struct Declared(Opcodes);

impl Telling for Declared {
    #[inline(always)] // read at the loop's entry, outside the loop
    #[allow(clippy::inline_always)]
    fn of(req: &Request) -> Declared {
        Declared(req.shared.opcodes.any)
    }

    #[inline(always)] // the loop pays a test and a branch, never a call
    #[allow(clippy::inline_always)]
    fn before(self, req: &mut Request, frame: FrameRef, at: u32, opcode: Opcode) {
        if self.0.contains(opcode) {
            req.vm_mut().stand(frame, at);
            tell(req, frame, opcode);
        }
    }

    #[inline(always)] // as `before`
    #[allow(clippy::inline_always)]
    fn tells(self, opcode: Opcode) -> bool {
        self.0.contains(opcode)
    }
}

/// [`Telling::before`] where one opline is dispatched outside the loop,
/// by a stepping host's [`crate::tier::step`], which is not generic over
/// the loop's choice and so pays one test of the set, a word that is zero
/// while none is declared. A parameter prologue served outside the loop
/// reads the set the same way, through [`Declared`]
/// ([`crate::exec::parameter_prologue`]).
#[inline(always)] // as `Telling::before`
#[allow(clippy::inline_always)]
pub(crate) fn tell_before(req: &mut Request, frame: FrameRef, opcode: Opcode) {
    let at = req.vm().opline(frame);
    Declared::of(req).before(req, frame, at, opcode);
}

/// php's `ZEND_USER_OPCODE` handler up to its dispatch: the chain of
/// the observers that declared `opcode` entered from the top, the later
/// supplier's first ([`chain::Opcode`]).
#[cold]
#[inline(never)]
fn tell(req: &mut Request, frame: FrameRef, opcode: Opcode) {
    let shared = std::sync::Arc::clone(&req.shared);
    Beneath::<chain::Opcode>::top(&shared.opcodes.links, OpcodeArgs { frame, opcode }).serve(req);
}

/// One link that declared the opcode told of the opline: `handler`
/// handed the frame standing at it and the ones beneath.
pub(crate) fn told(handler: OpcodeHandler, req: &mut Request, beneath: Beneath<'_, chain::Opcode>) {
    let at = Observable::Code(beneath.frame());
    handler(&mut Observed::standing(req, at), beneath);
}
