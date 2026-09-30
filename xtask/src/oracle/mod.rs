//! The oracle module (harness-design.md §4.3, §8; design §8): the pinned
//! `php`, its recipe, and the two functions every comparison goes through.
//! [`Oracle::run`] executes a program under the recipe and a regime and
//! returns what it wrote, normalised; `compare` gives the verdict, consulting
//! the divergence registry, so no instrument diffs on its own. The pin's
//! grammar is this module's: `oracle.toml` is refused on a key outside it.
//!
//! **What a lane holds.** Everything that touches the pin lives here, so
//! the three lanes, `phpt`, `witness` and `compile-identity`, share this
//! module and none imports another. An [`Oracle`] is the pin for the
//! length of one run, its digest asserted once before the first program
//! runs and never per program; a [`Runner`] is what the lane measured the
//! engine with, or why it could not; [`Source::tree`] is where the pinned
//! php-src is on this machine, the tree [`TREE_VARIABLE`] names first and
//! the pin's path second, which the runner and the tests that resolve a
//! subset ask and never read off the pin themselves; [`Promise`] is what a
//! runner says this machine has of the pin, so a test that would skip for
//! a fact the machine has not fails instead where the fact was promised
//! (#38); and [`expect`] is how an expectation that is a pattern rather
//! than bytes is met, which `verdict` takes as the caller's match.
//!
//! The version, the digest and the path are values of the pin and appear in
//! no source file (§10 rule 5).

pub(crate) mod expect;
pub(crate) mod linked;

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fmt;
use std::fs::{self, File};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::divergence::{Divergence, Registry};
use crate::scratch::Tree;

/// The environment variable that names the pinned php-src tree on this
/// machine, consulted ahead of the pin's `[source].path` (#36). The name
/// is upstream's own, what `run-tests.php:179` at the tag reads for the
/// same tree, and it is stated here and nowhere else: the messages print
/// it from here, a test hands its value to [`Pin::on_this_machine`], and
/// a runner that provisions the tree sets it (#37). The location, alone
/// among the pin's facts, is machine-local; the tag, the commit and the
/// file count are the file's.
pub const TREE_VARIABLE: &str = "TEST_PHP_SRCDIR";

/// The environment variable a runner sets to say what of the pin this
/// machine has (#38): a whitespace-separated list of the words
/// [`Promise::word`] gives, `tree` for the pinned php-src and `oracle` for
/// the pinned php. A test that would skip for a fact the machine has not,
/// saying so on stderr, fails instead where the fact was promised, naming
/// what it looked for; so a runner whose provisioning broke turns red
/// rather than quietly measuring less. The name is stated here and
/// nowhere else; the workflow reads it, beside [`TREE_VARIABLE`], off
/// `xtask status --section oracle` and sets it under that name (#36).
pub const PROMISE_VARIABLE: &str = "XTASK_PROMISED";

/// One fact of the pin a machine can promise to have: the tree or the
/// binary. What a guarded test asks through [`Promise::or_skip`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Promise {
    /// The pinned php-src, where [`Source::tree`] answers.
    Tree,
    /// The pinned php, at [`Binary::file`].
    Oracle,
}

impl Promise {
    const ALL: [Promise; 2] = [Promise::Tree, Promise::Oracle];

    /// The word [`PROMISE_VARIABLE`] lists for this fact.
    pub fn word(self) -> &'static str {
        match self {
            Promise::Tree => "tree",
            Promise::Oracle => "oracle",
        }
    }

    /// What every guarded test asks (#38): `found` when it is, and
    /// otherwise `None` after saying so on stderr, as the tests skip
    /// today, unless this machine promised the fact through
    /// [`PROMISE_VARIABLE`], in which case the error names what was
    /// looked for and the promise. Reads the variable off the process;
    /// [`Promised::or_skip`] is the same decision over a value handed in.
    pub fn or_skip<T>(self, found: Result<T>) -> Result<Option<T>> {
        Promised::load()?.or_skip(self, found)
    }
}

/// What this machine promised of the pin: the words [`PROMISE_VARIABLE`]
/// lists, as [`Promised::load`] read them or a test handed them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Promised(Vec<Promise>);

impl Promised {
    /// The promise as this machine makes it: [`PROMISE_VARIABLE`] in the
    /// process's environment, nothing promised where it is unset.
    pub fn load() -> Result<Promised> {
        match std::env::var_os(PROMISE_VARIABLE) {
            Some(value) => Promised::parse(&value.to_string_lossy()),
            None => Ok(Promised::default()),
        }
    }

    /// The promise as a value of the variable says it: each
    /// whitespace-separated word one of [`Promise::word`]'s. A word
    /// outside them is refused naming it and the words the variable
    /// takes, so a misspelt promise is never an unmade one.
    pub fn parse(value: &str) -> Result<Promised> {
        let mut promised = Vec::new();
        for word in value.split_whitespace() {
            let Some(promise) = Promise::ALL.into_iter().find(|p| p.word() == word) else {
                bail!(
                    "{PROMISE_VARIABLE} lists {word:?}, which promises nothing; the words it takes are {}",
                    Promise::ALL.map(Promise::word).join(" and ")
                );
            };
            if !promised.contains(&promise) {
                promised.push(promise);
            }
        }
        Ok(Promised(promised))
    }

    /// Whether this machine promised the fact.
    pub fn holds(&self, promise: Promise) -> bool {
        self.0.contains(&promise)
    }

    /// [`Promise::or_skip`] over this promise: `found` when it is;
    /// otherwise `None` after saying so on stderr, or an error naming
    /// what was looked for and the promise where the fact was promised.
    pub fn or_skip<T>(&self, promise: Promise, found: Result<T>) -> Result<Option<T>> {
        match found {
            Ok(found) => Ok(Some(found)),
            Err(absent) if self.holds(promise) => Err(anyhow::anyhow!(
                "{absent:#}; {PROMISE_VARIABLE} promised the {}",
                promise.word()
            )),
            Err(absent) => {
                eprintln!("{absent}; skipping");
                Ok(None)
            }
        }
    }
}

/// `oracle.toml`: the pinned oracle and its invocation recipe, read through
/// the manifest's `[oracle]` row and nowhere else.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pin {
    pub oracle: Binary,
    pub source: Source,
    pub recipe: Recipe,
    #[serde(default)]
    pub regime: BTreeMap<String, Regime>,
    /// `[sapi]`: the oracle's other hosts beside `php`, pinned from the
    /// same keg (design §1, #1161); absent until a person writes the row.
    #[serde(default)]
    pub sapi: Hosts,
    /// How many times the digest was asserted through this pin; not a
    /// value of the file.
    #[serde(skip)]
    asserted: Asserted,
}

/// `[sapi]`: the binaries upstream's run-tests.php finds beside `php` for
/// `sapi/cgi`, `sapi/fpm` and `sapi/phpdbg` (`get_binary`, run-tests.php at
/// the tag; php-fpm by `FPM\Tester::findExecutable`), each by path and
/// digest as `[oracle]` pins `php`. What asserts them before a run that
/// hosts a test through them is #1184's.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hosts {
    pub cgi: Option<Host>,
    pub fpm: Option<Host>,
    pub phpdbg: Option<Host>,
}

