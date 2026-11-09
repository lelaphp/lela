//! `xtask dispatch-census`, the census behind `IN_THE_LOOP` (design §5.8,
//! #4328): each variant's share of the oplines the loop dispatched, over
//! every program the list's doc names. Those are each program of
//! `docs/perf/workloads`, php-src's `Zend/micro_bench.php` at the pin, and
//! the four counting loops of #3273 ([`LOOPS`]). The list's doc admits a
//! variant met in more than a quarter of a percent of the oplines any of
//! them dispatched ([`THRESHOLD`]), "as a counting build of the loop told
//! them over each" (#3285).
//!
//! **The counting build.** `php` built in release with the bundle's
//! [`FEATURE`], into a target directory of its own, so the default build
//! stands ([`profile::featured`]). Its loop counts each opline it
//! dispatches by the `php_unit::Handler` that serves it, and its request
//! writes the counts to the error stream where it ends, one line per
//! variant met ([`LINE`]; `php_engine`'s `api::request::census`).
//!
//! **The run.** Each program runs on that build under the oracle's recipe
//! and [`MEMORY`], as the bench's arms run the kernels; the oracle itself
//! runs nothing. Each program writes one reading, scoped `workloads/<stem>`,
//! `Zend/micro_bench` or `loops/<name>`, or `program/<stem>` for a program
//! named on the command line. The reading's `numbers` hold `oplines`, the
//! oplines the loop dispatched; `variants`, how many variants served them;
//! and `over`, how many of those are over the threshold. Its `groups` hold
//! each variant's share of `oplines` in percent. Its message names the
//! variants over the threshold, the largest share first. A program the
//! engine wrote no counts for fails, and one whose tree is not on this
//! machine is a skip.
//!
//! The instrument decides nothing: the threshold is the one the list's doc
//! states, and the list is a person's to change.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};

use crate::bench::kernels::MEMORY;
use crate::carried::BUNDLE;
use crate::key::Snapshot;
use crate::manifest::Manifest;
use crate::oracle::{Ended, Invocation, Oracle, Program, hosts::CLI, profile};
use crate::reading::{Number, Reading, Status, Store, joined, plural};

/// The instrument's name, the reading's `instrument` field.
pub(crate) const INSTRUMENT: &str = "dispatch-census";

/// The bundle's feature the counting build turns on, `php_engine`'s by
/// the same name, and the directory under `target/featured/` it is built
/// into.
pub(crate) const FEATURE: &str = "dispatch-census";

/// What each line of the counts starts with, before the variant's name
/// and its count: `php_engine`'s `census::LINE`.
const LINE: &str = "dispatch-census ";

/// The share in percent a variant is over where `IN_THE_LOOP`'s doc, in
/// [`ARMS`], admits it: "more than a quarter of a percent".
pub(crate) const THRESHOLD: f64 = 0.25;

/// The file whose doc of `IN_THE_LOOP` states [`THRESHOLD`].
const ARMS: &str = "crates/php_engine/build/arms.rs";

/// The workloads, each program of the directory a reading of its own.
const WORKLOADS: &str = "docs/perf/workloads";

/// php-src's micro-benchmark, by its path in the pinned tree.
const MICRO_BENCH: &str = "Zend/micro_bench.php";

/// The four counting loops of #3273, each by its name and the body of
/// `k`'s loop ([`counting_loop`]).
pub(crate) const LOOPS: [(&str, &str); 4] = [
    ("empty", ""),
    ("add", "$s = $i + 1;"),
    ("add-assign", "$s += $i;"),
    ("mul", "$s = $s * 1.0000001;"),
];

/// How many times a counting loop goes round.
const ROUNDS: u32 = 1_000_000;

/// What one `xtask dispatch-census` run was asked for.
#[derive(Debug, Clone, Default, clap::Args)]
pub struct Ask {
    /// A program by its path, repeatable, run in place of the ones the
    /// list's doc names: its reading is scoped `program/<stem>`.
    #[arg(long, value_name = "PATH")]
    pub program: Vec<PathBuf>,
    /// The counting build to run; without it, `php` built in release with
    /// the bundle's `dispatch-census` feature, built now if the tree has
    /// moved.
    #[arg(long, value_name = "PATH")]
    pub engine: Option<PathBuf>,
}

