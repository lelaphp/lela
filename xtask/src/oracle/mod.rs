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
//! the pin's path second where the pin states one, which the runner and
//! the tests that resolve a subset ask and never read off the pin
//! themselves; [`Promise`] is what a
//! runner says this machine has of the pin, so a test that would skip for
//! a fact the machine has not fails instead where the fact was promised
//! (#38); and [`expect`] is how an expectation that is a pattern rather
//! than bytes is met, which `verdict` takes as the caller's match.
//!
//! The version, the digest and the path are values of the pin and appear in
//! no source file (§10 rule 5).

pub(crate) mod built_from;
mod closure;
#[cfg(test)]
mod dockerfile;
pub(crate) mod expect;
mod fastcgi;
mod formula;
pub(crate) mod hosts;
pub(crate) mod linked;
pub(crate) mod profile;
mod propose;
mod variant;

use built_from::BuiltFrom;
#[cfg(test)]
pub(crate) use closure::stand_in;
pub(crate) use closure::{Closure, Minos, Own};
pub use fastcgi::{Mapping, Param, Piece, Sent, Split, Variable};
pub(crate) use formula::Formula;
pub use propose::{Proposal, propose};
pub use variant::{Build, Oracles, Platform, Provenance, Variant};

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::fmt;
use std::fs::{self, File};
use std::io::Write as _;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::divergence::{Digests, Divergence, Moved, Registry};
use crate::layout::Layout;
use crate::reading::joined;
use crate::scratch::{self, Tree};

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
/// [`Promise::word`] gives, `tree` for the pinned php-src, `oracle` for
/// the pinned php, `mysql` for the `MySQL` server of
/// `ci/mysql/compose.yaml` (#3084), `locale` for the locales the tests
/// name and `main-stack` for a main thread whose size reads (#3180). A
/// test that would skip for a fact the machine has not, saying so on
/// stderr, fails instead where the fact was promised, naming what it
/// looked for; so a runner whose provisioning broke turns red rather
/// than quietly measuring less. The name is stated here and, for the
/// tests that cannot depend on xtask, in
/// `crates/php_compile/tests/support/skip.rs`; the workflow reads it,
/// beside [`TREE_VARIABLE`], off `xtask status --section oracle` and sets
/// it under that name (#36).
pub const PROMISE_VARIABLE: &str = "XTASK_PROMISED";

/// The environment variable `xtask test` sets for each test executable it
/// runs, to a file of that executable's own (#3085): a test that skips
/// for a fact this machine did not promise appends `<test>\t<word>` to
/// it, the test named after the thread libtest runs it on, so the reading
/// counts the tests that skipped. Unset, as under a bare `cargo test`,
/// nothing is written. `crates/php_compile/tests/support/skip.rs` spells
/// the name too.
pub const SKIPPED_VARIABLE: &str = "XTASK_SKIPPED";

/// One fact a machine can promise to have: the pin's tree or binary, the
/// `MySQL` server, the locales or the main thread's size. What a guarded
/// test asks through [`Promise::or_skip`], or, in a test that cannot
/// depend on xtask, through `crates/php_compile/tests/support/skip.rs`,
/// which spells the word itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Promise {
    /// The pinned php-src, where [`Source::tree`] answers.
    Tree,
    /// The pinned php, at [`Binary::file`].
    Oracle,
    /// The `MySQL` server of `ci/mysql/compose.yaml` on `127.0.0.1:3306`,
    /// which the tests of `ext/mysqli` and `ext/pdo_mysql` run against
    /// through `MysqlServer::with` (#3084).
    Mysql,
    /// Every locale `ext/standard`'s tests name, which CI generates.
    Locale,
    /// A main thread whose size `php_heap::main_call_stack_size` reads,
    /// which `php_compile`'s stack test measures past.
    MainStack,
}

impl Promise {
    const ALL: [Promise; 5] = [
        Promise::Tree,
        Promise::Oracle,
        Promise::Mysql,
        Promise::Locale,
        Promise::MainStack,
    ];

    /// The word [`PROMISE_VARIABLE`] lists for this fact.
    pub fn word(self) -> &'static str {
        match self {
            Promise::Tree => "tree",
            Promise::Oracle => "oracle",
            Promise::Mysql => "mysql",
            Promise::Locale => "locale",
            Promise::MainStack => "main-stack",
        }
    }

    /// What every guarded test asks (#38): `found` when it is, and
    /// otherwise `None` after saying so on stderr, as the tests skip
    /// today, unless this machine promised the fact through
    /// [`PROMISE_VARIABLE`], in which case the error names what was
    /// looked for and the promise. Reads the variable off the process,
    /// and a skip is written where [`SKIPPED_VARIABLE`] names a file;
    /// [`Promised::or_skip`] is the same decision over a value handed in,
    /// which writes none, so a test of the decision is no skip.
    pub fn or_skip<T>(self, found: Result<T>) -> Result<Option<T>> {
        let found = Promised::load()?.or_skip(self, found)?;
        if let (None, Some(file)) = (&found, std::env::var_os(SKIPPED_VARIABLE)) {
            self.skipped(Path::new(&file))?;
        }
        Ok(found)
    }

    /// This fact's skip, appended to `file` as one line, `<test>\t<word>`:
    /// the test is the name libtest gives the thread it runs on, and a
    /// thread the test started has none, so its skip is named by the
    /// thread's id and counts apart from its test's, a skip counted twice
    /// rather than one lost.
    fn skipped(self, file: &Path) -> Result<()> {
        let thread = thread::current();
        let test = thread
            .name()
            .map_or_else(|| format!("{:?}", thread.id()), str::to_owned);
        let line = format!("{test}\t{}\n", self.word());
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(file)
            .and_then(|mut out| out.write_all(line.as_bytes()))
            .with_context(|| format!("writing the skip of {test} to {}", file.display()))
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
                    Promise::ALL.map(Promise::word).join(", ")
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
    pub sapi: Sapi,
    /// `[variant.<name>]`: the oracle's builds with flags of their own,
    /// each pinned per platform, which a `[[crate]]` row names to run its
    /// lanes under (#1782); absent until a person writes one.
    #[serde(default)]
    pub variant: BTreeMap<String, Variant>,
    /// `[fastcgi]`: the HTTP-to-FastCGI mapping `[sapi] fpm` is driven
    /// under for a host php does not ship (#2334); [`Pin::fastcgi`].
    fastcgi: Option<Mapping>,
    /// `[closure]`: the Homebrew kegs the pinned binaries load, each at
    /// its version (#2657), held beside the digest. While the pin records
    /// none, nothing the binaries load is held.
    pub(crate) closure: Option<closure::Closure>,
    /// How many times the digest was asserted through this pin; not a
    /// value of the file.
    #[serde(skip)]
    asserted: Asserted,
    /// [`Pin::digest`]: set by [`Pin::parse`], not a value of the file.
    #[serde(skip)]
    digest: String,
    /// What parses but does not validate across the file's tables, each
    /// refusal's text: set by [`Pin::parse`], and what [`Pin::defects`]
    /// says (#3618 ruling 4).
    #[serde(skip)]
    defects: Vec<String>,
    /// The command every program's binary is run under, before it and its
    /// arguments: none from the file, and `unshare` for a System V subset
    /// on Linux (#1482), set by [`Pin::wrapped`].
    #[serde(skip)]
    wrapper: Vec<String>,
}

/// [`Pin::digest`] of the pin's text: the git blob id of its values with
/// the keys [`Provenance`] declares taken out of each variant's builds.
fn digest(text: &str) -> Result<String> {
    let mut table: toml::Table = toml::from_str(text)?;
    let variants = table.get_mut("variant").and_then(toml::Value::as_table_mut);
    for (_, variant) in variants.into_iter().flatten() {
        let builds = variant.as_table_mut().into_iter().flatten();
        for build in builds.filter_map(|(_, build)| build.as_table_mut()) {
            build.retain(|key, _| !Provenance::KEYS.contains(&key));
        }
    }
    Ok(crate::git::blob_id(table.to_string().as_bytes()))
}

/// `[sapi]`: the binaries upstream's run-tests.php finds beside `php` for
/// `sapi/cgi`, `sapi/fpm` and `sapi/phpdbg` (`get_binary`, run-tests.php at
/// the tag; php-fpm by `FPM\Tester::findExecutable`), each by path and
/// digest as `[oracle]` pins `php`. What asserts them before a run that
/// hosts a test through them is #1184's.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sapi {
    pub cgi: Option<Host>,
    pub fpm: Option<Host>,
    pub phpdbg: Option<Host>,
}

