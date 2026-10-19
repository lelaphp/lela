//! The compiled tier (`jit-design.md`; design §4.3 seam 8; ADR-0006):
//! [`Jit`], the `Tier` adapter beside the interpreter's `Nop`, which
//! `php_bundle` names as its `Tier` under the `jit` feature on the four
//! targets (`jit-design.md` §6). Externally the crate exports that one
//! item; what it reads of `php_engine` is `tier.lock`'s face.
//!
//! The tier declares its state as an extension does (`jit-design.md`
//! §4): `jit.mode` and `jit.compile`, and `JitEngine`, whose `policy`
//! reads php's `opcache.jit*` directives at engine start. Under
//! `Mode::Off` a safepoint answers `Continue` before it reads any word,
//! and an engine bound to `Jit` runs as one bound to `Nop` does;
//! otherwise a hot safepoint records (`dispatch`, `record`) and the
//! pipeline numbers what would compile (`compiler`). No `Backend` lowers
//! a trace yet (#2955), so nothing is entered.
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

use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use php_engine::tier::{
    Extension, ExtensionBuilder, Flow, FrameRef, Refusal, Request, Site, Start, StartupIni, Thrown,
    Tier,
};

mod compiler;
mod dispatch;
mod lower;
mod policy;
mod record;
mod table;
#[cfg(test)]
mod testing;
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
/// safepoints, it visits the word the site carries, and under `Mode::Off`
/// answers [`Flow::Continue`] before it reads it, so it touches no shared
/// state and the loop runs `frame.opline` as it would under `Nop`.
#[derive(Debug, Clone, Copy, Default)]
pub struct Jit;

impl Tier for Jit {
    #[inline(always)] // jit-design.md §4: one constant under Mode::Off, so the branch on `Flow` folds as `Nop`'s does
    #[allow(clippy::inline_always)]
    fn safepoint(req: &mut Request, frame: FrameRef, site: Site) -> Flow {
        dispatch::safepoint(req, frame, site)
    }

    /// The tier's state (`jit-design.md` §4): its two directives and
    /// [`JitEngine`], built last at engine start, after the opcache
    /// extension registered php's `opcache.jit*`.
    fn extension() -> Option<Extension> {
        Some(
            ExtensionBuilder::default()
                .ini(policy::ini())
                .engine_state(JitEngine::new)
                .rinit(JitEngine::request_started)
                .build("jit"),
        )
    }
}

/// The tier's engine state (`jit-design.md` §4): what the request
/// threads and the pipeline share.
#[derive(Debug)]
pub(crate) struct JitEngine {
    shared: Arc<Shared>,
}

/// What a safepoint, a recording and the pipeline read, by `Arc` so a
/// worker can hold it past the request that submitted a job
/// (`jit-design.md` §4.8): the `Policy` read once at engine start and the
/// lines that start logged. The trace table is not here: it is the
/// epoch's (`table::of`), so a restart's epoch starts with none.
///
/// The tier's lines go where php's `opcache.jit_debug` lines go, to
/// `stderr` (`Z/ext/opcache/jit/zend_jit_trace.c:8152-8250`), which for a
/// request is the host's error stream: the CLI's `stderr`, as php's.
#[derive(Debug)]
pub(crate) struct Shared {
    policy: policy::Policy,
    started: Box<[policy::Note]>,
    /// Whether a request wrote the start's lines yet: once per engine.
    told: AtomicBool,
    /// The engine's `PERF` outputs, which every epoch's table writes to.
    perf: table::Outputs,
}

impl Shared {
    /// `event` as php's `jit_debug` line, where `opcache.jit_debug` asks for it,
    /// added to `lines`.
    fn dump(&self, lines: &mut Vec<u8>, event: &policy::TraceEvent<'_>) {
        // Writing to a `Vec` does not fail.
        self.policy.dump(lines, event).unwrap_or_default();
    }
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

    /// The state over the startup ini: the `Policy` as the directives
    /// say, and the lines its start logged.
    fn of(ini: &StartupIni) -> JitEngine {
        let mut started = Vec::new();
        let policy = policy::Policy::from_ini(ini, &mut started);
        let perf = if policy.mode() == policy::Mode::Off {
            table::Outputs::none()
        } else {
            table::Outputs::start(policy.debug())
        };
        JitEngine {
            shared: Arc::new(Shared {
                policy,
                started: started.into_boxed_slice(),
                told: AtomicBool::new(false),
                perf,
            }),
        }
    }

    /// The first request's start writes the lines the engine's start
    /// logged, once and where `opcache.jit_debug` asks for any line, so
    /// php's default writes nothing (`jit-design.md` §4).
    #[expect(
        clippy::unnecessary_wraps,
        reason = "a request start answers a Thrown where it fails; this one never does"
    )]
    fn request_started(req: &mut Request) -> Result<(), Thrown> {
        let Some(engine) = req.engine::<JitEngine>() else {
            return Ok(());
        };
        let shared = &engine.shared;
        if !shared.policy.debugs() || shared.told.swap(true, Ordering::Relaxed) {
            return Ok(());
        }
        let mut lines = String::new();
        for note in &shared.started {
            // Writing to a `String` does not fail.
            writeln!(lines, "{note}").unwrap_or_default();
        }
        req.write_stderr(lines.as_bytes());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::JitEngine;
    use crate::policy::Mode;
    use crate::testing::{self, Lane, ini};

    /// The engine start reads the `Policy` and runs the `Mode` it names,
    /// whether or not a `Backend` lowers a trace.
    #[test]
    fn the_engine_start_runs_the_mode_asked() {
        assert_eq!(JitEngine::of(&ini(&[])).shared.policy.mode(), Mode::Off);
        let asked = JitEngine::of(&ini(&[("opcache.jit", "tracing")]));
        assert_eq!(asked.shared.policy.mode(), Mode::Tracing);
        let lane = JitEngine::of(&ini(&[("jit.mode", "force")]));
        assert_eq!(lane.shared.policy.mode(), Mode::Force);
    }

    /// The start's lines are written once, by the first request, and only
    /// where `opcache.jit_debug` asks for any: php's default writes none.
    #[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
    #[test]
    fn the_start_logs_once_and_only_under_jit_debug() {
        let quiet = testing::serve::<Lane>(&[("jit.mode", "force")], "echo 1;");
        assert_eq!(String::from_utf8_lossy(&quiet.stderr), "");
        let scratch = testing::Scratch::new();
        let engine = testing::engine::<Lane>(&[("jit.mode", "force"), ("opcache.jit_debug", "1")]);
        let first = scratch.serve(&engine, "echo 1;");
        assert_eq!(
            String::from_utf8_lossy(&first.stderr),
            "JIT: jit.mode=force\n"
        );
        let second = scratch.serve(&engine, "echo 1;");
        assert_eq!(String::from_utf8_lossy(&second.stderr), "");
    }
}
