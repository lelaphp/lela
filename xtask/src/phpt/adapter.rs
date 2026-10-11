//! The fresh-process adapter of design §8's `ScriptExec` seam: how one
//! `.phpt` is judged. The test's program is written where upstream writes
//! it, beside the test in the tree the subset stands in, the run's copy
//! of the pinned one (#1183), and run through the binary under test by its absolute path, as
//! upstream hands it (#331), with the sections that shape one run
//! (`--INI--`, `--ARGS--`, `--ENV--`, `--STDIN--`, `--CAPTURE_STDIO--`),
//! in the environment the `environment` module builds as upstream builds
//! it and with stdin a pipe as upstream's is,
//! its `SKIPIF` and `CLEAN` sections run beside it under the oracle by the
//! same shape of path, and what it wrote held to its `--EXPECT*--` section
//! with php's normalisation. A CGI-shaped test runs through the CGI host
//! beside the binary, as the `cgi` module has it, and a `--PHPDBG--` test
//! through the debugger host, as the `phpdbg` module has it. The verdict goes through [`oracle::verdict`], so
//! the registry decides what a difference means (harness §4.3). The tally,
//! the subsets and the reading are `phpt`'s; this is the per-item judge it
//! contributes.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use regex_lite::Regex;

use super::environment::Environment;
use super::file::{self, Phpt};
use super::miss::Miss;
use super::phpdbg;
use super::redirect::{Item, Redirect, Test};
use super::{Judged, Judgement, Lane, Subset};
use super::{cgi, http};
use crate::divergence::Registry;
use crate::oracle::expect::Expectation;
use crate::oracle::{
    self, Invocation, Observation, Oracle, PastLimit, Pin, Program, Stdin, Streams,
    Verdict as Compared,
};
use crate::tally::{Apart, Within};

/// The fresh-process adapter of design §8's `ScriptExec` seam, bound to
/// one run: the oracle and the registry, the binary it runs, the CGI and
/// debugger hosts beside it where there are, the tier lane it runs under,
/// and the extensions that binary loads, which `--EXTENSIONS--` is
/// answered from.
pub(super) struct Adapter<'a> {
    oracle: &'a Oracle<'a>,
    registry: &'a Registry,
    binary: &'a Path,
    hosts: Hosts,
    lane: Option<Lane>,
    loaded: Vec<String>,
    pinned_phpdbg: phpdbg::Pinned,
}

/// The hosts beside the binary under test a test of another shape runs
/// through: the CGI host and the debugger host, and for the served subset
/// the production server a CGI-shaped test is served to over HTTP in the
/// CGI host's place (the `http` module, #2348).
#[derive(Debug, Default)]
pub(super) struct Hosts {
    pub(super) cgi: Option<PathBuf>,
    pub(super) phpdbg: Option<PathBuf>,
    pub(super) http: Option<PathBuf>,
}

impl Hosts {
    /// How a CGI-shaped test's header names are held: folded where the
    /// production server writes them (#2570), as written otherwise.
    fn names(&self) -> cgi::Names {
        match self.http {
            Some(_) => cgi::Names::Folded,
            None => cgi::Names::AsWritten,
        }
    }
}

