//! The compiled tier (`jit-design.md`; design §4.3 seam 8; ADR-0006):
//! [`Jit`], the `Tier` adapter beside the interpreter's `Nop`, which
//! `php_bundle` names as its `Tier` under the `jit` feature on the four
//! targets (`jit-design.md` §6). Externally the crate exports that one
//! item; what it reads of `php_engine` is `tier.lock`'s face.
//!
//! Today the tier runs `Mode::Off` and nothing else (`jit-design.md` §4,
//! revision 4 item 5): a safepoint answers `Continue` before it reads any
//! word, so an engine bound to `Jit` runs as one bound to `Nop` does.
//! The rest of §4's table, `policy` with what it reads, lands with the
//! code that fills it.
#![deny(unsafe_code)]
// seams.toml rule lints.no-panic: Internal invariant failures are InternalError fatals with a message, not panics (design §10).
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::todo,
    clippy::unimplemented
)]

use php_engine::tier::{Flow, FrameRef, Request, Site, Tier};

// The crossing has no caller until a trace calls a kernel, so it is built
// for its tests and the bench bar alone (#2742).
#[cfg(any(test, feature = "bench"))]
#[allow(unsafe_code, reason = "seams.toml rule unsafe.homes: php_jit::enter")]
mod enter;

// The code arena has no caller until a trace is published, so it is built
// for its tests alone (#2741).
#[cfg(test)]
mod table;

/// The bench bar's program (#2742), behind the `bench` feature, so the
/// crate's own surface stays `Jit`.
#[cfg(feature = "bench")]
pub use enter::bench;

/// The compiled tier (`jit-design.md` §2.4): asked at the loop's three
/// safepoints, under `Mode::Off` it answers [`Flow::Continue`] before it
/// reads the word the site carries, so it touches no shared state and
/// the loop runs `frame.opline` as it would under `Nop`.
#[derive(Debug, Clone, Copy, Default)]
pub struct Jit;

impl Tier for Jit {
    #[inline(always)] // jit-design.md §4: one constant under Mode::Off, so the branch on `Flow` folds as `Nop`'s does
    #[allow(clippy::inline_always)]
    fn safepoint(_: &mut Request, _: FrameRef, _: Site) -> Flow {
        Flow::Continue
    }
}
