//! The compiled tier (`jit-design.md`; design §4.3 seam 8; ADR-0006):
//! [`Jit`], the `Tier` adapter beside the interpreter's `Nop`, which
//! `php_bundle` names as its `Tier` under the `jit` feature on the four
//! targets (`jit-design.md` §6). Externally the crate exports that one
//! item; what it reads of `php_engine` is `tier.lock`'s face.
//!
//! The tier declares its state as an extension does (`jit-design.md`
//! §4): `jit.mode` and `jit.compile`, and `JitEngine`, whose `policy`
//! reads php's `opcache.jit*` directives at engine start. Under
//! `Mode::Off` the engine binds `Nop`'s loop (`Jit::asked`), so it runs
//! as one bound to `Nop` does and no safepoint reads a word;
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

use std::cell::Cell;
use std::fmt::Write as _;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use php_engine::api::raising::{self, Level};
use php_engine::api::request::TierStatus;
use php_engine::tier::{
    self, Bound, Extension, ExtensionBuilder, Flow, FrameRef, Laid, Refusal, Request, Site, Start,
    Started, StartupIni, Thrown, Tier,
};
use php_unit::Opcode;

mod compiler;
mod dispatch;
use dispatch::JitRequest;
mod lower;
#[cfg(test)]
mod observed;
mod policy;
mod record;
mod stepping;
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
/// safepoints, it visits the word the site carries; under `Mode::Off`
/// it is never asked, the engine bound to `Nop`'s loop ([`Jit::asked`]).
#[derive(Debug, Clone, Copy, Default)]
pub struct Jit;

impl Tier for Jit {
    type State = JitRequest;

    /// The site visited ([`dispatch::safepoint`]); in the bench's build of
    /// the stepping arm alone, `cfg(php_jit_stepping)`, which only `xtask
    /// bench` sets, the serving stepped to its end instead ([`stepping`],
    /// #2946). Every other build's `cfg!` is `false` and the branch folds
    /// away.
    fn safepoint(req: &mut Request, frame: FrameRef, site: Site, held: tier::Held<'_>) -> Flow {
        if cfg!(php_jit_stepping) {
            return stepping::driven(req, frame);
        }
        dispatch::safepoint(req, frame, site, held)
    }

    /// The engine binds `Jit`'s loop where the tier runs from the start,
    /// both loops where it is set up and off and a request may turn
    /// `opcache.jit` on, and `Nop`'s alone where it never can, so the
    /// loop pays nothing at a safepoint, as php's VM keeps its own
    /// handlers where its compiled tier does not start
    /// ([`policy::Policy::bound`]); `Nop`'s where the engine built no
    /// [`JitEngine`].
    fn bound(started: &Started<'_>) -> Bound {
        started
            .engine::<JitEngine>()
            .map_or(Bound::Never, |engine| engine.shared.bound)
    }

    /// [`JitRequest`], laid with each request of an engine that binds
    /// `Jit`'s loop.
    fn lay(started: &Started<'_>) -> Option<JitRequest> {
        JitRequest::laid(*started)
    }

    /// php's `zend_jit_status` ([`policy::Policy::status`]): `req`'s
    /// `JIT_G` ([`tier::setting`]), and the buffer less what the
    /// traces of `req`'s epoch take ([`table::TraceTable::usage`]), none
    /// where no request of the epoch made its table.
    fn status(req: &Request) -> TierStatus {
        if req.engine::<JitEngine>().is_none() {
            return TierStatus::default();
        }
        let epoch = tier::epoch(req);
        let used = table::of_epoch(&epoch).map_or(0, |table| table.usage().bytes);
        policy::Policy::status(used, tier::setting(req))
    }