impl<'a> Adapter<'a> {
    pub(super) fn new(
        oracle: &'a Oracle<'a>,
        registry: &'a Registry,
        binary: &'a Path,
        hosts: Hosts,
        lane: Option<Lane>,
    ) -> Adapter<'a> {
        let loaded = loaded_extensions(oracle, binary).unwrap_or_else(|e| {
            println!("phpt: {} lists no extension ({e:#})", binary.display());
            Vec::new()
        });
        Adapter {
            oracle,
            registry,
            binary,
            hosts,
            lane,
            loaded,
            pinned_phpdbg: phpdbg::Pinned::default(),
        }
    }

    /// Every file of the subset as the items its run judges: a file as
    /// itself, and a `--REDIRECTTEST--` in its place as the set it names,
    /// each test of it under the redirect, once the redirecting test has
    /// passed what upstream holds it to before it redirects
    /// (run-tests.php:2027-2202). One that does not, or whose section names
    /// no set, is its verdict, reached here.
    pub(super) fn listed(&self, subset: &Subset, files: Vec<PathBuf>) -> Result<Vec<Item>> {
        let mut items = Vec::with_capacity(files.len());
        for file in files {
            let test = Test::own(file);
            let phpt = match Phpt::load(&test.file) {
                Ok(phpt) if phpt.has("REDIRECTTEST") => phpt,
                _ => {
                    items.push(Item::Test(test));
                    continue;
                }
            };
            let (id, name) = (test.id(subset), test.name(subset));
            if let Err(stopped) = self.admit(subset, &test, &phpt, (&id, &name))? {
                items.push(Item::Reached(stopped));
                continue;
            }
            match Redirect::read(self.oracle, subset, &test.file, &phpt)? {
                Ok((redirect, set)) => {
                    println!(
                        "phpt: {name} redirects {} tests of {} under {}",
                        set.len(),
                        redirect.set.under.display(),
                        redirect
                            .env
                            .iter()
                            .map(|(key, value)| format!("{key}={}", value.to_string_lossy()))
                            .collect::<Vec<_>>()
                            .join(" ")
                    );
                    let redirect = Arc::new(redirect);
                    items.extend(set.into_iter().map(|file| {
                        Item::Test(Test {
                            file,
                            via: Some(Arc::clone(&redirect)),
                        })
                    }));
                }
                Err(why) => {
                    let miss = Miss::bork(&why);
                    items.push(Item::Reached(
                        Judged::new(&id, &name, Judgement::Bork, why).keeping(miss),
                    ));
                }
            }
        }
        Ok(items)
    }

    /// One item, end to end.
    pub(super) fn judge(&self, subset: &Subset, item: &Item) -> Result<Judged> {
        let test = match item {
            Item::Reached(judged) => return Ok(judged.clone()),
            Item::Test(test) => test,
        };
        let id = test.id(subset);
        let name = test.name(subset);
        let phpt = match Phpt::load(&test.file) {
            Ok(phpt) => phpt,
            Err(borked) => {
                let miss = Miss::bork(&borked.0);
                return Ok(Judged::new(&id, &name, Judgement::Bork, borked.0).keeping(miss));
            }
        };
        // A test of a set that redirects again, which upstream's
        // `TestFile` refuses (run-tests.php:3830-3833); and a redirecting
        // test reached as a test, which [`Adapter::listed`] never hands on.
        if phpt.has("REDIRECTTEST") {
            let why = if test.via.is_some() {
                "Can't redirect a test from within a redirected test"
            } else {
                "a --REDIRECTTEST-- runs as the set it names, and this one was judged as a test"
            };
            return Ok(Judged::new(&id, &name, Judgement::Bork, why).keeping(Miss::bork(why)));
        }
        let Admitted {
            ini,
            env,
            expected_to_fail,
        } = match self.admit(subset, test, &phpt, (&id, &name))? {
            Ok(admitted) => admitted,
            Err(stopped) => return Ok(stopped),
        };
        let request = match cgi::request_of(&phpt, &test.beside(None), &env) {
            Ok(request) => request,
            Err((refused, why)) => return Ok(refused.judged(&id, &name, why)),
        };
        let actual = match self.run_program(subset, &phpt, test, &ini, (&env, request.as_ref()))? {
            Ok(actual) => actual,
            Err(Stopped::PastLimit(why)) => {
                return Ok(
                    Judged::new(&id, &name, Judgement::Fail, why).keeping(Miss::past_limit())
                );
            }
            Err(Stopped::Occupied(why) | Stopped::Unserved(why)) => {
                let miss = Miss::bork(&why);
                return Ok(Judged::new(&id, &name, Judgement::Bork, why).keeping(miss));
            }
        };
        // A CGI run's headers stand before its body, parted before the
        // body is normalised (run-tests.php:2530-2543).
        let (headers, actual) = match &request {
            Some(_) => cgi::parted(&actual, &self.oracle.pin().recipe.normalise),
            None => (Vec::new(), actual),
        };
        if let Some(construct) = unread(&actual) {
            let miss = Miss::unread(&construct);
            return Ok(Judged::new(&id, &name, Judgement::Unread, construct).keeping(miss));
        }
        let Some(expectation) = expectation_of(self.oracle.pin(), &phpt) else {
            let why = "missing section --EXPECT--, --EXPECTF-- or --EXPECTREGEX--";
            return Ok(Judged::new(&id, &name, Judgement::Bork, why).keeping(Miss::bork(why)));
        };
        let (met, trouble) = match expectation.met_under(self.oracle, &actual) {
            Ok(met) => (met, String::new()),
            // No room for the oracle's scratch: the run was not measured,
            // which the subset's reading says, and no verdict on the test.
            Err(e) if crate::scratch::Full::of(&e).is_some() => return Err(e),
            Err(e) => (false, format!("{e:#}\n")),
        };
        let (met, wanted, actual) = cgi::with_headers(
            phpt.text("EXPECTHEADERS").as_deref(),
            (&headers, self.hosts.names()),
            (met, expectation.text()),
            actual,
        );
        let mut compared = oracle::verdict(self.registry, &id, met, &wanted, &actual);
        if self.hosts.http.is_some() {
            compared = http::accepted(self.registry, &id, compared);
        }
        Ok(match (compared, expected_to_fail) {
            (Compared::Same, Some(why)) => Judged::new(
                &id,
                &name,
                Judgement::Pass,
                format!("the XFAIL section says {why:?} and the test passes"),
            ),
            (Compared::Same, None) => Judged::new(&id, &name, Judgement::Pass, ""),
            (Compared::Diverged(row), _) => {
                Judged::new(&id, &name, Judgement::Diverged, row.reason)
            }
            (Compared::Differs(difference), Some(why)) => {
                Judged::new(&id, &name, Judgement::XFail, format!("{why}\n{difference}"))
            }
            (Compared::Differs(difference), None) => Judged::new(
                &id,
                &name,
                Judgement::Fail,
                format!(
                    "{trouble}the {} section is not what ran:\n{difference}",
                    expectation.section()
                ),
            )
            .keeping(Miss::of(&expectation, &actual)),
        })
    }

    /// What a test must pass before its program runs, in upstream's
    /// order: a host for its shape, the extensions it needs, the
    /// `{ENV:NAME}` its `--INI--` names, and its `SKIPIF`; and what it runs
    /// with once it has passed them. The first it does not pass is its
    /// verdict.
    fn admit(
        &self,
        subset: &Subset,
        test: &Test,
        phpt: &Phpt,
        (id, name): (&str, &str),
    ) -> Result<Result<Admitted, Judged>> {
        if let Some(why) = unhosted(phpt, &self.hosts) {
            return Ok(Err(Judged::new(id, name, Judgement::Skip, why)));
        }
        if let Some(why) = (self.hosts.http.as_ref()).and_then(|_| http::unsent(phpt, &test.file)) {
            return Ok(Err(Judged::new(id, name, Judgement::Unsent, why)));
        }
        if let Some(why) = self.missing(phpt, test) {
            return Ok(Err(Judged::new(id, name, Judgement::Skip, why)));
        }
        let mut ini = match ini_of(phpt, &test.file) {
            Ok(ini) => ini,
            Err(why) => return Ok(Err(Judged::new(id, name, Judgement::Skip, why))),
        };
        ini.extend(self.lane.map(Lane::ini));
        let env = self.environment(phpt, test, &ini, &subset.root)?;
        let mut expected_to_fail = phpt.text("XFAIL").map(|why| why.trim().to_owned());
        if let Some(body) = phpt.filled("SKIPIF") {
            let out = self.run_section(subset, test, &env, ("skip", body), phpt.has("PHPDBG"))?;
            let said = String::from_utf8_lossy(&out.stdout).into_owned();
            if let Some(reason) = reason_after("skip", &said) {
                return Ok(Err(Judged::new(id, name, Judgement::Skip, reason)));
            }
            if let Some(reason) = reason_after("xfail", &said) {
                expected_to_fail = Some(reason);
            }
        }
        Ok(Ok(Admitted {
            ini,
            env,
            expected_to_fail,
        }))
    }

    /// The test's program, written where upstream writes it, run through
    /// the binary under test with the sections that shape one run, and
    /// taken away after; then `CLEAN`, which takes away what the test
    /// left. What it wrote, or the verdict it came to without an
    /// expectation to hold it to: a file already standing where the
    /// program goes borks the test rather than being overwritten, and a
    /// program still running at the recipe's limit fails it, since a hang
    /// is the engine's failure on this test as a wrong answer is and not
    /// the end of the lane (#691). Upstream's limit is the same 60 s
    /// (run-tests.php:1187), and past it upstream kills the process and
    /// holds what it wrote, with its `** ERROR: process timed out **`
    /// line, to the expectation, which fails (run-tests.php:1205-1209).
    /// The working directory is the root of the tree the subset stands in,
    /// as upstream runs every program from `TEST_PHP_SRCDIR`
    /// (run-tests.php:1173).
    ///
    /// A CGI-shaped test runs through the CGI host, `-C -f` before its
    /// program as run-tests.php hands it (run-tests.php:1897, 2342), in
    /// the environment of the request it describes, its body on stdin; a
    /// `--PHPDBG--` test through the debugger host, as the `phpdbg` module
    /// has it; and in the served subset a CGI-shaped test is served to the
    /// production server over HTTP, as the `http` module has it, a server
    /// that answered nothing borking it.
    fn run_program(
        &self,
        subset: &Subset,
        phpt: &Phpt,
        test: &Test,
        ini: &[String],
        (env, request): (&Environment, Option<&cgi::Request>),
    ) -> Result<Result<Vec<u8>, Stopped>> {
        let program = test.beside(None);
        if program.exists() {
            return Ok(Err(Stopped::Occupied(format!(
                "a file already stands where the test's program goes, {}",
                program.display()
            ))));
        }
        let body = phpt.section("FILE").unwrap_or_default().to_vec();
        let written = Written::new(&program, &body)?;
        let args = words(&phpt.text("ARGS").unwrap_or_default());
        let cgi_flags = [String::from("-C"), String::from("-f")];
        let capture = Capture::of(phpt);
        let invocation = Invocation {
            ini,
            args: &args,
            env: env.entries(),
            stdin: capture.stdin(phpt),
            streams: Streams::Pipes {
                merged: capture.stdout && capture.stderr,
            },
            cwd: Some(&subset.root),
            ..Invocation::new(self.binary, Program::Beside(&program))
        };
        let (dbg_flags, commands) = (phpdbg::FLAGS.map(String::from), phpdbg::stdin(phpt));
        let served = request.zip(self.hosts.http.as_deref());
        let ran = if let Some((request, host)) = served {
            http::served(&self.oracle.pin().recipe, host, &program, request, ini, env)
        } else {
            match (request, &self.hosts.cgi, &self.hosts.phpdbg) {
                (_, _, Some(host)) if phpt.has("PHPDBG") => {
                    self.pinned_phpdbg.asserted(self.oracle.pin(), host)?;
                    self.oracle.run_under(&Invocation {
                        binary: host,
                        program: if phpt.has("FILE") {
                            invocation.program
                        } else {
                            Program::Nothing
                        },
                        flags: &dbg_flags,
                        args: &phpdbg::args(phpt),
                        stdin: Stdin::Pipe(&commands),
                        ..invocation
                    })
                }
                (Some(request), Some(host), _) => self.oracle.run_under(&Invocation {
                    binary: host,
                    flags: &cgi_flags,
                    args: &request.args,
                    env: request.env.entries(),
                    stdin: request
                        .stdin
                        .as_deref()
                        .map_or(invocation.stdin, Stdin::File),
                    raw: true,
                    ..invocation
                }),
                _ => self.oracle.run_under(&invocation),
            }
            .map(|ran| ran.stdout)
        };
        drop(written);
        let output = match ran {
            Ok(stdout) => Ok(stdout),
            Err(e) => match e.downcast_ref::<PastLimit>() {
                Some(past) => Err(Stopped::PastLimit(past.to_string())),
                None if served.is_some() => Err(Stopped::Unserved(format!("{e:#}"))),
                None => return Err(e.context(format!("running {}", test.file.display()))),
            },
        };
        if let Some(body) = phpt.filled("CLEAN") {
            self.run_section(subset, test, env, ("clean", body), phpt.has("PHPDBG"))?;
        }
        Ok(output)
    }

    /// A section that runs beside the test rather than as it: `SKIPIF`,
    /// which decides whether the test applies at all, and `CLEAN`, which
    /// takes away what it left. Neither is given the test's own `--INI--`,
    /// as upstream gives neither. Both run under the oracle, not the
    /// engine: what they answer must be true of the corpus whatever the
    /// engine can do yet, and a young engine must not be trusted with
    /// cleanup. Upstream runs them under the binary it is testing, so a
    /// `SKIPIF` that asks what the engine has is answered here by php; it
    /// is the runner's one known difference from upstream's verdicts. Both
    /// run from the tree's root and in the test's environment, as the test's
    /// program does, so a `SKIPIF` that asks `TEST_PHP_EXECUTABLE` what it
    /// can do asks the binary under test, as upstream's asks it. Each reads
    /// an empty pipe on stdin, as upstream's does. A `--PHPDBG--` test's
    /// run under the pinned phpdbg with `-rr` (`debugger`), as upstream's
    /// run under the debugger it hosts the test through, where the pin
    /// names one.
    fn run_section(
        &self,
        subset: &Subset,
        test: &Test,
        env: &Environment,
        (kind, body): (&str, &[u8]),
        debugger: bool,
    ) -> Result<Observation> {
        let program = test.beside(Some(kind));
        let written = Written::new(&program, body)?;
        self.oracle.asserted()?;
        let pin = self.oracle.pin();
        let flags = phpdbg::SECTION_FLAGS.map(String::from);
        let (pinned, flags) = match pin.sapi.phpdbg.as_ref().filter(|_| debugger) {
            Some(host) => {
                self.pinned_phpdbg.asserted(pin, &host.path)?;
                (&host.path, &flags[..])
            }
            None => (&pin.oracle.path, &[][..]),
        };
        let out = self
            .oracle
            .run_under(&Invocation {
                flags,
                env: env.entries(),
                stdin: Stdin::Pipe(&[]),
                streams: Streams::Pipes { merged: false },
                cwd: Some(&subset.root),
                ..Invocation::new(pinned, Program::Beside(&program))
            })
            .with_context(|| format!("running the {kind} section of {}", test.file.display()))?;
        drop(written);
        Ok(out)
    }

    /// The `--EXTENSIONS--` the runner's binary does not load, in
    /// upstream's words, with a redirected test's redirecting test's
    /// after its own (run-tests.php:2029-2031).
    fn missing(&self, phpt: &Phpt, test: &Test) -> Option<String> {
        let wanted = phpt.text("EXTENSIONS").unwrap_or_default().into_owned();
        let redirected = test.via.iter().flat_map(|via| &via.extensions);
        let missing: Vec<&str> = wanted
            .split_whitespace()
            .chain(redirected.map(String::as_str))
            .filter(|name| !self.loaded.iter().any(|had| had == &name.to_lowercase()))
            .collect();
        if missing.is_empty() {
            return None;
        }
        Some(format!(
            "Required extension{} missing: {}",
            if missing.len() > 1 { "s" } else { "" },
            missing.join(", ")
        ))
    }

    /// What a test's programs find in their environment over the
    /// recipe's, the `SKIPIF`, the `FILE` and the `CLEAN` alike: the
    /// runner's, with the binary under test and the CGI host beside it,
    /// then the test's own over it, `TEST_PHP_EXTRA_ARGS` last, which is
    /// the recipe's flags and every ini entry the test runs under
    /// (run-tests.php:2115). The recipe's `TEST_PHP_EXECUTABLE` names the
    /// pinned php, which is right for a witness and wrong here: what a test
    /// spawns is the binary being measured.
    fn environment(
        &self,
        phpt: &Phpt,
        test: &Test,
        ini: &[String],
        root: &Path,
    ) -> Result<Environment> {
        let recipe = &self.oracle.pin().recipe;
        let redirected = test.via.as_ref().map_or(&[][..], |via| &via.env[..]);
        let hosts = (self.hosts.cgi.as_deref(), self.hosts.phpdbg.as_deref());
        let runner = Environment::of_runner(self.binary, hosts, root, &recipe.environment()?);
        Ok(runner.of_test(
            redirected,
            phpt,
            &test.file,
            extra_args(&recipe.args, recipe.ini.iter().chain(ini)),
        ))
    }
}

