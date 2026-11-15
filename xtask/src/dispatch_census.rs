//! `xtask dispatch-census`, the census behind `IN_THE_LOOP` (design §5.8):
//! each variant's share of the oplines the loop dispatched, over
//! every run of every program `docs/perf/workloads.toml` declares
//! ([`crate::workloads`]), each with the arguments it declares, and over
//! each of the bench's pages it declares, served from the rig ([`pages`]).
//! The list's doc admits [`CRITERION`] (as amended 2026-11-09), so a
//! run votes where its role votes ([`Role::votes`]) and it ended as
//! declared, and a variant over [`THRESHOLD`] in any run that votes is
//! voted in.
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
//! line, which votes nothing. A page is served by the counting build's
//! built-in server, and its reading is scoped `pages/<name>`; where no rig
//! is described on this machine, each page is a skip that says so. The
//! reading's `numbers` hold `oplines`, the
//! oplines the loop dispatched; `variants`, how many variants served them;
//! and `over`, how many of those are over the threshold. Its `groups` hold
//! each variant's share of `oplines` in percent. Its message names the
//! variants over the threshold, the largest share first. A run the engine
//! wrote no counts for fails, and so does one that ended otherwise than
//! declared, a program on another status or a page answering other than
//! 200, its counts kept out of the vote; a program whose tree is not on
//! this machine is a skip.
//!
//! **The vote.** Over the declared set, one more reading, scoped
//! [`VOTE`], names the variants the vote would put in the loop's body and
//! the build's own placement does not, and those the placement puts there
//! and the vote does not, each with its highest share in a run that votes
//! and that run ([`Vote`]). The placement is `IN_THE_LOOP` and
//! `PATH_IN_THE_LOOP` in [`ARMS`], a variant of the second counting as
//! in. That reading informs and fails nothing: the list is a person's to
//! change, and the instrument decides nothing. It names each
//! declared run of a role that votes that did not vote, a skip, such as a
//! page where no rig is here, or a fail, its `missed` number counting
//! them, and where there is one it is a skip, not a pass: a vote over part
//! of the criterion's runs would name as leaving a variant only a missing
//! run meets, and a fail would hold a machine without the rig to a
//! reading that decides nothing, each missing run's own reading already
//! saying why it did not vote.

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
use crate::workloads::{ANSWERS, Declaration, Page, Pages, Role, Text};

mod pages;

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

/// `IN_THE_LOOP`'s criterion, word for word as ruled and amended
/// 2026-11-09, which the list's doc in [`ARMS`] and design §5.8 carry.
pub(crate) const CRITERION: &str = "every variant met in more than a quarter of a percent of the oplines dispatched by any run docs/perf/workloads.toml declares in a role that votes, a kernel's or the bench's: a program run with the arguments it declares and ending as it declares, or a bench page served from the rig as it declares and answering 200";

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
    /// A page of the bench, served from the rig.
    Page(Page),
    /// Nowhere on this machine, and why.
    Unfound(String),
}

/// One run the census counts: its reading's scope, its program's text or
/// its page, the arguments after it, how it should end, and its role,
/// none for a program named on the command line, which votes nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Counted {
    scope: String,
    source: Source,
    args: Vec<String>,
    ends: Ends,
    role: Option<Role>,
}