impl Sapi {
    /// The pinned host of `kind`'s kind; none for the CLI, which is
    /// `[oracle]` itself.
    pub(crate) fn of(&self, kind: hosts::EngineHost) -> Option<&Host> {
        [
            (hosts::CGI, &self.cgi),
            (hosts::FPM, &self.fpm),
            (hosts::PHPDBG, &self.phpdbg),
        ]
        .into_iter()
        .find_map(|(each, pinned)| (each == kind).then_some(pinned.as_ref()))
        .flatten()
    }

    /// [`Sapi::of`], and the failure where the pin names none.
    pub(crate) fn named(&self, kind: hosts::EngineHost) -> Result<&Host> {
        self.of(kind)
            .ok_or_else(|| anyhow::anyhow!("oracle.toml's [sapi] names no {}", kind.binary))
    }
}

/// One of `[sapi]`'s binaries: its path on this machine and its digest.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Host {
    pub path: PathBuf,
    pub md5: String,
}

impl Host {
    /// The binary's digest against the pin's; a mismatch refuses the run,
    /// as [`Pin`]'s does for `php`. Reached through
    /// [`Oracle::host_asserted`], which holds the closure first.
    fn assert_digest(&self) -> Result<()> {
        let actual = md5_of(&self.path, "the pinned host at ")?;
        if actual != self.md5 {
            bail!(
                "{} has md5 {actual}; the pin's [sapi] says {}. Report the drift; the pin moves only by a person's hand",
                self.path.display(),
                self.md5
            );
        }
        Ok(())
    }
}

/// The md5 of the file at `path`, as the pin names a binary by, `what`
/// naming it before its path where the file cannot be read.
fn md5_of(path: &Path, what: &str) -> Result<String> {
    let bytes = fs::read(path).with_context(|| format!("reading {what}{}", path.display()))?;
    Ok(format!("{:x}", md5::compute(bytes)))
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
    /// The Homebrew formula the binary's keg is poured from, `php` where
    /// the pin names none (#4383): what `scripts/ci/pinned-php.sh` pours,
    /// and the keg [`Pin::defects`] holds the pinned paths to. A
    /// variant's binary is built and not poured, and keeps the main
    /// oracle's.
    #[serde(default)]
    pub(crate) formula: Formula,
    pub path: PathBuf,
    pub md5: String,
    /// The oracle variant this binary is, none for `[oracle]`'s own: set
    /// by [`Pin::variant`], not a value of the file.
    #[serde(skip)]
    variant: Option<String>,
    /// Why this machine has no build of the variant to run, its platform
    /// pinning none: set by [`Pin::variant`], not a value of the file.
    #[serde(skip)]
    unpinned: Option<String>,
}

impl Binary {
    /// The pinned php on this machine: its path when a file is there, and
    /// otherwise an error naming the path and the version, and the variant
    /// where it is one. What a test that spawns php asks through
    /// [`Promise::or_skip`].
    pub fn file(&self) -> Result<&Path> {
        if let Some(why) = &self.unpinned {
            bail!("{why}");
        }
        if self.path.is_file() {
            Ok(&self.path)
        } else {
            bail!(
                "the pinned php {}{} is not at {}",
                self.version,
                self.of(),
                self.path.display()
            )
        }
    }

    /// Why no program can run under this binary on this machine, none where
    /// one can: what a lane's skip says for want of the oracle, the path it
    /// looked at, or the platform a variant pins no build for (#1890).
    pub fn absent(&self) -> Option<String> {
        if let Some(why) = &self.unpinned {
            return Some(why.clone());
        }
        (!self.path.is_file()).then(|| {
            format!(
                "the pinned php{} is not at {}",
                self.of(),
                self.path.display()
            )
        })
    }

    /// The oracle variant this binary is, none for the main oracle.
    pub fn variant(&self) -> Option<&str> {
        self.variant.as_deref()
    }

    /// What a reading taken under this binary adds to its message: the
    /// variant it was read under, nothing for the main oracle (#1890).
    pub(crate) fn read_under(&self) -> Option<String> {
        self.variant
            .as_ref()
            .map(|name| format!("read under the oracle variant {name}"))
    }

    /// The words that name the variant after `php`, none for the main
    /// oracle's.
    fn of(&self) -> String {
        self.variant
            .as_ref()
            .map(|name| format!(" of the oracle variant {name}"))
            .unwrap_or_default()
    }
}

/// `[source]`: the php-src tree at the same tag.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Source {
    pub tag: String,
    pub commit: String,
    /// Where the file says the tree is on the maintainer's machine, where
    /// it says so at all: the place [`Source::tree`] tries after
    /// [`TREE_VARIABLE`], so a pin without it loads and names the tree by
    /// the variable alone (#4260); stated by [`Source::path`] for the
    /// renderer.
    path: Option<PathBuf>,
    pub phpt_files: u64,
    /// The tree [`TREE_VARIABLE`] names on this machine, as [`Pin::load`]
    /// read it or a test handed it, its value as written; not a value of
    /// the file, and absent under [`Pin::parse`], which is the pin as the
    /// file says it.
    #[serde(skip)]
    named: Option<PathBuf>,
}

/// What [`Source::tree`] found the pinned php-src through: one place it
/// looks, in the order it looks, and what a refusal there names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Through {
    /// The directory [`TREE_VARIABLE`] names, a leading `~/` or `$HOME/`
    /// read as the home directory.
    Variable,
    /// `[source].path`, as the pin states it.
    Pin,
}

impl fmt::Display for Through {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Through::Variable => write!(f, "named by {TREE_VARIABLE}"),
            Through::Pin => f.write_str("the pin's path"),
        }
    }
}

impl Source {
    /// Where the pinned php-src tree is on this machine: the directory
    /// [`TREE_VARIABLE`] names when it is one, else the pin's path when the
    /// pin states one and it is one, held to the pin's commit. The one
    /// answer the phpt runner and the tests that resolve a subset take, so
    /// the location, alone among the pin's facts, is machine-local (#36).
    /// The variable's value is read as written, but for a leading `~/` or
    /// `$HOME/`, which is the home directory: Claude Code passes a
    /// setting's `env` value with no shell between, and nothing else in
    /// it is expanded (#4260). An absent tree is an error naming every
    /// place tried, so a runner's failure reads as an unset variable and
    /// not as a path that could never have existed there. A tree that is
    /// a checkout at another commit is refused naming both revisions and
    /// is never passed over for the other place (#37): a corpus at the
    /// wrong revision measures the wrong thing, more quietly than no
    /// corpus at all. A tree that is no checkout, a tarball unpacked at
    /// the tag, states no commit and is taken at its word.
    pub fn tree(&self) -> Result<PathBuf> {
        self.found().map(|(_, tree)| tree)
    }

    /// [`Source::tree`] and the place it found the tree through, which
    /// `status --section oracle` renders.
    pub(crate) fn found(&self) -> Result<(Through, PathBuf)> {
        let places = self.places();
        if let Some((place, tree)) = places.iter().find(|(_, tree)| tree.is_dir()) {
            self.held(tree, *place)?;
            return Ok((*place, tree.clone()));
        }
        let read = places.iter().find(|(place, _)| *place == Through::Variable);
        let variable = match (&self.named, read) {
            (Some(named), Some((_, read))) if read != named => format!(
                "{TREE_VARIABLE}={}, read as {}, is not a directory",
                named.display(),
                read.display()
            ),
            (Some(named), _) => format!("{TREE_VARIABLE}={} is not a directory", named.display()),
            (None, _) => format!("{TREE_VARIABLE} is unset"),
        };
        let pinned = match &self.path {
            Some(path) => format!("the pin's path {} is not a directory", path.display()),
            None => "the pin states no `[source].path`".to_owned(),
        };
        bail!(
            "the pinned php-src ({}) is at neither place tried: {variable}, and {pinned}",
            self.tag
        )
    }