/// What a test runs with once it has passed what it is held to: its ini, its
/// environment, and why it is expected to fail where it is.
struct Admitted {
    ini: Vec<String>,
    env: Environment,
    expected_to_fail: Option<String>,
}

/// Why a test's program gave nothing to hold to its expectation, each
/// with its words.
enum Stopped {
    /// It was still running at the recipe's limit: a fail.
    PastLimit(String),
    /// A file already stood where it goes: a bork.
    Occupied(String),
    /// The production server answered nothing: a bork.
    Unserved(String),
}

/// What a test may not run beside in its run: its conflict keys and the
/// three files its run writes beside it, so no two tests whose files would
/// stand at one path run at once; alone where its conflict keys name
/// `all`, as upstream has it (run-tests.php:1365-1370). Another run is in
/// a copy of its own and holds nothing against this one (#1183). A test of
/// a redirected set holds its redirecting test as a key too, so the set
/// runs one test after another, as upstream's worker runs it inside the
/// redirecting test (run-tests.php:2230-2232). A verdict reached where the
/// set was listed runs nothing and holds nothing.
pub(super) fn apart(item: &Item, dirs: &mut BTreeMap<PathBuf, Vec<String>>) -> Apart {
    let Item::Test(test) = item else {
        return Apart::default();
    };
    let mut keys = file::conflicts(&test.file, dirs);
    let within = if keys.iter().any(|key| key == "all") {
        Within::Alone
    } else {
        Within::Batch
    };
    keys.retain(|key| key != "all");
    keys.extend(
        [None, Some("skip"), Some("clean")].map(|kind| test.beside(kind).display().to_string()),
    );
    keys.extend(test.via.iter().map(|via| via.by.display().to_string()));
    Apart { within, keys }
}

