//! The bars over the seven kernels and `Zend/bench.php`, the programs the
//! tier's gates are measured on (`jit-design.md` §7,
//! `research/jit-research.r1.md` §E.11). Each is a pair per program:
//! two [`Php`] arms, a binary and the regimes and ini it runs every
//! program under, measured program by program under the protocol, the
//! reading carrying each program's ratio and their geomean (`Arms::Set`).
//!
//! **The bars today.** G3's denominator (#2948, [`oracle`]): the pinned
//! php under `[regime.jit-tracing]` against itself under
//! `[regime.jit-off]`, what php's own JIT buys on these programs, and its
//! A/A control, `jit-off` against `jit-off`. G2 (#2965, [`compiled`]):
//! the same pair and control on the engine built with the tier's feature,
//! what the engine's tier buys. G1 (#2947, [`door`]): the engine built
//! with the tier's feature against the engine built without it over the
//! seven kernels alone, [`KERNELS`], its geomean held to a bound, one
//! plus the tolerance of its `[[bench]]` row in `seams.toml` (#3130).
//!
//! **What every arm runs under.** The recipe, as the oracle's every run
//! is, then the regimes, then [`INI`]: php.ini-production's values for the
//! two entries the recipe sets for correctness at a cost to every run, and
//! a memory limit `string_munge.php` fits under. Its peak is 133,644,288
//! bytes under `jit-off` and 135,725,056 under `jit-tracing`, beside the
//! recipe's 128M (134,217,728), so the tracing arm's run is refused there
//! and the interpreter's is not. The recipe's JIT thresholds,
//! `opcache.jit_hot_loop=1` and its three neighbours, stand.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::{
    Arm, Arms, Counted, Declared, Environment, Held, PRODUCTION, Sample, Side, Sides, Spawned,
    Unit, ran, second,
};
use crate::carried::BUNDLE;
use crate::manifest::Manifest;
use crate::oracle::{Pin, hosts::CLI, profile};
use crate::phpt::Lane;

/// The programs, by stem under `docs/perf/workloads/`: the seven kernels
/// (#2736, #2740), and `bench_quiet.php`, php-src's `Zend/bench.php` at the
/// pinned tag with its two timing lines printing names alone, so the arms'
/// bytes can be held equal (protocol §5, §11).
pub(crate) const PROGRAMS: &[&str] = &[
    "fib",
    "nbody",
    "mandelbrot",
    "spectralnorm",
    "binary_trees",
    "fannkuch",
    "string_munge",
    "bench_quiet",
];

/// The seven kernels alone, [`PROGRAMS`] less `bench_quiet.php`: G1's
/// programs, and the trace-dump diff's (`jit-design.md` §4.1).
pub(crate) const KERNELS: &[&str] = PROGRAMS.split_at(7).0;

/// The ini every arm runs each program under after the recipe's and its
/// regimes': [`PRODUCTION`] and the memory limit the module's rustdoc
/// states.
const INI: [&str; 3] = [PRODUCTION[0], PRODUCTION[1], MEMORY];

/// The memory limit `string_munge.php` fits under on every arm, the
/// module's rustdoc says why, which the trace-dump diff runs the kernels
/// under too.
pub(crate) const MEMORY: &str = "memory_limit=512M";

/// One arm of a bar over [`PROGRAMS`]: its name, the php binary, the
/// pin's regimes it runs each program under in order, and ini after
/// theirs, G1's `opcache.jit_buffer_size=0` on its jit arm.
#[derive(Clone)]
pub(crate) struct Php {
    pub(crate) name: String,
    pub(crate) binary: PathBuf,
    pub(crate) regimes: Vec<&'static str>,
    pub(crate) ini: Vec<String>,
}

impl Php {
    /// The arm running `script`: the recipe's arguments and the regimes',
    /// then the recipe's ini, the regimes', [`INI`] and the arm's own as
    /// `-d`, as the oracle module composes a run (`Oracle::run_under`), in
    /// the recipe's environment.
    fn running(&self, pin: &Pin, script: &Path, env: &Environment) -> Result<Arm> {
        let regimes = (self.regimes.iter())
            .map(|name| pin.regime(name))
            .collect::<Result<Vec<_>>>()?;
        let recipe = &pin.recipe;
        let mut args = recipe.args.clone();
        args.extend(regimes.iter().flat_map(|r| r.args.iter().cloned()));
        let ini = (recipe.ini.iter())
            .chain(regimes.iter().flat_map(|r| &r.ini))
            .cloned()
            .chain(INI.map(str::to_owned))
            .chain(self.ini.iter().cloned());
        args.extend(ini.flat_map(|line| ["-d".to_owned(), line]));
        args.push(script.display().to_string());
        let (name, env) = (self.name.clone(), env.clone());
        let (program, reads) = (self.binary.clone(), vec![script.to_path_buf()]);
        Ok(Arm {
            name,
            program,
            args,
            env,
            reads,
            ..Arm::default()
        })
    }
}