    /// The places [`Source::tree`] tries, in its order: the variable's
    /// value, its home expanded, where it is set, and the pin's path where
    /// the pin states one.
    pub(crate) fn places(&self) -> Vec<(Through, PathBuf)> {
        let named = self.named.as_deref().map(at_home);
        let named = named.map(|tree| (Through::Variable, tree));
        let pinned = self.path.clone().map(|path| (Through::Pin, path));
        named.into_iter().chain(pinned).collect()
    }
}

/// `value` with a leading `~/` or `$HOME/` read as this process's home
/// directory, as [`expanded`] reads it: how a variable that names a place
/// on this machine is read, [`TREE_VARIABLE`] and the rig's (#4367).
pub(crate) fn at_home(value: &Path) -> PathBuf {
    let home = std::env::var_os("HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from);
    expanded(value, home.as_deref())
}

/// `value` with a leading `~/` or `$HOME/` read as `home`, and taken as
/// written otherwise: `x~/y`, `$HOMEX/y`, a bare `~` and any value where
/// `home` is unknown are literal, as no shell stands between a setting
/// and the variable to expand anything else (#4260).
fn expanded(value: &Path, home: Option<&Path>) -> PathBuf {
    let bytes = value.as_os_str().as_encoded_bytes();
    let lead = ["~", "$HOME"].into_iter().find(|lead| {
        (bytes.strip_prefix(lead.as_bytes())).is_some_and(|rest| rest.starts_with(b"/"))
    });
    match (lead, home) {
        (Some(lead), Some(home)) => match value.strip_prefix(lead) {
            Ok(rest) => home.join(rest),
            Err(_) => value.to_path_buf(),
        },
        _ => value.to_path_buf(),
    }
}

impl Source {
    /// Where the pin says the tree is, as the file states it, none where
    /// it states none: what `status --section oracle` renders beside
    /// [`Source::named`], never what a lane reads the tree from, which is
    /// [`Source::tree`].
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// What [`TREE_VARIABLE`] names on this machine, as [`Pin::load`] read
    /// it or a test handed it, as written; none where it is unset or
    /// empty.
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
    fn held(&self, tree: &Path, how: Through) -> Result<()> {
        if !checkout(tree) {
            return Ok(());
        }
        let head = crate::git::elsewhere(tree, &["rev-parse", "--verify", "HEAD^{commit}"])
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
            let listed = crate::git::elsewhere(tree, &["ls-files", "-z"])
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

impl Recipe {
    /// `program`'s environment as the recipe has it: cleared where
    /// `env_clear` says, then `[recipe.env]` as this run gives it
    /// ([`Recipe::environment`]), so nothing of the shell's reaches what it
    /// runs. Every process the harness runs under the recipe starts this
    /// way, the oracle and the linked engine alike.
    pub(crate) fn environ<'c>(&self, program: &'c mut Command) -> Result<&'c mut Command> {
        if self.env_clear {
            program.env_clear();
        }
        Ok(program.envs(self.environment()?))
    }

    /// `[recipe.env]` as this run's programs get it: [`RUN`] in a value is
    /// the run's own directory ([`run_dir`]), and the directory such a
    /// value names is laid down the first time it is named, with an empty
    /// `.php_history` in the one `HOME` names, so no program reaches the
    /// user's own home or temporary directory (#1705, #1730).
    pub(crate) fn environment(&self) -> Result<BTreeMap<String, String>> {
        let mut env = self.env.clone();
        for (name, value) in &mut env {
            if !value.contains(RUN) {
                continue;
            }
            *value = value.replace(RUN, &run_dir()?.display().to_string());
            match fs::create_dir(&*value) {
                Ok(()) if name == "HOME" => {
                    let history = Path::new(&*value).join(".php_history");
                    fs::write(&history, "")
                        .map_err(|err| scratch::made(err, &history, "laying down"))?;
                }
                Ok(()) => {}
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(err) => return Err(scratch::made(err, Path::new(&*value), "laying down")),
            }
        }
        Ok(env)
    }
}

/// What a value of `[recipe.env]` names the run's own directory by, as a
/// test's `--ENV--` names the scratch one `{TMP}` (#1730).
pub(crate) const RUN: &str = "{RUN}";

/// The run's own directory, one per harness process: a scratch tree made
/// the first time a program names it ([`Recipe::environment`]) and
/// removed when the run ends ([`run_ended`]), or, where the process is
/// killed first, by the next run's sweep. It stands under `/tmp` and not
/// the temporary directory: a test binds a unix socket to a name
/// `tempnam(sys_get_temp_dir(), ...)` makes, and macOS's per-user
/// temporary directory, 57 bytes resolved, leaves too few of a socket
/// path's 104 for the run's directory and the name (`sapi/cli`'s
/// 022.phpt). [`Tree::short`] lays it there.
static RUN_DIR: Mutex<Option<Tree>> = Mutex::new(None);

/// Where the run's own directory stands, made now if no program has
/// named it yet.
pub(crate) fn run_dir() -> Result<PathBuf> {
    let mut held = crate::locked(&RUN_DIR);
    if let Some(tree) = &*held {
        return Ok(tree.path().to_path_buf());
    }
    let tree = Tree::short("run")?;
    let path = tree.path().to_path_buf();
    *held = Some(tree);
    Ok(path)
}

/// The run's own directory removed, as the run ends.
pub(crate) fn run_ended() {
    crate::locked(&RUN_DIR).take();
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
    pub extension_number_token: Option<String>,
}

/// `[regime.<name>]`: what a run under the regime is handed beyond the
/// recipe, each after the recipe's own: `ini`, as `-d` after the recipe's
/// ini, and `args`, after the recipe's args, for what no ini setting
/// reaches, as php's `-e` reaches `ZEND_COMPILE_EXTENDED_INFO` (#1780). A
/// regime names either or both, and one that names neither is the recipe.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Regime {
    #[serde(default)]
    pub ini: Vec<String>,
    #[serde(default)]
    pub args: Vec<String>,
}

/// What a regime's line names the workspace root by (#1634): a file of
/// the workspace a regime hands a host, as `[regime.ffi-preload]` hands
/// `opcache.preload` one, stands in the pin as `{workspace}/<path>`, and
/// [`Pin::load`] writes the root in, so the oracle's run and the engine's
/// are handed the one path.
pub(crate) const WORKSPACE: &str = "{workspace}";

impl Regime {
    /// Whether one of the regime's ini lines sets `directive`.
    pub(crate) fn sets(&self, directive: &str) -> bool {
        self.value(directive).is_some()
    }

    /// The value the regime's ini lines set `directive` to, the last
    /// where several do, as php reads repeated `-d`s.
    pub(crate) fn value(&self, directive: &str) -> Option<&str> {
        (self.ini.iter()).rev().find_map(|line| {
            let (name, value) = line.split_once('=')?;
            (name.trim() == directive).then(|| value.trim())
        })
    }

    /// [`WORKSPACE`] in each of the regime's lines, its ini and its
    /// arguments, the workspace at `root`.
    fn placed(&mut self, root: &Path) {
        let root = root.display().to_string();
        for line in self.ini.iter_mut().chain(&mut self.args) {
            *line = line.replace(WORKSPACE, &root);
        }
    }
}

/// The one argument of php's command line an engine linked into the
/// harness takes, as every host reads it: `-e`, which a host hands its
/// engine's builder as extended info (#1780, #1822).
pub(crate) const EXTENDED_INFO: &str = "-e";

/// One or more regimes as an adapter with no command line takes them,
/// the engine linked into the harness: their ini in the order named, laid
/// over the engine's defaults as a host's `-d` lays it, and what their
/// arguments ask of the engine as it is built, which is
/// [`EXTENDED_INFO`] and nothing else. Any other argument is refused,
/// since it reaches a binary's command line and no linked engine, and
/// dropping it from one side of a comparison alone is the flag
/// swallowed silently #1780 rejects. The caller names the regime.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Linked {
    pub(crate) ini: Vec<String>,
    /// [`EXTENDED_INFO`] was among the arguments.
    pub(crate) extended_info: bool,
}

impl Linked {
    /// `regimes` in the order a binary's run hands them.
    pub(crate) fn of<'a>(regimes: impl IntoIterator<Item = &'a Regime>) -> Result<Linked> {
        let mut linked = Linked::default();
        for regime in regimes {
            let unknown: Vec<&String> = regime
                .args
                .iter()
                .filter(|arg| *arg != EXTENDED_INFO)
                .collect();
            if !unknown.is_empty() {
                bail!(
                    "carries the arguments {:?}, which reach a binary's command line and no linked engine; the linked engine takes `{EXTENDED_INFO}` alone",
                    regime.args
                );
            }
            linked.extended_info |= !regime.args.is_empty();
            linked.ini.extend(regime.ini.iter().cloned());
        }
        Ok(linked)
    }
}

