//! What serves a fence marked `process` (harness-design.md §8, revision
//! 61; #1062): the `php` binary in a process of its own, never the engine
//! linked into this one. Such a snippet forks, sends or takes a signal, or
//! sets the process's ids, session or limits, and the harness runs its
//! lane on threads: a `fork()` there copies one thread of a process whose
//! other threads hold what they hold, a signal lands on whichever thread
//! the kernel picks, and a `setrlimit` is the harness's own. The binary
//! serves one request on the one thread it has, as design revision 35
//! requires of pcntl.
//!
//! The binary is `php`, `sapi/cli`'s host as it is built for every other
//! lane (#2168), which this module builds once per run with cargo where
//! the run meets its first such fence, in the lanes' profile whatever
//! profile this harness was built in (`profile::build`, harness revision
//! 92, #2217), the build the phpt lane looks for first. It runs
//! under the recipe exactly as the oracle does
//! ([`Oracle::run_under`]), and every request it serves appends the
//! natives it entered to the file `PHP_WITNESS_TRACE` names, a forked
//! child's request its own beside its parent's: so coverage is observed
//! for a fence served here as for one served by the linked engine (harness
//! §4.5 condition 2, revision 58), what a fence entered and nothing it
//! only spells.
//!
//! A fence with a `request` is served by `sapi/cgi`'s php-cgi, through a
//! link of that name, beside php-cgi for the oracle ([`served_as_cgi`],
//! #1197, ADR-0008); a fence with a `request fpm` by `sapi/fpm`'s
//! php-fpm, beside php-fpm (`served_as_fpm`, #1291); and one with a
//! `request makala` by `sapi/makala`'s makala over HTTP, beside php-fpm
//! under the pin's `[fastcgi]` mapping (`makala::served`, #2347).
//!
//! A `process` fence of a crate behind a bundle feature the binary under
//! test does not turn on (`Witness::feature`) is served by `php` built
//! with that feature, into the target directory the phpt lane builds that
//! engine in ([`profile::featured`]), so the child it starts carries its
//! own extension as the oracle's variant does, and the two lanes share
//! one build (#2721).

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, PoisonError};

use anyhow::{Result, anyhow};

use super::anchor::Entry;
use crate::carried::BUNDLE;
use crate::oracle::hosts::{CGI, CLI, EngineHost, FPM, MAKALA, PHPDBG};
use crate::oracle::profile;
use crate::oracle::{Invocation, Observation, Oracle, Program, Stdin};
use crate::phpt::environment::Environment;
use crate::phpt::{cgi, file::Phpt};
use crate::scratch::Tree;

// The hosts a fence is served by, each its own binary: `sapi/cli`'s
// serves a fence marked `process`, `sapi/cgi`'s one with a `request`,
// `sapi/fpm`'s one with a `request fpm` (#1291), `sapi/phpdbg`'s one
// with a `request phpdbg` (#1348) and `sapi/makala`'s one with a
// `request makala` (#2347); `oracle::hosts` states each once.

/// The variable the binaries read the trace file's path from
/// (`php_sapi::start`).
const TRACE: &str = "PHP_WITNESS_TRACE";

/// The binaries for one run: each built at the first fence that asks for
/// it, and the build's failure the trouble each such fence borks on.
#[derive(Debug)]
pub(crate) struct Binary {
    root: PathBuf,
    cli: OnceLock<Result<PathBuf, String>>,
    cgi: OnceLock<Result<PathBuf, String>>,
    fpm: OnceLock<Result<PathBuf, String>>,
    phpdbg: OnceLock<Result<PathBuf, String>>,
    makala: OnceLock<Result<PathBuf, String>>,
    /// `php` built with one feature of the bundle on, by feature.
    featured: Mutex<BTreeMap<String, Result<PathBuf, String>>>,
}

impl Binary {
    /// The binaries of the workspace at `root`, not yet built.
    pub(crate) fn of(root: &Path) -> Binary {
        Binary {
            root: root.to_path_buf(),
            cli: OnceLock::new(),
            cgi: OnceLock::new(),
            fpm: OnceLock::new(),
            phpdbg: OnceLock::new(),
            makala: OnceLock::new(),
            featured: Mutex::default(),
        }
    }

    /// The binaries of the workspace at `root`, with `path` standing in
    /// for `host`'s, so a test serves a fence or a host's line by a host
    /// whose bytes or failure it chooses.
    #[cfg(test)]
    pub(crate) fn standing(root: &Path, host: EngineHost, path: PathBuf) -> Binary {
        let binary = Binary::of(root);
        if let Ok(slot) = binary.slot(host) {
            slot.set(Ok(path)).ok();
        }
        binary
    }