/// How a counted run should end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ends {
    /// A program's, on this status.
    Exits(Ended),
    /// A page's, answering this status.
    Answers(u16),
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
    /// from `tree`, the pinned php-src where it was found, and then each
    /// page it declares, a skip where `rig` is absent.
    fn programs(
        &self,
        declared: &Declaration,
        tree: &Result<PathBuf>,
        rig: &Result<pages::Located>,
    ) -> Result<Vec<Counted>> {
        if !self.program.is_empty() {
            let named = |path: &PathBuf| {
                let stem = path.file_stem().and_then(|s| s.to_str());
                let stem = stem.filter(|stem| !stem.is_empty());
                let stem = stem.with_context(|| format!("{} names no program", path.display()))?;
                Ok(Counted {
                    scope: format!("program/{stem}"),
                    source: Source::File(std::path::absolute(path)?),
                    args: Vec::new(),
                    ends: Ends::Exits(Ended::Status(0)),
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
                ends: Ends::Exits(run.workload.ends),
                role: Some(run.workload.role),
            }
        });
        let mut counted: Vec<Counted> = counted.collect();
        if let Some(pages) = &declared.pages {
            counted.extend(pages.page.iter().map(|page| Counted {
                scope: format!("pages/{}", page.name),
                source: match rig {
                    Ok(pages::Located::Absent(why)) => Source::Unfound(why.clone()),
                    Ok(pages::Located::Here(_)) | Err(_) => Source::Page(page.clone()),
                },
                args: Vec::new(),
                ends: Ends::Answers(ANSWERS),
                role: Some(pages.role),
            }));
        }
        Ok(counted)
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
    /// whose run ended as declared where `otherwise` is none, and as it
    /// says where it is one: a pass where it ended so, and a fail, its
    /// counts kept, where it did not. A run with no counts fails.
    fn written_into(
        &self,
        scope: &str,
        otherwise: Option<&str>,
        reading: &mut Reading,
    ) -> Result<()> {
        let oplines = self.oplines();
        if oplines == 0 {
            reading.status = Status::Fail;
            reading.message = format!(
                "{scope}: the engine wrote no counts, so it is not built with the bundle's `{FEATURE}` feature or the run ended before its request did"
            );
            if let Some(otherwise) = otherwise {
                write!(reading.message, "; {otherwise}").ok();
            }
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
        if let Some(otherwise) = otherwise {
            reading.status = Status::Fail;
            write!(said, "; {otherwise}, so its counts are out of the vote").ok();
        }
        reading.message = said;
        Ok(())
    }
}

/// The vote over the runs that vote: each variant met in one, with its
/// highest share in any of them and that run's scope, and each declared
/// run of a role that votes that did not vote, with its reading's status.
#[derive(Debug, Clone, Default, PartialEq)]
struct Vote {
    met: BTreeMap<String, (f64, String)>,
    voters: usize,
    missed: Vec<(String, Status)>,
}

impl Vote {
    /// `counted`, whose reading is `reading` and whose counts are
    /// `census` where it wrote any: cast where its role votes and its
    /// reading passed, and missed where its role votes and it did not,
    /// a skip or a fail.
    fn take(&mut self, counted: &Counted, reading: &Reading, census: Option<&Census>) {
        if !counted.votes() {
            return;
        }
        match (reading.status, census) {
            (Status::Pass, Some(census)) => self.cast(&counted.scope, census),
            (status, _) => self.missed.push((counted.scope.clone(), status)),
        }
    }

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
        reading.count("missed", self.missed.len() as u64)?;
        let listed = |names: &[&String]| {
            if names.is_empty() {
                "none".to_owned()
            } else {
                joined(names.iter().map(|name| shown(name)), ", ")
            }
        };
        let mut said = format!(
            "{} run{} of a role that votes voted {} variant{} over a quarter of a percent, against the build's {} placed by {} in {ARMS}; {} would enter: {}; {} would leave: {}",
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
        reading.status = if self.missed.is_empty() {
            Status::Pass
        } else {
            Status::Skip
        };
        if self.missed.is_empty() {
            said.push_str("; this informs and fails nothing");
        } else {
            let missed = (self.missed.iter())
                .map(|(scope, status)| format!("{scope} ({})", status.as_str()));
            write!(
                said,
                "; {} declared run{} of a role that votes did not vote: {}; so this is a vote over part of the criterion's runs, a skip, and what it says would enter or leave is unread",
                self.missed.len(),
                plural(self.missed.len()),
                joined(missed, ", ")
            )
            .ok();
        }
        reading.message = said;
        Ok(())
    }
}

/// `counts`, what `counted` dispatched and how it ended where it ended
/// otherwise than declared, written into its `reading` and taken by
/// `vote`: a run that could not be counted fails, naming why.
fn tallied(
    counted: &Counted,
    counts: Result<(Census, Option<String>)>,
    reading: &mut Reading,
    vote: &mut Vote,
) {
    let scope = &counted.scope;
    let written = counts.and_then(|(census, otherwise)| {
        census.written_into(scope, otherwise.as_deref(), reading)?;
        Ok(census)
    });
    let census = match written {
        Ok(census) => Some(census),
        Err(trouble) => {
            reading.status = Status::Fail;
            reading.message = format!("{scope}: {trouble:#}");
            None
        }
    };
    vote.take(counted, reading, census.as_ref());
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
    let rig = pages::located();
    let programs = ask.programs(&declared, &pin.source.tree(), &rig)?;
    let oracle = pin.oracle();
    let engine = engine(root, ask);
    let mut readings = Vec::new();
    let mut vote = Vote::default();
    // The rig is copied once, before the first page served.
    let mut copied: Option<Result<pages::Copied, String>> = None;
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
                vote.take(counted, &reading, None);
                readings.push(reading);
                continue;
            }
            (Err(why), _) => Err(anyhow::anyhow!("the counting build did not build: {why:#}")),
            (Ok(engine), Source::Page(page)) => {
                let copied = copied.get_or_insert_with(|| match &rig {
                    Ok(pages::Located::Here(rig)) => rig.copied().map_err(|e| format!("{e:#}")),
                    Ok(pages::Located::Absent(why)) => Err(why.clone()),
                    Err(why) => Err(format!("{why:#}")),
                });
                match (copied, &declared.pages) {
                    (Ok(copied), Some(declared)) => {
                        served(copied, engine, (declared, page), counted.ends)
                    }
                    (Err(why), _) => Err(anyhow::anyhow!("the rig: {why}")),
                    (Ok(_), None) => Err(anyhow::anyhow!("no pages are declared")),
                }
            }
            (Ok(engine), _) => counts(&oracle, engine, counted),
        };
        tallied(counted, counts, &mut reading, &mut vote);
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
/// [`MEMORY`], with the arguments it declares, and how its run ended
/// where it ended otherwise than declared.
fn counts(
    oracle: &Oracle<'_>,
    engine: &Path,
    counted: &Counted,
) -> Result<(Census, Option<String>)> {
    let program = match &counted.source {
        Source::File(path) => {
            ensure!(path.is_file(), "there is no program at {}", path.display());
            Program::File(path)
        }
        Source::Snippet(code) => Program::Snippet(code),
        Source::Page(page) => anyhow::bail!("the page {} is served, not run", page.path),
        Source::Unfound(why) => anyhow::bail!("{why}"),
    };
    let ini = [MEMORY.to_owned()];
    let ran = oracle.run_under(&Invocation {
        ini: &ini,
        args: &counted.args,
        ..Invocation::new(engine, program)
    })?;
    let otherwise = match counted.ends {
        Ends::Exits(ends) if ran.exit == ends => None,
        Ends::Exits(ends) => Some(format!(
            "the run ended on {} where its program declares {ends}",
            ran.exit
        )),
        Ends::Answers(_) => anyhow::bail!("a program ends on a status, not an answer"),
    };
    Ok((Census::read(&ran.stderr)?, otherwise))
}

/// The counts `page` dispatched on `engine`, served from the rig's copy
/// as `declared` says, and what it answered where it answered otherwise
/// than `ends`.
fn served(
    copied: &pages::Copied,
    engine: &Path,
    (declared, page): (&Pages, &Page),
    ends: Ends,
) -> Result<(Census, Option<String>)> {
    let (census, status) = copied.count(engine, declared, page)?;
    let otherwise = match ends {
        Ends::Answers(answers) if status == answers => None,
        Ends::Answers(answers) => Some(format!(
            "the page answered {status} where it should answer {answers}"
        )),
        Ends::Exits(_) => bail!("a page ends on an answer, not a status"),
    };
    Ok((census, otherwise))
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
        census
            .written_into("workloads/loop-empty", None, &mut reading)
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
    /// one that ended otherwise than declared fails, saying how, its
    /// counts kept and out of the vote; and one that ended as declared
    /// passes, as ruled 2026-11-09.
    #[test]
    fn a_run_fails_with_no_counts_or_an_end_other_than_its_declared() {
        let mut reading = Reading::new(INSTRUMENT, "workloads/fib");
        let otherwise = "the run ended on status 255 where its program declares status 0";
        Census::default()
            .written_into("workloads/fib", Some(otherwise), &mut reading)
            .unwrap();
        assert_eq!(reading.status, Status::Fail);
        assert!(
            reading.message.contains("`dispatch-census` feature")
                && reading.message.ends_with(otherwise),
            "{}",
            reading.message
        );
        let census = Census::read(b"dispatch-census Jmp 1\n").unwrap();
        census
            .written_into("workloads/fib", Some(otherwise), &mut reading)
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
            .written_into("workloads/fib", None, &mut declared)
            .unwrap();
        assert_eq!(declared.status, Status::Pass, "{}", declared.message);
    }

    /// A declaration of a program of each kind and one page.
    const DECLARED: &str = r#"
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

[pages]
first_read = 1
about = "x"
role = "bench"
warm = 3
ini = []
page = [{ name = "front", path = "/" }]
"#;

    /// A rig described as here.
    fn here() -> Result<pages::Located> {
        let described = "rig = \"/r\"\ncopy = [\"d\"]\nroot = \"d\"\nsnapshot = \"s\"\n\
                         database = \"d/b\"\nhost = \"h:1\"\n";
        let rig = pages::Rig::parse(described, Path::new("/m"))?;
        Ok(pages::Located::Here(rig))
    }

    /// Without a program named, the census counts every run the
    /// declaration yields: a file in place, a pinned file from the tree or
    /// a skip naming why it is not here, and code as a snippet, each with
    /// the arguments and the end it declares; a kernel votes, and a native
    /// and a probe do not.
    #[test]
    fn the_census_counts_every_declared_run_with_its_arguments() {
        let declared = Declaration::parse(DECLARED, Path::new("/w")).unwrap();
        let tree = PathBuf::from("/src");
        let programs = Ask::default()
            .programs(&declared, &Ok(tree), &here())
            .unwrap();
        let counted = |scope: &str, source, args: &[&str], ends, role| Counted {
            scope: scope.to_owned(),
            source,
            args: args.iter().map(|a| (*a).to_owned()).collect(),
            ends: Ends::Exits(Ended::Status(ends)),
            role: Some(role),
        };
        let file = |path: &str| Source::File(PathBuf::from(path));
        assert_eq!(
            programs[..4],
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
        let votes: Vec<bool> = programs[..4].iter().map(Counted::votes).collect();
        assert_eq!(votes, [true, false, false, false]);
        let absent = Ask::default()
            .programs(&declared, &Err(anyhow::anyhow!("gone")), &here())
            .unwrap();
        assert!(matches!(&absent[2].source, Source::Unfound(why) if why.contains("gone")));
    }

    /// After the programs, each page is counted, served from the rig, held
    /// to answer 200 and voting as a bench's, or a skip saying why where
    /// no rig is here. A program named is counted alone, scoped by its
    /// stem, held to end on 0 and voting nothing.
    #[test]
    fn each_page_is_counted_from_the_rig_and_a_named_program_alone() {
        let declared = Declaration::parse(DECLARED, Path::new("/w")).unwrap();
        let tree = Ok(PathBuf::from("/src"));
        let programs = Ask::default().programs(&declared, &tree, &here()).unwrap();
        let front = Page {
            name: "front".to_owned(),
            path: "/".to_owned(),
        };
        assert_eq!(
            programs[4..],
            [Counted {
                scope: "pages/front".to_owned(),
                source: Source::Page(front),
                args: Vec::new(),
                ends: Ends::Answers(200),
                role: Some(Role::Bench),
            }]
        );
        assert!(programs[4].votes());
        let nowhere = Ok(pages::Located::Absent("no rig".to_owned()));
        let absent = Ask::default().programs(&declared, &tree, &nowhere).unwrap();
        assert!(matches!(&absent[4].source, Source::Unfound(why) if why == "no rig"));
        let named = Ask {
            program: vec![PathBuf::from("/w/wordpress.php")],
            ..Ask::default()
        };
        let named = named.programs(&declared, &tree, &nowhere).unwrap();
        assert_eq!(
            named,
            [Counted {
                scope: "program/wordpress".to_owned(),
                source: Source::File(PathBuf::from("/w/wordpress.php")),
                args: Vec::new(),
                ends: Ends::Exits(Ended::Status(0)),
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

    /// A declared run of a role that votes is cast where its reading
    /// passes, and named as missed where it is a skip, a page with no rig
    /// here, or a fail, a page that answered 301 or a run that could not
    /// be counted; a run whose role votes nothing is neither. With one
    /// missed the vote's reading is a skip naming each, and with none a
    /// pass.
    #[test]
    fn the_vote_names_each_declared_voter_that_did_not_vote() {
        let run = |scope: &str, role| Counted {
            scope: scope.to_owned(),
            source: Source::Unfound(String::new()),
            args: Vec::new(),
            ends: Ends::Answers(ANSWERS),
            role: Some(role),
        };
        let counts = |otherwise: Option<&str>| -> Result<(Census, Option<String>)> {
            let census = Census::read(b"dispatch-census Jmp 4\n").unwrap();
            Ok((census, otherwise.map(str::to_owned)))
        };
        let mut vote = Vote::default();
        let mut take = |counted: &Counted, counts| {
            let mut reading = Reading::new(INSTRUMENT, &counted.scope);
            tallied(counted, counts, &mut reading, &mut vote);
            reading.status
        };
        let fib = run("workloads/fib", Role::Kernel);
        assert_eq!(take(&fib, counts(None)), Status::Pass);
        let answered = Some("the page answered 301 where it should answer 200");
        let post = run("pages/post", Role::Bench);
        assert_eq!(take(&post, counts(answered)), Status::Fail);
        let login = run("pages/login", Role::Bench);
        let gone = Err(anyhow::anyhow!("the rig: gone"));
        assert_eq!(take(&login, gone), Status::Fail);
        let shapes = run("workloads/shapes", Role::Shape);
        assert_eq!(take(&shapes, counts(answered)), Status::Fail);
        let front = run("pages/front", Role::Bench);
        let mut skipped = Reading::new(INSTRUMENT, "pages/front");
        skipped.message = "pages/front: nothing ran; no rig".to_owned();
        vote.take(&front, &skipped, None);
        assert_eq!(vote.voters, 1);
        let placed: BTreeSet<String> = ["Jmp".to_owned()].into();
        let mut reading = Reading::new(INSTRUMENT, VOTE);
        vote.written_into(&placed, &mut reading).unwrap();
        assert_eq!(reading.status, Status::Skip);
        assert_eq!(reading.int("voters"), Some(1));
        assert_eq!(reading.int("missed"), Some(3));
        assert!(
            reading.message.contains(
                "; 3 declared runs of a role that votes did not vote: pages/post (fail), pages/login (fail), pages/front (skip); "
            ),
            "{}",
            reading.message
        );
        let mut whole = Vote::default();
        whole.cast(
            "workloads/fib",
            &Census::read(b"dispatch-census Jmp 4\n").unwrap(),
        );
        let mut reading = Reading::new(INSTRUMENT, VOTE);
        whole.written_into(&placed, &mut reading).unwrap();
        assert_eq!(reading.status, Status::Pass);
        assert_eq!(reading.int("missed"), Some(0));
        assert!(
            reading
                .message
                .ends_with("; this informs and fails nothing")
        );
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