/// What a bar reads of each run of its arms.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Read {
    /// Its time.
    Time,
    /// Its time and what its process retired, per opline of the count
    /// [`counted`] takes of the program (#2946).
    PerOpline,
    /// Its process's peak physical footprint (G4, #2961).
    Peak,
    /// Its traces' code per opline they compiled ([`code_per_opline`],
    /// G4, #2961).
    Code,
}

/// A bar over `programs`, [`PROGRAMS`] or a part of it, its arms named by
/// `arms` from the pin once it is measured, or why this machine has none,
/// which is its skip; each run read as `read` says.
pub(crate) fn bar(
    root: &Path,
    (id, programs, read): (&str, &'static [&'static str], Read),
    about: String,
    held: Held,
    arms: impl FnOnce(&Path, &Pin) -> Result<Result<[Php; 2], String>> + 'static,
) -> Declared {
    let root = root.to_path_buf();
    let start = move || -> Result<Sides> {
        let manifest = Manifest::load(&root)?;
        let pin = manifest.pin()?;
        let arms = match arms(&root, pin)? {
            Ok(arms) => arms,
            Err(why) => return Ok(Err(why)),
        };
        let env = Environment {
            clear: pin.recipe.env_clear,
            set: pin.recipe.environment()?,
        };
        let workloads = root.join("docs/perf/workloads");
        let mut set = Vec::new();
        for program in programs {
            let script = workloads.join(format!("{program}.php"));
            let [a, b] = &arms;
            let under = |regime| {
                let php = Php {
                    regimes: vec![regime],
                    ..a.clone()
                };
                php.running(pin, &script, &env)
            };
            let count = || counted(program, ["jit-tracing", "jit-off"].map(under));
            let oplines = (read == Read::PerOpline).then(count).transpose()?;
            let side = |php: &Php| -> Result<Box<dyn Side>> {
                let mut arm = php.running(pin, &script, &env)?;
                arm.retired = matches!(read, Read::PerOpline | Read::Peak);
                let side = Spawned::from(arm);
                Ok(match (read, oplines) {
                    (Read::PerOpline, Some(count)) => Box::new(Counted { side, count }),
                    (Read::Peak, _) => Box::new(Parsed { side, value: peak }),
                    (Read::Code, _) => Box::new(Parsed {
                        side,
                        value: code_per_opline,
                    }),
                    _ => Box::new(side),
                })
            };
            set.push(((*program).to_owned(), [side(a)?, side(b)?]));
        }
        Ok(Ok(Arms::Set(set)))
    };
    Declared {
        id: id.to_owned(),
        about,
        held,
        start: Box::new(start),
    }
}

/// A php binary for a bar, or why this machine has none, the bar's skip.
type Binary = fn(&Path, &Pin) -> Result<Result<PathBuf, String>>;

/// `whose` tier, `binary`, under `jit-tracing` against `jit-off` over
/// `programs`, the bar `id`, a series; and `jit-off` against itself,
/// `<id>-aa`, its A/A control; each run read as `read` says.
fn tracing(
    root: &Path,
    (id, whose): (&str, &str),
    (programs, read): (&'static [&'static str], Read),
    binary: Binary,
) -> [Declared; 2] {
    [
        ("", ["jit-tracing", "jit-off"], Held::Series),
        ("-aa", ["jit-off", "jit-off"], Held::Control),
    ]
    .map(|(aa, [a, b], held)| {
        let over = match read {
            Read::Time => "over the seven kernels and Zend/bench.php",
            Read::PerOpline => "per opline over the seven kernels",
            Read::Peak => "by peak footprint over the seven kernels",
            Read::Code => "by code per opline compiled over the seven kernels",
        };
        let about = format!("{whose} under [regime.{a}] against [regime.{b}] {over}");
        let id = format!("{id}{aa}");
        bar(
            root,
            (&id, programs, read),
            about,
            held,
            move |root, pin| {
                Ok(binary(root, pin)?.map(|binary| {
                    let arm = |name: String, regime| Php {
                        name,
                        binary: binary.clone(),
                        regimes: vec![regime],
                        ini: Vec::new(),
                    };
                    [arm(a.to_owned(), a), arm(second(a, b), b)]
                }))
            },
        )
    })
}