/// Where one program's text is.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Source {
    /// A file, run in its own directory.
    File(PathBuf),
    /// Code the recipe's preamble opens, run as a witness's snippet is.
    Snippet(String),
    /// Nowhere on this machine, and why.
    Unfound(String),
}

/// One program a run counts: its reading's scope and its text.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Counted {
    scope: String,
    source: Source,
}

impl Ask {
    /// The programs the run counts: those named, or every program the
    /// list's doc names, the workloads by name, `micro_bench.php` from
    /// `tree`, the pinned php-src where it was found, and the four loops.
    fn programs(&self, root: &Path, tree: Result<PathBuf>) -> Result<Vec<Counted>> {
        let counted = |scope: String, source: Source| Counted { scope, source };
        if !self.program.is_empty() {
            let named = |path: &PathBuf| {
                let stem = path.file_stem().and_then(|s| s.to_str());
                let stem = stem.filter(|stem| !stem.is_empty());
                let stem = stem.with_context(|| format!("{} names no program", path.display()))?;
                let path = std::path::absolute(path)?;
                Ok(counted(format!("program/{stem}"), Source::File(path)))
            };
            return self.program.iter().map(named).collect();
        }
        let dir = root.join(WORKLOADS);
        let mut workloads: Vec<PathBuf> = std::fs::read_dir(&dir)
            .with_context(|| format!("reading {}", dir.display()))?
            .map(|entry| entry.map(|entry| entry.path()))
            .collect::<std::io::Result<_>>()?;
        workloads.retain(|path| path.extension().is_some_and(|ext| ext == "php"));
        workloads.sort();
        let mut programs = Vec::new();
        for path in workloads {
            let stem = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default();
            programs.push(counted(
                format!("workloads/{stem}"),
                Source::File(path.clone()),
            ));
        }
        let micro = match tree {
            Ok(tree) => Source::File(tree.join(MICRO_BENCH)),
            Err(why) => Source::Unfound(format!("the pinned php-src is not here: {why:#}")),
        };
        let scope = MICRO_BENCH.trim_end_matches(".php").to_owned();
        programs.push(counted(scope, micro));
        for (name, body) in LOOPS {
            let source = Source::Snippet(counting_loop(body));
            programs.push(counted(format!("loops/{name}"), source));
        }
        Ok(programs)
    }
}

/// The counting loop of #3273 over `body`, after the recipe's preamble.
fn counting_loop(body: &str) -> String {
    format!(
        "function k($n){{ $s=0.0; for($i=1;$i<$n;$i++){{ {body} }} return $s; }} k({ROUNDS});\n"
    )
}

/// The counts one program's run wrote: each variant's name and the
/// oplines it served, summed over every request the run served.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Census {
    served: BTreeMap<String, u64>,
}

impl Census {
    /// The counts in `stream`, the run's error stream: each line that
    /// starts with [`LINE`], its variant's name and its count. A line of
    /// that start that does not name both is refused, never read as none.
    pub(crate) fn read(stream: &[u8]) -> Result<Census> {
        let mut census = Census::default();
        let text = String::from_utf8_lossy(stream);
        for line in text.lines() {
            let Some(said) = line.strip_prefix(LINE) else {
                continue;
            };
            let parsed = said
                .split_once(' ')
                .and_then(|(name, n)| Some((name, n.parse::<u64>().ok()?)));
            let Some((name, n)) = parsed.filter(|(name, _)| !name.is_empty()) else {
                anyhow::bail!(
                    "the counting build wrote {line:?}, which names no variant and count"
                );
            };
            *census.served.entry(name.to_owned()).or_default() += n;
        }
        Ok(census)
    }

    /// The oplines the loop dispatched.
    pub(crate) fn oplines(&self) -> u64 {
        self.served.values().sum()
    }

