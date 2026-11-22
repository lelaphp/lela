//! The packages lane's logic (spec #32, ruled on #69): how one pinned
//! Composer package's test suite is run under the oracle and under Lela,
//! and what the two runs come to. The lane plans and the adapter runs:
//! this crate holds the steps of one package's run, the reading of its
//! runner's log ([`cases`]) and the matching of its tests across the two
//! runs ([`matched`]), and reaches the world only through a [`Shell`] the
//! adapter, `xtask/src/packages`, implements under xtask's own process
//! limits. So its tests read a plan and a scripted shell's answers, with
//! no network and no process. It depends on no workspace crate.
//!
//! **One package's run.** The source is fetched at the commit the list
//! pins and held to the digest the list pins for it, then the lock of its
//! development dependencies to that lock's digest, so a run takes nothing
//! it has not pinned; a digest that differs is refused before anything of
//! the package is installed or run. The lock is laid in as
//! `composer.lock` and installed once, under the oracle's php, and the
//! suite runs through the package's own runner once under the oracle and
//! once under Lela, each writing an XML log. Every install and every
//! suite is given the services' fixed addresses, which the adapter reads
//! from the one file the nightly job starts the services ([`Service`]) by.
//!
//! **One package's proposal** ([`propose`]). The source is fetched at the
//! commit the registry names for the version and digested before anything
//! writes in it; the lock of its development dependencies is resolved at
//! that version under the oracle's php, kept where the list will name it
//! and digested; and what it requires says which services its suite needs
//! and which runner it has.
//!
//! **The denominator** (R10). The tests the oracle passes are the found
//! set. A test is met when the oracle passes it and Lela passes it; one
//! the oracle fails, errors or skips is outside, a number and never met;
//! one Lela's run does not report, as when a crash ends its suite, is
//! unreported, which misses as a failure does. A suite that reports no
//! test under the oracle did not run there, and the package is left out,
//! never counted as passing; so is one the list marks as needing what the
//! lane does not provide.

#![forbid(unsafe_code)]

mod junit;
#[cfg(test)]
mod tests;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

pub use junit::{Case, Outcome, cases};

/// Which php a step runs under.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Php {
    /// The pinned php.
    Oracle,
    /// Lela's.
    Engine,
}

/// What a step runs: the adapter says where each is on this machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Program {
    Git,
    /// Composer, run under the oracle's php.
    Composer,
    /// A php, with the oracle's recipe before the step's arguments.
    Php(Php),
}

/// One program run: its arguments, the directory it runs in, and the
/// environment it is given over the adapter's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    pub program: Program,
    pub args: Vec<String>,
    pub dir: PathBuf,
    pub env: Vec<(String, String)>,
}

/// One thing a package's run asks of the world.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Run a program, answered [`Came::Exited`].
    Run(Command),
    /// The digest of a file's bytes, or of a tree's files, `.git` left
    /// out, spelt `sha256:<hex>`: answered [`Came::Digest`].
    Digest(PathBuf),
    /// Copy a file, answered [`Came::Exited`] with 0.
    Copy { from: PathBuf, to: PathBuf },
    /// A file's text, answered [`Came::Text`], none where it is not there.
    Read(PathBuf),
}

/// What a step came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Came {
    /// The program's exit status, none where a signal or the limit ended
    /// it, and what it said last.
    Exited(Option<i32>, String),
    Digest(String),
    Text(Option<String>),
    /// The shell could not take the step, and why.
    Failed(String),
}

/// The world as the lane reaches it: the adapter's, or a test's script.
pub trait Shell {
    fn run(&mut self, step: &Step) -> Came;
}

/// The services the lane provides, each at a fixed address: the nightly
/// job starts them from compose files kept in the tree (R11), and every
/// suite is given all of their addresses, whatever the package needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Service {
    Mysql,
    Postgres,
    Redis,
}

impl Service {
    pub const ALL: [Service; 3] = [Service::Mysql, Service::Postgres, Service::Redis];