/// G3's denominator and its control (#2948): the pinned php's tier. A
/// machine without the pinned php measures neither, each a skip that says
/// so.
pub(crate) fn oracle(root: &Path) -> [Declared; 2] {
    let bar = ("oracle-jit", "the pinned php");
    tracing(root, bar, (PROGRAMS, Read::Time), |_, pin| {
        if let Some(why) = pin.oracle.absent() {
            return Ok(Err(why));
        }
        pin.oracle().asserted()?;
        Ok(Ok(pin.oracle.path.clone()))
    })
}

/// G2 and its control (#2965): the tier of the engine built with its
/// feature, the build [`door`]'s first arm runs. `jit-tracing` asks the
/// engine's `Policy` for `Mode::Tracing` and `jit-off` for `Mode::Off`
/// (`jit-design.md` §4.7); its traces compile on the worker thread, as
/// `jit.compile` asks by default.
pub(crate) fn compiled(root: &Path) -> [Declared; 2] {
    let bar = ("engine-jit", "the engine built as jit-feature");
    tracing(root, bar, (PROGRAMS, Read::Time), |root, _| {
        Build::Jit.built(root).map(Ok)
    })
}

/// A build of the engine's `php` an arm runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Build {
    /// With the tier's feature of the bundle on, `php_jit::Jit` bound.
    Jit,
    /// The default build, without it.
    Plain,
    /// [`Build::Jit`] with `cfg(php_jit_stepping)` set, whose tier steps
    /// a serving to its end at the first safepoint it is asked at
    /// (`php_jit`'s `stepping`, #2946).
    Stepping,
    /// [`Build::Jit`] with `cfg(php_jit_usage)` set, whose request's end
    /// writes `JIT usage: <bytes> bytes, <oplines> oplines, <traces>
    /// traces`, what its traces took (G4, #2961).
    Usage,
}

impl Build {
    /// The arm's name, the directory under `target/featured/` the build
    /// goes into where it is not the default build's, and the cfg the
    /// bench alone sets on it.
    fn spec(self) -> (&'static str, Option<&'static str>, Option<&'static str>) {
        match self {
            Build::Jit => ("jit-feature", Some(Lane::FEATURE), None),
            Build::Plain => ("no-jit-feature", None, None),
            Build::Stepping => ("stepping", Some("stepping"), Some("php_jit_stepping")),
            Build::Usage => ("usage", Some("usage"), Some("php_jit_usage")),
        }
    }

    /// The arm's name.
    fn name(self) -> &'static str {
        self.spec().0
    }

    /// The build in the bench lane's profile: every build but
    /// [`Build::Plain`] with the tier's feature, into the directory the
    /// lanes build [`Build::Jit`] in ([`profile::featured`]) or one of its
    /// own, a cfg set where the build names one.
    fn built(self, root: &Path) -> Result<PathBuf> {
        let (_, dir, cfg) = self.spec();
        let on = dir.map(|_| format!("{BUNDLE}/{}", Lane::FEATURE));
        let into = dir.map(|dir| profile::featured(root, dir));
        let (package, binary) = (CLI.package(), CLI.binary);
        let into = (into.as_deref(), cfg);
        profile::released(root, (&package, binary), on.as_deref(), into)
    }
}