    /// Each variant with its share of [`Census::oplines`] in percent, the
    /// largest first and the names in order among equal shares.
    pub(crate) fn shares(&self) -> Vec<(&str, f64)> {
        let total = self.oplines();
        #[allow(clippy::cast_precision_loss)] // a count past 2^53 is not one a run reaches
        let share = |n: u64| n as f64 * 100.0 / total as f64;
        let mut shares: Vec<(&str, f64)> = (self.served.iter())
            .map(|(name, n)| (name.as_str(), share(*n)))
            .collect();
        shares.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(b.0)));
        shares
    }

    /// The variants over [`THRESHOLD`], the largest share first.
    pub(crate) fn over(&self) -> Vec<(&str, f64)> {
        let mut shares = self.shares();
        shares.retain(|(_, share)| *share > THRESHOLD);
        shares
    }

    /// The reading's numbers, groups, status and message for `scope`,
    /// whose run ended as `ended`.
    fn written_into(&self, scope: &str, ended: Ended, reading: &mut Reading) -> Result<()> {
        let oplines = self.oplines();
        if oplines == 0 {
            reading.status = Status::Fail;
            reading.message = format!(
                "{scope}: the engine wrote no counts, so it is not built with the bundle's `{FEATURE}` feature or the run ended before its request did; it ended on {ended}"
            );
            return Ok(());
        }
        let over = self.over();
        reading.count("oplines", oplines)?;
        reading.count("variants", self.served.len() as u64)?;
        reading.count("over", over.len() as u64)?;
        for (name, share) in self.shares() {
            reading
                .groups
                .insert(name.to_owned(), Number::Float(rounded(share)));
        }
        let named = over
            .iter()
            .map(|(name, share)| format!("{name} {share:.3}%"));
        let mut said = format!(
            "{scope}: {oplines} oplines dispatched by {} variant{}; {} over a quarter of a percent{}",
            self.served.len(),
            plural(self.served.len()),
            over.len(),
            if over.is_empty() {
                String::new()
            } else {
                format!(": {}", joined(named, ", "))
            }
        );
        if ended != Ended::Status(0) {
            write!(said, "; the run ended on {ended}").ok();
        }
        reading.status = Status::Pass;
        reading.message = said;
        Ok(())
    }
}

/// A share in percent to six places, as the reading keeps it.
fn rounded(share: f64) -> f64 {
    (share * 1e6).round() / 1e6
}

/// `xtask dispatch-census`: [`run`], its readings recorded.
pub(crate) fn command(root: &Path, manifest: &Manifest, store: &Store, ask: &Ask) -> Result<()> {
    crate::record(store, &run(root, manifest, ask)?)
}

/// The counting build: the one asked for, or `php` built in release with
/// the bundle's [`FEATURE`] into its own directory.
fn engine(root: &Path, ask: &Ask) -> Result<PathBuf> {
    if let Some(engine) = &ask.engine {
        return Ok(std::path::absolute(engine)?);
    }
    let features = format!("{BUNDLE}/{FEATURE}");
    let into = profile::featured(root, FEATURE);
    let (package, binary) = (CLI.package(), CLI.binary);
    profile::released(
        root,
        (&package, binary),
        Some(&features),
        (Some(&into), None),
    )
}

