//! `xtask trace-dump`, the trace-dump diff (`jit-design.md` §4.1;
//! harness §7): which roots the tier records and how each recording
//! ends, held to php's. Each of the seven kernels ([`KERNELS`]) is run by
//! the oracle under the recipe and `[regime.jit-inline]`, whose
//! `opcache.jit_debug` asks php for its `TRACE` lines on the second
//! stream, and by the engine built with the tier's feature, the build the
//! phpt lane's `--jit` and the witness lane's jit fences run
//! ([`Build`]), under the same invocation and `jit.mode=force` and
//! `jit.compile=inline`, the engine-only directives php ignores. The
//! oracle's lines are kept in the store under the kernel and the pin's
//! digest, the engine's beside them, and the two are held through
//! [`oracle::compare`] under the divergence key `trace:dump/<kernel>`,
//! one reading per kernel.
//!
//! **The lines held.** A `---- TRACE` line: a root's or a side trace's
//! `start`, its `stop` or `abort`, and `compiled` or `blacklisted`, the
//! lines of `zend_jit_trace_hot_root` and `zend_jit_trace_hot_side`
//! (`Z/ext/opcache/jit/zend_jit_trace.c:8152-8250` at the tag). The paths
//! in them are the program's directory as the recipe's token, as every
//! run's stream is. A `TRACE n exit m` line is php's on each pass through
//! an exit that has no side trace: an engine that grows none yet (#2964)
//! writes one per pass, millions on a kernel, so the run clears the exit
//! bit from the regime's `opcache.jit_debug` on both sides and the
//! diff holds what the regime's other five bits print, a side trace's
//! exit number among them on its `start` line. Every other line of the
//! stream, the engine's `JIT: jit.mode=force` among them, is not a trace
//! line and is not held.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};

use crate::bench::kernels::{KERNELS, MEMORY};
use crate::crosscheck::first_difference;
use crate::divergence::{Kind, Registry};
use crate::key::Snapshot;
use crate::manifest::Manifest;
use crate::oracle::{
    self, Ended, Invocation, Observation, Oracle, Program, Regime, Verdict, hosts::CLI, profile,
};
use crate::phpt::{Build, Lane};
use crate::reading::{Reading, Status, Store};

/// The instrument's name, the reading's `instrument` field.
pub(crate) const INSTRUMENT: &str = "trace-dump";

/// The regime both sides run under.
const REGIME: &str = "jit-inline";

/// php's bit for a trace's exit line, `ZEND_JIT_DEBUG_TRACE_EXIT`
/// (`Z/ext/opcache/jit/zend_jit.h:70` at the tag).
const TRACE_EXIT: u64 = 1 << 15;

/// What a line the diff holds starts with.
const HELD: &[u8] = b"---- TRACE ";

/// What one `xtask trace-dump` run was asked for.
#[derive(Debug, Clone, Default, clap::Args)]
pub struct Ask {
    /// One kernel by its name under `docs/perf/workloads`, repeatable;
    /// default the seven.
    #[arg(long, value_name = "NAME", conflicts_with = "program")]
    pub kernel: Vec<String>,
    /// A program outside the kernels by its path, repeatable: its reading
    /// is scoped by the file's stem.
    #[arg(long, value_name = "PATH")]
    pub program: Vec<PathBuf>,
    /// The binary whose lines are held to the oracle's; without it, `php`
    /// built with the tier's feature, built now if the tree has moved.
    #[arg(long, value_name = "PATH")]
    pub engine: Option<PathBuf>,
}

impl Ask {
    /// Each program the run holds, by its scope: the programs named, or
    /// the kernels named or all seven.
    fn programs(&self, root: &Path) -> Vec<(String, PathBuf)> {
        if !self.program.is_empty() {
            let stem = |path: &Path| path.file_stem().map(|s| s.to_string_lossy().into_owned());
            let scoped = |path: &PathBuf| (stem(path).unwrap_or_default(), path.clone());
            return self.program.iter().map(scoped).collect();
        }
        let named: Vec<&str> = self.kernel.iter().map(String::as_str).collect();
        let kernels = if named.is_empty() {
            KERNELS
        } else {
            &named[..]
        };
        let workloads = root.join("docs/perf/workloads");
        let at = |k: &&str| ((*k).to_owned(), workloads.join(format!("{k}.php")));
        kernels.iter().map(at).collect()
    }
}

/// The divergence key of a program's lines.
pub(crate) fn id_of(scope: &str) -> String {
    format!("{}dump/{scope}", Kind::Trace.prefix())
}