/// G1, the door cost, and its control (#2947): the engine built with the
/// tier's feature, `php_jit::Jit` bound and `opcache.jit_buffer_size=0`,
/// against the engine built without the feature, over the seven kernels
/// and held to a bound, its `[[bench]]` row's: within the A/A drift,
/// floored at 2%, above which `jit-design.md` §3 is revisited (design
/// §9.2, protocol §12.3, #3130); and the build without the feature against
/// itself, its A/A control. Both arms of each run one command line,
/// `[regime.jit-tracing]` and the zero buffer, so the binary is the one
/// difference: the featured engine's `Policy` asks for tracing and runs
/// `Mode::Off` (`jit-design.md` §4.7), and the other has no tier to ask.
pub(crate) fn door(root: &Path) -> [Declared; 2] {
    [
        ("g1", Build::Jit, Held::Bound),
        ("g1-aa", Build::Plain, Held::Control),
    ]
    .map(|(id, first, held)| {
        let (a, b) = (first.name(), Build::Plain.name());
        let about = format!(
            "the engine built as {a} with opcache.jit_buffer_size=0 against {b} over the seven kernels"
        );
        bar(root, (id, KERNELS, Read::Time), about, held, move |root, _| {
            let arm = |name: String, build: Build| -> Result<Php> {
                Ok(Php {
                    name,
                    binary: build.built(root)?,
                    regimes: vec!["jit-tracing"],
                    ini: vec!["opcache.jit_buffer_size=0".to_owned()],
                })
            };
            Ok(Ok([arm(a.to_owned(), first)?, arm(second(a, b), Build::Plain)?]))
        })
    })
}

/// What `step` costs per opline under recording (#2946, `jit-design.md`
/// §4.1, §10), and its control: [`Build::Stepping`]'s tier, which steps
/// each kernel to its end at the first safepoint it is asked at and
/// samples each opline's operands as the recorder does, under
/// `jit-tracing` against the same build under `jit-off`, whose engine
/// binds `execute::<Nop>`, per opline of what the stepping run counted.
pub(crate) fn stepping(root: &Path) -> [Declared; 2] {
    let bar = ("step", "the stepping build");
    tracing(root, bar, (KERNELS, Read::PerOpline), |root, _| {
        Build::Stepping.built(root).map(Ok)
    })
}

/// G4, memory and code (#2961, `research/jit-research.r1.md` §E.11), on
/// the seven kernels, from [`Build::Usage`]: its peak physical footprint
/// with the tier compiling, under `jit-tracing`, against the
/// interpreter's, under `jit-off`, the bar `g4-rss` and its control; and
/// `g4-code`, the bytes its traces take per opline they compiled against
/// the pinned php's under `jit-tracing`, each as its own `jit_debug`
/// lines state them ([`code_per_opline`]).
pub(crate) fn g4(root: &Path) -> [Declared; 3] {
    let [rss, rss_aa] = tracing(
        root,
        ("g4-rss", "the usage build"),
        (KERNELS, Read::Peak),
        |root, _| Build::Usage.built(root).map(Ok),
    );
    let about = "the usage build's code per opline compiled against the pinned php's under [regime.jit-tracing] over the seven kernels".to_owned();
    let code = bar(
        root,
        ("g4-code", KERNELS, Read::Code),
        about,
        Held::Series,
        |root, pin| {
            if let Some(why) = pin.oracle.absent() {
                return Ok(Err(why));
            }
            pin.oracle().asserted()?;
            let php = |name: &str, binary, debug: u32| Php {
                name: name.to_owned(),
                binary,
                regimes: vec!["jit-tracing"],
                ini: vec![format!("opcache.jit_debug={debug}")],
            };
            let mut engine = php("engine", Build::Usage.built(root)?, SIZE);
            // Compiled where it is recorded, as php compiles, so the end's
            // count and the request's name the same traces.
            engine.ini.push("jit.compile=inline".to_owned());
            Ok(Ok([
                engine,
                php("php", pin.oracle.path.clone(), SIZE | LISTED),
            ]))
        },
    );
    [rss, rss_aa, code]
}

/// php's `ZEND_JIT_DEBUG_SIZE`: `JIT memory usage: <bytes>` at the end,
/// its JIT buffer's `buffer_size - buffer_free`, stubs and alignment
/// included (`Z/ext/opcache/jit/zend_jit.c:3866-3868`).
const SIZE: u32 = 1 << 9;

/// php's `TRACE_START`, `TRACE_STOP`, `TRACE_COMPILED`, `TRACE_ABORT` and
/// `TRACE_BYTECODE` (`Z/ext/opcache/jit/zend_jit.h:67-73`): each trace's
/// oplines listed as it is recorded, and whether it compiled.
const LISTED: u32 = (1 << 12) | (1 << 13) | (1 << 14) | (1 << 16) | (1 << 18);

/// A process arm whose run's value is read from what it wrote and what
/// its process retired, in bytes (G4, #2961).
struct Parsed {
    side: Spawned,
    value: fn(&Sample, &[u8]) -> Result<f64>,
}