/// What `item` holds among every run on the machine while it runs: its
/// conflict keys as upstream reads them, `all` among them, and none of the
/// paths of the run's own copy (the `across` module, #1313).
pub(super) fn held(item: &Item, dirs: &mut BTreeMap<PathBuf, Vec<String>>) -> Vec<String> {
    let Item::Test(test) = item else {
        return Vec::new();
    };
    file::conflicts(&test.file, dirs)
}

/// How many neighbours one thread judges in sequence: upstream's batch,
/// the tests a worker is handed when it is ready
/// (run-tests.php:1537-1541). The batch is `min(32, ceil(tests /
/// workers))` and the loop that fills it takes one more, since it tests
/// the count before it adds; the tests are those left once the ones
/// alone are taken out (run-tests.php:1365-1370, 1399), and the workers
/// no more than those tests (run-tests.php:1383). Neighbours in the sort
/// share a worker this way, not a directory: a directory larger than a
/// batch runs over several at once, and a boundary may fall between two
/// tests that make one path relative to the root (#1091).
pub(super) fn batch(apart: &[Apart], jobs: NonZeroUsize) -> NonZeroUsize {
    const MOST: usize = 32;
    let tests = apart
        .iter()
        .filter(|apart| apart.within == Within::Batch)
        .count();
    let workers = jobs.get().min(tests).max(1);
    NonZeroUsize::MIN.saturating_add(tests.div_ceil(workers).clamp(1, MOST))
}