/// Where a side's lines for `scope` are kept: under the store, the
/// instrument and the scope, `<digest>.oracle` and `<digest>.engine` for
/// the pin's digest, so a pin bump leaves the old lines beside the new.
pub fn kept(store: &Store, digest: &str, scope: &str, side: &str) -> PathBuf {
    let dir = store.root().join(INSTRUMENT).join(scope);
    dir.join(format!("{digest}.{side}"))
}

/// The lines of `stream` the diff holds, each ended by a newline.
pub(crate) fn lines(stream: &[u8]) -> Vec<u8> {
    let held = stream
        .split(|b| *b == b'\n')
        .filter(|l| l.starts_with(HELD));
    held.flat_map(|line| line.iter().chain(b"\n"))
        .copied()
        .collect()
}

/// The ini both sides run under after the recipe's and `regime`'s: the
/// kernels' memory limit, the regime's `opcache.jit_debug` without
/// [`TRACE_EXIT`], and the engine's two directives.
fn ini(regime: &Regime) -> Result<Vec<String>> {
    let debug = (regime.ini.iter().rev())
        .find_map(|line| line.strip_prefix("opcache.jit_debug="))
        .with_context(|| format!("[regime.{REGIME}] sets no opcache.jit_debug"))?;
    let bits = match debug.strip_prefix("0x") {
        Some(hex) => u64::from_str_radix(hex, 16),
        None => debug.parse(),
    }
    .with_context(|| format!("[regime.{REGIME}]'s opcache.jit_debug={debug} is not a number"))?;
    let lane = format!("{}=force", Lane::DIRECTIVE);
    let debug = format!("opcache.jit_debug={}", bits & !TRACE_EXIT);
    Ok(vec![
        MEMORY.to_owned(),
        debug,
        lane,
        "jit.compile=inline".to_owned(),
    ])
}

/// The engine's `php` for the run: the one asked for, or `php` built
/// with the tier's feature into the directory the phpt lane builds it in.
fn engine(root: &Path, ask: &Ask) -> Result<PathBuf> {
    if let Some(engine) = &ask.engine {
        // Each side runs in its program's directory.
        return Ok(std::path::absolute(engine)?);
    }
    let build = Build::of(vec![Lane::FEATURE]);
    let into = build.target_dir(root);
    profile::build(
        root,
        &CLI.package(),
        CLI.binary,
        Some(&build.features()),
        Some(&into),
    )
}

/// Run what was asked for: one reading per program, written by the
/// caller through `record`, and both sides' lines kept in the store.
pub fn run(
    root: &Path,
    manifest: &Manifest,
    registry: &Registry,
    store: &Store,
    ask: &Ask,
) -> Result<Vec<Reading>> {
    let pin = &manifest.pin;
    let snapshot = Snapshot::disk(root)?;
    let ini = ini(pin.regime(REGIME)?)?;
    let oracle = pin.oracle();
    let mut built = None;
    let mut readings = Vec::new();
    for (scope, program) in ask.programs(root) {
        // What the lines are a function of is the whole engine and the
        // program, so the reading is stamped over the workspace.
        let reading = Reading::stamped_over(INSTRUMENT, &scope, &snapshot, Path::new("."))?;
        let mut reading = reading.under(pin);
        if let Some(why) = pin.oracle.absent() {
            reading.status = Status::Skip;
            reading.message = format!("{scope}: nothing ran under [regime.{REGIME}]; {why}");
            readings.push(reading);
            continue;
        }
        oracle.asserted()?;
        let binary = built.get_or_insert_with(|| engine(root, ask).map_err(|e| format!("{e:#}")));
        let judged = match binary {
            Ok(binary) => held(
                &oracle,
                (store, registry),
                (&program, binary),
                &ini,
                &mut reading,
            ),
            Err(why) => Err(anyhow!("the engine's php did not build: {why}")),
        };
        if let Err(trouble) = judged {
            reading.status = Status::Fail;
            reading.message = format!("{scope}: {trouble:#}");
        }
        readings.push(reading);
    }
    Ok(readings)
}

