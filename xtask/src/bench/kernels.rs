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

use anyhow::Result;

use super::{Arm, Arms, Declared, Environment, Held, PRODUCTION, Side, Sides, Spawned, second};
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

/// A bar over `programs`, [`PROGRAMS`] or a part of it, its arms named by
/// `arms` from the pin once it is measured, or why this machine has none,
/// which is its skip.
pub(crate) fn bar(
    root: &Path,
    (id, programs): (&str, &'static [&'static str]),
    about: String,
    held: Held,
    arms: impl FnOnce(&Pin) -> Result<Result<[Php; 2], String>> + 'static,
) -> Declared {
    let root = root.to_path_buf();
    let start = move || -> Result<Sides> {
        let manifest = Manifest::load(&root)?;
        let pin = manifest.pin()?;
        let arms = match arms(pin)? {
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
            let side = |php: &Php| -> Result<Box<dyn Side>> {
                Ok(Box::new(Spawned::from(php.running(pin, &script, &env)?)))
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
/// every program, the bar `id`, a series; and `jit-off` against itself,
/// `<id>-aa`, its A/A control.
fn tracing(root: &Path, (id, whose): (&str, &str), binary: Binary) -> [Declared; 2] {
    [
        ("", ["jit-tracing", "jit-off"], Held::Series),
        ("-aa", ["jit-off", "jit-off"], Held::Control),
    ]
    .map(|(aa, [a, b], held)| {
        let about = format!(
            "{whose} under [regime.{a}] against [regime.{b}] over the seven kernels and Zend/bench.php"
        );
        let at = root.to_path_buf();
        bar(root, (&format!("{id}{aa}"), PROGRAMS), about, held, move |pin| {
            let binary = match binary(&at, pin)? {
                Ok(binary) => binary,
                Err(why) => return Ok(Err(why)),
            };
            let arm = |name: String, regime| Php {
                name,
                binary: binary.clone(),
                regimes: vec![regime],
                ini: Vec::new(),
            };
            Ok(Ok([arm(a.to_owned(), a), arm(second(a, b), b)]))
        })
    })
}

/// G3's denominator and its control (#2948): the pinned php's tier. A
/// machine without the pinned php measures neither, each a skip that says
/// so.
pub(crate) fn oracle(root: &Path) -> [Declared; 2] {
    tracing(root, ("oracle-jit", "the pinned php"), |_, pin| {
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
    tracing(root, bar, |root, _| Build::Jit.built(root).map(Ok))
}

/// A build of the engine's `php` an arm runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Build {
    /// With the tier's feature of the bundle on, `php_jit::Jit` bound.
    Jit,
    /// The default build, without it.
    Plain,
}

impl Build {
    /// The arm's name.
    fn name(self) -> &'static str {
        match self {
            Build::Jit => "jit-feature",
            Build::Plain => "no-jit-feature",
        }
    }

    /// The build in the bench lane's profile, [`Build::Jit`] into the
    /// directory the lanes build it in ([`profile::featured`]).
    fn built(self, root: &Path) -> Result<PathBuf> {
        let jit = self == Build::Jit;
        let on = jit.then(|| format!("{BUNDLE}/{}", Lane::FEATURE));
        let into = jit.then(|| profile::featured(root, Lane::FEATURE));
        let (package, binary) = (CLI.package(), CLI.binary);
        profile::released(root, (&package, binary), on.as_deref(), into.as_deref())
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
        let at = root.to_path_buf();
        bar(root, (id, KERNELS), about, held, move |_| {
            let arm = |name: String, build: Build| -> Result<Php> {
                Ok(Php {
                    name,
                    binary: build.built(&at)?,
                    regimes: vec!["jit-tracing"],
                    ini: vec!["opcache.jit_buffer_size=0".to_owned()],
                })
            };
            Ok(Ok([arm(a.to_owned(), first)?, arm(second(a, b), Build::Plain)?]))
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The engine's tier and php's are each a series beside its A/A
    /// control, named for whose tier it is, and G1 is held to its bound.
    #[test]
    fn each_tier_is_a_series_beside_its_control_and_the_door_a_bound() {
        let root = Path::new(".");
        let bars = [oracle(root), compiled(root), door(root)].map(|b| b.map(|b| (b.id, b.held)));
        let pair = |id: &str, held| [(id.to_owned(), held), (format!("{id}-aa"), Held::Control)];
        let series = |id| pair(id, Held::Series);
        let door = pair("g1", Held::Bound);
        assert_eq!(bars, [series("oracle-jit"), series("engine-jit"), door]);
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