impl Pin {
    /// Load the pin at `path` as this machine has it: the file's facts
    /// placed in the directory it stands in, the workspace root, where a
    /// variant's relative path and a regime's [`WORKSPACE`] are read; and
    /// the tree [`TREE_VARIABLE`] names in the process's environment.
    pub fn load(path: &Path) -> Result<Pin> {
        let text = fs::read_to_string(path)
            .with_context(|| format!("reading the oracle pin {}", path.display()))?;
        let mut pin = Pin::parse(&text).with_context(|| format!("{}", path.display()))?;
        if let Some(dir) = path.parent() {
            for variant in pin.variant.values_mut() {
                variant.placed(dir);
            }
            for regime in pin.regime.values_mut() {
                regime.placed(dir);
            }
        }
        // An empty value is unset, as `getenv` reads it in run-tests.php.
        let named = std::env::var_os(TREE_VARIABLE)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from);
        Ok(pin.on_this_machine(named))
    }

    /// Parse the pin's text and hold it to the grammar. The pin is as the
    /// file says it, blind to this machine; `load` consults the machine.
    pub fn parse(text: &str) -> Result<Pin> {
        let mut pin: Pin = toml::from_str(text)?;
        let closure = pin.closure.as_ref().map(Closure::digest);
        pin.defects = (pin.variant.iter())
            .filter_map(|(name, variant)| variant.held(name, closure.as_ref()).err())
            .map(|err| format!("{err:#}"))
            .collect();
        let poured = pin
            .oracle
            .formula
            .held(&pin.oracle.version, &pin.binaries());
        pin.defects.extend(poured);
        pin.digest = digest(text)?;
        Ok(pin)
    }

    /// The pinned binaries a keg holds, `php` and each host `[sapi]`
    /// names, each by the key that names its path, and php again where
    /// `[recipe.env]` exports it to the programs it runs, as
    /// run-tests.php exports the binary under test. No digest is asserted
    /// at that one, so a bump that left it behind, as the last left the
    /// hosts (#3374), would run the keg before it, which stays installed.
    fn binaries(&self) -> Vec<(&'static str, &Path)> {
        let hosts = [
            ("[sapi].cgi.path", &self.sapi.cgi),
            ("[sapi].fpm.path", &self.sapi.fpm),
            ("[sapi].phpdbg.path", &self.sapi.phpdbg),
        ];
        let hosts = hosts
            .into_iter()
            .filter_map(|(key, host)| Some((key, host.as_ref()?.path.as_path())));
        let exported = self.recipe.env.get("TEST_PHP_EXECUTABLE").map(Path::new);
        let exported = exported.map(|path| ("[recipe.env].TEST_PHP_EXECUTABLE", path));
        std::iter::once(("[oracle].path", self.oracle.path.as_path()))
            .chain(hosts)
            .chain(exported)
            .collect()
    }

    /// What parses in the pin but does not validate, a cross-file check's
    /// refusal each: the instruments that read the pin refuse with it, and
    /// the rest judge by what loaded (#3618 ruling 4).
    pub fn defects(&self) -> &[String] {
        &self.defects
    }

    /// The digest of the pin's values, every section's and none of its
    /// comments, leaving out each build's [`Provenance`] and nothing else
    /// (#2969): what a reading taken under the oracle's recipe records, so
    /// a change to the recipe, its environment or a binary stales it as a
    /// change to its crate's tree does (#1387), and a record of what a
    /// binary was made from does not. Computed by [`digest`] and nowhere
    /// else.
    pub fn digest(&self) -> &str {
        &self.digest
    }

    /// The pin with the tree [`TREE_VARIABLE`] names on this machine:
    /// `load` hands the process's value, and a test hands the tree it laid
    /// down, which [`Source::tree`] then answers ahead of the pin's path.
    #[must_use]
    pub fn on_this_machine(mut self, named: Option<PathBuf>) -> Pin {
        self.source.named = named;
        self
    }

    /// The pin with every program run under `wrapper`, a command that ends
    /// in the binary and its arguments, as `unshare --ipc --` runs one in a
    /// namespace of its own (#1482). The digest is the file's still: what
    /// the reading records is the recipe, and the wrapper is the run's.
    #[must_use]
    pub(crate) fn wrapped(mut self, wrapper: Vec<String>) -> Pin {
        self.wrapper = wrapper;
        self
    }

    /// The HTTP-to-FastCGI mapping, refused where the pin carries none.
    pub fn fastcgi(&self) -> Result<&Mapping> {
        self.fastcgi
            .as_ref()
            .context("the pin carries no [fastcgi] mapping (#2334)")
    }

    /// The regime by name; an unknown name is refused beside the known ones.
    pub fn regime(&self, name: &str) -> Result<&Regime> {
        self.regime.get(name).with_context(|| {
            format!(
                "no regime {name:?} in the pin; the regimes are {}",
                joined(self.regime.keys(), ", ")
            )
        })
    }

    /// The pin a crate's lanes run under where its row names the oracle
    /// variant `name` (#1782): this pin with the variant's build for this
    /// machine's [`Platform`] standing in for `[oracle]`'s binary, so every
    /// lane, adapter and digest assertion reads it as it reads the main
    /// oracle. The recipe is the main oracle's, as the variant's configure
    /// line is the main oracle's plus its flags; an environment entry that
    /// names the main oracle's binary names the variant's, as run-tests.php
    /// exports the binary under test; and no host of `[sapi]` is the
    /// variant's, so none is pinned beside it. The digest is the file's,
    /// which covers every variant. Where the variant pins no build for this
    /// machine the binary says so through [`Binary::absent`], the skip a
    /// lane writes for an absent oracle; a name the pin does not carry is
    /// refused beside the names it does.
    pub fn variant(&self, name: &str) -> Result<Pin> {
        let Some(variant) = self.variant.get(name) else {
            return Err(self.unknown(name));
        };
        let platform = Platform::this();
        let build = platform.and_then(|platform| variant.build(platform));
        let unpinned = match (platform, build) {
            (_, Some(_)) => None,
            (Some(platform), None) => Some(format!(
                "the oracle variant {name} pins no build for {}",
                platform.key()
            )),
            (None, None) => Some(format!(
                "the oracle variant {name} pins no build for this machine ({}-{}), which is none of the platforms a build is pinned for",
                std::env::consts::OS,
                std::env::consts::ARCH
            )),
        };
        let mut pin = self.clone();
        pin.asserted = Asserted::default();
        pin.sapi = Sapi::default();
        pin.oracle = Binary {
            version: self.oracle.version.clone(),
            formula: self.oracle.formula,
            path: build.map(|b| b.path.clone()).unwrap_or_default(),
            md5: build.map(|b| b.md5.clone()).unwrap_or_default(),
            variant: Some(name.to_owned()),
            unpinned,
        };
        let main = &self.oracle.path;
        for value in pin.recipe.env.values_mut() {
            if !main.as_os_str().is_empty() && Path::new(value.as_str()) == main {
                *value = pin.oracle.path.display().to_string();
            }
        }
        Ok(pin)
    }

    /// The refusal of a variant name the pin does not carry, naming it
    /// beside the names it does.
    fn unknown(&self, name: &str) -> anyhow::Error {
        let names: Vec<&str> = self.variant.keys().map(String::as_str).collect();
        anyhow::anyhow!(
            "no oracle variant {name:?} in the pin; the variants are {}",
            if names.is_empty() {
                "none".to_owned()
            } else {
                crate::reading::listed(&names)
            }
        )
    }

    /// The pin as one run holds it: the digest is asserted before the
    /// first program runs through the value and never again, so a lane of
    /// any length hashes the binary once. Nothing runs under the pin
    /// except through this.
    pub fn oracle(&self) -> Oracle<'_> {
        Oracle {
            pin: self,
            digest: OnceLock::new(),
            spawned: Spawned::default(),
            place: Place::Named,
        }
    }

    /// How many times the digest was asserted through this pin: one per
    /// run is the rule, and a test reads it.
    pub fn digests_asserted(&self) -> u64 {
        self.asserted.0.load(Ordering::Relaxed)
    }

    /// The binary's digest against the pin's, and the kegs it and the
    /// `[sapi]` hosts load against `[closure]`, each `brew pin`ned as the
    /// keg they stand in is; a mismatch refuses the run.
    fn assert_digest(&self) -> Result<()> {
        self.asserted.0.fetch_add(1, Ordering::Relaxed);
        if let Some(why) = &self.oracle.unpinned {
            bail!("{why}");
        }
        let path = &self.oracle.path;
        let actual = md5_of(path, "the pinned php at ")?;
        if actual != self.oracle.md5 {
            bail!(
                "{} has md5 {actual}; the pin says {} for php {}{}. Report the drift; the pin moves only by a person's hand",
                path.display(),
                self.oracle.md5,
                self.oracle.version,
                self.oracle.of()
            );
        }
        let Some(closure) = &self.closure else {
            return Ok(());
        };
        let hosts = [&self.sapi.cgi, &self.sapi.fpm, &self.sapi.phpdbg];
        let hosts = hosts.into_iter().flatten().map(|host| host.path.as_path());
        let binaries: Vec<&Path> = std::iter::once(path.as_path())
            .chain(hosts.filter(|host| host.is_file()))
            .collect();
        closure.held(&binaries)
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
    /// No program: an empty word where its path goes, as run-tests.php
    /// hands `-f ""` to a `--PHPDBG--` test with no `FILE`
    /// (run-tests.php:2275-2279), whose commands name what to run.
    Nothing,
}