    /// The same with `path` standing in for `php` built with `feature`.
    #[cfg(test)]
    pub(crate) fn featuring(self, feature: &str, path: PathBuf) -> Binary {
        let mut featured = self.featured.lock().unwrap_or_else(PoisonError::into_inner);
        featured.insert(feature.to_owned(), Ok(path));
        drop(featured);
        self
    }

    /// Where the CLI's binary is, built from the tree now if this run has
    /// not built it yet; what `crosscheck` starts as the built-in server.
    pub(crate) fn cli(&self) -> Result<&Path> {
        self.host(CLI)
    }

    /// Where `host`'s binary is, as [`Binary::cli`]: what a fence with a
    /// request is served by, and what a host's own line runs (#1975).
    pub(super) fn host(&self, host: EngineHost) -> Result<&Path> {
        built(self.slot(host)?, &self.root, host)
    }

    /// Where `host`'s binary is kept once built.
    fn slot(&self, host: EngineHost) -> Result<&OnceLock<Result<PathBuf, String>>> {
        [
            (CLI, &self.cli),
            (CGI, &self.cgi),
            (FPM, &self.fpm),
            (PHPDBG, &self.phpdbg),
            (MAKALA, &self.makala),
        ]
        .into_iter()
        .find_map(|(kind, slot)| (kind == host).then_some(slot))
        .ok_or_else(|| anyhow!("{} is no host the witness lane builds", host.krate))
    }

    /// Where `php` built with `feature` of the bundle on is, built now
    /// into [`profile::featured`] if this run has not built it yet; the
    /// CLI's binary where there is no feature.
    fn cli_with(&self, feature: Option<&str>) -> Result<PathBuf> {
        let Some(feature) = feature else {
            return self.cli().map(Path::to_path_buf);
        };
        let mut featured = self.featured.lock().unwrap_or_else(PoisonError::into_inner);
        let built = featured.entry(feature.to_owned()).or_insert_with(|| {
            let on = format!("{BUNDLE}/{feature}");
            let into = profile::featured(&self.root, feature);
            profile::build(
                &self.root,
                &CLI.package(),
                CLI.binary,
                Some(&on),
                Some(&into),
            )
            .map_err(|e| format!("{e:#}"))
        });
        built.clone().map_err(|why| anyhow!("{why}"))
    }

    /// `snippet` served by the binary, the one built with `feature` where
    /// there is one, under the recipe, and `regime`'s arguments and ini
    /// after it where the fence names one, as the oracle serves it; with
    /// the natives every request it served entered, in the order they
    /// were appended.
    pub(crate) fn serve(
        &self,
        oracle: &Oracle<'_>,
        snippet: &str,
        regime: Option<&str>,
        feature: Option<&str>,
    ) -> Result<(Observation, Vec<Entry>)> {
        let binary = self.cli_with(feature)?;
        traced(|env| {
            oracle.run_under(&Invocation {
                regimes: regime.as_slice(),
                env,
                ..Invocation::new(&binary, Program::Snippet(snippet))
            })
        })
    }
}

/// What `serve` observed, served with `PHP_WITNESS_TRACE` naming a file
/// of its own in the environment it is handed, and the natives every
/// request it served entered, in the order they were appended. A request
/// that entered no native appends nothing, and a binary that never
/// reached its end leaves no file: both are no entry.
pub(super) fn traced(
    serve: impl FnOnce(&[(String, std::ffi::OsString)]) -> Result<Observation>,
) -> Result<(Observation, Vec<Entry>)> {
    let traces = Tree::new("witness-trace")?;
    let trace = traces.join("entered");
    let observed = serve(&[(TRACE.to_owned(), trace.clone().into_os_string())])?;
    let text = fs::read_to_string(&trace).unwrap_or_default();
    Ok((observed, text.lines().filter_map(Entry::parse).collect()))
}

impl Binary {
    /// `snippet` served as the request `request` describes by the CGI's
    /// binary run under the name php-cgi, under `regime` where the fence
    /// names one, as [`Binary::serve`] serves a `process` fence; with the
    /// natives it entered.
    pub(crate) fn serve_cgi(
        &self,
        oracle: &Oracle<'_>,
        snippet: &str,
        request: &str,
        regime: Option<&str>,
    ) -> Result<(Observation, Vec<Entry>)> {
        let binary = self.host(CGI)?;
        let hosts = Tree::new("witness-cgi")?;
        let host = cgi::linked(binary, hosts.path())?;
        traced(|env| served_as_cgi(oracle, &host, snippet, request, regime, env))
    }

    /// `snippet` served as the request `request` describes by `sapi/fpm`'s
    /// binary, as [`Binary::serve_cgi`] serves one by php-cgi; with the
    /// natives its child entered.
    pub(crate) fn serve_fpm(
        &self,
        oracle: &Oracle<'_>,
        snippet: &str,
        request: &str,
    ) -> Result<(Observation, Vec<Entry>)> {
        let binary = self.host(FPM)?;
        traced(|env| super::fpm::served_as_fpm(oracle, binary, snippet, request, env))
    }