    /// How a unit published now lays its words ([`policy::Policy::laid`]):
    /// as `JIT_G(on)` of `req` says ([`tier::setting`]), the start's
    /// where the engine publishes it for no request; sealed where the
    /// engine built no [`JitEngine`]. The engine asks this of a unit it
    /// compiled, and lays one the file cache read back sealed, as php
    /// restores op arrays from its file cache with no counters:
    ///
    /// ```php
    /// $dir = realpath(sys_get_temp_dir()) . '/jit-file-cache-' . getmypid(); mkdir($dir); mkdir("$dir/fc");
    /// file_put_contents("$dir/main.php", '<?php if ($argv[1] === "switch") { ini_set("opcache.jit", "tracing"); } include __DIR__ . "/loop.php";');
    /// file_put_contents("$dir/loop.php", '<?php $s = 0; for ($i = 0; $i < 4; $i++) { $s += $i; } echo $s, "\n";');
    /// touch("$dir/main.php", time() - 60); touch("$dir/loop.php", time() - 60);
    /// foreach (['keep', 'switch'] as $run) {
    ///     $cmd = [PHP_BINARY, '-n', '-d', 'opcache.enable_cli=1', '-d', 'opcache.jit=off', '-d', 'opcache.jit_debug=12288', '-d', "opcache.file_cache=$dir/fc", "$dir/main.php", $run];
    ///     $p = proc_open($cmd, [1 => ['pipe', 'w'], 2 => ['pipe', 'w']], $pipes);
    ///     echo stream_get_contents($pipes[1]), str_replace($dir, 'DIR', stream_get_contents($pipes[2]));
    ///     proc_close($p);
    /// }
    /// exec('rm -rf ' . escapeshellarg($dir));
    /// ```
    /// ```expect process
    /// 6
    /// 6
    /// ```
    fn laid(started: &Started<'_>, req: Option<&Request>) -> Laid {
        let Some(engine) = started.engine::<JitEngine>() else {
            return Laid::Sealed;
        };
        let policy = &engine.shared.policy;
        let jit_g = req.map_or(policy.start_setting(), |req| *tier::setting(req));
        policy.laid(jit_g.on)
    }

    /// php's `JIT_G` past its start ([`policy::Policy::start_setting`]):
    /// the start's directives as the tier's `Policy` read them, folded as
    /// `zend_jit_config` folds `opcache.jit` and rewritten as php's start
    /// rewrites `JIT_G` past it, in place of `startup`, the opcache
    /// extension's fold of the same host value, which knows nothing of
    /// the tier's start; `startup` where the engine built no
    /// [`JitEngine`].
    fn setting(started: &Started<'_>, startup: TierStatus) -> TierStatus {
        started
            .engine::<JitEngine>()
            .map_or(startup, |engine| engine.shared.policy.start_setting())
    }

    /// The tier's state (`jit-design.md` §4): its two directives and
    /// [`JitEngine`], built last at engine start, after the opcache
    /// extension registered php's `opcache.jit*`; its request's,
    /// [`JitRequest`], is the tier's own ([`Jit::lay`]).
    /// Under `cfg(php_jit_usage)`, which only `xtask bench` sets, a
    /// request's end also writes what its epoch's traces took
    /// ([`JitEngine::request_ended`], #2961).
    fn extension() -> Option<Extension> {
        let tier = ExtensionBuilder::default()
            .ini(policy::ini())
            .engine_state(JitEngine::new)
            .rinit(JitEngine::request_started);
        #[cfg(php_jit_usage)]
        let tier = tier.rshutdown(JitEngine::request_ended);
        Some(tier.build("jit"))
    }
}

impl JitRequest {
    /// The state a request of the engine `started` is laid with, its
    /// engine state's [`Shared`]; none where the engine built no
    /// [`JitEngine`].
    fn laid(started: Started<'_>) -> Option<JitRequest> {
        let engine = started.engine::<JitEngine>()?;
        Some(JitRequest::of(&engine.shared, engine.compiler.submitter()))
    }

    /// The state a test's recorder lays over its `Policy` alone, with no
    /// engine state of the tier's: no `Backend` and no `PERF` file.
    #[cfg(test)]
    fn recorder(policy: &policy::Policy) -> JitRequest {
        let shared = Arc::new(Shared {
            policy: policy.clone(),
            bound: policy.bound(false),
            backend: None,
            started: Box::default(),
            told: AtomicBool::new(false),
            perf: table::Outputs::none(),
        });
        JitRequest::of(&shared, compiler::Submitter::INLINE)
    }

    /// The state of a request over `shared`, submitting through
    /// `submitter`, recording nothing and having submitted nothing.
    fn of(shared: &Arc<Shared>, submitter: compiler::Submitter) -> JitRequest {
        JitRequest {
            shared: Arc::clone(shared),
            submitter,
            recording: Cell::new(false),
            submitted: Cell::new(None),
            headers: crate::enter::Headers::default(),
            epoch: std::cell::OnceCell::new(),
            table: std::cell::OnceCell::new(),
            #[cfg(test)]
            escapes_crossed: Cell::new(0),
        }
    }
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
    /// Which loops the engine binds ([`Jit::bound`]).
    bound: Bound,
    backend: Option<lower::Backend>,
    started: Box<[policy::Note]>,
    /// Whether a request wrote the start's warning and lines yet: once
    /// per engine.
    told: AtomicBool,
    /// The engine's `PERF` outputs, which every epoch's table writes to.
    perf: table::Outputs,
}

