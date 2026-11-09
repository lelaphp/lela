//! `xtask dispatch-census`, the census behind `IN_THE_LOOP` (design §5.8,
//! #4328): each variant's share of the oplines the loop dispatched, over
//! every run of every program `docs/perf/workloads.toml` declares
//! ([`crate::workloads`]), each with the arguments it declares. The
//! list's doc admits [`CRITERION`] (#4335), so a run votes where its
//! program's role is a dispatch role and it ended as its program declares,
//! and a variant over [`THRESHOLD`] in any run that votes is voted in.
//!
//! **The counting build.** `php` built in release with the bundle's
//! [`FEATURE`], into a target directory of its own, so the default build
//! stands ([`profile::featured`]). Its loop counts each opline it
//! dispatches by the `php_unit::Handler` that serves it, and its request
//! writes the counts to the error stream where it ends, one line per
//! variant met ([`LINE`]; `php_engine`'s `api::request::census`).
//!
//! **The run.** Each run is run on that build under the oracle's recipe
//! and [`MEMORY`], as the bench's arms run the kernels; the oracle itself
//! runs nothing. Each writes one reading, scoped `workloads/<run>`, the
//! run's name being its workload's stem and `/<profile>` after it where it
//! runs under one, or `program/<stem>` for a program named on the command
//! line, which votes nothing. The reading's `numbers` hold `oplines`, the
//! oplines the loop dispatched; `variants`, how many variants served them;
//! and `over`, how many of those are over the threshold. Its `groups` hold
//! each variant's share of `oplines` in percent. Its message names the
//! variants over the threshold, the largest share first. A run the engine
//! wrote no counts for fails, and so does one that ended otherwise than
//! its program declares, its counts kept out of the vote; a program whose
//! tree is not on this machine is a skip.
//!
//! **The vote.** Over the declared set, one more reading, scoped
//! [`VOTE`], names the variants the vote would put in the loop's body and
//! the build's own placement does not, and those the placement puts there
//! and the vote does not, each with its highest share in a run that votes
//! and that run ([`Vote`]). The placement is `IN_THE_LOOP` and
//! `PATH_IN_THE_LOOP` in [`ARMS`], a variant of the second counting as
//! in. That reading informs and fails nothing: the list is a person's to
//! change (#4335), and the instrument decides nothing.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail, ensure};

use crate::bench::kernels::MEMORY;
use crate::carried::BUNDLE;
use crate::key::Snapshot;
use crate::manifest::Manifest;
use crate::oracle::{Ended, Invocation, Oracle, Program, hosts::CLI, profile};
use crate::reading::{Number, Reading, Status, Store, joined, plural};
use crate::workloads::{Declaration, Role, Text};

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

/// `IN_THE_LOOP`'s criterion, word for word as #4335 ruled it, which the
/// list's doc in [`ARMS`] and design §5.8 carry.
pub(crate) const CRITERION: &str = "every variant met in more than a quarter of a percent of the oplines dispatched by any program docs/perf/workloads.toml declares in a dispatch role, run with the arguments it declares and ending as it declares";

/// The file whose doc of `IN_THE_LOOP` states [`CRITERION`], and whose
/// `IN_THE_LOOP` and `PATH_IN_THE_LOOP` are the build's placement.
const ARMS: &str = "crates/php_engine/build/arms.rs";

/// The two lists of [`ARMS`] that place a variant in the loop's body.
const PLACED: [&str; 2] = ["IN_THE_LOOP", "PATH_IN_THE_LOOP"];

/// The scope of the vote's reading.
pub(crate) const VOTE: &str = "vote";

/// What one `xtask dispatch-census` run was asked for.
#[derive(Debug, Clone, Default, clap::Args)]
pub struct Ask {
    /// A program by its path, repeatable, run in place of the declared
    /// ones: its reading is scoped `program/<stem>`, and it votes nothing.
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

/// One run the census counts: its reading's scope, its program's text,
/// the arguments after it, how it should end, and its program's role,
/// none for a program named on the command line, which votes nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Counted {
    scope: String,
    source: Source,
    args: Vec<String>,
    ends: Ended,
    role: Option<Role>,
}

impl Counted {
    /// Whether its counts vote, where its run ends as declared.
    fn votes(&self) -> bool {
        self.role.is_some_and(Role::votes)
    }
}