/// One of `[sapi]`'s binaries: its path on this machine and its digest.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Host {
    pub path: PathBuf,
    pub md5: String,
}

/// A count of digest assertions: disclosure, so a test can hold a lane to
/// one per run rather than one per program.
#[derive(Debug, Default)]
struct Asserted(AtomicU64);

impl Clone for Asserted {
    fn clone(&self) -> Asserted {
        Asserted(AtomicU64::new(self.0.load(Ordering::Relaxed)))
    }
}

/// `[oracle]`: the `php` binary by version, path on this machine, and digest.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Binary {
    pub version: String,
    pub path: PathBuf,
    pub md5: String,
}

impl Binary {
    /// The pinned php on this machine: its path when a file is there, and
    /// otherwise an error naming the path and the version. What a test
    /// that spawns php asks through [`Promise::or_skip`].
    pub fn file(&self) -> Result<&Path> {
        if self.path.is_file() {
            Ok(&self.path)
        } else {
            bail!(
                "the pinned php {} is not at {}",
                self.version,
                self.path.display()
            )
        }
    }
}

/// `[source]`: the php-src tree at the same tag.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Source {
    pub tag: String,
    pub commit: String,
    /// Where the file says the tree is on the maintainer's machine; read
    /// through [`Source::tree`] and nowhere else, and stated by
    /// [`Source::path`] for the renderer.
    path: PathBuf,
    pub phpt_files: u64,
    /// The tree [`TREE_VARIABLE`] names on this machine, as [`Pin::load`]
    /// read it or a test handed it; not a value of the file, and absent
    /// under [`Pin::parse`], which is the pin as the file says it.
    #[serde(skip)]
    named: Option<PathBuf>,
}

impl Source {
    /// Where the pinned php-src tree is on this machine: the directory
    /// [`TREE_VARIABLE`] names when it is one, else the pin's path when it
    /// is one, held to the pin's commit. The one answer the phpt runner
    /// and the tests that resolve a subset take, so the location, alone
    /// among the pin's facts, is machine-local (#36). An absent tree is an
    /// error naming both places tried, so a runner's failure reads as an
    /// unset variable and not as a path that could never have existed
    /// there. A tree that is a checkout at another commit is refused
    /// naming both revisions and is never passed over for the other place
    /// (#37): a corpus at the wrong revision measures the wrong thing,
    /// more quietly than no corpus at all. A tree that is no checkout, a
    /// tarball unpacked at the tag, states no commit and is taken at its
    /// word.
    pub fn tree(&self) -> Result<PathBuf> {
        if let Some(named) = &self.named
            && named.is_dir()
        {
            self.held(named, &format!("named by {TREE_VARIABLE}"))?;
            return Ok(named.clone());
        }
        if self.path.is_dir() {
            self.held(&self.path, "the pin's path")?;
            return Ok(self.path.clone());
        }
        let variable = match &self.named {
            Some(named) => format!("{TREE_VARIABLE}={} is not a directory", named.display()),
            None => format!("{TREE_VARIABLE} is unset"),
        };
        bail!(
            "the pinned php-src ({}) is at neither place tried: {variable}, and the pin's path {} is not a directory",
            self.tag,
            self.path.display()
        )
    }
}

impl Source {
    /// Where the pin says the tree is, as the file states it: what
    /// `status --section oracle` renders beside [`Source::named`], never
    /// what a lane reads the tree from, which is [`Source::tree`].
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// What [`TREE_VARIABLE`] names on this machine, as [`Pin::load`] read
    /// it or a test handed it; none where it is unset or empty.
    pub fn named(&self) -> Option<&Path> {
        self.named.as_deref()
    }

    /// Hold a checkout at `tree` to the pin's commit: `HEAD` there must
    /// begin with `[source].commit`, which the pin states abbreviated.
    /// The tree is a checkout when it has a `.git`, a directory or a
    /// worktree's file; git is asked in the tree itself, so a scratch
    /// directory inside some other repository is not read as that
    /// repository's checkout. `how` says which place the tree was found
    /// through, so the refusal names it beside both revisions.
    fn held(&self, tree: &Path, how: &str) -> Result<()> {
        if !checkout(tree) {
            return Ok(());
        }
        let head = crate::git::git(tree, &["rev-parse", "--verify", "HEAD^{commit}"], None)
            .with_context(|| {
                format!(
                    "reading the commit of the php-src checkout at {}",
                    tree.display()
                )
            })?;
        let head = head.trim();
        if self.commit.is_empty() || !head.starts_with(&self.commit) {
            bail!(
                "the php-src checkout at {} ({how}) is at commit {head}; the pin says {} ({}). Lay the tree down at the pin's tag, or name another with {TREE_VARIABLE}; the pin moves only by a person's hand",
                tree.display(),
                self.commit,
                self.tag
            );
        }
        Ok(())
    }

    /// The files the pinned tree at `tree` holds, relative to it, as the
    /// tree is held to the pin: in a checkout, the files git tracks there,
    /// so what a run or a person left standing in it is not the pin's
    /// (#1183); in a tree that is no checkout, every file, taken at its
    /// word as its commit is.
    pub(crate) fn files(tree: &Path) -> Result<Vec<PathBuf>> {
        if checkout(tree) {
            let listed = crate::git::git(tree, &["ls-files", "-z"], None)
                .with_context(|| format!("listing the php-src checkout at {}", tree.display()))?;
            return Ok(listed
                .split('\0')
                .filter(|file| !file.is_empty())
                .map(PathBuf::from)
                .collect());
        }
        let mut found = Vec::new();
        crate::tally::files_under(tree, &mut found)
            .with_context(|| format!("listing the php-src tree at {}", tree.display()))?;
        Ok(found
            .into_iter()
            .filter_map(|file| file.strip_prefix(tree).ok().map(Path::to_path_buf))
            .collect())
    }
}

/// Whether the tree at `tree` is a checkout: it has a `.git`, a directory
/// or a worktree's file.
fn checkout(tree: &Path) -> bool {
    tree.join(".git").exists()
}

/// `[recipe]`: how a program is run and how its streams are normalised.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Recipe {
    pub script_name: String,
    pub preamble: String,
    pub args: Vec<String>,
    pub ini: Vec<String>,
    pub env_clear: bool,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    pub limits: Limits,
    pub normalise: Normalise,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    pub timeout_s: u64,
}

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Normalise {
    pub crlf_to_lf: bool,
    pub trim: String,
    pub scratch_token: String,
    /// The token that stands where the digits of `extension #<digits>`
    /// stood, the module's load number in `php --re`'s first line, which
    /// is the oracle's build order and never the bundle's (#317). Absent
    /// from the pin, the number is compared as any other byte.
    #[serde(default)]
    pub extension_number_token: Option<String>,
}