/// What one run wrote and how it ended, normalised.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observation {
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// How the process ended: its exit status, or the signal.
    pub exit: Ended,
    /// Wall time, disclosure only; the limit is the recipe's.
    pub wall: Duration,
}

/// How a process ended: the status it exited with, or the signal that
/// ended it, which a lane names where a binary died (#1069), and a phpt
/// run keeps where its program died (#4130), as `{"signal":11}` or
/// `{"status":139}`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Ended {
    /// It exited with this status.
    Status(i32),
    /// This signal ended it.
    Signal(i32),
}

impl Ended {
    /// How a process that `status` reports on ended.
    pub(crate) fn of(status: std::process::ExitStatus) -> Ended {
        use std::os::unix::process::ExitStatusExt;
        match status.code() {
            Some(code) => Ended::Status(code),
            None => Ended::Signal(status.signal().unwrap_or_default()),
        }
    }
}

impl fmt::Display for Ended {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Ended::Status(code) => write!(f, "status {code}"),
            Ended::Signal(signal) => write!(f, "signal {signal}"),
        }
    }
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
    /// What it left standing outside its group, stopped with it
    /// ([`crate::scratch::stop_naming`]).
    pub(crate) stopped: Vec<u32>,
}

impl fmt::Display for PastLimit {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} ran past the recipe's limit of {} s on {} and was killed",
            self.binary.display(),
            self.limit_s,
            self.script.display()
        )?;
        let stopped: Vec<String> = self.stopped.iter().map(u32::to_string).collect();
        match stopped.as_slice() {
            [] => Ok(()),
            pids => write!(
                f,
                ", with what it left outside its group: {}",
                pids.join(", ")
            ),
        }
    }
}

impl std::error::Error for PastLimit {}

/// What one program run by [`Oracle::to_end`] came to: its two streams,
/// how it ended, none where it ran past the limit and was killed, its wall
/// time, and what the caller did while it ran.
#[derive(Debug)]
pub(crate) struct ToEnd<T> {
    pub(crate) stdout: Vec<u8>,
    pub(crate) stderr: Vec<u8>,
    pub(crate) exit: Option<Ended>,
    pub(crate) wall: Duration,
    pub(crate) during: T,
}

/// A program [`Oracle::to_end`] has started and not yet waited for: its
/// process id, and how much it has written to stdout so far.
pub(crate) struct Running<'a> {
    pub(crate) pid: u32,
    taken: &'a Taken,
}

impl Running<'_> {
    /// The bytes on stdout so far, stderr's among them where it is merged.
    pub(crate) fn written(&self) -> u64 {
        match self.taken {
            Taken::Files { out, .. } => fs::metadata(out).map_or(0, |m| m.len()),
            Taken::Pipes { out, .. } => out
                .bytes
                .lock()
                .map_or(0, |held| u64::try_from(held.len()).unwrap_or(u64::MAX)),
        }
    }
}

/// One invocation under the pin's recipe: which binary runs and which
/// program, and what the run carries over the recipe. Each field past the
/// program ends its rustdoc in one of harness-design.md §4.3's two groups,
/// what a `.phpt` section adds or what no section gives, and §4.3 renders
/// them from here (`status --section invocation`, #1319). Every field past
/// the program is empty for a witness, which is why `run` takes none of
/// them.
#[derive(Debug, Clone)]
pub struct Invocation<'a> {
    /// The pinned php for the oracle; the engine's own binary for a phpt
    /// lane, so that both run under one recipe.
    pub binary: &'a Path,
    pub program: Program<'a>,
    /// The `[regime.<name>]`s of the pin the run stands under, in order.
    /// Each one's args come after the recipe's and the regimes' before it,
    /// and each one's ini after the recipe's and the regimes' before it,
    /// so a lane composes the rows a person wrote rather than a row of its
    /// own (`compile-identity` dumps under `dump` and one more, #1822).
    ///
    /// No section gives it.
    pub regimes: &'a [&'a str],
    /// Ini after the recipe's and the regime's: a test's `--INI--`.
    ///
    /// A `.phpt` section adds it.
    pub ini: &'a [String],
    /// Flags before the program's path, where php takes a mode of its own.
    /// `-l`, which compiles a file and does not run it, is the
    /// compile-identity lane's, and both binaries get it from here so that
    /// one recipe stands behind both. Empty for a phpt test and a witness,
    /// which run what they are given.
    ///
    /// No section gives it.
    pub flags: &'a [String],
    /// Arguments after the program's path: a test's `--ARGS--`.
    ///
    /// A `.phpt` section adds it.
    pub args: &'a [String],
    /// Environment entries over the recipe's, a later one of a name over
    /// an earlier. They are a test's environment as run-tests.php builds
    /// it, and the request a CGI-shaped test describes, whose values are
    /// the bytes the test wrote and no encoding of them. An entry whose value is empty takes
    /// the name out of the environment, the recipe's included, as php's
    /// `proc_open` leaves an empty value out of the environment it hands a
    /// program (`php_array_to_envp`, `ext/standard/proc_open.c:174`), and
    /// run-tests.php hands every program through it (#1186).
    ///
    /// A `.phpt` section adds it.
    pub env: &'a [(String, OsString)],
    /// What the program reads on stdin: a test's `--STDIN--`, or a
    /// CGI-shaped test's body.
    ///
    /// A `.phpt` section adds it.
    pub stdin: Stdin<'a>,
    /// How the program's two streams out are taken: to files apart, or
    /// through pipes as run-tests.php takes a test's, stderr into stdout
    /// where its `--CAPTURE_STDIO--` captures both.
    ///
    /// A `.phpt` section adds it.
    pub streams: Streams,
    /// The working directory: the program's own where `None`, which is
    /// a witness's; php-src's root for a phpt test and the sections run
    /// beside it, as run-tests.php runs them (#952).
    ///
    /// No section gives it.
    pub cwd: Option<&'a Path>,
    /// Whether what the program wrote is handed back as it wrote it and
    /// not normalised: a CGI run's, whose headers run-tests.php parts
    /// from the body before it trims (run-tests.php:2530-2533).
    ///
    /// No section gives it.
    pub raw: bool,
}

impl<'a> Invocation<'a> {
    /// `binary` running `program` under the recipe and nothing more.
    pub fn new(binary: &'a Path, program: Program<'a>) -> Invocation<'a> {
        Invocation {
            binary,
            program,
            regimes: &[],
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
                let create = |path: &Path| {
                    File::create(path).map_err(|e| scratch::made(e, path, "creating"))
                };
                cmd.stdout(create(&out)?).stderr(create(&err)?);
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
        std::io::Write::write_all(&mut pipe, &bytes).ok();
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
            tell.send(()).ok();
        });
        Drained { bytes, done }
    }

    /// What came through the pipe: all of it once its last writer closed
    /// it, or what had come by `until` where a process the program started
    /// still holds it open.
    fn taken(self, until: Instant) -> Vec<u8> {
        self.done
            .recv_timeout(until.saturating_duration_since(Instant::now()))
            .ok();
        self.bytes
            .lock()
            .map(|mut held| std::mem::take(&mut *held))
            .unwrap_or_default()
    }
}