/// Run what was asked for: one reading per program, written by the
/// caller through `record`, and one line naming every variant over the
/// threshold in any of them.
pub fn run(root: &Path, manifest: &Manifest, ask: &Ask) -> Result<Vec<Reading>> {
    let pin = manifest.pin()?;
    let snapshot = Snapshot::disk(root)?;
    let programs = ask.programs(root, pin.source.tree())?;
    let oracle = pin.oracle();
    let engine = engine(root, ask);
    let mut readings = Vec::new();
    let mut over_any = BTreeSet::new();
    for counted in programs {
        let scope = &counted.scope;
        // What a program dispatches is a function of the whole engine and
        // the program, so the reading is stamped over the workspace.
        let reading = Reading::stamped_over(INSTRUMENT, scope, &snapshot, Path::new("."))?;
        let mut reading = reading.under(pin);
        let counts = match (&engine, &counted.source) {
            (_, Source::Unfound(why)) => {
                reading.message = format!("{scope}: nothing ran; {why}");
                readings.push(reading);
                continue;
            }
            (Err(why), _) => Err(anyhow::anyhow!("the counting build did not build: {why:#}")),
            (Ok(engine), source) => counts(&oracle, engine, source),
        };
        let counted = counts.and_then(|(census, ended)| {
            census.written_into(scope, ended, &mut reading)?;
            over_any.extend(census.over().into_iter().map(|(name, _)| name.to_owned()));
            Ok(())
        });
        if let Err(trouble) = counted {
            reading.status = Status::Fail;
            reading.message = format!("{scope}: {trouble:#}");
        }
        readings.push(reading);
    }
    println!(
        "{INSTRUMENT}: {} variant{} over a quarter of a percent, the threshold `IN_THE_LOOP`'s doc in {ARMS} states, in any program: {}",
        over_any.len(),
        plural(over_any.len()),
        joined(&over_any, ", ")
    );
    Ok(readings)
}