/// `[regime.<name>]`: the compiled tier's regimes, `-d` overrides added
/// after the recipe's ini.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Regime {
    pub ini: Vec<String>,
}

impl Pin {
    /// Load the pin at `path` as this machine has it: the file's facts,
    /// and the tree [`TREE_VARIABLE`] names in the process's environment.
    pub fn load(path: &Path) -> Result<Pin> {
        let text = fs::read_to_string(path)
            .with_context(|| format!("reading the oracle pin {}", path.display()))?;
        let pin = Pin::parse(&text).with_context(|| format!("{}", path.display()))?;
        // An empty value is unset, as `getenv` reads it in run-tests.php.
        let named = std::env::var_os(TREE_VARIABLE)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from);
        Ok(pin.on_this_machine(named))
    }

    /// Parse the pin's text and hold it to the grammar. The pin is as the
    /// file says it, blind to this machine; `load` consults the machine.
    pub fn parse(text: &str) -> Result<Pin> {
        Ok(toml::from_str(text)?)
    }

    /// The pin with the tree [`TREE_VARIABLE`] names on this machine:
    /// `load` hands the process's value, and a test hands the tree it laid
    /// down, which [`Source::tree`] then answers ahead of the pin's path.
    #[must_use]
    pub fn on_this_machine(mut self, named: Option<PathBuf>) -> Pin {
        self.source.named = named;
        self
    }

    /// The regime by name; an unknown name is refused beside the known ones.
    pub fn regime(&self, name: &str) -> Result<&Regime> {
        self.regime.get(name).with_context(|| {
            format!(
                "no regime {name:?} in the pin; the regimes are {}",
                self.regime
                    .keys()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })
    }

    /// The pin as one run holds it: the digest is asserted before the
    /// first program runs through the value and never again, so a lane of
    /// any length hashes the binary once. Nothing runs under the pin
    /// except through this.
    pub fn oracle(&self) -> Oracle<'_> {
        Oracle {
            pin: self,
            digest: OnceLock::new(),
        }
    }

    /// How many times the digest was asserted through this pin: one per
    /// run is the rule, and a test reads it.
    pub fn digests_asserted(&self) -> u64 {
        self.asserted.0.load(Ordering::Relaxed)
    }

    /// The binary's digest against the pin's; a mismatch refuses the run.
    fn assert_digest(&self) -> Result<()> {
        self.asserted.0.fetch_add(1, Ordering::Relaxed);
        let path = &self.oracle.path;
        let bytes = fs::read(path)
            .with_context(|| format!("reading the pinned php at {}", path.display()))?;
        let actual = format!("{:x}", md5::compute(bytes));
        if actual != self.oracle.md5 {
            bail!(
                "{} has md5 {actual}; the pin says {} for php {}. Report the drift; the pin moves only by a person's hand",
                path.display(),
                self.oracle.md5,
                self.oracle.version
            );
        }
        Ok(())
    }
}

/// What `run` executes: a snippet, written to the recipe's `script_name` with its
/// preamble and run by that name, or a file that already is a program, run
/// from its directory by its absolute path, as upstream's runner hands a
/// test's program to php (#331).
#[derive(Debug, Clone, Copy)]
pub enum Program<'a> {
    Snippet(&'a str),
    File(&'a Path),
    /// A program written beside a `.phpt` test, its `FILE`, `SKIPIF` or
    /// `CLEAN`: run as a file is, and what it wrote left with the test's
    /// directory as it printed it, since run-tests.php matches a test's
    /// expectation against the raw bytes and an `EXPECTF` asks for text
    /// inside that directory, `%s%etests` (#1089). A snippet's directory
    /// and a file's are the pin's token; a test's is not.
    Beside(&'a Path),
}

/// What one run wrote and how it ended, normalised.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observation {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// The exit code, or `None` when a signal ended the process.
    pub exit: Option<i32>,
    /// Wall time, disclosure only; the limit is the recipe's.
    pub wall: Duration,
}

/// How a run ended that the recipe's limit ended: the process ran past
/// `[recipe.limits] timeout_s` and was killed and reaped. [`Oracle::run_under`]
/// returns it as its error, so a caller that has a verdict for it reads it
/// with `downcast_ref` and every other caller's run ends as before: the phpt
/// lane's is that test's `fail`, since a hang is the engine's failure on the
/// test as a wrong answer is (#691), and the lane goes on to the next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PastLimit {
    pub(crate) binary: PathBuf,
    pub(crate) script: PathBuf,
    /// The recipe's limit, in seconds.
    pub(crate) limit_s: u64,
}

impl fmt::Display for PastLimit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} ran past the recipe's limit of {} s on {} and was killed",
            self.binary.display(),
            self.limit_s,
            self.script.display()
        )
    }
}

impl std::error::Error for PastLimit {}

/// One invocation under the pin's recipe: which binary runs, which program,
/// which regime, and the four things a `.phpt` section adds to a run. Every
/// field past the program is empty for a witness, which is why `run` takes
/// none of them.
#[derive(Debug, Clone)]
pub struct Invocation<'a> {
    /// The pinned php for the oracle; the engine's own binary for a phpt
    /// lane, so that both run under one recipe.
    pub binary: &'a Path,
    pub program: Program<'a>,
    /// A `[regime.<name>]` of the pin, its ini after the recipe's.
    pub regime: Option<&'a str>,
    /// Ini after the recipe's and the regime's: a test's `--INI--`.
    pub ini: &'a [String],
    /// Flags before the program's path, where php takes a mode of its own:
    /// `-l`, which compiles a file and does not run it, is the
    /// compile-identity lane's, and both binaries get it from here so that
    /// one recipe stands behind both. Empty for a phpt test and a witness,
    /// which run what they are given.
    pub flags: &'a [String],
    /// Arguments after the program's path: a test's `--ARGS--`.
    pub args: &'a [String],
    /// Environment entries over the recipe's, a later one of a name over
    /// an earlier: a test's environment as run-tests.php builds it, and the
    /// request a CGI-shaped test describes, whose values are the bytes the
    /// test wrote and no encoding of them. An entry whose value is empty
    /// takes the name out of the environment, the recipe's included, as
    /// php's `proc_open` leaves an empty value out of the environment it
    /// hands a program (`php_array_to_envp`, `ext/standard/proc_open.c:174`),
    /// and run-tests.php hands every program through it (#1186).
    pub env: &'a [(String, OsString)],
    /// What the program reads on stdin.
    pub stdin: Stdin<'a>,
    /// How the program's two streams out are taken.
    pub streams: Streams,
    /// The working directory: the program's own where `None`, which is
    /// a witness's; php-src's root for a phpt test and the sections run
    /// beside it, as run-tests.php runs them (#952).
    pub cwd: Option<&'a Path>,
    /// Whether what the program wrote is handed back as it wrote it and
    /// not normalised: a CGI run's, whose headers run-tests.php parts
    /// from the body before it trims (run-tests.php:2530-2533).
    pub raw: bool,
}