    /// `snippet` handed to `sapi/phpdbg`'s binary with `commands` on its
    /// prompt, as [`Binary::serve_cgi`] serves one by php-cgi; with the
    /// natives it entered.
    pub(super) fn serve_phpdbg(
        &self,
        oracle: &Oracle<'_>,
        snippet: &str,
        commands: &str,
    ) -> Result<(Observation, Vec<Entry>)> {
        let binary = self.host(PHPDBG)?;
        traced(|env| served_as_phpdbg(oracle, binary, snippet, commands, env))
    }
}

/// `snippet` handed to the debugger host `host` as run-tests.php hands a
/// `--PHPDBG--` test's program (`-qIb -f`), `commands` and a newline on
/// its stdin for the prompt to read, under the recipe. What the oracle and
/// the engine are each held to for a fence with a `request phpdbg`.
pub(crate) fn served_as_phpdbg(
    oracle: &Oracle<'_>,
    host: &Path,
    snippet: &str,
    commands: &str,
    env: &[(String, std::ffi::OsString)],
) -> Result<Observation> {
    let flags = crate::phpt::phpdbg::FLAGS.map(String::from);
    let stdin = format!("{commands}\n");
    oracle.run_under(&Invocation {
        flags: &flags,
        env,
        stdin: Stdin::Pipe(stdin.as_bytes()),
        ..Invocation::new(host, Program::Snippet(snippet))
    })
}

/// `snippet` served as a request of the web by the CGI host `host`: the
/// program written as the recipe's `script_name` in a tree of its own,
/// with the recipe's preamble; the request `request`'s sections describe
/// translated as the phpt lane translates a CGI-shaped test's
/// ([`cgi::request`]), over `env` with the request's `--ENV--` after it;
/// and the program run under the recipe and `regime`, whose arguments and
/// ini php-cgi takes on its command line as `php` does (#1729), by its
/// path with `-C -f` before it, the body on stdin. What the oracle and the
/// engine are each held to for a fence with a `request`.
pub(crate) fn served_as_cgi(
    oracle: &Oracle<'_>,
    host: &Path,
    snippet: &str,
    request: &str,
    regime: Option<&str>,
    env: &[(String, std::ffi::OsString)],
) -> Result<Observation> {
    let recipe = &oracle.pin().recipe;
    let own = oracle.tree("witness-request")?;
    let program = own.write(&recipe.script_name, format!("{}{snippet}", recipe.preamble))?;
    let program = crate::oracle::run_by(&program)?;
    let described = described(&own, &program, request, env)?;
    let flags = [String::from("-C"), String::from("-f")];
    oracle.run_under(&Invocation {
        regimes: regime.as_slice(),
        flags: &flags,
        args: &described.args,
        env: described.env.entries(),
        stdin: described.stdin.as_deref().map_or(Stdin::Null, Stdin::File),
        ..Invocation::new(host, Program::File(&program))
    })
}

/// The request a fence's `request` sections describe, translated in
/// `own` as the phpt lane translates a CGI-shaped test's
/// ([`cgi::request`]): over `env` with the CGI variables emptied and the
/// sections' `--ENV--` after it, for `program`.
pub(super) fn described(
    own: &Tree,
    program: &Path,
    request: &str,
    env: &[(String, std::ffi::OsString)],
) -> Result<cgi::Request> {
    let test = format!("--TEST--\nwitness\n{request}\n--FILE--\n\n--EXPECT--\n\n");
    let path = own.join("witness.phpt");
    let phpt = Phpt::parse(&path, test.as_bytes())
        .map_err(|borked| anyhow!("the request fence is not a test's sections: {borked}"))?;
    let mut env = Environment::from(env.to_vec());
    env.reset();
    env.extend(crate::phpt::env_of(&phpt, &path));
    cgi::request(&phpt, program, &env)
        .map_err(|(_, why)| anyhow!("the request fence describes no request: {why}"))
}

/// Where `host`'s binary is, built into `slot` from the tree at `root`
/// now if this run has not built it yet.
fn built<'a>(
    slot: &'a OnceLock<Result<PathBuf, String>>,
    root: &Path,
    host: EngineHost,
) -> Result<&'a Path> {
    slot.get_or_init(|| build(root, host).map_err(|e| format!("{e:#}")))
        .as_deref()
        .map_err(|why| anyhow!("{why}"))
}

/// The host's own binary, built in the lanes' profile
/// ([`profile::build`]): every request it serves records the natives it
/// entered where `PHP_WITNESS_TRACE` names a file, with no build of its
/// own for the lane (#2168).
fn build(root: &Path, host: EngineHost) -> Result<PathBuf> {
    profile::build(root, &host.package(), host.binary, None, None)
}