impl Side for Parsed {
    fn name(&self) -> &str {
        self.side.name()
    }

    fn program(&self) -> &Path {
        self.side.program()
    }

    fn reads(&self) -> &[PathBuf] {
        self.side.reads()
    }

    fn once(&mut self) -> Result<Sample> {
        let (ran, said) = self.side.ran()?;
        let value = (self.value)(&ran, &said)?;
        Ok(Sample { value, ..ran })
    }

    fn unit(&self) -> Unit {
        Unit::Bytes
    }
}

/// A run's peak physical footprint in bytes.
#[allow(clippy::cast_precision_loss)]
fn peak(ran: &Sample, _: &[u8]) -> Result<f64> {
    let peak = ran
        .peak
        .context("this platform states no process's peak footprint")?;
    Ok(peak as f64)
}

/// The bytes a run's traces took per opline they compiled: its `JIT
/// memory usage` line, php's `ZEND_JIT_DEBUG_SIZE` on either side, over
/// the oplines the engine's `JIT usage` line names or php's
/// `TRACE_BYTECODE` listing names of each trace it compiled.
#[allow(clippy::cast_precision_loss)]
fn code_per_opline(_: &Sample, said: &[u8]) -> Result<f64> {
    let said = String::from_utf8_lossy(said);
    let line = |prefix: &str| said.lines().find_map(|line| line.strip_prefix(prefix));
    let bytes = line("JIT memory usage: ").and_then(|n| n.trim().parse::<u64>().ok());
    let engine = line("JIT usage: ").map(|usage| usage.split(' ').nth(2));
    let oplines = match engine {
        Some(oplines) => oplines.and_then(|n| n.parse::<u64>().ok()),
        None => Some(compiled_oplines(&said)),
    };
    match (bytes, oplines) {
        (Some(bytes), Some(oplines)) if oplines > 0 => Ok(bytes as f64 / oplines as f64),
        _ => bail!(
            "the run stated no code size over a compiled opline: {}",
            said.trim()
        ),
    }
}

/// The oplines php's `TRACE_BYTECODE` listing names of each trace it
/// compiled: a line opening on a four-digit opline number between a
/// trace's `start` and its `stop`, counted where the trace's `compiled`
/// line follows.
fn compiled_oplines(said: &str) -> u64 {
    let (mut listed, mut stopped, mut compiled) = (0, 0, 0);
    for line in said.lines() {
        let Some(trace) = line.strip_prefix("---- TRACE ") else {
            let numbered = line
                .get(..4)
                .is_some_and(|n| n.bytes().all(|b| b.is_ascii_digit()));
            listed += u64::from(numbered);
            continue;
        };
        if trace.contains(" start (") {
            (listed, stopped) = (0, 0);
        } else if trace.contains(" stop") {
            stopped = listed;
        } else if trace.ends_with(" compiled") {
            compiled += stopped;
        }
    }
    compiled
}