/// The counts `source` dispatched on `engine` under the recipe and
/// [`MEMORY`], and how its run ended.
fn counts(oracle: &Oracle<'_>, engine: &Path, source: &Source) -> Result<(Census, Ended)> {
    let program = match source {
        Source::File(path) => {
            ensure!(path.is_file(), "there is no program at {}", path.display());
            Program::File(path)
        }
        Source::Snippet(code) => Program::Snippet(code),
        Source::Unfound(why) => anyhow::bail!("{why}"),
    };
    let ini = [MEMORY.to_owned()];
    let ran = oracle.run_under(&Invocation {
        ini: &ini,
        ..Invocation::new(engine, program)
    })?;
    Ok((Census::read(&ran.stderr)?, ran.exit))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The counts are the engine's lines on the error stream, summed per
    /// variant over every request; every other line, a program's own
    /// among them, is not read, and one of their start naming no count
    /// is refused.
    #[test]
    fn the_counts_are_the_engines_lines_summed_per_variant() {
        let stream = b"PHP Warning: x\n\ndispatch-census AddCvCv 3\ndispatch-census Jmp 1\n\ndispatch-census AddCvCv 4\n";
        let census = Census::read(stream).unwrap();
        assert_eq!(census.oplines(), 8);
        assert_eq!(census.shares(), [("AddCvCv", 87.5), ("Jmp", 12.5)]);
        assert!(Census::read(b"dispatch-census Jmp many\n").is_err());
        assert_eq!(Census::read(b"no counts\n").unwrap().oplines(), 0);
    }

    /// A variant is over the threshold where its share is more than a
    /// quarter of a percent, and one at a quarter exactly is not.
    #[test]
    fn a_variant_is_over_at_more_than_a_quarter_of_a_percent() {
        let mut census = Census::default();
        census.served.insert("Hot".to_owned(), 3);
        census.served.insert("Edge".to_owned(), 25);
        census.served.insert("Bulk".to_owned(), 9972);
        let over: Vec<&str> = census.over().into_iter().map(|(name, _)| name).collect();
        assert_eq!(over, ["Bulk"]);
        let mut reading = Reading::new(INSTRUMENT, "loops/empty");
        census
            .written_into("loops/empty", Ended::Status(0), &mut reading)
            .unwrap();
        assert_eq!(reading.status, Status::Pass);
        assert_eq!(reading.int("oplines"), Some(10000));
        assert_eq!(reading.int("over"), Some(1));
        assert_eq!(reading.groups.get("Edge"), Some(&Number::Float(0.25)));
        assert!(
            reading
                .message
                .ends_with("1 over a quarter of a percent: Bulk 99.720%"),
            "{}",
            reading.message
        );
    }

    /// A run the engine wrote no counts for fails, naming the feature,
    /// and one that ended otherwise than with 0 says how.
    #[test]
    fn a_run_with_no_counts_fails_naming_the_feature() {
        let mut reading = Reading::new(INSTRUMENT, "workloads/fib");
        let ended = Ended::Status(255);
        Census::default()
            .written_into("workloads/fib", ended, &mut reading)
            .unwrap();
        assert_eq!(reading.status, Status::Fail);
        assert!(
            reading.message.contains("`dispatch-census` feature"),
            "{}",
            reading.message
        );
        let census = Census::read(b"dispatch-census Jmp 1\n").unwrap();
        census
            .written_into("workloads/fib", ended, &mut reading)
            .unwrap();
        assert!(
            reading.message.ends_with("; the run ended on status 255"),
            "{}",
            reading.message
        );
    }

    /// Without a program named, the run counts every workload by its
    /// stem, `micro_bench.php` from the pinned tree or a skip naming why
    /// it is not here, and the four loops; a program named is counted
    /// alone, scoped by its stem.
    #[cfg(tracked)]
    #[test]
    fn the_run_counts_the_programs_the_lists_doc_names() {
        let root = crate::package_dir().join("..");
        let tree = PathBuf::from("/src");
        let programs = Ask::default().programs(&root, Ok(tree.clone())).unwrap();
        let scopes: Vec<&str> = programs.iter().map(|c| c.scope.as_str()).collect();
        assert!(scopes.contains(&"workloads/fib"), "{scopes:?}");
        let tail = &scopes[scopes.len() - 5..];
        assert_eq!(
            tail,
            [
                "Zend/micro_bench",
                "loops/empty",
                "loops/add",
                "loops/add-assign",
                "loops/mul"
            ]
        );
        let micro = &programs[programs.len() - 5].source;
        assert_eq!(micro, &Source::File(tree.join("Zend/micro_bench.php")));
        let Source::Snippet(add) = &programs[programs.len() - 3].source else {
            panic!("a loop is a snippet");
        };
        assert_eq!(
            add,
            "function k($n){ $s=0.0; for($i=1;$i<$n;$i++){ $s = $i + 1; } return $s; } k(1000000);\n"
        );
        let absent = Ask::default()
            .programs(&root, Err(anyhow::anyhow!("gone")))
            .unwrap();
        assert!(
            matches!(&absent[absent.len() - 5].source, Source::Unfound(why) if why.contains("gone"))
        );
        let named = Ask {
            program: vec![PathBuf::from("/w/wordpress.php")],
            ..Ask::default()
        };
        let named = named.programs(&root, Ok(tree)).unwrap();
        assert_eq!(
            named,
            [Counted {
                scope: "program/wordpress".to_owned(),
                source: Source::File(PathBuf::from("/w/wordpress.php")),
            }]
        );
    }

    /// One place per rule: the threshold is the one `IN_THE_LOOP`'s doc
    /// states, and the line the counts start with is the one the engine
    /// writes, so a change to either side fails here.
    #[cfg(tracked)]
    #[test]
    fn the_threshold_and_the_line_are_the_engines() {
        let root = crate::package_dir().join("..");
        let arms = std::fs::read_to_string(root.join(ARMS)).unwrap();
        let doc = arms.split("const IN_THE_LOOP").next().unwrap();
        assert!(
            doc.replace("\n/// ", " ")
                .contains("more than a quarter of a percent"),
            "{ARMS} no longer states the threshold {THRESHOLD}%"
        );
        assert!((THRESHOLD - 0.25).abs() < f64::EPSILON);
        let census = root.join("crates/php_engine/src/api/request/census.rs");
        let census = std::fs::read_to_string(census).unwrap();
        assert!(census.contains(&format!("const LINE: &str = {LINE:?};")));
        let bundle = std::fs::read_to_string(root.join("crates/php_bundle/Cargo.toml")).unwrap();
        assert!(
            bundle.contains(&format!("\n{FEATURE} = [")),
            "the bundle has no {FEATURE}"
        );
    }
}
