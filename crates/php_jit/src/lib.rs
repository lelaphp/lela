//! The compiled tier (`jit-design.md`; design §4.3 seam 8; ADR-0006):
//! [`Jit`], the `Tier` adapter beside the interpreter's `Nop`, which
//! `php_bundle` names as its `Tier` under the `jit` feature on the four
//! targets (`jit-design.md` §6). Externally the crate exports that one
//! item; what it reads of `php_engine` is `tier.lock`'s face.
//!
//! The tier declares its state as an extension does (`jit-design.md`
//! §4): `jit.mode` and `jit.compile`, and `JitEngine`, whose `policy`
//! reads php's `opcache.jit*` directives at engine start. No `Backend`
//! lowers a trace yet (#2955), so `Mode` is `Off` whatever they say: a
//! safepoint answers `Continue` before it reads any word, and an engine
//! bound to `Jit` runs as one bound to `Nop` does.
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

use php_engine::tier::{
    Extension, ExtensionBuilder, Flow, FrameRef, Refusal, Request, Site, Start, StartupIni, Tier,
};

mod policy;
mod record;
mod table;
mod types;

// The crossing has no caller until a trace calls a kernel, so it is built
// for its tests and the bench bar alone (#2742).
#[cfg(any(test, feature = "bench"))]
#[allow(unsafe_code, reason = "seams.toml rule unsafe.homes: php_jit::enter")]
mod enter;

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

    /// The tier's state (`jit-design.md` §4): its two directives and
    /// [`JitEngine`], built last at engine start, after the opcache
    /// extension registered php's `opcache.jit*`.
    fn extension() -> Option<Extension> {
        Some(
            ExtensionBuilder::default()
                .ini(policy::ini())
                .engine_state(JitEngine::new)
                .build("jit"),
        )
    }
}

/// The tier's engine state (`jit-design.md` §4): the `Policy` read once at
/// engine start, and the lines its start logs, kept for the tier's log
/// until the engine has a sink a tier writes to.
#[derive(Debug)]
#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "dispatch (#2952) reads the policy and the tier's log (#2949's ticket) its lines"
    )
)]
pub(crate) struct JitEngine {
    policy: policy::Policy,
    started: Box<[policy::Note]>,
}

impl JitEngine {
    /// `JitEngine::new`, at engine start.
    #[expect(
        clippy::unnecessary_wraps,
        reason = "an engine start answers a Refusal where it fails; this one never does"
    )]
    fn new(start: &Start) -> Result<JitEngine, Refusal> {
        Ok(JitEngine::of(start.ini()))
    }

    /// The state over the startup ini: the `Policy` as the directives say,
    /// then `Off` with its one line, since no `Backend` lowers a trace on
    /// any target yet (#2955).
    fn of(ini: &StartupIni) -> JitEngine {
        let mut started = Vec::new();
        let policy = policy::Policy::from_ini(ini, &mut started).without_backend(&mut started);
        JitEngine {
            policy,
            started: started.into_boxed_slice(),
        }
    }
}

#[cfg(test)]
mod tests {
    use php_engine::tier::StartupIni;

    use super::JitEngine;
    use crate::policy::{Mode, Note, OffBecause};

    fn ini(entries: &[(&str, &str)]) -> StartupIni {
        StartupIni {
            entries: entries
                .iter()
                .map(|(name, value)| (name.as_bytes().to_vec(), Some(value.as_bytes().to_vec())))
                .collect(),
        }
    }

    /// The engine start reads the `Policy` and is `Off` with one line:
    /// php's default because `opcache.jit` is `disable`, and an
    /// `opcache.jit` that asks for the tier because no `Backend` lowers a
    /// trace yet.
    #[test]
    fn the_engine_start_runs_off_with_one_line_saying_why() {
        let started = JitEngine::of(&ini(&[]));
        assert_eq!(started.policy.mode(), Mode::Off);
        assert_eq!(&*started.started, &[Note::Off(OffBecause::Disabled)]);
        let asked = JitEngine::of(&ini(&[("opcache.jit", "tracing")]));
        assert_eq!(asked.policy.mode(), Mode::Off);
        assert_eq!(&*asked.started, &[Note::Off(OffBecause::NoBackend)]);
    }
}