/// The process group a program leads, which it was spawned into
/// (`process_group(0)`) so that whatever it starts is in the group too: a
/// server a test forks, a `php -S`, a worker it leaves running (#1211). The
/// group is killed whole when this value drops, past the limit, once the
/// program has exited and its pipes have reached their end, and on every
/// error between, so nothing a program started outlives its run, as
/// `proc_terminate` ends upstream's on its timeout (run-tests.php
/// `system_with_timeout`). A kill that finds the group empty is no error.
///
/// A run killed outright drops nothing, so its programs stand past the
/// limit; the next run that reads its journal takes their groups as
/// [`Group::standing`] finds them, and drops them (#2522).
pub(crate) struct Group(nix::unistd::Pid);

impl Group {
    fn of(child: &std::process::Child) -> Result<Group> {
        let pid = i32::try_from(child.id()).context("a process id past pid_t")?;
        Ok(Group(nix::unistd::Pid::from_raw(pid)))
    }

    /// The group a program spawned as `pid` leads, where the process
    /// running under that number now is that program, as
    /// [`Process::spawned_as`] tells it by `last`, the second of its run's
    /// journal's last write: one of a cut-off run's that still stands, and
    /// never a process that took the number since.
    pub(crate) fn standing(pid: u32, last: i64) -> Option<Group> {
        Process::of(pid).filter(|found| found.spawned_as(pid, last))?;
        Some(Group(nix::unistd::Pid::from_raw(i32::try_from(pid).ok()?)))
    }
}

/// A process as `ps` reads it: the group it is in, and the second since
/// the epoch it started at, to the whole second `ps` prints its age in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Process {
    pub group: u32,
    pub started: i64,
}

impl Process {
    /// The process running under `pid` now, none where none runs.
    fn of(pid: u32) -> Option<Process> {
        let ps = Command::new("ps")
            .args(["-o", "pgid=,etime=", "-p", &pid.to_string()])
            .output()
            .ok()?;
        let now = SystemTime::now().duration_since(std::time::UNIX_EPOCH);
        Process::read(&String::from_utf8_lossy(&ps.stdout), now.ok()?.as_secs())
    }

    /// The process `ps -o pgid=,etime=` printed `now`, a second since the
    /// epoch: its age is `[[dd-]hh:]mm:ss` on macOS and under procps alike.
    pub(crate) fn read(printed: &str, now: u64) -> Option<Process> {
        let (group, etime) = printed.trim().split_once(char::is_whitespace)?;
        let (days, clock) = etime.trim().split_once('-').unwrap_or(("0", etime.trim()));
        let age = clock
            .split(':')
            .try_fold(0, |age, part| Some(age * 60 + part.parse::<i64>().ok()?))?
            + days.parse::<i64>().ok()? * 86_400;
        Some(Process {
            group: group.parse().ok()?,
            started: i64::try_from(now).ok()? - age,
        })
    }

    /// Whether this is the program spawned as `pid` that a run's journal
    /// names started and never ended by `last`, the second of its last
    /// write: a process that leads its own group under that number, as
    /// every program is spawned (`process_group(0)`), and started by then,
    /// a second's give for the whole seconds `ps` prints. No two living
    /// processes share a number, so one that took it started once the
    /// program had ended, past its journal's last write; and one in
    /// another group is not a program's.
    pub(crate) fn spawned_as(self, pid: u32, last: i64) -> bool {
        self.group == pid && self.started <= last + 1
    }
}

impl Drop for Group {
    fn drop(&mut self) {
        nix::sys::signal::killpg(self.0, nix::sys::signal::Signal::SIGKILL).ok();
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
    /// Every process a program of the run was spawned as.
    spawned: Spawned,
    /// Where the run's programs stand.
    place: Place,
}

/// Where the scratch trees a run's programs stand in are made: under the
/// names their callers give them, or under longer names apart from those.
/// The second is how `witness --accept --streams` runs a fence again from
/// a directory whose path differs from the first's in its length as well
/// as its name, so a number the path moves, a `string(92)` or a memory
/// count over `$_SERVER`, differs between the two runs and is refused
/// rather than written (#801).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Place {
    Named,
    Apart,
}

impl Place {
    /// A scratch tree named for `name` in this place, at a path of one
    /// length whatever the session and the process ([`Tree::short`]).
    pub(crate) fn tree(self, name: &str) -> Result<Tree> {
        match self {
            Place::Named => Tree::short(name),
            Place::Apart => Tree::short(&format!("{name}-apart-from-the-first")),
        }
    }
}

/// The processes the programs of one run were spawned as, each the leader
/// of its group, kept until a lane takes them: the one record of which
/// processes are the run's, which is how what they left of System V IPC
/// is told from what any other process on the machine holds (#1412), and
/// of when each ran, from spawn to exit, which is how an object the
/// kernel names no process on is told as made during one of them (#1482).
///
/// Where a lane holds a System V claim, each program's start and end is
/// written to the claim's file as well, so its last write bounds the
/// window a run cut off left behind it (`sysv::Claim`, #2448).
#[derive(Debug, Default)]
pub(crate) struct Spawned {
    programs: Mutex<Programs>,
    journal: Mutex<Option<File>>,
}

/// What [`Spawned`] kept since the last take: each program's process, and
/// each program's run as the wall clock read it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Programs {
    pub pids: BTreeSet<u32>,
    pub runs: Vec<Ran>,
    /// The number the next program is given, so its exit finds its own
    /// run whatever was taken in between.
    next: u64,
}

/// One program's run by the wall clock: from just before its spawn to
/// just after its exit, the exit none where the run ended in an error
/// before the program was seen to exit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Ran {
    pub from: SystemTime,
    pub to: Option<SystemTime>,
    number: u64,
}

impl Ran {
    /// A run from `from` to `to`, as a lane's test writes one.
    #[cfg(test)]
    pub(crate) fn between(from: SystemTime, to: Option<SystemTime>) -> Ran {
        Ran {
            from,
            to,
            number: 0,
        }
    }
}

impl Spawned {
    /// One program spawned as `pid` at `from`; the number
    /// [`Spawned::ended`] closes its run by.
    fn add(&self, pid: u32, from: SystemTime) -> u64 {
        let mut programs = crate::locked(&self.programs);
        let number = programs.next;
        programs.next += 1;
        programs.pids.insert(pid);
        programs.runs.push(Ran {
            from,
            to: None,
            number,
        });
        self.journal(format_args!("{} {number} {pid}", crate::sysv::STARTED));
        number
    }

    /// `line` written to the journal where there is one. A write that
    /// fails leaves the window its last write bounds, which the lane's
    /// own sweep settles whole when the run is not cut off.
    fn journal(&self, line: fmt::Arguments<'_>) {
        let journal = crate::locked(&self.journal);
        if let Some(mut file) = journal.as_ref() {
            writeln!(file, "{line}").ok();
        }
    }

    /// The program numbered `number` seen to exit now; one a take has
    /// forgotten since is not looked for.
    fn ended(&self, number: u64) {
        let now = SystemTime::now();
        let mut programs = crate::locked(&self.programs);
        if let Some(ran) = programs.runs.iter_mut().rev().find(|r| r.number == number) {
            ran.to = Some(now);
        }
        drop(programs);
        self.journal(format_args!("{} {number}", crate::sysv::ENDED));
    }

    /// The programs spawned since the last take, and none kept after but
    /// the count, so a number is never given twice.
    fn take(&self) -> Programs {
        let mut programs = crate::locked(&self.programs);
        let next = programs.next;
        std::mem::replace(
            &mut *programs,
            Programs {
                next,
                ..Programs::default()
            },
        )
    }
}