impl<'a> Invocation<'a> {
    /// `binary` running `program` under the recipe and nothing more.
    pub fn new(binary: &'a Path, program: Program<'a>) -> Invocation<'a> {
        Invocation {
            binary,
            program,
            regime: None,
            ini: &[],
            flags: &[],
            args: &[],
            env: &[],
            stdin: Stdin::Null,
            streams: Streams::Files,
            cwd: None,
            raw: false,
        }
    }
}

/// What a program reads on stdin, each as upstream's runner hands it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Stdin<'a> {
    /// `/dev/null`: what a witness, a compile and every run of the recipe's
    /// own read, and a test whose `--CAPTURE_STDIO--` leaves stdin out,
    /// which upstream hands its own stdin and this runner, which hands a
    /// test no terminal, hands nothing.
    #[default]
    Null,
    /// A pipe, these bytes written into it and then closed: what
    /// run-tests.php hands every program of a test, its `--STDIN--` or
    /// nothing, the `SKIPIF` and `CLEAN` beside it too
    /// (`system_with_timeout`, run-tests.php:1163-1182), so `fseek(STDIN)`
    /// and `fsync(STDIN)` fail as they fail on a pipe (#1186).
    Pipe(&'a [u8]),
    /// A file holding these bytes: a CGI-shaped test's body, which
    /// run-tests.php saves and redirects in with `<`
    /// (run-tests.php:2346-2418).
    File(&'a [u8]),
}

/// How a program's two streams out are taken, each as its lane's runner
/// takes them.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Streams {
    /// Each to a file of its own: a witness's, which pins the two apart, a
    /// compile's and a rendering's.
    #[default]
    Files,
    /// Through pipes, as run-tests.php takes every program of a test
    /// (`system_with_timeout`, run-tests.php:1163-1233): stdout's, with
    /// stderr in it where `merged`, upstream's `2>&1` for a test that
    /// captures both (run-tests.php:1885-1889), and in a pipe of its own
    /// otherwise, as a `SKIPIF`'s is. So `php://stdout` does not seek and a
    /// descriptor sent over a socket is a pipe's, as under upstream (#1186).
    Pipes { merged: bool },
}

/// Where a program's two streams go: set on its command before it starts,
/// and read back once it has ended.
enum Taken {
    Files { out: PathBuf, err: PathBuf },
    Pipes { out: Drained, err: Option<Drained> },
}

impl Taken {
    /// The streams `how` names, set on `cmd`: files in `streams`, or pipes
    /// whose draining starts now.
    fn set(cmd: &mut Command, streams: &Tree, how: Streams) -> Result<Taken> {
        match how {
            Streams::Files => {
                let (out, err) = (streams.join("stdout"), streams.join("stderr"));
                cmd.stdout(File::create(&out).context("creating the stdout file")?)
                    .stderr(File::create(&err).context("creating the stderr file")?);
                Ok(Taken::Files { out, err })
            }
            Streams::Pipes { merged } => {
                let (out, out_writer) = std::io::pipe().context("opening the stdout pipe")?;
                let (err, err_writer) = if merged {
                    let shared = out_writer.try_clone().context("sharing the stdout pipe")?;
                    (None, shared)
                } else {
                    let (err, writer) = std::io::pipe().context("opening the stderr pipe")?;
                    (Some(Drained::start(err)), writer)
                };
                cmd.stdout(out_writer).stderr(err_writer);
                Ok(Taken::Pipes {
                    out: Drained::start(out),
                    err,
                })
            }
        }
    }

    /// What the program wrote to stdout and to stderr, a pipe's by `until`
    /// at the latest; stderr is empty where it went into stdout.
    fn read(self, until: Instant) -> Result<(Vec<u8>, Vec<u8>)> {
        match self {
            Taken::Files { out, err } => Ok((
                fs::read(&out).context("reading php's stdout")?,
                fs::read(&err).context("reading php's stderr")?,
            )),
            Taken::Pipes { out, err } => Ok((
                out.taken(until),
                err.map(|err| err.taken(until)).unwrap_or_default(),
            )),
        }
    }
}

/// A pipe's bytes written into the program's stdin beside the wait, as
/// upstream writes them before it reads, and the pipe closed after them:
/// a program that never reads them, or ends first, ends the write with a
/// broken pipe, which is its business and not the run's.
fn fed(stdin: Stdin<'_>, child: &mut std::process::Child) -> Option<thread::JoinHandle<()>> {
    let (Stdin::Pipe(bytes), Some(mut pipe)) = (stdin, child.stdin.take()) else {
        return None;
    };
    let bytes = bytes.to_vec();
    Some(thread::spawn(move || {
        let _ = std::io::Write::write_all(&mut pipe, &bytes);
    }))
}

/// One stream of a program's, taken through a pipe: drained on a thread
/// of its own as it is written, so neither stream holds the program up
/// while the other fills, and read to its end once the program and
/// whatever it started are gone.
struct Drained {
    bytes: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
    done: std::sync::mpsc::Receiver<()>,
}

impl Drained {
    fn start(mut pipe: std::io::PipeReader) -> Drained {
        let bytes = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let (tell, done) = std::sync::mpsc::channel();
        let into = std::sync::Arc::clone(&bytes);
        thread::spawn(move || {
            let mut chunk = [0u8; 8192];
            while let Ok(n) = std::io::Read::read(&mut pipe, &mut chunk) {
                if n == 0 {
                    break;
                }
                if let Ok(mut held) = into.lock() {
                    held.extend_from_slice(&chunk[..n]);
                }
            }
            let _ = tell.send(());
        });
        Drained { bytes, done }
    }

    /// What came through the pipe: all of it once its last writer closed
    /// it, or what had come by `until` where a process the group kill did
    /// not reach still holds it open.
    fn taken(self, until: Instant) -> Vec<u8> {
        let _ = self
            .done
            .recv_timeout(until.saturating_duration_since(Instant::now()));
        self.bytes
            .lock()
            .map(|mut held| std::mem::take(&mut *held))
            .unwrap_or_default()
    }
}

/// The process group a program leads, which it was spawned into
/// (`process_group(0)`) so that whatever it starts is in the group too: a
/// server a test forks, a `php -S`, a worker it leaves running (#1211). The
/// group is killed whole when this value drops, past the limit, after the
/// program exits and on every error between, so nothing a program started
/// outlives its run, as `proc_terminate` ends upstream's on its timeout
/// (run-tests.php `system_with_timeout`). A kill that finds the group
/// empty is no error.
struct Group(nix::unistd::Pid);

impl Group {
    fn of(child: &std::process::Child) -> Result<Group> {
        let pid = i32::try_from(child.id()).context("a process id past pid_t")?;
        Ok(Group(nix::unistd::Pid::from_raw(pid)))
    }
}

impl Drop for Group {
    fn drop(&mut self) {
        let _ = nix::sys::signal::killpg(self.0, nix::sys::signal::Signal::SIGKILL);
    }
}