impl Ask {
    /// The runs the census counts: the programs named, each held to end on
    /// status 0, or every run `declared` yields, a pinned file of it read
    /// from `tree`, the pinned php-src where it was found.
    fn programs(&self, declared: &Declaration, tree: &Result<PathBuf>) -> Result<Vec<Counted>> {
        if !self.program.is_empty() {
            let named = |path: &PathBuf| {
                let stem = path.file_stem().and_then(|s| s.to_str());
                let stem = stem.filter(|stem| !stem.is_empty());
                let stem = stem.with_context(|| format!("{} names no program", path.display()))?;
                Ok(Counted {
                    scope: format!("program/{stem}"),
                    source: Source::File(std::path::absolute(path)?),
                    args: Vec::new(),
                    ends: Ended::Status(0),
                    role: None,
                })
            };
            return self.program.iter().map(named).collect();
        }
        let counted = declared.runs().map(|run| {
            let source = match run.text() {
                Text::File(path) => Source::File(path.clone()),
                Text::Code(code) => Source::Snippet(code.clone()),
                Text::Pinned(path) => match tree {
                    Ok(tree) => Source::File(tree.join(path)),
                    Err(why) => Source::Unfound(format!("the pinned php-src is not here: {why:#}")),
                },
            };
            Counted {
                scope: format!("workloads/{}", run.name()),
                source,
                args: run.args().to_vec(),
                ends: run.workload.ends,
                role: Some(run.workload.role),
            }
        });
        Ok(counted.collect())
    }
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
    /// whose run ended as `ended` where it should have ended as `ends`:
    /// a pass where it ended so, and a fail, its counts kept, where it
    /// did not. A run with no counts fails.
    fn written_into(
        &self,
        scope: &str,
        (ended, ends): (Ended, Ended),
        reading: &mut Reading,
    ) -> Result<()> {
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
        reading.status = Status::Pass;
        if ended != ends {
            reading.status = Status::Fail;
            write!(
                said,
                "; the run ended on {ended} where its program declares {ends}, so its counts are out of the vote"
            )
            .ok();
        }
        reading.message = said;
        Ok(())
    }
}

/// The vote over the runs that vote: each variant met in one, with its
/// highest share in any of them and that run's scope.
#[derive(Debug, Clone, Default, PartialEq)]
struct Vote {
    met: BTreeMap<String, (f64, String)>,
    voters: usize,
}

impl Vote {
    /// `census`, the counts of the run `scope`, cast.
    fn cast(&mut self, scope: &str, census: &Census) {
        self.voters += 1;
        for (name, share) in census.shares() {
            let highest = self.met.get(name).is_some_and(|(had, _)| *had >= share);
            if !highest {
                self.met.insert(name.to_owned(), (share, scope.to_owned()));
            }
        }
    }

    /// The variants over [`THRESHOLD`] in a run that voted.
    fn voted(&self) -> impl Iterator<Item = (&String, &(f64, String))> {
        self.met.iter().filter(|(_, (share, _))| *share > THRESHOLD)
    }

    /// `names`, the highest share in a run that voted first, and in order
    /// among equal shares.
    fn by_share<'n>(&self, names: impl Iterator<Item = &'n String>) -> Vec<&'n String> {
        let share = |name: &String| self.met.get(name).map_or(0.0, |(share, _)| *share);
        let mut names: Vec<&String> = names.collect();
        names.sort_by(|a, b| share(b).total_cmp(&share(a)).then(a.cmp(b)));
        names
    }

    /// The vote against `placed`, the variants the build places in the
    /// loop's body, as `reading`'s numbers and message: the variants that
    /// would enter, the largest share first, and those that would leave.
    fn written_into(&self, placed: &BTreeSet<String>, reading: &mut Reading) -> Result<()> {
        let shown = |name: &str| match self.met.get(name) {
            Some((share, scope)) => format!("{name} {share:.3}% {scope}"),
            None => format!("{name} met in no run that votes"),
        };
        let voted: BTreeSet<&String> = self.voted().map(|(name, _)| name).collect();
        let enter = self.by_share(voted.iter().copied().filter(|n| !placed.contains(*n)));
        let leave = self.by_share(placed.iter().filter(|n| !voted.contains(n)));
        reading.count("voters", self.voters as u64)?;
        reading.count("voted", voted.len() as u64)?;
        reading.count("placed", placed.len() as u64)?;
        reading.count("enter", enter.len() as u64)?;
        reading.count("leave", leave.len() as u64)?;
        let listed = |names: &[&String]| {
            if names.is_empty() {
                "none".to_owned()
            } else {
                joined(names.iter().map(|name| shown(name)), ", ")
            }
        };
        reading.status = Status::Pass;
        reading.message = format!(
            "{} run{} of a dispatch role voted {} variant{} over a quarter of a percent, against the build's {} placed by {} in {ARMS}; {} would enter: {}; {} would leave: {}; this informs and fails nothing",
            self.voters,
            plural(self.voters),
            voted.len(),
            plural(voted.len()),
            placed.len(),
            PLACED.join(" and "),
            enter.len(),
            listed(&enter),
            leave.len(),
            listed(&leave),
        );
        Ok(())
    }
}