/// The extensions a binary loads under the recipe, as upstream asks for
/// them: `Core` dropped and each module under its crate's name,
/// [`crate::oracle::crate_name`] (run-tests.php:2908-2916). Upstream asks a program,
/// `get_loaded_extensions()` joined by `implode`; this lane asks the
/// binary's own listing, `-m`, which ends at the first blank line, so
/// the answer is the host's and stands before the engine serves one
/// native of `ext/standard` (#261). php reads `-m` off the line whatever
/// follows it, so the recipe's program is written and not served.
fn loaded_extensions(oracle: &Oracle<'_>, binary: &Path) -> Result<Vec<String>> {
    let listing = [String::from("-m")];
    let out = oracle.run_under(&Invocation {
        flags: &listing,
        ..Invocation::new(binary, Program::Snippet(""))
    })?;
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
        .skip(1)
        .take_while(|line| !line.trim().is_empty())
        .map(str::trim)
        .filter(|name| !name.is_empty() && *name != "Core")
        .map(crate::oracle::crate_name)
        .collect())
}

/// Why this runner cannot host a test: a CGI-shaped one where no CGI host
/// stands beside the binary, and a debugger's where no debugger host
/// does, as upstream's runner skips each (run-tests.php:1894-1896, 1914).
fn unhosted(phpt: &Phpt, hosts: &Hosts) -> Option<String> {
    if phpt.is_cgi() && hosts.cgi.is_none() && hosts.http.is_none() {
        return Some("CGI not available".into());
    }
    if phpt.has("PHPDBG") && hosts.phpdbg.is_none() {
        return Some("phpdbg not available".into());
    }
    None
}