impl<'a> Oracle<'a> {
    /// The pin: its recipe and its regimes, which a lane reads beside the
    /// runs.
    pub fn pin(&self) -> &'a Pin {
        self.pin
    }

    /// The same run under `pin`, a copy of this one's pin that differs in
    /// what a subset changes, its environment or its wrapper: the digest
    /// as this one asserted it, so the binary is not hashed again, and
    /// the programs it spawns its own to take.
    pub(crate) fn beside<'b>(&self, pin: &'b Pin) -> Oracle<'b> {
        let digest = OnceLock::new();
        if let Some(found) = self.digest.get() {
            digest.set(found.clone()).ok();
        }
        Oracle {
            pin,
            digest,
            spawned: Spawned::default(),
            place: Place::Named,
        }
    }

    /// The same run with its programs standing [`Place::Apart`]: the
    /// digest as this one asserted it, and the programs it spawns its own.
    pub(crate) fn apart(&self) -> Oracle<'a> {
        Oracle {
            place: Place::Apart,
            ..self.beside(self.pin)
        }
    }

    /// A scratch tree named for `name` where this run's programs stand.
    pub(crate) fn tree(&self, name: &str) -> Result<Tree> {
        self.place.tree(name)
    }

    /// The processes this run's programs were spawned as since the last
    /// call, each program's own, the leader of its group, and not what it
    /// forked, and when each program ran.
    pub(crate) fn take_spawned(&self) -> Programs {
        self.spawned.take()
    }

    /// Each program's start and end written to `journal` from now, a
    /// System V claim's file, or to none (#2448).
    pub(crate) fn journal(&self, journal: Option<File>) {
        *crate::locked(&self.spawned.journal) = journal;
    }

    /// Run `program` under the pinned binary and the recipe, with
    /// `regime`'s args and ini after it when one is named, and return what
    /// it wrote, normalised. The digest is asserted before the first run
    /// through this value; the process runs with the recipe's environment
    /// alone, in the program's directory, under the recipe's timeout.
    pub fn run(&self, program: Program<'_>, regime: Option<&str>) -> Result<Observation> {
        self.asserted()?;
        self.run_under(&Invocation {
            regimes: regime.as_slice(),
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
        let regimes = call
            .regimes
            .iter()
            .map(|name| pin.regime(name))
            .collect::<Result<Vec<_>>>()?;
        let recipe = &pin.recipe;

        let scratch = Scratch::for_program(recipe, call.program, self.place)?;
        let mut cmd = match pin.wrapper.split_first() {
            Some((wrapper, before)) => {
                let mut cmd = Command::new(wrapper);
                cmd.args(before).arg(call.binary);
                cmd
            }
            None => Command::new(call.binary),
        };
        cmd.args(&recipe.args)
            .args(regimes.iter().flat_map(|r| &r.args));
        for ini in recipe
            .ini
            .iter()
            .chain(regimes.iter().flat_map(|r| &r.ini))
            .chain(call.ini)
        {
            cmd.arg("-d").arg(ini);
        }
        cmd.args(call.flags)
            .arg(&scratch.script)
            .args(call.args)
            .current_dir(call.cwd.unwrap_or(&scratch.dir));
        recipe.environ(&mut cmd)?;
        for (name, value) in call.env {
            if value.is_empty() {
                cmd.env_remove(name);
            } else {
                cmd.env(name, value);
            }
        }
        let limit = Duration::from_secs(recipe.limits.timeout_s);
        let ended = self.to_end(cmd, call.streams, call.stdin, limit, |_| ())?;
        let Some(exit) = ended.exit else {
            return Err(PastLimit {
                binary: call.binary.to_path_buf(),
                script: scratch.script.clone(),
                limit_s: recipe.limits.timeout_s,
                stopped: crate::scratch::stop_naming(&scratch.script),
            }
            .into());
        };
        let (stdout, stderr) = if call.raw {
            (ended.stdout, ended.stderr)
        } else {
            (
                scratch.normalise(&recipe.normalise, ended.stdout),
                scratch.normalise(&recipe.normalise, ended.stderr),
            )
        };
        Ok(Observation {
            stdout,
            stderr,
            exit,
            wall: ended.wall,
        })
    }

    /// `cmd` run to its end in a process group of its own, its streams
    /// taken as `how` says and `stdin` on its stdin, and `during` called
    /// with the program [`Running`] once it has started, before the wait:
    /// what every program of the run goes through, [`Oracle::run_under`]'s
    /// and a host's own line's (`witness`'s host lines, #1975). A program
    /// still running at `limit` is killed with its group and reaped, and
    /// comes back with no exit and nothing read.
    pub(crate) fn to_end<T>(
        &self,
        mut cmd: Command,
        how: Streams,
        stdin: Stdin<'_>,
        limit: Duration,
        during: impl FnOnce(&Running<'_>) -> T,
    ) -> Result<ToEnd<T>> {
        // The streams, and the program's stdin, stand in a tree apart from
        // the program's directory, so a snippet that lists or reads its
        // own directory finds its own file there and never these; a tree
        // and not a directory under the snippet's, since php's
        // `memory_get_usage()` counts the path in `$_SERVER` and a witness
        // pins that number.
        let streams = Tree::new("streams")?;
        let taken = Taken::set(&mut cmd, &streams, how)?;
        cmd.stdin(stdin_of(&streams, stdin)?).process_group(0);
        let program = PathBuf::from(cmd.get_program());

        let started = Instant::now();
        let from = SystemTime::now();
        let spawned = cmd.spawn();
        // The command holds the pipes' writing ends until it drops, and a
        // pipe ends only when its last writer closes it.
        drop(cmd);
        let mut child = spawned.with_context(|| format!("running {}", program.display()))?;
        let run = self.spawned.add(child.id(), from);
        let group = Group::of(&child)?;
        let feeding = fed(stdin, &mut child);
        let during = during(&Running {
            pid: child.id(),
            taken: &taken,
        });
        let status = loop {
            if let Some(status) = child.try_wait().context("waiting for php")? {
                self.spawned.ended(run);
                break status;
            }
            if started.elapsed() > limit {
                // The group killed while its leader still stands, so the
                // number is its own, and then the leader reaped, so it is
                // gone and not a zombie when the caller hears of it.
                drop(group);
                child.wait().context("reaping php past the limit")?;
                self.spawned.ended(run);
                return Ok(ToEnd {
                    stdout: Vec::new(),
                    stderr: Vec::new(),
                    exit: None,
                    wall: started.elapsed(),
                    during,
                });
            }
            thread::sleep(Duration::from_millis(5));
        };
        let wall = started.elapsed();
        // A pipe is read to its end before what the program started goes
        // with it, as upstream's `system_with_timeout` reads until `feof`
        // within its timeout: a child sharing the program's stdout writes
        // there after the program exits, as gh20582.phpt's `proc_open`ed
        // `php -r` does (#1491), and killing it first drops what it had
        // still to write. A process that holds the pipe open past the
        // limit leaves what came by then, and the kill then reaches it.
        let (stdout, stderr) = taken.read(started + limit)?;
        drop(group);
        if let Some(feeding) = feeding {
            feeding.join().ok();
        }
        Ok(ToEnd {
            stdout,
            stderr,
            exit: Some(Ended::of(status)),
            wall,
            during,
        })
    }

    /// A `[sapi]` host held before a program runs through it: the pin's
    /// digest and `[closure]` as [`Oracle::asserted`] holds them, the
    /// host's loads among the closure's, then the host's own digest.
    pub(crate) fn host_asserted(&self, host: &Host) -> Result<()> {
        self.asserted()?;
        host.assert_digest()
    }

    /// The pinned host of `kind`'s kind, its digest asserted, `[oracle]`
    /// itself for the CLI: the failure where the pin names none, none is
    /// at its path, or its digest is not the pin's.
    pub(crate) fn pinned_host(&self, kind: hosts::EngineHost) -> Result<PathBuf> {
        if kind == hosts::CLI {
            self.asserted()?;
            return Ok(self.pin.oracle.path.clone());
        }
        let host = self.pin.sapi.named(kind)?;
        self.host_asserted(host)?;
        Ok(host.path.clone())
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
/// ADR-0008): [`hosts::CLI`]'s.
pub const ENGINE: &str = hosts::CLI.binary;

/// The cargo line that builds every host of [`hosts::HOSTS`] in the
/// correctness lanes' profile ([`profile::LANE`], revision 82): their
/// crates by package, so the build turns on the features those crates
/// declare and no others. A line naming the binaries alone builds them
/// with every workspace member's features unified, the harness's
/// extensions among them, which is not the binary the lane measures.
pub(crate) fn engine_build() -> String {
    format!("cargo {}", profile::hosts().join(" "))
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
    /// the engine's own is held to the tree by [`against_the_tree`], over
    /// the crate directories of `layout`.
    ///
    /// The profiles are tried in [`profile::PREFERRED`]'s order, the
    /// lanes' own first (revision 82). A binary the tree has moved past is
    /// passed over for one it has not, so a profile is preferred among the
    /// binaries that are the tree's and not over them: before this, a
    /// release binary built once and never again was the lane's engine for
    /// every run after, however far the tree moved, and a debug build could
    /// not displace it (#475). Where every binary found is behind the tree,
    /// the first is the one the skip names.
    pub fn resolve(
        root: &Path,
        layout: &Layout,
        pin: &Pin,
        named: Option<&Path>,
    ) -> Result<Runner> {
        if let Some(path) = named {
            let path = named_binary(path)?;
            let digest = md5_of(&path, "")?;
            return Ok(if digest == pin.oracle.md5 {
                // The pinned oracle is a control and never this tree's
                // engine, so when the tree was last written says nothing
                // about it; the pin holds it by its digest instead.
                Runner::Control(path)
            } else {
                against_the_tree(root, layout, path)
            });
        }
        let mut behind = None;
        let target = root.join("target");
        for profile in profile::PREFERRED {
            let built = profile.binary(&target, ENGINE);
            if !built.is_file() {
                continue;
            }
            match against_the_tree(root, layout, built) {
                Runner::Stale(why) => {
                    behind.get_or_insert(Runner::Stale(why));
                }
                found => return Ok(found),
            }
        }
        if let Some(behind) = behind {
            return Ok(behind);
        }
        let looked: Vec<String> = profile::PREFERRED
            .iter()
            .map(|profile| {
                profile
                    .binary(Path::new("target"), ENGINE)
                    .display()
                    .to_string()
            })
            .collect();
        Ok(Runner::Absent(format!(
            "the engine's binary is not built: none of {}; a CLI request reaches `serve` with php_engine (design §5), and `{}` builds the lanes'",
            looked.join(", "),
            engine_build()
        )))
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
/// whether it is this tree's is when it was written against what it is
/// built from, the paths cargo's dep-info beside it names
/// ([`BuiltFrom::newest`]), which is what cargo decides a relink by: a
/// binary cargo leaves standing as fresh is fresh here, whatever else
/// under the crate directories was written since (#3109). A binary whose
/// write time cannot be read, and a tree with no engine source in it, are
/// left as the engine: this says a binary is behind the tree only where it
/// measured that it is.
///
/// The other hosts' binaries beside it ([`hosts::HOSTS`]) are what a
/// CGI-shaped test, `sapi/fpm/tests` and a `--PHPDBG--` test of the lane
/// run; since ADR-0008 each is built by a crate of its own, so a build of
/// `php` alone leaves them where they were: where one stands there it is
/// held to the tree with the engine's, and the lot is behind the tree when
/// any is. Each is held to its own dep-info, so writing `sapi/fpm` moves
/// no `php`, as cargo relinks none.
fn against_the_tree(root: &Path, layout: &Layout, path: PathBuf) -> Runner {
    let behind = hosts::HOSTS.iter().find_map(|host| {
        // The binary under test is `path`, whatever its name; the other
        // hosts stand beside it by theirs.
        let binary = if *host == hosts::CLI {
            path.clone()
        } else {
            path.with_file_name(host.binary)
        };
        let built = binary.metadata().and_then(|at| at.modified()).ok()?;
        let host_dir = Path::new(host.krate);
        let (source, written) = BuiltFrom::of(&binary).newest(root, layout, host_dir)?;
        (written > built).then_some((binary, source))
    });
    let Some((path, source)) = behind else {
        return Runner::Engine(path);
    };
    Runner::Stale(format!(
        "the engine's binary at {} was built before {} was last written, so a lane running it \
         would measure an engine this tree no longer describes while stamping the reading with \
         the tree. Rebuild it: `{}`, the build every correctness lane runs.",
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
            let path = streams.write("stdin", bytes)?;
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
    pub(crate) fn for_program(
        recipe: &Recipe,
        program: Program<'_>,
        place: Place,
    ) -> Result<Scratch> {
        Ok(match program {
            Program::Snippet(code) => {
                let own = place.tree("oracle")?;
                let script = PathBuf::from(&recipe.script_name);
                own.write(&script, format!("{}{code}", recipe.preamble))?;
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
            Program::Nothing => Scratch {
                dir: PathBuf::from("."),
                script: PathBuf::new(),
                tokenised: false,
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

/// `haystack` with each `needle`, which is not empty, as `with`.
pub(crate) fn replace(haystack: &[u8], needle: &[u8], with: &[u8]) -> Vec<u8> {
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
    /// The bytes differ, and the row that accepted the difference pins
    /// lines of a side that are not these (#3291).
    Moved(Moved),
    /// The bytes differ and nothing accepts it: the line diff, expected as
    /// `-`, actual as `+`.
    Differs(String),
}

/// Compare the oracle's bytes with the engine's for the divergence key `id`.
/// Equal bytes are `Same` whatever the registry says; a difference is
/// `Diverged` under an accepted row for `id`, `Moved` where the row's
/// digests are not the lines' (#3291), and `Differs` otherwise. An
/// `open` or `fixed` row accepts nothing, and nor does a `trace:dump/`
/// row that names no digests, which `divergence-reason` refuses. Proposing a row for a `Differs`
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
    let Some(row) = registry.accepted(id) else {
        return Verdict::Differs(diff(expected, actual));
    };
    match row.digests {
        None if row.must_pin() => Verdict::Differs(diff(expected, actual)),
        None => Verdict::Diverged(row.clone()),
        Some(pinned) => match Digests::of(expected, actual) {
            now if now == pinned => Verdict::Diverged(row.clone()),
            now => Verdict::Moved(Moved {
                id: row.id.clone(),
                pinned,
                now,
            }),
        },
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
mod skipped_tests {
    use super::Promise;
    use crate::scratch::Tree;

    /// A skip is the test's line, named as libtest names its thread, and a
    /// thread the test started is named by its id.
    #[test]
    fn a_skip_is_a_line_naming_the_test_and_the_word() {
        let tree = Tree::new("oracle-skipped").expect("a scratch tree");
        let file = tree.join("skipped");
        Promise::Tree
            .skipped(&file)
            .expect("the test's skip is written");
        let apart = std::thread::spawn(move || Promise::Mysql.skipped(&file));
        let written = apart.join().expect("the started thread ends");
        written.expect("the started thread's skip is written");
        let text = std::fs::read_to_string(tree.join("skipped")).expect("the skips are read");
        let (own, started) = text.split_once('\n').expect("two lines");
        assert_eq!(
            own,
            "oracle::skipped_tests::a_skip_is_a_line_naming_the_test_and_the_word\ttree"
        );
        assert!(started.starts_with("ThreadId(") && started.ends_with(")\tmysql\n"));
    }
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

#[cfg(test)]
mod expanded_tests {
    use super::expanded;
    use std::path::{Path, PathBuf};

    const HOME: &str = "/Users/someone";

    fn read(value: &str) -> PathBuf {
        expanded(Path::new(value), Some(Path::new(HOME)))
    }

    /// A leading `~/` or `$HOME/` is the home directory, as a shell would
    /// read it, and the rest of the value is the path beneath it.
    #[test]
    fn a_leading_tilde_or_home_is_the_home_directory() {
        assert_eq!(read("~/x"), Path::new(HOME).join("x"));
        assert_eq!(read("$HOME/x"), Path::new(HOME).join("x"));
        assert_eq!(read("~/php-src/8.5"), Path::new(HOME).join("php-src/8.5"));
    }

    /// Nothing else is expanded: a `~/` past the start, a variable whose
    /// name only begins with `HOME`, a bare `~`, and a value read where the
    /// home is unknown are each taken as written.
    #[test]
    fn anything_else_is_taken_as_written() {
        for value in ["x~/y", "$HOMEX/y", "~", "$HOME", "/abs/~/y", "~user/y"] {
            assert_eq!(read(value), Path::new(value), "{value}");
        }
        assert_eq!(expanded(Path::new("~/x"), None), Path::new("~/x"));
    }
}
