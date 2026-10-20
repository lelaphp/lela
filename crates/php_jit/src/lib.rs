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
//! pipeline compiles what it recorded through the engine's `Backend`
//! and publishes it in the epoch's trace table (`compiler`); the next
//! visit to a compiled word enters its trace and follows it from trace to
//! trace (`dispatch::run`, `enter`). A host with no `Backend` runs the
//! tier `Off`.
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

    /// The tier's state (`jit-design.md` §4): its two directives,
    /// [`JitEngine`], built last at engine start, after the opcache
    /// extension registered php's `opcache.jit*`, and [`JitRequest`].
    fn extension() -> Option<Extension> {
        Some(
            ExtensionBuilder::default()
                .ini(policy::ini())
                .engine_state(JitEngine::new)
                .request_state::<JitRequest>()
                .rinit(JitEngine::request_started)
                .build("jit"),
        )
    }
}

/// The tier's request state (`jit-design.md` §4): whether a recording is
/// under way on the request, php's `JIT_G(tracing)`, which no fiber switch
/// saves (§4.9, revision 30), set and cleared by `record`'s guard
/// alone and read through `record::recording`.
#[derive(Debug, Default)]
pub(crate) struct JitRequest {
    recording: bool,
}

/// The tier's engine state (`jit-design.md` §4): where a recording
/// compiles, and what the request threads and the pipeline share. The
/// field order joins the worker before what it shares drops.
#[derive(Debug)]
pub(crate) struct JitEngine {
    compiler: compiler::Compiler,
    shared: Arc<Shared>,
}

/// What a safepoint, a recording and the pipeline read, by `Arc` so a
/// worker can hold it past the request that submitted a job
/// (`jit-design.md` §4.8): the `Policy` read once at engine start, the
/// host's `Backend`, none under `Mode::Off`, and the lines that start
/// logged. The trace table is not here: it is the epoch's (`table::of`),
/// so a restart's epoch starts with none.
///
/// The tier's lines go where php's `opcache.jit_debug` lines go, to
/// `stderr` (`Z/ext/opcache/jit/zend_jit_trace.c:8152-8250`), which for a
/// request is the host's error stream: the CLI's `stderr`, as php's.
#[derive(Debug)]
pub(crate) struct Shared {
    policy: policy::Policy,
    backend: Option<lower::Backend>,
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
    /// say, the host's `Backend` over the kernels `enter` lists, or the
    /// tier `Off` where the host has none, the lines its start logged,
    /// and the [`compiler::Compiler`] it runs.
    fn of(ini: &StartupIni) -> JitEngine {
        JitEngine::over(ini, lower::Backend::new)
    }

    /// [`JitEngine::of`] with the `Backend` `backend` makes for the host.
    fn over(
        ini: &StartupIni,
        backend: impl FnOnce(
            &policy::Policy,
            lower::Symbols,
        ) -> Result<lower::Backend, lower::Unsupported>,
    ) -> JitEngine {
        let mut started = Vec::new();
        let mut policy = policy::Policy::from_ini(ini, &mut started);
        let backend = (policy.mode() != policy::Mode::Off)
            .then(|| backend(&policy, enter::wrapped::symbols()))
            .and_then(|made| {
                made.map_err(|lower::Unsupported(why)| {
                    policy.off();
                    started.push(policy::Note::Off(policy::OffBecause::NoBackend(why)));
                })
                .ok()
            });
        let perf = if policy.mode() == policy::Mode::Off {
            table::Outputs::none()
        } else {
            table::Outputs::start(policy.debug())
        };
        let shared = Arc::new(Shared {
            policy,
            backend,
            started: started.into_boxed_slice(),
            told: AtomicBool::new(false),
            perf,
        });
        JitEngine {
            compiler: compiler::Compiler::new(&shared),
            shared,
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

    /// The engine start reads the `Policy`, runs the `Mode` it names and
    /// builds the host's `Backend` for it, none under `Off`.
    #[test]
    fn the_engine_start_runs_the_mode_asked() {
        let off = JitEngine::of(&ini(&[]));
        assert_eq!(off.shared.policy.mode(), Mode::Off);
        assert!(off.shared.backend.is_none());
        let asked = JitEngine::of(&ini(&[("opcache.jit", "tracing")]));
        assert_eq!(asked.shared.policy.mode(), Mode::Tracing);
        assert!(asked.shared.backend.is_some(), "this host has a backend");
        let lane = JitEngine::of(&ini(&[("jit.mode", "force")]));
        assert_eq!(lane.shared.policy.mode(), Mode::Force);
    }

    /// A host with no `Backend` runs the tier `Off`, and its start says
    /// why (`jit-design.md` §4.3).
    #[test]
    fn a_host_with_no_backend_runs_off() {
        let none = JitEngine::over(&ini(&[("opcache.jit", "tracing")]), |_, _| {
            Err(crate::lower::Unsupported("a test's host"))
        });
        assert_eq!(none.shared.policy.mode(), Mode::Off);
        assert!(none.shared.backend.is_none());
        let said: Vec<String> = none
            .shared
            .started
            .iter()
            .map(ToString::to_string)
            .collect();
        assert_eq!(said, ["JIT: off: no backend: a test's host"]);
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