/// The `TRACE` lines a request writes, under the `JIT_G(debug)` bits it
/// has where it writes them, as php reads `JIT_G(debug)` at each
/// line (`Z/ext/opcache/jit/zend_jit_trace.c:8152-8250`), so a request's
/// `ini_set('opcache.jit_debug', ...)` changes the lines it writes from
/// then on (#3297); the bits the start alone sets, `PERF` and its kin,
/// a request cannot change ([`policy::Policy::debug`]). A recording whose
/// bits ask for a root's end lines compiles on the request
/// ([`policy::JitDebug::ends`]), and the worker writes none.
///
/// Under `opcache.jit=tracing` with the engine's `opcache.jit_debug` at
/// zero, a request that asks for the `start` and `stop` lines writes them
/// for a loop in a file it includes after:
///
/// ```php
/// $dir = realpath(sys_get_temp_dir()) . '/jit-debug-' . getmypid(); mkdir($dir);
/// file_put_contents("$dir/main.php", '<?php var_dump(ini_set("opcache.jit_debug", "12288")); include __DIR__ . "/loop.php";');
/// file_put_contents("$dir/loop.php", '<?php $s = 0; for ($i = 0; $i < 4; $i++) { $s += $i; } echo $s, "\n";');
/// touch("$dir/main.php", time() - 60); touch("$dir/loop.php", time() - 60);
/// $cmd = [PHP_BINARY, '-n', '-d', 'opcache.enable_cli=1', '-d', 'opcache.jit=tracing', '-d', 'opcache.jit_debug=0', '-d', 'opcache.jit_hot_loop=1', "$dir/main.php"];
/// $p = proc_open($cmd, [1 => ['pipe', 'w'], 2 => ['pipe', 'w']], $pipes);
/// echo stream_get_contents($pipes[1]), str_replace($dir, 'DIR', stream_get_contents($pipes[2]));
/// proc_close($p); unlink("$dir/main.php"); unlink("$dir/loop.php"); rmdir($dir);
/// ```
/// ```expect process
/// string(1) "0"
/// 6
/// ---- TRACE 1 start (loop) $main() DIR/loop.php:1
/// ---- TRACE 1 stop (loop)
///
/// ```
#[derive(Debug, Default)]
pub(crate) struct Lines {
    debug: policy::JitDebug,
    written: Vec<u8>,
}

impl Lines {
    /// The lines `req` writes now, under its `JIT_G(debug)`
    /// ([`tier::setting`]), which the opcache extension's handler
    /// rewrites at each `ini_set('opcache.jit_debug', ...)`; none for a
    /// tier that runs `Off`, which reads no bit.
    pub(crate) fn of(req: &Request, shared: &Shared) -> Lines {
        let debug = if shared.policy.mode() == policy::Mode::Off {
            policy::JitDebug::default()
        } else {
            policy::JitDebug::bits(i64::from(tier::setting(req).debug))
        };
        Lines::under(debug)
    }

    /// The lines written under `debug`.
    pub(crate) fn under(debug: policy::JitDebug) -> Lines {
        Lines {
            debug,
            written: Vec::new(),
        }
    }

    /// The bits the lines are written under.
    pub(crate) fn debug(&self) -> policy::JitDebug {
        self.debug
    }

    /// `event`'s line added, where the bits ask for it.
    pub(crate) fn dump(&mut self, event: &policy::TraceEvent<'_>) {
        // Writing to a `Vec` does not fail.
        self.debug
            .dump(&mut self.written, event)
            .unwrap_or_default();
    }

    /// The lines written to the request's error stream, where php's
    /// `jit_debug` lines go.
    pub(crate) fn write(self, req: &mut Request) {
        if !self.written.is_empty() {
            req.response_mut().write_stderr(&self.written);
        }
    }

    /// What was written.
    #[cfg(test)]
    pub(crate) fn written(&self) -> &[u8] {
        &self.written
    }
}