/// The pinned oracle for the length of one run: the pin, and whether its
/// digest has been asserted yet. Made by [`Pin::oracle`], once per run;
/// every program the run executes goes through it.
#[derive(Debug)]
pub struct Oracle<'a> {
    pin: &'a Pin,
    /// What the one assertion found, kept so a mismatch refuses every
    /// later run the same way rather than hashing again.
    digest: OnceLock<Result<(), String>>,
}

impl<'a> Oracle<'a> {
    /// The pin: its recipe and its regimes, which a lane reads beside the
    /// runs.
    pub fn pin(&self) -> &'a Pin {
        self.pin
    }

    /// Run `program` under the pinned binary and the recipe, with
    /// `regime`'s ini after it when one is named, and return what
    /// it wrote, normalised. The digest is asserted before the first run
    /// through this value; the process runs with the recipe's environment
    /// alone, in the program's directory, under the recipe's timeout.
    pub fn run(&self, program: Program<'_>, regime: Option<&str>) -> Result<Observation> {
        self.asserted()?;
        self.run_under(&Invocation {
            regime,
            ..Invocation::new(&self.pin.oracle.path, program)
        })
    }

    /// Run one invocation under the recipe and return what it wrote,
    /// normalised. The digest is the pinned binary's alone and `run` asserts
    /// it; a phpt lane runs the engine's binary here, under the same
    /// recipe, which is what makes the two comparable (harness-design.md
    /// §4.3, design §8).
    pub fn run_under(&self, call: &Invocation<'_>) -> Result<Observation> {
        let pin = self.pin;
        let regime = call.regime.map(|name| pin.regime(name)).transpose()?;
        let recipe = &pin.recipe;

        let scratch = Scratch::for_program(recipe, call.program)?;
        let mut cmd = Command::new(call.binary);
        cmd.args(&recipe.args);
        for ini in recipe
            .ini
            .iter()
            .chain(regime.into_iter().flat_map(|r| &r.ini))
            .chain(call.ini)
        {
            cmd.arg("-d").arg(ini);
        }
        cmd.args(call.flags)
            .arg(&scratch.script)
            .args(call.args)
            .current_dir(call.cwd.unwrap_or(&scratch.dir))
            .process_group(0);
        if recipe.env_clear {
            cmd.env_clear();
        }
        cmd.envs(&recipe.env);
        for (name, value) in call.env {
            if value.is_empty() {
                cmd.env_remove(name);
            } else {
                cmd.env(name, value);
            }
        }
        // The streams, and the program's stdin, stand in a tree apart from
        // the program's directory, so a snippet that lists or reads its
        // own directory finds its own file there and never these; a tree
        // and not a directory under the snippet's, since php's
        // `memory_get_usage()` counts the path in `$_SERVER` and a witness
        // pins that number.
        let streams = Tree::new("streams")?;
        let taken = Taken::set(&mut cmd, &streams, call.streams)?;
        cmd.stdin(stdin_of(&streams, call.stdin)?);

        let started = Instant::now();
        let spawned = cmd.spawn();
        // The command holds the pipes' writing ends until it drops, and a
        // pipe ends only when its last writer closes it.
        drop(cmd);
        let mut child = spawned.with_context(|| format!("running {}", call.binary.display()))?;
        let group = Group::of(&child)?;
        let feeding = fed(call.stdin, &mut child);
        let limit = Duration::from_secs(recipe.limits.timeout_s);
        let status = loop {
            if let Some(status) = child.try_wait().context("waiting for php")? {
                break status;
            }
            if started.elapsed() > limit {
                // The group killed while its leader still stands, so the
                // number is its own, and then the leader reaped, so it is
                // gone and not a zombie when the caller hears of it.
                drop(group);
                child.wait().context("reaping php past the limit")?;
                return Err(PastLimit {
                    binary: call.binary.to_path_buf(),
                    script: scratch.script.clone(),
                    limit_s: recipe.limits.timeout_s,
                }
                .into());
            }
            thread::sleep(Duration::from_millis(5));
        };
        let wall = started.elapsed();
        // What the program started goes with it, which closes whatever of
        // its pipes those held, so the reads below reach their end.
        drop(group);
        if let Some(feeding) = feeding {
            let _ = feeding.join();
        }

        let (stdout, stderr) = taken.read(started + limit)?;
        let (stdout, stderr) = if call.raw {
            (stdout, stderr)
        } else {
            (
                scratch.normalise(&recipe.normalise, stdout),
                scratch.normalise(&recipe.normalise, stderr),
            )
        };
        Ok(Observation {
            stdout,
            stderr,
            exit: status.code(),
            wall,
        })
    }

    /// Assert the digest now rather than at the first program: what a
    /// lane does before its loop, so a mismatch refuses the run and not
    /// each program in turn. The first call hashes; every later call,
    /// here or from `run`, reads what it found.
    pub fn asserted(&self) -> Result<()> {
        self.digest
            .get_or_init(|| self.pin.assert_digest().map_err(|e| format!("{e:#}")))
            .clone()
            .map_err(anyhow::Error::msg)
    }
}

/// The engine's binary, the one `sapi/cli` builds (design §4.1,
/// ADR-0008).
pub const ENGINE: &str = "php";

/// Beside [`ENGINE`], the CGI host's binary, the one `sapi/cgi` builds
/// (ADR-0008), which a CGI-shaped test is run through.
pub(crate) const ENGINE_CGI: &str = "php-cgi";

/// The cargo line that builds [`ENGINE`] and [`ENGINE_CGI`]: their two
/// crates by package, so the build turns on the features those crates
/// declare and no others. A line naming the binaries alone builds them
/// with every workspace member's features unified, the harness's
/// extensions among them, which is not the binary the lane measures.
pub(crate) fn engine_build() -> String {
    let packages: Vec<String> = [crate::carried::BINARY, "sapi/cgi"]
        .iter()
        .map(|krate| format!("-p {}", crate::layout::package(krate)))
        .collect();
    format!("cargo build {}", packages.join(" "))
}

/// The extensions php names otherwise than their crates, the module's
/// name beside the crate's, as upstream's own runner pairs them
/// (`remap_loaded_extensions_names`, run-tests.php:2908-2916): opcache
/// registers as `Zend OPcache`, the name `php -m`, `php --re` and
/// `ReflectionExtension` answer to, and every other extension is its
/// crate's name in php's casing, which php looks up without regard to
/// case. The one table both directions read.
const RENAMED: [(&str, &str); 1] = [("Zend OPcache", "opcache")];

/// The name php knows the extension of crate `ext/<krate>` by: what a
/// lane hands `--re`, `--ri` and `ReflectionExtension`.
pub(crate) fn module_name(krate: &str) -> &str {
    RENAMED
        .iter()
        .find(|(_, own)| *own == krate)
        .map_or(krate, |(module, _)| module)
}

/// The crate's name for the module php lists as `module`, lowered as
/// upstream lowers every other name: what `--EXTENSIONS--` spells.
pub(crate) fn crate_name(module: &str) -> String {
    RENAMED
        .iter()
        .find(|(named, _)| *named == module)
        .map_or_else(|| module.to_lowercase(), |(_, own)| (*own).to_owned())
}