/// `program` run by both sides, both sides' lines kept, and the verdict
/// on the engine's written into `reading`, scoped to the program.
fn held(
    oracle: &Oracle<'_>,
    (store, registry): (&Store, &Registry),
    (program, engine): (&Path, &Path),
    ini: &[String],
    reading: &mut Reading,
) -> Result<()> {
    if !program.is_file() {
        bail!("there is no program at {}", program.display());
    }
    let ran = |binary: &Path| -> Result<Observation> {
        oracle.run_under(&Invocation {
            regimes: &[REGIME],
            ini,
            ..Invocation::new(binary, Program::File(program))
        })
    };
    let theirs = ran(&oracle.pin().oracle.path)?;
    let ours = ran(engine).context("the engine's run")?;
    let scope = reading.scope.clone();
    let (expected, actual) = (lines(&theirs.stderr), lines(&ours.stderr));
    for (side, bytes) in [("oracle", &expected), ("engine", &actual)] {
        let path = kept(store, oracle.pin().digest(), &scope, side);
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        fs::write(&path, bytes).with_context(|| format!("keeping {}", path.display()))?;
    }
    let count = |bytes: &[u8]| String::from_utf8_lossy(bytes).lines().count();
    reading.count("oracle_lines", count(&expected) as u64)?;
    reading.count("engine_lines", count(&actual) as u64)?;
    let id = id_of(&scope);
    let (status, verdict) = match oracle::compare(registry, &id, &expected, &actual) {
        Verdict::Same => {
            let n = count(&expected);
            (
                Status::Pass,
                format!("the engine's {n} TRACE lines are the oracle's"),
            )
        }
        Verdict::Diverged(row) => {
            let accepts = format!("{} accepts it: {}", row.id, row.reason);
            (
                Status::Pass,
                format!("the engine's TRACE lines differ from the oracle's, and {accepts}"),
            )
        }
        Verdict::Differs(diff) => (Status::Fail, first_difference(&diff, "trace dump")),
    };
    let ended = [("oracle", &theirs), ("engine", &ours)].into_iter();
    let ended = ended.filter(|(_, seen)| seen.exit != Ended::Status(0));
    let mut notes: Vec<String> = ended
        .map(|(side, seen)| format!("the {side}'s run ended on {}", seen.exit))
        .collect();
    if theirs.stdout != ours.stdout {
        notes.push("the engine's first stream is not the oracle's".to_owned());
    }
    let under = format!("under [regime.{REGIME}] and {}", ini.join(" "));
    reading.status = status;
    reading.message = std::iter::once(format!("{scope}: {under}: {verdict}"))
        .chain(notes)
        .collect::<Vec<_>>()
        .join("; ");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The diff holds a stream's `---- TRACE` lines and nothing else: no
    /// exit line, no engine note and no blank line between roots.
    #[test]
    fn the_diff_holds_the_trace_lines_alone() {
        let stream = b"JIT: jit.mode=force\n---- TRACE 1 start (loop) $main() a.php:3\n---- TRACE 1 stop (loop)\n\n     TRACE 1 exit 2 $main() a.php:6\nWarning: x";
        let held = "---- TRACE 1 start (loop) $main() a.php:3\n---- TRACE 1 stop (loop)\n";
        assert_eq!(String::from_utf8(lines(stream)).unwrap(), held);
        assert!(lines(b"4950").is_empty());
    }

    /// Both sides run the regime's `opcache.jit_debug` less the exit bit,
    /// read in either base php reads it, after the memory limit and before
    /// the engine's two directives; a regime with no `jit_debug` refuses
    /// the run.
    #[test]
    fn both_sides_run_the_regimes_debug_bits_less_the_exit_line() {
        let regime = |debug: &str| Regime {
            ini: vec![
                "opcache.jit=tracing".to_owned(),
                format!("opcache.jit_debug={debug}"),
            ],
            ..Regime::default()
        };
        let ran = ini(&regime("258048")).unwrap();
        let expected = [
            MEMORY,
            "opcache.jit_debug=225280",
            "jit.mode=force",
            "jit.compile=inline",
        ];
        assert_eq!(ran, expected);
        assert_eq!(ini(&regime("0x3F000")).unwrap(), expected);
        assert_eq!(ini(&regime("4096")).unwrap()[1], "opcache.jit_debug=4096");
        let none = ini(&Regime::default()).unwrap_err().to_string();
        assert!(none.contains("sets no opcache.jit_debug"), "{none}");
    }

    /// Without a program named, the run holds the seven kernels, each by
    /// its name, at its path under the workloads; a program named is
    /// scoped by its stem.
    #[test]
    fn the_run_holds_the_seven_kernels_unless_a_program_is_named() {
        let root = Path::new("/w");
        let programs = Ask::default().programs(root);
        let scopes: Vec<&str> = programs.iter().map(|(scope, _)| scope.as_str()).collect();
        assert_eq!(scopes, KERNELS);
        assert_eq!(programs[0].1, root.join("docs/perf/workloads/fib.php"));
        let one = Ask {
            kernel: vec!["nbody".to_owned()],
            ..Ask::default()
        };
        assert_eq!(
            one.programs(root),
            [(
                "nbody".to_owned(),
                root.join("docs/perf/workloads/nbody.php")
            )]
        );
        let named = Ask {
            program: vec![PathBuf::from("x/fiber_hot_loop.php")],
            ..Ask::default()
        };
        assert_eq!(named.programs(root)[0].0, "fiber_hot_loop");
        assert_eq!(id_of("fib"), "trace:dump/fib");
    }
}