    /// What the list names it by in a package's `needs`.
    pub fn name(self) -> &'static str {
        match self {
            Service::Mysql => "mysql",
            Service::Postgres => "postgres",
            Service::Redis => "redis",
        }
    }

    /// The service the list's `need` names, none where the lane provides
    /// no such service.
    fn named(need: &str) -> Option<Service> {
        Service::ALL.into_iter().find(|s| s.name() == need)
    }
}

/// One package as the list pins it, what its run needs of the row.
#[derive(Debug, Clone, Copy)]
pub struct Package<'a> {
    /// Where its source is fetched from, a git repository.
    pub repository: &'a str,
    pub commit: &'a str,
    /// Its source's digest at that commit, as [`Step::Digest`] spells one.
    pub digest: &'a str,
    /// The lock of its development dependencies, and that lock's digest.
    pub lock: &'a Path,
    pub lock_digest: &'a str,
    /// The runner, a path in the source run under each php, which writes
    /// an XML log where `--log-junit` says, as `phpunit`'s and Pest's do.
    pub runner: &'a str,
    /// The runner's arguments before the log's.
    pub args: &'a [String],
    /// The services its suite needs, by [`Service::name`].
    pub needs: &'a [String],
    /// The services' addresses, the environment every install and suite
    /// is given.
    pub env: &'a [(String, String)],
}

/// What one package's run came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Not judged, and why: a step the run could not take, or a source or
    /// lock refused for a digest that is not the pin's.
    Unjudged(String),
    /// Left out by rule, and why: its suite did not run under the oracle,
    /// or the list marks it as needing what the lane does not provide.
    LeftOut(String),
    Judged(Matched),
}

/// A package's tests across the two runs: each the oracle passed, as Lela's
/// run held it, by id, and how many the oracle did not pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Matched {
    pub found: Vec<Test>,
    pub outside: u64,
}

/// One test the oracle passed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Test {
    pub id: String,
    pub held: Held,
    /// What Lela's run said of it where it was not met.
    pub note: String,
}

/// How Lela's run held a test the oracle passed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Held {
    Met,
    /// Lela's run failed, errored or skipped it.
    Missed,
    /// Lela's run did not report it.
    Unreported,
}

/// The flag a runner is told where to write its XML log by.
const LOG: &str = "--log-junit";

/// Run `package` in `work`, a directory of its own, through `shell`.
pub fn run(package: &Package<'_>, work: &Path, shell: &mut impl Shell) -> Verdict {
    if let Some(need) = (package.needs.iter()).find(|need| Service::named(need).is_none()) {
        return Verdict::LeftOut(format!(
            "the list marks it as needing {need}, which the lane does not provide"
        ));
    }
    taken(package, work, shell).unwrap_or_else(Verdict::Unjudged)
}

/// The steps of [`run`], the first that could not be taken ending it.
fn taken(package: &Package<'_>, work: &Path, shell: &mut impl Shell) -> Result<Verdict, String> {
    let source = fetched(package.repository, package.commit, work, shell)?;
    held(shell, &source, package.digest, "its source")?;
    held(shell, package.lock, package.lock_digest, "its lock")?;
    let lock = source.join("composer.lock");
    let laid = Step::Copy {
        from: package.lock.to_path_buf(),
        to: lock,
    };
    exited(shell, &laid)?;
    let install = ["install", "--no-interaction", "--no-progress"];
    exited(
        shell,
        &command(Program::Composer, &install, &source, package.env),
    )?;
    let (theirs, said) = suite(package, Php::Oracle, work, shell)?;
    if theirs.is_empty() {
        return Ok(Verdict::LeftOut(format!(
            "its suite did not run under the oracle: no test reported, {said}"
        )));
    }
    let (ours, _) = suite(package, Php::Engine, work, shell)?;
    Ok(Verdict::Judged(matched(&theirs, &ours)))
}

/// `repository` fetched at `commit` into `work`'s `source`, the tree it
/// returns.
fn fetched(
    repository: &str,
    commit: &str,
    work: &Path,
    shell: &mut impl Shell,
) -> Result<PathBuf, String> {
    let source = work.join("source");
    let git = |args: &[&str], dir: &Path| command(Program::Git, args, dir, &[]);
    exited(shell, &git(&["init", "-q", "source"], work))?;
    let fetch = ["fetch", "-q", "--depth", "1", repository, commit];
    exited(shell, &git(&fetch, &source))?;
    let checkout = ["-c", "advice.detachedHead=false", "checkout", "-q", commit];
    exited(shell, &git(&checkout, &source))?;
    Ok(source)
}