/// What ran the program a lane measures: the fresh-process adapter of
/// design §8's `ScriptExec` seam, shared by `phpt` and `witness`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Runner {
    /// The engine's own binary, one process per program.
    Engine(PathBuf),
    /// The engine linked into the harness, served in this process through
    /// [`linked::serve`]: the other adapter of design §8's `ScriptExec`
    /// seam, and the engine as much as the binary is, so its reading is a
    /// pass or a fail. What a lane stands on where no binary is built.
    Linked,
    /// The pinned oracle standing in for the engine: a control of the
    /// lane itself and never a reading of the engine, so its reading is
    /// a skip whatever the programs say.
    Control(PathBuf),
    /// A binary the tree has moved past, and why: built before the
    /// engine's sources were last written, so what it would measure is an
    /// engine this tree no longer describes while the reading is stamped
    /// with the tree. Nothing runs under it and the reading is a skip held
    /// to that cause, as `permit` refuses to judge on a build whose digest
    /// is not the tree's (harness-design.md §4.4, #475).
    Stale(String),
    /// Nothing to run, and why.
    Absent(String),
}

impl Runner {
    /// The binary to run: the one named, or the engine's where cargo puts
    /// it. A binary whose digest is the pin's is the lane's control, and
    /// the engine's own is held to the tree by [`against_the_tree`].
    ///
    /// A binary the tree has moved past is passed over for one it has not,
    /// so `release` is preferred among the binaries that are the tree's
    /// and not over them: before this, a release binary built once and
    /// never again was the lane's engine for every run after, however far
    /// the tree moved, and a debug build could not displace it (#475). Where every binary found is behind the tree, the first is
    /// the one the skip names.
    pub fn resolve(root: &Path, pin: &Pin, named: Option<&Path>) -> Result<Runner> {
        if let Some(path) = named {
            let path = named_binary(path)?;
            let bytes = fs::read(&path).with_context(|| format!("reading {}", path.display()))?;
            let digest = format!("{:x}", md5::compute(bytes));
            return Ok(if digest == pin.oracle.md5 {
                // The pinned oracle is a control and never this tree's
                // engine, so when the tree was last written says nothing
                // about it; the pin holds it by its digest instead.
                Runner::Control(path)
            } else {
                against_the_tree(root, path)
            });
        }
        let mut behind = None;
        for profile in ["release", "debug"] {
            let built = root.join("target").join(profile).join(ENGINE);
            if !built.is_file() {
                continue;
            }
            match against_the_tree(root, built) {
                Runner::Stale(why) => {
                    behind.get_or_insert(Runner::Stale(why));
                }
                found => return Ok(found),
            }
        }
        if let Some(behind) = behind {
            return Ok(behind);
        }
        Ok(Runner::Absent(format!(
            "the engine's binary is not built: no target/release/{ENGINE} and no target/debug/{ENGINE}; a CLI request reaches `serve` with php_engine (design §5)"
        )))
    }

    /// The rung below the binary, for a lane the linked engine can serve:
    /// [`Runner::Linked`] where [`resolve`](Runner::resolve) found no
    /// binary and where what it found is behind the tree, and what it
    /// found otherwise. The linked engine is built from the tree the
    /// reading is stamped with, so a stale binary falls to it rather than
    /// to a skip. A lane whose programs the in-process adapter does not
    /// serve, a `.phpt` with its sections and its own directory, does not
    /// take this rung and keeps the absence.
    #[must_use]
    pub fn or_linked(self) -> Runner {
        match self {
            Runner::Absent(_) | Runner::Stale(_) => Runner::Linked,
            found => found,
        }
    }

    /// The binary, when there is one to run.
    pub fn binary(&self) -> Option<&Path> {
        match self {
            Runner::Engine(path) | Runner::Control(path) => Some(path),
            Runner::Linked | Runner::Stale(_) | Runner::Absent(_) => None,
        }
    }

    /// What a reading names the engine it ran by: the binary's path and
    /// the md5 of its bytes, so two readings stamped with one tree can be
    /// told apart when the binary under them is not the same (#475).
    ///
    /// A rung that is not the engine's own binary has none: the pinned
    /// oracle standing in for it is a control and the lane's words name it
    /// as one, the linked engine is the harness's own build of the tree
    /// the reading is already stamped with, and the two rungs where
    /// nothing ran carry why instead.
    pub fn stamp(&self) -> Option<String> {
        let Runner::Engine(path) = self else {
            return None;
        };
        Some(match fs::read(path) {
            Ok(bytes) => format!("{} md5 {:x}", path.display(), md5::compute(bytes)),
            Err(why) => format!(
                "{}, whose bytes this run could not read: {why}",
                path.display()
            ),
        })
    }
}

/// Where the engine's binary is built from: the crates that go into it,
/// every directory of [`crate::layout::CRATE_DIRS`] (design §4.1,
/// ADR-0008). `xtask` is not among them, since the harness is not linked
/// into `php`, and `target` is no source at all.
fn engine_sources() -> impl Iterator<Item = &'static str> {
    crate::layout::CRATE_DIRS.iter().map(|d| d.dir)
}

/// The newest source of the engine under `root`, and when it was written.
/// `None` where the tree carries none, and then no binary is behind
/// anything: a workspace of a test's own with no engine in it is not a
/// tree a binary can have fallen behind.
fn newest_source(root: &Path) -> Option<(PathBuf, SystemTime)> {
    let mut found = Vec::new();
    for dir in engine_sources() {
        let _ = crate::tally::files_under(&root.join(dir), &mut found);
    }
    found
        .into_iter()
        .filter(|path| is_engine_source(path.strip_prefix(root).unwrap_or(path)))
        .filter_map(|path| {
            let written = path.metadata().and_then(|at| at.modified()).ok()?;
            Some((path, written))
        })
        .max_by_key(|(_, written)| *written)
}

/// Whether one file under [`engine_sources`], named relative to the root,
/// is code the binary is built from: a `.rs` the crate is compiled from or
/// a `.toml` its `build.rs` reads, and not one of the files
/// [`crate::sources::rust`] leaves out of a crate's sources for the same
/// reason. A test, a bench, an example or a fixture is not linked into
/// `php`, so writing one moves no binary and leaves none behind the tree.
fn is_engine_source(relative: &Path) -> bool {
    relative
        .extension()
        .is_some_and(|kind| kind == "rs" || kind == "toml")
        && !relative.components().any(|part| {
            matches!(
                part.as_os_str().to_str(),
                Some("tests" | "tests.rs" | "benches" | "examples" | "fixtures")
            )
        })
}

/// A binary named on the command line, by the path every program is
/// handed: one that names no file is refused before anything runs, and a
/// relative one is made absolute against the directory `xtask` was invoked
/// from, since a lane's programs run in a tree of their own where the
/// relative path names nothing and every test of the subset skipped for
/// it (#1259).
fn named_binary(path: &Path) -> Result<PathBuf> {
    if !path.is_file() {
        bail!("no binary at {}", path.display());
    }
    if path.is_absolute() {
        return Ok(path.to_path_buf());
    }
    fs::canonicalize(path).with_context(|| format!("making {} absolute", path.display()))
}