/// How many oplines the stepping arm executes of `program`, the sum of
/// the `stepped <n> oplines` lines its tier writes on the second stream,
/// once its first stream is held equal to `execute::<Nop>`'s run of the
/// same program: the assertion #2946 asks of the bar before it measures.
fn counted(program: &str, [stepping, nop]: [Result<Arm>; 2]) -> Result<f64> {
    let (stepping, nop) = (Spawned::from(stepping?), Spawned::from(nop?));
    let (stepped, said) = ran(&stepping.arm, &stepping.program)?;
    let (ran_nop, _) = ran(&nop.arm, &nop.program)?;
    if stepped.wrote != ran_nop.wrote {
        bail!(
            "{program}: the stepping loop wrote other bytes than execute::<Nop>; nothing is measured"
        );
    }
    let said = String::from_utf8_lossy(&said);
    let counts = said
        .lines()
        .filter_map(|line| line.strip_prefix("stepped ")?.strip_suffix(" oplines"));
    let oplines = counts.map(str::parse::<u64>).sum::<Result<u64, _>>();
    let oplines = oplines.with_context(|| format!("{program}: the stepping loop's count"))?;
    if oplines == 0 {
        bail!("{program}: the stepping build stepped no opline; its tier was never asked");
    }
    #[allow(clippy::cast_precision_loss)]
    Ok(oplines as f64)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// php's listing counts the oplines of each trace it compiled and
    /// none of one it aborted, over its `JIT memory usage`; the engine's
    /// line states both; a run that compiled nothing is refused (#2961).
    #[test]
    fn code_per_opline_is_each_runs_bytes_over_the_oplines_it_compiled() {
        let php = "---- TRACE 1 start (enter) fib() f.php:13\n0000 CV0($n) = RECV 1\n\
                   0001 T1 = IS_SMALLER CV0($n) int(2)\n---- TRACE 1 abort (return)\n\n\
                   ---- TRACE 1 start (loop) f() f.php:3\n0004 INIT_FCALL 1 144\n     >init fib\n\
                   0000  CV0($n) = RECV 1\n---- TRACE 1 stop (link to 1)\n---- TRACE 1 compiled\n\n\
                   JIT memory usage: 600\n";
        let ran = Sample::of(0.0, Vec::new());
        assert_eq!(compiled_oplines(php), 2);
        let near = |got: f64, want: f64| (got - want).abs() < 1e-9;
        assert!(near(code_per_opline(&ran, php.as_bytes()).unwrap(), 300.0));
        let engine = b"JIT usage: 1100 bytes, 40 oplines, 3 traces\n\nJIT memory usage: 1200\n";
        assert!(near(code_per_opline(&ran, engine).unwrap(), 30.0));
        let nothing = b"JIT usage: 0 bytes, 0 oplines, 0 traces\n\nJIT memory usage: 0\n";
        assert!(code_per_opline(&ran, nothing).is_err());
    }

    /// The engine's tier and php's are each a series beside its A/A
    /// control, named for whose tier it is, G1 is held to its bound, and
    /// the stepping loop is a series beside its control (#2946).
    #[test]
    fn each_tier_and_the_stepping_loop_is_a_series_beside_its_control_and_the_door_a_bound() {
        let root = Path::new(".");
        let bars = [oracle(root), compiled(root), door(root), stepping(root)];
        let bars = bars.map(|b| b.map(|b| (b.id, b.held)));
        let g4 = g4(root).map(|b| (b.id, b.held));
        let rss = [("g4-rss", Held::Series), ("g4-rss-aa", Held::Control)];
        let g4_ids = rss.into_iter().chain([("g4-code", Held::Series)]);
        assert_eq!(
            g4.to_vec(),
            g4_ids
                .map(|(id, held)| (id.to_owned(), held))
                .collect::<Vec<_>>()
        );
        let pair = |id: &str, held| [(id.to_owned(), held), (format!("{id}-aa"), Held::Control)];
        let series = |id| pair(id, Held::Series);
        let door = pair("g1", Held::Bound);
        let tiers = [
            series("oracle-jit"),
            series("engine-jit"),
            door,
            series("step"),
        ];
        assert_eq!(bars, tiers);
    }

    /// Each program is committed where the protocol keeps its workloads
    /// (§11), and each arm runs it under the recipe, its regime and
    /// [`INI`], in that order, with the program's path last.
    #[cfg(tracked)]
    #[test]
    fn each_program_is_committed_and_run_under_the_recipe_then_the_regime() {
        let root = crate::package_dir().parent().unwrap().to_path_buf();
        for program in PROGRAMS {
            let path = root.join(format!("docs/perf/workloads/{program}.php"));
            assert!(path.is_file(), "{}", path.display());
        }
        let pin = Manifest::load(&root).unwrap().pin.unwrap();
        let php = Php {
            name: "jit-off".to_owned(),
            binary: PathBuf::from("php"),
            regimes: vec!["jit-off"],
            ini: vec!["opcache.jit_buffer_size=0".to_owned()],
        };
        let script = root.join("docs/perf/workloads/fib.php");
        let arm = php.running(&pin, &script, &Environment::default()).unwrap();
        let args = arm.args.join(" ");
        let (recipe, regime) = (
            args.find("-d memory_limit=128M"),
            args.find("opcache.jit=off"),
        );
        let (limit, own) = (
            args.find("memory_limit=512M"),
            args.find("jit_buffer_size=0"),
        );
        assert!(args.starts_with("-n "), "{args}");
        assert!(recipe < regime && regime < limit && limit < own, "{args}");
        assert!(args.ends_with("fib.php"), "{args}");
        assert_eq!(
            arm.reads,
            [script],
            "the script is pinned beside the binary"
        );
    }
}