/// The variants the build at `root` places in the loop's body: every
/// name of [`PLACED`]'s lists in [`ARMS`].
fn placed(root: &Path) -> Result<BTreeSet<String>> {
    let text = crate::read(&root.join(ARMS))?;
    let file = syn::parse_file(&text).with_context(|| format!("parsing {ARMS}"))?;
    let mut placed = BTreeSet::new();
    for list in PLACED {
        let item = file.items.iter().find_map(|item| match item {
            syn::Item::Const(c) if c.ident == list => Some(&c.expr),
            _ => None,
        });
        let item = item.with_context(|| format!("{ARMS} has no const {list}"))?;
        let syn::Expr::Reference(reference) = item.as_ref() else {
            bail!("{ARMS}'s {list} is no `&[..]`");
        };
        let syn::Expr::Array(array) = reference.expr.as_ref() else {
            bail!("{ARMS}'s {list} is no `&[..]`");
        };
        for name in &array.elems {
            let syn::Expr::Lit(syn::ExprLit {
                lit: syn::Lit::Str(name),
                ..
            }) = name
            else {
                bail!("{ARMS}'s {list} holds other than string literals");
            };
            placed.insert(name.value());
        }
    }
    Ok(placed)
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

/// Run what was asked for: one reading per run, and over the declared
/// set the vote's, written by the caller through `record`; the vote's
/// message is printed.
pub fn run(root: &Path, manifest: &Manifest, ask: &Ask) -> Result<Vec<Reading>> {
    let pin = manifest.pin()?;
    let snapshot = Snapshot::disk(root)?;
    let declared = Declaration::load(root)?;
    let programs = ask.programs(&declared, &pin.source.tree())?;
    let oracle = pin.oracle();
    let engine = engine(root, ask);
    let mut readings = Vec::new();
    let mut vote = Vote::default();
    // What a program dispatches is a function of the whole engine and the
    // program, so each reading is stamped over the workspace.
    let stamped = |scope: &str| -> Result<Reading> {
        let reading = Reading::stamped_over(INSTRUMENT, scope, &snapshot, Path::new("."))?;
        Ok(reading.under(pin))
    };
    for counted in &programs {
        let scope = &counted.scope;
        let mut reading = stamped(scope)?;
        let counts = match (&engine, &counted.source) {
            (_, Source::Unfound(why)) => {
                reading.message = format!("{scope}: nothing ran; {why}");
                readings.push(reading);
                continue;
            }
            (Err(why), _) => Err(anyhow::anyhow!("the counting build did not build: {why:#}")),
            (Ok(engine), _) => counts(&oracle, engine, counted),
        };
        let written = counts.and_then(|(census, ended)| {
            census.written_into(scope, (ended, counted.ends), &mut reading)?;
            if reading.status == Status::Pass && counted.votes() {
                vote.cast(scope, &census);
            }
            Ok(())
        });
        if let Err(trouble) = written {
            reading.status = Status::Fail;
            reading.message = format!("{scope}: {trouble:#}");
        }
        if let Some(role) = counted.role.filter(|role| !role.votes()) {
            let said = format!("; its role, {}, votes nothing", role.name());
            reading.message.push_str(&said);
        }
        readings.push(reading);
    }
    if ask.program.is_empty() {
        let mut reading = stamped(VOTE)?;
        vote.written_into(&placed(root)?, &mut reading)?;
        println!("{INSTRUMENT}: `IN_THE_LOOP` admits {CRITERION}");
        println!("{INSTRUMENT}: {}", reading.message);
        readings.push(reading);
    }
    Ok(readings)
}

/// The counts `counted` dispatched on `engine` under the recipe and
/// [`MEMORY`], with the arguments it declares, and how its run ended.
fn counts(oracle: &Oracle<'_>, engine: &Path, counted: &Counted) -> Result<(Census, Ended)> {
    let program = match &counted.source {
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
        args: &counted.args,
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
        let mut reading = Reading::new(INSTRUMENT, "workloads/loop-empty");
        let ended = (Ended::Status(0), Ended::Status(0));
        census
            .written_into("workloads/loop-empty", ended, &mut reading)
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

    /// A run the engine wrote no counts for fails, naming the feature;
    /// one that ended otherwise than its program declares fails, saying
    /// how, its counts kept and out of the vote; and one that ended on a
    /// status its program declares passes, as #4335's (c) ruled.
    #[test]
    fn a_run_fails_with_no_counts_or_an_end_other_than_its_declared() {
        let mut reading = Reading::new(INSTRUMENT, "workloads/fib");
        let (zero, other) = (Ended::Status(0), Ended::Status(255));
        Census::default()
            .written_into("workloads/fib", (other, zero), &mut reading)
            .unwrap();
        assert_eq!(reading.status, Status::Fail);
        assert!(
            reading.message.contains("`dispatch-census` feature"),
            "{}",
            reading.message
        );
        let census = Census::read(b"dispatch-census Jmp 1\n").unwrap();
        census
            .written_into("workloads/fib", (other, zero), &mut reading)
            .unwrap();
        assert_eq!(reading.status, Status::Fail, "{}", reading.message);
        assert_eq!(reading.int("oplines"), Some(1));
        assert!(
            reading.message.ends_with(
                "; the run ended on status 255 where its program declares status 0, so its counts are out of the vote"
            ),
            "{}",
            reading.message
        );
        let mut declared = Reading::new(INSTRUMENT, "workloads/fib");
        census
            .written_into("workloads/fib", (other, other), &mut declared)
            .unwrap();
        assert_eq!(declared.status, Status::Pass, "{}", declared.message);
    }

    /// Without a program named, the census counts every run the
    /// declaration yields: a file in place, a pinned file from the tree or
    /// a skip naming why it is not here, and code as a snippet, each with
    /// the arguments and the end it declares; a native votes nothing. A
    /// program named is counted alone, scoped by its stem, held to end on
    /// 0 and voting nothing.
    #[test]
    fn the_census_counts_every_declared_run_with_its_arguments() {
        let text = r#"
[[workload]]
stem = "fib"
first_read = 1
about = "x"
role = "kernel"

[[workload]]
stem = "modes"
first_read = 1
about = "x"
role = "native"
exit = 3
profile = [{ name = "small", args = ["small", "1"] }]

[[workload]]
stem = "micro_bench"
first_read = 1
about = "x"
role = "probe"
pinned = "Zend/micro_bench.php"

[[workload]]
stem = "loop-add"
first_read = 1
about = "x"
role = "probe"
code = "k(1);\n"
"#;
        let declared = Declaration::parse(text, Path::new("/w")).unwrap();
        let tree = PathBuf::from("/src");
        let programs = Ask::default().programs(&declared, &Ok(tree)).unwrap();
        let counted = |scope: &str, source, args: &[&str], ends, role| Counted {
            scope: scope.to_owned(),
            source,
            args: args.iter().map(|a| (*a).to_owned()).collect(),
            ends: Ended::Status(ends),
            role: Some(role),
        };
        let file = |path: &str| Source::File(PathBuf::from(path));
        assert_eq!(
            programs,
            [
                counted(
                    "workloads/fib",
                    file("/w/docs/perf/workloads/fib.php"),
                    &[],
                    0,
                    Role::Kernel
                ),
                counted(
                    "workloads/modes/small",
                    file("/w/docs/perf/workloads/modes.php"),
                    &["small", "1"],
                    3,
                    Role::Native
                ),
                counted(
                    "workloads/micro_bench",
                    file("/src/Zend/micro_bench.php"),
                    &[],
                    0,
                    Role::Probe
                ),
                counted(
                    "workloads/loop-add",
                    Source::Snippet("k(1);\n".to_owned()),
                    &[],
                    0,
                    Role::Probe
                ),
            ]
        );
        let votes: Vec<bool> = programs.iter().map(Counted::votes).collect();
        assert_eq!(votes, [true, false, true, true]);
        let absent = Ask::default()
            .programs(&declared, &Err(anyhow::anyhow!("gone")))
            .unwrap();
        assert!(matches!(&absent[2].source, Source::Unfound(why) if why.contains("gone")));
        let named = Ask {
            program: vec![PathBuf::from("/w/wordpress.php")],
            ..Ask::default()
        };
        let named = named
            .programs(&declared, &Ok(PathBuf::from("/src")))
            .unwrap();
        assert_eq!(
            named,
            [Counted {
                scope: "program/wordpress".to_owned(),
                source: Source::File(PathBuf::from("/w/wordpress.php")),
                args: Vec::new(),
                ends: Ended::Status(0),
                role: None,
            }]
        );
        assert!(!named[0].votes());
    }

    /// The vote keeps each variant's highest share among the runs cast and
    /// that run; against the placement, a variant over the threshold in a
    /// run and not placed would enter, and a placed one over it in none
    /// would leave, met or not, each with its share and run; the reading
    /// passes whatever it names.
    #[test]
    fn the_vote_names_who_would_enter_and_leave_against_the_placement() {
        let census = |lines: &[u8]| Census::read(lines).unwrap();
        let mut vote = Vote::default();
        vote.cast(
            "workloads/a",
            &census(b"dispatch-census Jmp 999\ndispatch-census Rare 1\n"),
        );
        vote.cast(
            "workloads/b",
            &census(b"dispatch-census Jmp 1\ndispatch-census New 3\n"),
        );
        let placed: BTreeSet<String> = ["Jmp", "Rare", "Unmet"].map(str::to_owned).into();
        let mut reading = Reading::new(INSTRUMENT, VOTE);
        vote.written_into(&placed, &mut reading).unwrap();
        assert_eq!(reading.status, Status::Pass);
        assert_eq!(reading.int("voters"), Some(2));
        assert_eq!(reading.int("enter"), Some(1));
        assert_eq!(reading.int("leave"), Some(2));
        let said = &reading.message;
        assert!(
            said.contains("1 would enter: New 75.000% workloads/b;"),
            "{said}"
        );
        assert!(
            said.contains(
                "2 would leave: Rare 0.100% workloads/a, Unmet met in no run that votes;"
            ),
            "{said}"
        );
        assert!(!said.contains("Jmp 99.900% workloads/a"), "{said}");
    }

    /// One place per rule: the criterion is the one `IN_THE_LOOP`'s doc
    /// and design §5.8 state word for word, its threshold the
    /// instrument's, the line the counts start with is the one the engine
    /// writes, and the placement is the build's two lists, so a change to
    /// either side fails here.
    #[cfg(tracked)]
    #[test]
    fn the_criterion_the_threshold_and_the_line_are_the_engines() {
        let root = crate::package_dir().join("..");
        let arms = std::fs::read_to_string(root.join(ARMS)).unwrap();
        let doc = arms.split("const IN_THE_LOOP").next().unwrap();
        let doc = doc.replace("\n/// ", " ");
        assert!(
            doc.contains(CRITERION),
            "{ARMS} no longer states the criterion"
        );
        let design = std::fs::read_to_string(root.join("third-engine-design.md")).unwrap();
        let section = design.split("### 5.8 Dispatch").nth(1).unwrap();
        let section = section.split("\n### ").next().unwrap();
        assert!(
            section.contains(CRITERION),
            "design §5.8 no longer states the criterion"
        );
        assert!(CRITERION.contains("more than a quarter of a percent"));
        assert!((THRESHOLD - 0.25).abs() < f64::EPSILON);
        let census = root.join("crates/php_engine/src/api/request/census.rs");
        let census = std::fs::read_to_string(census).unwrap();
        assert!(census.contains(&format!("const LINE: &str = {LINE:?};")));
        let bundle = std::fs::read_to_string(root.join("crates/php_bundle/Cargo.toml")).unwrap();
        assert!(
            bundle.contains(&format!("\n{FEATURE} = [")),
            "the bundle has no {FEATURE}"
        );
        let placed = placed(&root).unwrap();
        assert!(placed.contains("VerifyReturnType") && placed.contains("Jmp"));
        assert!(placed.len() > 100, "{}", placed.len());
    }
}