/// The construct the compiler could not read, when what the program wrote
/// is the compiler's own refusal and nothing else: one `Parse error` line
/// whose message ends in `this compiler cannot read yet`, the sentence
/// `php_compile` raises for a construct it does not read, or in `this
/// compiler cannot read`, the same refusal from its reader for a shape of
/// one it does, both in the frame php gives a parse error (#163). php never writes those words, and a
/// parse error stands before any test code ran, so a body that is the one
/// line is the compiler's refusal and a body with anything else in it is
/// the test's own, held to its expectation (harness §4.5 condition 1,
/// #319). The `PHP ` prefix is `log_errors`' rendering, read for a test
/// whose `--INI--` sends the diagnostic to the log the runner captures.
pub(super) fn unread(actual: &[u8]) -> Option<String> {
    let text = String::from_utf8_lossy(actual);
    let text = text.trim();
    if text.lines().count() != 1 {
        return None;
    }
    let refusal = Regex::new(
        r"^(?:PHP )?Parse error:\s+(.+ this compiler cannot read(?: yet)?) in .+ on line \d+$",
    )
    .ok()?;
    refusal.captures(text).map(|found| found[1].to_owned())
}

/// Which of a test's three streams the runner captures: all three,
/// upstream's default, or those its `--CAPTURE_STDIO--` names, as
/// run-tests.php reads the section (run-tests.php:1875-1889). The two
/// streams out are captured as one where both are, upstream's `2>&1`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Capture {
    stdin: bool,
    stdout: bool,
    stderr: bool,
}

impl Capture {
    fn of(phpt: &Phpt) -> Capture {
        match phpt.text("CAPTURE_STDIO") {
            Some(named) => {
                let named = named.to_uppercase();
                Capture {
                    stdin: named.contains("STDIN"),
                    stdout: named.contains("STDOUT"),
                    stderr: named.contains("STDERR"),
                }
            }
            None => Capture {
                stdin: true,
                stdout: true,
                stderr: true,
            },
        }
    }

