//! The bars over the seven kernels and `Zend/bench.php`, the programs the
//! tier's gates are measured on (`jit-design.md` §7,
//! `research/jit-research.r1.md` §E.11). Each is a pair per program:
//! two [`Php`] arms, a binary and the regimes and ini it runs every
//! program under, measured program by program under the protocol, the
//! reading carrying each program's ratio and their geomean (`Arms::Set`).
//!
//! **The bars today.** G3's denominator (#2948): the pinned php under
//! `[regime.jit-tracing]` against itself under `[regime.jit-off]`, what
//! php's own JIT buys on these programs, and its A/A control, `jit-off`
//! against `jit-off`. G1 (#2947) and G2 (#2965) are [`bar`]s over the same
//! programs whose arms are the engine's builds.
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
use crate::manifest::Manifest;
use crate::oracle::Pin;

/// The programs, by stem under `docs/perf/workloads/`: the seven kernels
/// (#2736, #2740), and `bench_quiet.php`, php-src's `Zend/bench.php` at the
/// pinned tag with its two timing lines printing names alone, so the arms'
/// bytes can be held equal (protocol §5, §11).
pub(crate) const PROGRAMS: [&str; 8] = [
    "fib",
    "nbody",
    "mandelbrot",
    "spectralnorm",
    "binary_trees",
    "fannkuch",
    "string_munge",
    "bench_quiet",
];

/// The ini every arm runs each program under after the recipe's and its
/// regimes': [`PRODUCTION`] and the memory limit the module's rustdoc
/// states.
const INI: [&str; 3] = [PRODUCTION[0], PRODUCTION[1], "memory_limit=512M"];

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
        let program = self.binary.clone();
        Ok(Arm {
            name,
            program,
            args,
            stdin: String::new(),
            env,
        })
    }
}

/// A bar over [`PROGRAMS`], its arms named by `arms` from the pin once
/// it is measured, or why this machine has none, which is its skip.
pub(crate) fn bar(
    root: &Path,
    id: &str,
    about: String,
    held: Held,
    arms: impl FnOnce(&Pin) -> Result<Result<[Php; 2], String>> + 'static,
) -> Declared {
    let root = root.to_path_buf();
    let start = move || -> Result<Sides> {
        let pin = Manifest::load(&root)?.pin;
        let arms = match arms(&pin)? {
            Ok(arms) => arms,
            Err(why) => return Ok(Err(why)),
        };
        let env = Environment {
            clear: pin.recipe.env_clear,
            set: pin.recipe.environment()?,
        };
        let workloads = root.join("docs/perf/workloads");
        let mut set = Vec::new();
        for program in PROGRAMS {
            let script = workloads.join(format!("{program}.php"));
            let [a, b] = &arms;
            let side = |php: &Php| -> Result<Box<dyn Side>> {
                Ok(Box::new(Spawned::from(php.running(&pin, &script, &env)?)))
            };
            set.push((program.to_owned(), [side(a)?, side(b)?]));
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

/// G3's denominator and its control (#2948): the pinned php under
/// `jit-tracing` against `jit-off`, a series, and `jit-off` against
/// itself, its A/A control. A machine without the pinned php measures
/// neither, each a skip that says so.
pub(crate) fn oracle(root: &Path) -> [Declared; 2] {
    [
        ("oracle-jit", ["jit-tracing", "jit-off"], Held::Series),
        ("oracle-jit-aa", ["jit-off", "jit-off"], Held::Control),
    ]
    .map(|(id, [a, b], held)| {
        let about = format!(
            "the pinned php under [regime.{a}] against [regime.{b}] over the seven kernels and Zend/bench.php"
        );
        bar(root, id, about, held, move |pin| {
            if let Some(why) = pin.oracle.absent() {
                return Ok(Err(why));
            }
            pin.oracle().asserted()?;
            let arm = |name: String, regime| Php {
                name,
                binary: pin.oracle.path.clone(),
                regimes: vec![regime],
                ini: Vec::new(),
            };
            Ok(Ok([arm(a.to_owned(), a), arm(second(a, b), b)]))
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each program is committed where the protocol keeps its workloads
    /// (§11), and each arm runs it under the recipe, its regime and
    /// [`INI`], in that order, with the program's path last.
    #[test]
    fn each_program_is_committed_and_run_under_the_recipe_then_the_regime() {
        let root = crate::package_dir().parent().unwrap().to_path_buf();
        for program in PROGRAMS {
            let path = root.join(format!("docs/perf/workloads/{program}.php"));
            assert!(path.is_file(), "{}", path.display());
        }
        let pin = Manifest::load(&root).unwrap().pin;
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
    }
}