/// A package the registry names at one version, as a proposal pins it.
#[derive(Debug, Clone, Copy)]
pub struct Candidate<'a> {
    /// Its source's git repository and the commit of the version.
    pub repository: &'a str,
    pub commit: &'a str,
    pub version: &'a str,
    /// Where the lock the proposal resolves is kept, beside the list.
    pub lock: &'a Path,
    /// What it requires, its own dependencies and its suite's, by name.
    pub requires: &'a [String],
}

/// What a proposal pins a candidate by beyond what the registry says.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proposed {
    /// Its source's digest at the commit, and its lock's.
    pub digest: String,
    pub lock_digest: String,
    /// Its runner where it is not `phpunit`.
    pub runner: Option<&'static str>,
    /// The services its suite needs, each once, in order.
    pub needs: Vec<&'static str>,
}

/// The clients of a service a package can require, each with the service
/// it reaches: a package that requires one has a suite that talks to it.
const CLIENTS: [(&str, &str); 13] = [
    ("ext-mysqli", "mysql"),
    ("ext-pdo_mysql", "mysql"),
    ("ext-pgsql", "postgres"),
    ("ext-pdo_pgsql", "postgres"),
    ("ext-redis", "redis"),
    ("predis/predis", "redis"),
    ("ext-memcached", "memcached"),
    ("ext-memcache", "memcached"),
    ("ext-mongodb", "mongodb"),
    ("mongodb/mongodb", "mongodb"),
    ("elasticsearch/elasticsearch", "elasticsearch"),
    ("ext-amqp", "rabbitmq"),
    ("php-amqplib/php-amqplib", "rabbitmq"),
];

/// Propose `candidate`, working in `work`, a directory of its own: the
/// first step that could not be taken ends it, and says why.
pub fn propose(
    candidate: &Candidate<'_>,
    work: &Path,
    shell: &mut impl Shell,
) -> Result<Proposed, String> {
    let source = fetched(candidate.repository, candidate.commit, work, shell)?;
    let digest = digested(shell, &source)?;
    let update = [
        "update",
        "--no-install",
        "--no-interaction",
        "--no-progress",
        "--no-plugins",
        "--no-scripts",
    ];
    let root = [(
        "COMPOSER_ROOT_VERSION".to_owned(),
        candidate.version.to_owned(),
    )];
    exited(shell, &command(Program::Composer, &update, &source, &root))?;
    let kept = Step::Copy {
        from: source.join("composer.lock"),
        to: candidate.lock.to_path_buf(),
    };
    exited(shell, &kept)?;
    let requires = |name: &str| candidate.requires.iter().any(|r| r == name);
    let mut needs: Vec<&str> = (CLIENTS.iter())
        .filter(|(client, _)| requires(client))
        .map(|(_, service)| *service)
        .collect();
    needs.sort_unstable();
    needs.dedup();
    Ok(Proposed {
        digest,
        lock_digest: digested(shell, candidate.lock)?,
        runner: requires("pestphp/pest").then_some("vendor/bin/pest"),
        needs,
    })
}

/// `program` with `args` in `dir`, given `env`.
fn command(program: Program, args: &[&str], dir: &Path, env: &[(String, String)]) -> Step {
    Step::Run(Command {
        program,
        args: args.iter().map(|a| (*a).to_owned()).collect(),
        dir: dir.to_path_buf(),
        env: env.to_vec(),
    })
}

/// `step` taken, and refused unless it exited 0.
fn exited(shell: &mut impl Shell, step: &Step) -> Result<(), String> {
    match shell.run(step) {
        Came::Exited(Some(0), _) => Ok(()),
        came => Err(format!("{} came to {}", worded(step), said(&came))),
    }
}