    /// The program's stdin: a pipe holding its `--STDIN--`, or nothing,
    /// where the runner captures stdin; `/dev/null` where the section
    /// leaves it to a terminal this runner does not hand a test.
    fn stdin(self, phpt: &Phpt) -> Stdin<'_> {
        if self.stdin {
            Stdin::Pipe(phpt.section("STDIN").unwrap_or_default())
        } else {
            Stdin::Null
        }
    }
}

/// The expectation the test carries, normalised as php normalises it.
fn expectation_of(pin: &Pin, phpt: &Phpt) -> Option<Expectation> {
    if let Some(body) = phpt.section("EXPECT") {
        return Some(Expectation::Exact(trimmed(pin, body)));
    }
    if let Some(body) = phpt.section("EXPECTF") {
        return Some(Expectation::Format(trimmed(pin, body)));
    }
    phpt.section("EXPECTREGEX")
        .map(|body| Expectation::Pattern(trimmed(pin, body)))
}

/// The recipe's normalisation over a section, which is what the stdout it
/// is compared with has been through; the oracle module owns the rule.
fn trimmed(pin: &Pin, body: &[u8]) -> Vec<u8> {
    oracle::normalise(&pin.recipe.normalise, body.to_vec())
}

/// A test's `--INI--`, one `-d` line each, with upstream's four
/// substitutions; an unset `{ENV:NAME}` is a skip with upstream's reason.
/// Each line is `name=value` with both halves trimmed, as
/// `settings2array` (run-tests.php:2950) splits and trims them, so an
/// `input_encoding = ISO-8859-1` is the entry and not a name php knows
/// nothing of.
pub(crate) fn ini_of(phpt: &Phpt, test: &Path) -> Result<Vec<String>, String> {
    let Some(body) = phpt.text("INI") else {
        return Ok(Vec::new());
    };
    let body = substituted(&body, test);
    let mail = Regex::new(r"\{MAIL:(\S+)\}").map_err(|e| e.to_string())?;
    let body = mail.replace_all(&body, "tee $1 >/dev/null").into_owned();
    let env = Regex::new(r"\{ENV:(\S+)\}").map_err(|e| e.to_string())?;
    let mut unset = None;
    let body = replace_all(&env, &body, |name| {
        std::env::var(name).unwrap_or_else(|_| {
            unset = Some(format!("Environment variable {name} is not set"));
            String::new()
        })
    });
    if let Some(reason) = unset {
        return Err(reason);
    }
    Ok(body
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(name, value)| format!("{}={}", name.trim(), value.trim()))
        .collect())
}

/// `TEST_PHP_EXTRA_ARGS` as upstream renders it: the flags, each after a
/// space, then a space, then every ini entry as ` -d "name=value"`
/// (run-tests.php:2115). The entries are gathered as `settings2array`
/// gathers them, a later value for a name taking the earlier one's place
/// (run-tests.php:2950-2969), and each value goes through `addslashes`
/// as `settings2params` puts it (run-tests.php:2971-2996).
fn extra_args<'a>(flags: &[String], ini: impl IntoIterator<Item = &'a String>) -> String {
    let mut settings: Vec<(&str, &str)> = Vec::new();
    for entry in ini {
        let Some((name, value)) = entry.split_once('=') else {
            continue;
        };
        let (name, value) = (name.trim(), value.trim());
        match settings.iter_mut().find(|(had, _)| *had == name) {
            Some(setting) => setting.1 = value,
            None => settings.push((name, value)),
        }
    }
    let mut rendered = String::new();
    for flag in flags {
        write!(rendered, " {flag}").ok();
    }
    rendered.push(' ');
    for (name, value) in settings {
        write!(rendered, " -d \"{name}={}\"", addslashes(value)).ok();
    }
    rendered
}

/// php's `addslashes`: a backslash before each `'`, `"` and `\`, and a
/// NUL written as `\0`.
fn addslashes(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        match c {
            '\'' | '"' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            '\0' => out.push_str("\\0"),
            c => out.push(c),
        }
    }
    out
}

/// `{PWD}` and `{TMP}`, which stand for the test's own directory and the
/// scratch one.
fn substituted(body: &str, test: &Path) -> String {
    let beside = test
        .parent()
        .map(|dir| dir.to_string_lossy().into_owned())
        .unwrap_or_default();
    let scratch = std::env::temp_dir().to_string_lossy().into_owned();
    body.replace("{PWD}", &beside).replace("{TMP}", &scratch)
}