/// One binary held to the tree a lane's reading is stamped with: itself
/// where the engine's sources are no newer than it, and [`Runner::Stale`]
/// where one of them is.
///
/// The engine's binary carries no digest of its own sources the way a
/// build of the harness does (harness-design.md §4.4), so what says
/// whether it is this tree's is when it was written, which is what cargo
/// decides a rebuild by. A binary whose write time cannot be read, and a
/// tree with no engine source in it, are left as the engine: this says a
/// binary is behind the tree only where it measured that it is.
///
/// The CGI host's binary beside it, [`ENGINE_CGI`], is what a CGI-shaped
/// test of the lane runs, and since ADR-0008 it is built by a crate of its
/// own, so a build of `php` alone leaves it where it was: where it stands
/// there it is held to the tree with the engine's, and the pair is behind
/// the tree when either is.
fn against_the_tree(root: &Path, path: PathBuf) -> Runner {
    let Some((source, written)) = newest_source(root) else {
        return Runner::Engine(path);
    };
    let cgi = path.with_file_name(ENGINE_CGI);
    let behind = [path.as_path(), cgi.as_path()].into_iter().find(|binary| {
        binary
            .metadata()
            .and_then(|at| at.modified())
            .is_ok_and(|built| written > built)
    });
    let Some(behind) = behind else {
        return Runner::Engine(path);
    };
    let path = behind;
    Runner::Stale(format!(
        "the engine's binary at {} was built before {} was last written, so a lane running it \
         would measure an engine this tree no longer describes while stamping the reading with \
         the tree. Rebuild it: `{}`, or with `--release` for the release binary a lane prefers.",
        path.display(),
        source.display(),
        engine_build()
    ))
}

/// The program's stdin as [`Stdin`] says: nothing, a pipe the run writes
/// once the program stands, or a file in the run's streams holding the
/// bytes.
fn stdin_of(streams: &Tree, stdin: Stdin<'_>) -> Result<Stdio> {
    match stdin {
        Stdin::Null => Ok(Stdio::null()),
        Stdin::Pipe(_) => Ok(Stdio::piped()),
        Stdin::File(bytes) => {
            let path = streams.join("stdin");
            fs::write(&path, bytes).context("writing the program's stdin")?;
            Ok(Stdio::from(
                File::open(&path).context("opening the program's stdin")?,
            ))
        }
    }
}

/// The recipe's normalisation of bytes that are not a stream of one run:
/// CRLF to LF, php's trim set off both ends, and the load number after
/// `extension #` replaced by the pin's token where the pin carries one.
/// What a run's streams go through is [`Scratch::normalise`], which is
/// this and the scratch prefix; what a `.phpt` expectation and the
/// engine's rendering go through is this alone, since neither ran in a
/// directory. The trim set and the token are the recipe's and are read
/// nowhere else (design §8).
pub fn normalise(rules: &Normalise, bytes: Vec<u8>) -> Vec<u8> {
    let bytes = if rules.crlf_to_lf {
        replace(&bytes, b"\r\n", b"\n")
    } else {
        bytes
    };
    let trim: &[u8] = rules.trim.as_bytes();
    let start = bytes
        .iter()
        .position(|b| !trim.contains(b))
        .unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|b| !trim.contains(b))
        .map_or(start, |i| i + 1);
    let bytes = &bytes[start..end];
    match &rules.extension_number_token {
        Some(token) => renumber(bytes, token.as_bytes()),
        None => bytes.to_vec(),
    }
}

/// `extension #` and at least one ASCII digit, so `extension #10 ctype` is
/// `extension #<token> ctype`; the phrase followed by no digit is left as
/// it stands.
const LOAD_NUMBER: &[u8] = b"extension #";

/// Every `extension #<digits>` in `haystack` with the digits replaced by
/// `token`, the rest byte for byte; an `--EXPECTF--` section's `%d` in
/// that place is replaced too, since what it is matched against carries
/// the token where the digits were (`sapi/cli/tests/006.phpt`).
fn renumber(haystack: &[u8], token: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(haystack.len());
    let mut i = 0;
    while i < haystack.len() {
        if haystack[i..].starts_with(LOAD_NUMBER) {
            let after = i + LOAD_NUMBER.len();
            let digits = if haystack[after..].starts_with(b"%d") {
                2
            } else {
                haystack[after..]
                    .iter()
                    .take_while(|b| b.is_ascii_digit())
                    .count()
            };
            if digits > 0 {
                out.extend_from_slice(LOAD_NUMBER);
                out.extend_from_slice(token);
                i = after + digits;
                continue;
            }
        }
        out.push(haystack[i]);
        i += 1;
    }
    out
}

/// Where a program runs: its directory and the path it is run by, and for
/// a snippet the directory of its own it is written into, removed on
/// drop. The linked adapter holds one too, so a snippet stands as a file
/// under both adapters. What the fresh-process adapter captures stands
/// apart, in a tree [`Oracle::run_under`] holds.
pub(crate) struct Scratch {
    pub(crate) dir: PathBuf,
    script: PathBuf,
    /// Whether `dir` is replaced by the pin's token in what the program
    /// wrote: every program's but one written beside a test.
    tokenised: bool,
    _snippet: Option<Tree>,
}

impl Scratch {
    pub(crate) fn for_program(recipe: &Recipe, program: Program<'_>) -> Result<Scratch> {
        Ok(match program {
            Program::Snippet(code) => {
                let own = Tree::new("oracle")?;
                let script = PathBuf::from(&recipe.script_name);
                fs::write(own.join(&script), format!("{}{code}", recipe.preamble))
                    .with_context(|| format!("writing {}", recipe.script_name))?;
                Scratch {
                    dir: own.path().to_path_buf(),
                    script,
                    tokenised: true,
                    _snippet: Some(own),
                }
            }
            Program::File(path) | Program::Beside(path) => Scratch {
                dir: dir_of(path),
                script: run_by(path)?,
                tokenised: matches!(program, Program::File(_)),
                _snippet: None,
            },
        })
    }

    /// The recipe's normalisation of a stream: [`normalise_in`] over the
    /// directory the program ran in, or [`normalise`] alone for a program
    /// written beside a test.
    pub(crate) fn normalise(&self, rules: &Normalise, bytes: Vec<u8>) -> Vec<u8> {
        if self.tokenised {
            normalise_in(rules, &self.dir, bytes)
        } else {
            normalise(rules, bytes)
        }
    }
}

/// The directory a file program runs in: its own, and the current one
/// for a bare name.
fn dir_of(path: &Path) -> PathBuf {
    path.parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map_or_else(|| PathBuf::from("."), Path::to_path_buf)
}