impl JitEngine {
    /// `JitEngine::new`, at engine start.
    #[expect(
        clippy::unnecessary_wraps,
        reason = "an engine start answers a Refusal where it fails; this one never does"
    )]
    fn new(start: &Start) -> Result<JitEngine, Refusal> {
        Ok(JitEngine::over(
            start.ini(),
            start.host(),
            |opcode| tier::observes(start, opcode),
            lower::Backend::new,
        ))
    }

    /// The state over the startup ini of an engine whose extensions
    /// observe no opcode: the `Policy` as the directives say, the host's
    /// `Backend` over the kernels `enter` lists, or the tier `Off` where
    /// the host has none, the lines its start logged, and the
    /// [`compiler::Compiler`] it runs.
    #[cfg(test)]
    fn of(ini: &StartupIni) -> JitEngine {
        JitEngine::over(ini, "", |_| false, lower::Backend::new)
    }

    /// The state over the startup ini, for the host `host` names, where
    /// `observes` says which opcodes the engine's extensions observe,
    /// which refuses the tier past the silence opcodes
    /// ([`policy::Policy::check_support`]), with the `Backend` `backend`
    /// makes for the host.
    fn over(
        ini: &StartupIni,
        host: &str,
        observes: impl Fn(Opcode) -> bool,
        backend: impl FnOnce(
            &policy::Policy,
            lower::Symbols,
        ) -> Result<lower::Backend, lower::Unsupported>,
    ) -> JitEngine {
        let mut started = Vec::new();
        let mut policy = policy::Policy::from_ini(ini, host, &mut started);
        policy.check_support(observes);
        let backend = if policy.mode() == policy::Mode::Off {
            None
        } else {
            backend(&policy, enter::wrapped::symbols()).ok()
        };
        let bound = policy.bound(backend.is_some());
        if backend.is_none() {
            policy.off();
        }
        let perf = if policy.mode() == policy::Mode::Off {
            table::Outputs::none()
        } else {
            table::Outputs::start(policy.debug())
        };
        let shared = Arc::new(Shared {
            policy,
            bound,
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

    /// The G4 bar's line (#2961), at a request's end in the bench's
    /// build alone: the bytes the epoch's traces take, the oplines they
    /// compiled and how many there are, as its tables counted them
    /// ([`table::TraceTable::usage`]); php's count of compiled oplines is
    /// its `TRACE_BYTECODE` listing, which this tier does not write.
    #[cfg(php_jit_usage)]
    fn request_ended(req: &mut Request) {
        let epoch = tier::epoch(req);
        let Some(used) = table::of_epoch(&epoch).map(table::TraceTable::usage) else {
            return;
        };
        let line = format!(
            "JIT usage: {} bytes, {} oplines, {} traces\n",
            used.bytes, used.oplines, used.traces
        );
        req.response_mut().write_stderr(line.as_bytes());
    }

    /// The first request's start raises the warning of a refused tier,
    /// php's `zend_error` from `zend_jit_check_support`, at `Unknown` on
    /// line 0 as a diagnostic of the engine's start is shown, and writes
    /// the lines the engine's start logged, where `opcache.jit_debug`
    /// asks for any line, so php's default writes nothing; each once per
    /// engine (`jit-design.md` §4, §4.3).
    #[expect(
        clippy::unnecessary_wraps,
        reason = "a request start answers a Thrown where it fails; this one never does"
    )]
    fn request_started(req: &mut Request) -> Result<(), Thrown> {
        let Some(engine) = req.engine::<JitEngine>() else {
            return Ok(());
        };
        let shared = &engine.shared;
        let refused = shared.policy.refused();
        if (refused.is_none() && !shared.policy.debugs())
            || shared.told.swap(true, Ordering::Relaxed)
        {
            return Ok(());
        }
        let mut lines = String::new();
        if shared.policy.debugs() {
            for note in &shared.started {
                // Writing to a `String` does not fail.
                writeln!(lines, "{note}").unwrap_or_default();
            }
        }
        if let Some(refused) = refused {
            raising::startup(req, Level::Warning, &refused.to_string());
        }
        req.response_mut().write_stderr(lines.as_bytes());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{Bound, JitEngine};
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

    /// The engine start decides once how the engine binds the tier
    /// (#3133): `Nop`'s loop wherever php's tier never starts, so under
    /// `opcache.jit=disable` no request can switch it on, and `Jit`'s
    /// where it runs from the start or is set up and off, which a request
    /// may turn on (#3139).
    #[test]
    fn the_engine_start_binds_jits_loop_only_where_the_tier_runs() {
        let bound = |asked: &[(&str, &str)], host: &'static str| {
            JitEngine::over(&ini(asked), host, |_| false, crate::lower::Backend::new)
                .shared
                .bound
        };
        let tracing = ("opcache.jit", "tracing");
        for (asked, host, expected) in [
            (&[][..], "", Bound::Never),
            (&[("opcache.jit", "disable")][..], "", Bound::Never),
            (&[("opcache.jit", "sideways")][..], "", Bound::Never),
            (
                &[tracing, ("opcache.jit_buffer_size", "0")][..],
                "",
                Bound::Never,
            ),
            (&[tracing, ("opcache.enable", "0")][..], "", Bound::Never),
            (&[tracing][..], "cli", Bound::Never),
            (&[("opcache.jit", "off")][..], "", Bound::Runtime),
            (&[tracing][..], "", Bound::On),
            (
                &[tracing, ("opcache.enable_cli", "1")][..],
                "cli",
                Bound::On,
            ),
            (&[("jit.mode", "force")][..], "", Bound::On),
        ] {
            assert_eq!(bound(asked, host), expected, "{asked:?} on {host:?}");
        }
        let refused = JitEngine::over(&ini(&[tracing]), "", |_| true, crate::lower::Backend::new);
        assert_eq!(refused.shared.bound, Bound::Never, "a refused tier");
        let unbacked = JitEngine::over(
            &ini(&[tracing]),
            "",
            |_| false,
            |_, _| Err(crate::lower::Unsupported("a test's host")),
        );
        assert_eq!(unbacked.shared.bound, Bound::Never, "no backend");
    }

    /// A host with no `Backend` runs the tier `Off` (`jit-design.md`
    /// §4.3) and reads no `opcache.jit_debug` bit, as a php built with no
    /// compiled tier has none: its start says nothing.
    #[test]
    fn a_host_with_no_backend_runs_off() {
        let asked = [("opcache.jit", "tracing"), ("opcache.jit_debug", "0xfffff")];
        let none = JitEngine::over(
            &ini(&asked),
            "",
            |_| false,
            |_, _| Err(crate::lower::Unsupported("a test's host")),
        );
        assert_eq!(none.shared.policy.mode(), Mode::Off);
        assert!(none.shared.backend.is_none());
        assert!(!none.shared.policy.debugs());
        assert!(none.shared.started.is_empty());
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

    /// On the command line php starts opcache, and so its compiled tier,
    /// only under `opcache.enable_cli` (#3126): the loop the pinned php
    /// writes no `TRACE` line for without it writes none here, and with
    /// it writes the lines of a loop it compiled.
    #[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
    #[test]
    fn the_command_line_runs_the_tier_only_under_opcache_enable_cli() {
        let program = "function f($x) { return $x + 1; } function g($x) { return f($x) * 2; }
            $s = 0; for ($i = 0; $i < 5; $i++) { $s = g($s) + 1; } echo $s, \"\\n\";";
        let asked = [
            ("opcache.jit", "tracing"),
            ("opcache.jit_hot_loop", "1"),
            ("opcache.jit_debug", "0x3000"),
        ];
        let off =
            testing::Scratch::new().serve(&testing::engine_on::<Lane>("cli", &asked), program);
        assert_eq!(testing::stdout(&off), "93\n");
        assert_eq!(String::from_utf8_lossy(&off.stderr), "");
        let mut on = asked.to_vec();
        on.push(("opcache.enable_cli", "1"));
        let traced =
            testing::Scratch::new().serve(&testing::engine_on::<Lane>("cli", &on), program);
        assert_eq!(testing::stdout(&traced), "93\n");
        assert!(
            String::from_utf8_lossy(&traced.stderr).contains("---- TRACE 1 start (loop)"),
            "{}",
            String::from_utf8_lossy(&traced.stderr)
        );
    }

    /// With its compiled tier disabled php writes no `opcache.jit_debug` line
    /// whatever the bits ask (`gh13232.phpt`), and neither does a tier
    /// that runs `Off` for any reason, nor one set up and off, whose
    /// program was published while it was off and so is never traced.
    #[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
    #[test]
    fn a_tier_that_runs_off_writes_no_line_whatever_jit_debug_asks() {
        for asked in [
            &[("opcache.jit", "disable")][..],
            &[("opcache.jit", "off")],
            &[("opcache.jit", "1245")],
            &[("opcache.jit", "tracing"), ("opcache.jit_buffer_size", "0")],
            &[("opcache.jit", "disable"), ("jit.compile", "sideways")],
        ] {
            let mut ini = asked.to_vec();
            ini.push(("opcache.jit_debug", "0xfffff"));
            let served = testing::serve::<Lane>(&ini, "echo \"Done\\n\";");
            assert_eq!(testing::stdout(&served), "Done\n");
            assert_eq!(String::from_utf8_lossy(&served.stderr), "", "{asked:?}");
        }
    }
}