/// The digest of `path` held to `pinned`: a source or lock that differs
/// is refused.
fn held(shell: &mut impl Shell, path: &Path, pinned: &str, what: &str) -> Result<(), String> {
    match digested(shell, path)? {
        digest if digest == pinned => Ok(()),
        digest => Err(format!(
            "refused: {what}'s digest is {digest}, and the list pins {pinned}"
        )),
    }
}

/// The digest of `path`.
fn digested(shell: &mut impl Shell, path: &Path) -> Result<String, String> {
    let step = Step::Digest(path.to_path_buf());
    match shell.run(&step) {
        Came::Digest(digest) => Ok(digest),
        came => Err(format!("{} came to {}", worded(&step), said(&came))),
    }
}

/// The suite run under `php`, and the cases its log reports, none where
/// it wrote no log; and how its run ended, in words.
fn suite(
    package: &Package<'_>,
    php: Php,
    work: &Path,
    shell: &mut impl Shell,
) -> Result<(Vec<Case>, String), String> {
    let log = work.join(match php {
        Php::Oracle => "oracle.xml",
        Php::Engine => "engine.xml",
    });
    let log_path = log.display().to_string();
    let args: Vec<&str> = std::iter::once(package.runner)
        .chain(package.args.iter().map(String::as_str))
        .chain([LOG, log_path.as_str()])
        .collect();
    let step = command(Program::Php(php), &args, &work.join("source"), package.env);
    let ended = match shell.run(&step) {
        came @ Came::Exited(..) => said(&came),
        came => return Err(format!("{} came to {}", worded(&step), said(&came))),
    };
    let read = Step::Read(log);
    match shell.run(&read) {
        Came::Text(text) => Ok((text.as_deref().map(cases).unwrap_or_default(), ended)),
        came => Err(format!("{} came to {}", worded(&read), said(&came))),
    }
}

/// A step, as a message names it.
fn worded(step: &Step) -> String {
    match step {
        Step::Run(c) => format!("{:?} {}", c.program, c.args.join(" ")),
        Step::Digest(path) => format!("the digest of {}", path.display()),
        Step::Copy { from, to } => format!("copying {} to {}", from.display(), to.display()),
        Step::Read(path) => format!("reading {}", path.display()),
    }
}

/// What a step came to, as a message says it.
fn said(came: &Came) -> String {
    match came {
        Came::Exited(Some(code), tail) => format!("exit {code}{}", tailed(tail)),
        Came::Exited(None, tail) => format!("a signal or the limit{}", tailed(tail)),
        Came::Digest(digest) => format!("the digest {digest}"),
        Came::Text(_) => "a file's text".to_owned(),
        Came::Failed(why) => why.clone(),
    }
}

/// The last line a program said, after a colon; nothing where it said none.
fn tailed(tail: &str) -> String {
    tail.lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .map(|l| format!(": {}", l.trim()))
        .unwrap_or_default()
}

/// Each test the oracle passed, as Lela's run held it, in the ids' order;
/// and how many the oracle did not pass. A test reported more than once
/// stands at its worst outcome on either side.
pub fn matched(theirs: &[Case], ours: &[Case]) -> Matched {
    let (theirs, ours) = (worst(theirs), worst(ours));
    let mut found = Vec::new();
    let mut outside = 0;
    for (id, case) in theirs {
        if case.outcome != Outcome::Passed {
            outside += 1;
            continue;
        }
        let (held, note) = match ours.get(id) {
            None => (Held::Unreported, "Lela's run did not report it".to_owned()),
            Some(ours) if ours.outcome == Outcome::Passed => (Held::Met, String::new()),
            Some(ours) => (
                Held::Missed,
                format!("{}: {}", ours.outcome.as_str(), ours.message),
            ),
        };
        found.push(Test {
            id: id.to_owned(),
            held,
            note,
        });
    }
    Matched { found, outside }
}

/// Each id's worst outcome.
fn worst(cases: &[Case]) -> BTreeMap<&str, &Case> {
    let mut by_id: BTreeMap<&str, &Case> = BTreeMap::new();
    for case in cases {
        let kept = by_id.entry(&case.id).or_insert(case);
        if case.outcome > kept.outcome {
            *kept = case;
        }
    }
    by_id
}