/// The path a file program is run by: its directory as the kernel
/// resolves it, joined with the file's own name, which is what upstream's
/// runner hands php, `realpath` of the test's directory and the program's
/// name. `__FILE__`, `ReflectionClass::getFileName()`, the name php's
/// dump prints into each block and every error site print the path a
/// program was run by, so a `%s<name>.php` expectation, whose `%s` is one
/// character or more, is met only by a name with its directory before it
/// (#331). The fresh-process adapter runs a file by this path and the
/// linked compiler is handed the same one, so the two sides of a dump
/// carry one name; [`normalise_in`] puts the token where the directory
/// was, so what is compared carries no machine's path.
pub fn run_by(path: &Path) -> Result<PathBuf> {
    let dir = dir_of(path);
    let name = path
        .file_name()
        .with_context(|| format!("{} names no file", path.display()))?;
    Ok(dir.canonicalize().unwrap_or(dir).join(name))
}

/// The recipe's normalisation of what a program wrote or was compiled
/// under in `dir`: [`normalise`], and then the directory, as given and as
/// the kernel resolves it, replaced by the token.
pub fn normalise_in(rules: &Normalise, dir: &Path, bytes: Vec<u8>) -> Vec<u8> {
    let mut bytes = normalise(rules, bytes);
    let mut forms = vec![dir.to_path_buf()];
    if let Ok(real) = dir.canonicalize() {
        forms.push(real);
    }
    forms.sort_by_key(|p| std::cmp::Reverse(p.as_os_str().len()));
    for form in forms {
        let form = form.to_string_lossy();
        if !form.is_empty() {
            bytes = replace(&bytes, form.as_bytes(), rules.scratch_token.as_bytes());
        }
    }
    bytes
}

fn replace(haystack: &[u8], needle: &[u8], with: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(haystack.len());
    let mut i = 0;
    while i < haystack.len() {
        if haystack[i..].starts_with(needle) {
            out.extend_from_slice(with);
            i += needle.len();
        } else {
            out.push(haystack[i]);
            i += 1;
        }
    }
    out
}

/// What a comparison found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Same,
    /// The bytes differ and the registry accepts it under this row.
    Diverged(Divergence),
    /// The bytes differ and nothing accepts it: the line diff, expected as
    /// `-`, actual as `+`.
    Differs(String),
}

/// Compare the oracle's bytes with the engine's for the divergence key `id`.
/// Equal bytes are `Same` whatever the registry says; a difference is
/// `Diverged` under an accepted row for `id` and `Differs` otherwise. An
/// `open` or `fixed` row accepts nothing. Proposing a row for a `Differs`
/// is the caller's, through `Registry::propose`.
pub fn compare(registry: &Registry, id: &str, expected: &[u8], actual: &[u8]) -> Verdict {
    verdict(registry, id, expected == actual, expected, actual)
}

/// The verdict when what stands for equality is a match the caller made
/// rather than the bytes themselves: the phpt runner's `--EXPECTF--` and
/// `--EXPECTREGEX--`, whose expectation is a pattern. `met` stands where
/// `expected == actual` stands in `compare`, so one place decides what the
/// registry means and no instrument diffs on its own.
pub fn verdict(
    registry: &Registry,
    id: &str,
    met: bool,
    expected: &[u8],
    actual: &[u8],
) -> Verdict {
    if met {
        return Verdict::Same;
    }
    match registry.accepted(id) {
        Some(row) => Verdict::Diverged(row.clone()),
        None => Verdict::Differs(diff(expected, actual)),
    }
}

/// A line diff of `expected` against `actual`: common lines with a space,
/// expected-only with `-`, actual-only with `+`. Bytes that are not UTF-8
/// are shown lossily.
pub fn diff(expected: &[u8], actual: &[u8]) -> String {
    let expected = String::from_utf8_lossy(expected);
    let actual = String::from_utf8_lossy(actual);
    let a: Vec<&str> = expected.lines().collect();
    let b: Vec<&str> = actual.lines().collect();
    let mut out = String::new();
    for step in align(a.len(), b.len(), |i, j| a[i] == b[j]) {
        let (mark, line) = match step {
            Step::Both(i, _) => (' ', a[i]),
            Step::Expected(i) => ('-', a[i]),
            Step::Actual(j) => ('+', b[j]),
        };
        out.push(mark);
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// One line of an alignment, by its place in the lines it came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Step {
    /// An expected line and the actual line it stands for.
    Both(usize, usize),
    /// An expected line nothing actual stands for.
    Expected(usize),
    /// An actual line no expected line stands for.
    Actual(usize),
}

/// The most cells the middle of an alignment is given a table of: past
/// it, the middle is every actual line and then every expected line, as
/// two whole programs that part at the first line are.
const ALIGNED_CELLS: usize = 1 << 22;

/// `expected` lines against `actual` ones, in order, where `same(i, j)`
/// says whether expected line `i` stands for actual line `j`: the longest
/// common subsequence, with an actual line ahead of an expected one where
/// either would do. `diff` aligns by equality; phpt's miss aligns by what
/// one line of an `--EXPECTF--` matches (#1185). The lines the two share
/// at either end are taken first, so the table is the middle's alone.
pub(crate) fn align(
    expected: usize,
    actual: usize,
    same: impl Fn(usize, usize) -> bool,
) -> Vec<Step> {
    let mut head = 0;
    while head < expected.min(actual) && same(head, head) {
        head += 1;
    }
    let mut tail = 0;
    while tail < (expected - head).min(actual - head)
        && same(expected - 1 - tail, actual - 1 - tail)
    {
        tail += 1;
    }
    let (a, b) = (expected - head - tail, actual - head - tail);
    let mut steps: Vec<Step> = (0..head).map(|n| Step::Both(n, n)).collect();
    if a.saturating_mul(b) > ALIGNED_CELLS {
        steps.extend((head..head + b).map(Step::Actual));
        steps.extend((head..head + a).map(Step::Expected));
    } else {
        let same = |i: usize, j: usize| same(head + i, head + j);
        let mut table = vec![vec![0usize; b + 1]; a + 1];
        for i in (0..a).rev() {
            for j in (0..b).rev() {
                table[i][j] = if same(i, j) {
                    table[i + 1][j + 1] + 1
                } else {
                    table[i + 1][j].max(table[i][j + 1])
                };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < a || j < b {
            if i < a && j < b && same(i, j) {
                steps.push(Step::Both(head + i, head + j));
                i += 1;
                j += 1;
            } else if j < b && (i == a || table[i][j + 1] >= table[i + 1][j]) {
                steps.push(Step::Actual(head + j));
                j += 1;
            } else {
                steps.push(Step::Expected(head + i));
                i += 1;
            }
        }
    }
    steps.extend((0..tail).map(|n| Step::Both(expected - tail + n, actual - tail + n)));
    steps
}

#[cfg(test)]
mod renumber_tests {
    use super::renumber;

    #[test]
    fn a_load_number_and_an_expectf_digit_run_in_its_place_are_the_token() {
        assert_eq!(
            renumber(
                b"extension #6 pcre, extension #%d pcre, extension #x",
                b"%N%"
            ),
            b"extension #%N% pcre, extension #%N% pcre, extension #x"
        );
    }
}