/// `Regex::replace_all` with a closure over the one capture, which
/// `regex_lite` does not take directly.
fn replace_all(regex: &Regex, text: &str, mut with: impl FnMut(&str) -> String) -> String {
    let mut out = String::with_capacity(text.len());
    let mut last = 0;
    for found in regex.captures_iter(text) {
        let Some(whole) = found.get(0) else { continue };
        let name = found.get(1).map_or("", |m| m.as_str());
        out.push_str(&text[last..whole.start()]);
        out.push_str(&with(name));
        last = whole.end();
    }
    out.push_str(&text[last..]);
    out
}

/// The reason a `SKIPIF` gave, when its first word is `word`:
/// upstream's `/^skip\s*(.+)/i` over what the section wrote.
pub(crate) fn reason_after(word: &str, said: &str) -> Option<String> {
    let said = said.trim_start();
    if said.len() < word.len() || !said[..word.len()].eq_ignore_ascii_case(word) {
        return None;
    }
    Some(
        said[word.len()..]
            .trim()
            .lines()
            .next()
            .unwrap_or("")
            .to_owned(),
    )
}

/// A command line's words, split on whitespace: a test's `--ARGS--`.
fn words(text: &str) -> Vec<String> {
    text.split_whitespace().map(str::to_owned).collect()
}

/// A file written beside a test for the length of its run and taken away
/// after, which is where upstream puts a test's program too. A run killed
/// while it stands leaves it in the run's copy, which the next run's sweep
/// takes away whole (#1183).
pub(super) struct Written {
    path: PathBuf,
}

impl Written {
    pub(super) fn new(path: &Path, body: &[u8]) -> Result<Written> {
        fs::write(path, body).map_err(|err| crate::scratch::made(err, path, "writing"))?;
        Ok(Written {
            path: path.to_path_buf(),
        })
    }
}

impl Drop for Written {
    fn drop(&mut self) {
        fs::remove_file(&self.path).ok();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_skipif_reason_is_the_text_after_the_word() {
        assert_eq!(
            reason_after("skip", "skip no pcre\n").as_deref(),
            Some("no pcre")
        );
        assert_eq!(
            reason_after("skip", "SKIP  no pcre").as_deref(),
            Some("no pcre")
        );
        assert_eq!(reason_after("skip", "skip").as_deref(), Some(""));
        assert_eq!(reason_after("skip", "nothing"), None);
        assert_eq!(
            reason_after("xfail", "xfail known"),
            Some("known".to_owned())
        );
    }

    #[test]
    fn an_ini_line_is_its_name_and_value_trimmed_around_the_first_equals() {
        let text = "--TEST--\nn\n--INI--\ninput_encoding = ISO-8859-1\n  precision=14 \nno equals\na==b\n--FILE--\nx\n--EXPECT--\n1\n";
        let phpt = Phpt::parse(Path::new("x.phpt"), text.as_bytes()).unwrap();
        assert_eq!(
            ini_of(&phpt, Path::new("x.phpt")).unwrap(),
            ["input_encoding=ISO-8859-1", "precision=14", "a==b"]
        );
    }

    #[test]
    fn the_extra_args_are_the_flags_and_each_setting_once_with_its_last_value_slashed() {
        let ini = ["precision=14", "error_log=", "precision = 3", "x=a\"b'c\\d"].map(String::from);
        assert_eq!(
            extra_args(&["-n".to_owned()], &ini),
            r#" -n  -d "precision=3" -d "error_log=" -d "x=a\"b\'c\\d""#
        );
    }

    #[test]
    fn every_stream_is_captured_and_stdin_a_pipe_unless_capture_stdio_says_otherwise() {
        let phpt = |text: &str| Phpt::parse(Path::new("x.phpt"), text.as_bytes()).unwrap();
        let plain = phpt("--TEST--\nn\n--FILE--\nx\n--EXPECT--\n1\n");
        let all = Capture::of(&plain);
        assert!(all.stdout && all.stderr);
        assert_eq!(all.stdin(&plain), Stdin::Pipe(b""));
        let fed = phpt("--TEST--\nn\n--STDIN--\nabc\n--FILE--\nx\n--EXPECT--\n1\n");
        assert_eq!(Capture::of(&fed).stdin(&fed), Stdin::Pipe(b"abc\n"));
        let out = phpt("--TEST--\nn\n--CAPTURE_STDIO--\nSTDOUT\n--FILE--\nx\n--EXPECT--\n1\n");
        let only = Capture::of(&out);
        assert!(only.stdout && !only.stderr);
        assert_eq!(only.stdin(&out), Stdin::Null);
    }
}
