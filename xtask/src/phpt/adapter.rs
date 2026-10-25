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
use super::{Judged, Judgement, Lane, Subset, borked};
use super::{cgi, http};
use crate::divergence::Registry;
use crate::oracle::expect::Expectation;
use crate::oracle::{
    self, Ended, Invocation, Observation, Oracle, PastLimit, Pin, Program, Stdin, Streams,
    Verdict as Compared,
};
use crate::reading::joined;
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
                        joined(
                            redirect
                                .env
                                .iter()
                                .map(|(key, value)| format!("{key}={}", value.to_string_lossy())),
                            " "
                        )
                    );
                    let redirect = Arc::new(redirect);
                    items.extend(set.into_iter().map(|file| {
                        Item::Test(Test {
                            file,
                            via: Some(Arc::clone(&redirect)),
                        })
                    }));
                }
                Err(why) => items.push(Item::Reached(borked(&id, &name, why))),
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
            Err(why) => return Ok(borked(&id, &name, why.0)),
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
            return Ok(borked(&id, &name, why));
        }
        let Admitted {
            ini,
            env,
            expected_to_fail,
            warned,
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
                return Ok(borked(&id, &name, why));
            }
        };
        // What ran is normalised as upstream trims it, after its
        // `Termsig=` line; a CGI run's headers stand before its body,
        // parted before the body is normalised (run-tests.php:2530-2543).
        let rules = &self.oracle.pin().recipe.normalise;
        let (headers, actual) = match &request {
            Some(_) => cgi::parted(&actual, rules),
            None => (Vec::new(), oracle::normalise(rules, actual)),
        };
        if let Some(construct) = unread(&actual) {
            let miss = Miss::unread(&construct);
            return Ok(Judged::new(&id, &name, Judgement::Unread, construct).keeping(miss));
        }
        let Some(expectation) = expectation_of(self.oracle.pin(), &phpt) else {
            let why = "missing section --EXPECT--, --EXPECTF-- or --EXPECTREGEX--";
            return Ok(borked(&id, &name, why));
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
                Judgement::Warn,
                format!("the XFAIL section says {why:?} and the test passes"),
            ),
            (Compared::Same, None) => Judged::new(&id, &name, Judgement::Pass, ""),
            (Compared::Diverged(row), _) => {
                Judged::new(&id, &name, Judgement::Diverged, row.reason)
            }
            (Compared::Moved(moved), _) => {
                Judged::new(&id, &name, Judgement::Fail, moved.to_string())
            }
            (Compared::Differs(difference), Some(why)) => {
                Judged::new(&id, &name, Judgement::XFail, format!("{why}\n{difference}"))
            }
            (Compared::Differs(difference), None) => Judged::new(
                &id,
                &name,
                Judgement::Fail,
                format!(
                    "{trouble}{}the {} section is not what ran:\n{difference}",
                    warned.map_or_else(String::new, |why| format!("warn: {why}\n")),
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
        let mut warned = None;
        if let Some(body) = phpt.filled("SKIPIF") {
            let shown = SKIPIF_SHOWS.map(String::from);
            let section = ("skip", body, &shown[..]);
            let out = self.run_section(subset, test, &env, section, phpt.has("PHPDBG"))?;
            match Said::ran(&out) {
                Said::Skip(why) => return Ok(Err(Judged::new(id, name, Judgement::Skip, why))),
                Said::Bork(said) => {
                    return Ok(Err(borked(
                        id,
                        name,
                        format!("invalid output from SKIPIF: {said}"),
                    )));
                }
                Said::XFail(why) => expected_to_fail = Some(why),
                Said::Warn(why) => warned = Some(why),
                Said::Run => {}
            }
        }
        Ok(Ok(Admitted {
            ini,
            env,
            expected_to_fail,
            warned,
        }))
    }

    /// The test's program, written where upstream writes it, run through
    /// the binary under test with the sections that shape one run, and
    /// taken away after; then `CLEAN`, which takes away what the test
    /// left. What [`system_with_timeout`] handed back, not yet normalised,
    /// or the verdict it came to without an expectation to hold it to: a
    /// file already standing where the program goes borks the test rather
    /// than being overwritten, and a program still running at the
    /// recipe's limit fails it, since a hang is the engine's failure on
    /// this test as a wrong answer is and not the end of the lane (#691). Upstream's limit is the same 60 s
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
        let dbg_args = phpdbg::args(phpt);
        let served = request.zip(self.hosts.http.as_deref());
        let ran = if let Some((request, host)) = served {
            http::served(&self.oracle.pin().recipe, host, &program, request, ini, env)
        } else {
            let call = match (request, &self.hosts.cgi, &self.hosts.phpdbg) {
                (_, _, Some(host)) if phpt.has("PHPDBG") => {
                    self.pinned_phpdbg.asserted(self.oracle.pin(), host)?;
                    Invocation {
                        binary: host,
                        program: if phpt.has("FILE") {
                            invocation.program
                        } else {
                            Program::Nothing
                        },
                        flags: &dbg_flags,
                        args: &dbg_args,
                        stdin: Stdin::Pipe(&commands),
                        ..invocation
                    }
                }
                (Some(request), Some(host), _) => Invocation {
                    binary: host,
                    flags: &cgi_flags,
                    args: &request.args,
                    env: request.env.entries(),
                    stdin: request
                        .stdin
                        .as_deref()
                        .map_or(invocation.stdin, Stdin::File),
                    ..invocation
                },
                _ => invocation,
            };
            system_with_timeout(self.oracle, &call)
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
            self.run_section(subset, test, env, ("clean", body, &[]), phpt.has("PHPDBG"))?;
        }
        Ok(output)
    }

    /// A section that runs beside the test rather than as it: `SKIPIF`,
    /// which decides whether the test applies at all, and `CLEAN`, which
    /// takes away what it left. Neither is given the test's own `--INI--`,
    /// as upstream gives neither; a `SKIPIF` runs under [`SKIPIF_SHOWS`]
    /// and a `CLEAN` under nothing more. Both run under the oracle, not the
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
    /// names one. What a section wrote is what [`system_with_timeout`]
    /// hands back, which a `SKIPIF`'s reader trims as upstream trims it
    /// ([`Said::ran`]).
    fn run_section(
        &self,
        subset: &Subset,
        test: &Test,
        env: &Environment,
        (kind, body, ini): (&str, &[u8], &[String]),
        debugger: bool,
    ) -> Result<Vec<u8>> {
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
        let call = Invocation {
            ini,
            flags,
            env: env.entries(),
            stdin: Stdin::Pipe(&[]),
            streams: Streams::Pipes { merged: false },
            cwd: Some(&subset.root),
            ..Invocation::new(pinned, Program::Beside(&program))
        };
        let out = system_with_timeout(self.oracle, &call)
            .with_context(|| format!("running the {kind} section of {}", test.file.display()))?;
        drop(written);
        Ok(out)
    }

    /// The `--EXTENSIONS--` the runner's binary does not load, in
    /// upstream's words, with a redirected test's redirecting test's
    /// after its own (run-tests.php:2029-2031).
    fn missing(&self, phpt: &Phpt, test: &Test) -> Option<String> {
        let wanted = phpt.extensions();
        let redirected = test.via.iter().flat_map(|via| &via.extensions);
        let missing: Vec<&str> = (wanted.iter().chain(redirected))
            .map(String::as_str)
            .filter(|name| !self.loaded.iter().any(|had| had == name))
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
    /// The reason a `warn` of the `SKIPIF` gave, which a failure is
    /// reported under, as upstream's `WARN&FAIL` (run-tests.php:2677).
    warned: Option<String>,
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
        let named = phpt.text("CAPTURE_STDIO").map(|named| named.to_uppercase());
        // A test that names no stream captures every one.
        let captures = |stream: &str| named.as_ref().is_none_or(|n| n.contains(stream));
        Capture {
            stdin: captures("STDIN"),
            stdout: captures("STDOUT"),
            stderr: captures("STDERR"),
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
    let beside = test.parent().unwrap_or(Path::new(""));
    Ok(with_pwd(&ini_before_pwd(phpt)?, beside))
}

/// A test's `--INI--` as [`ini_of`] reads it, with `{PWD}` kept for the
/// directory its program is served from: a host that serves the program
/// elsewhere than beside the test puts its own there by [`with_pwd`]
/// (`fpm-compare`, #2705).
pub(crate) fn ini_before_pwd(phpt: &Phpt) -> Result<Vec<String>, String> {
    let Some(body) = phpt.text("INI") else {
        return Ok(Vec::new());
    };
    let scratch = std::env::temp_dir().to_string_lossy().into_owned();
    let body = body.replace("{TMP}", &scratch);
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

/// `ini` with each `{PWD}` as `dir`, the directory the program runs in,
/// which upstream puts there as the test's own (`dirname($file)`,
/// run-tests.php), where it writes the program.
pub(crate) fn with_pwd(ini: &[String], dir: &Path) -> Vec<String> {
    let dir = dir.to_string_lossy();
    ini.iter().map(|line| line.replace("{PWD}", &dir)).collect()
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

/// The ini a `SKIPIF` runs under after the recipe's: its errors shown and
/// not those of startup, as upstream's command line has it
/// (run-tests.php:2134).
const SKIPIF_SHOWS: [&str; 2] = ["display_errors=1", "display_startup_errors=0"];

/// What a `SKIPIF` wrote on stdout, with upstream's `Termsig=` line,
/// trimmed as php's `trim` trims and read as upstream reads it (run-tests.php:2154-2190, 3595-3597).
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Said {
    /// It begins with `skip`, any case: the first line after the word.
    Skip(String),
    /// It begins with `xfail`: why the test is expected to fail.
    XFail(String),
    /// `warn` and a reason: the test runs, and a failure of it is
    /// reported under the reason, as upstream's `WARN&FAIL` is.
    Warn(String),
    /// The test runs: nothing; `nocache` and anything after it; `info`
    /// with a reason; `xleak` or `flaky`, sections this runner does not
    /// act on.
    Run,
    /// Anything else, which upstream borks as no answer a `SKIPIF` may
    /// give: a warning before a `skip` among it (#2857).
    Bork(String),
}

impl Said {
    /// What a `SKIPIF`'s run says, read as upstream reads what its
    /// [`system_with_timeout`] handed back.
    pub(crate) fn ran(handed: &[u8]) -> Said {
        Said::of(&String::from_utf8_lossy(handed))
    }

    /// What a `SKIPIF` said, `written` being what its run handed back.
    pub(crate) fn of(written: &str) -> Said {
        let said = written.trim_matches(TRIM);
        let after = |word: &str| {
            let head = said.get(..word.len())?;
            head.eq_ignore_ascii_case(word).then(|| &said[word.len()..])
        };
        let runs = said.is_empty()
            || said.starts_with("nocache")
            || reason(said, r"(?i)^info\s*(.+)").is_some()
            || after("xleak").or(after("flaky")).is_some();
        match (
            after("skip"),
            after("xfail"),
            reason(said, r"(?i)^warn\s+(.+)"),
        ) {
            (Some(_), _, _) => Said::Skip(reason(said, r"(?i)^skip\s*(.+)").unwrap_or_default()),
            (_, Some(rest), _) => Said::XFail(rest.trim_start_matches(TRIM).to_owned()),
            (_, _, Some(why)) => Said::Warn(why),
            _ if runs => Said::Run,
            _ => Said::Bork(said.to_owned()),
        }
    }
}

/// Upstream's `system_with_timeout`: `call` run by `oracle` with what it
/// wrote handed back as it wrote it (`raw`), with its `Termsig=` line
/// ([`handed_back`]), for the caller to trim after it as upstream trims
/// after it. The one way the runner runs a test's program and the
/// sections beside it.
pub(crate) fn system_with_timeout(oracle: &Oracle<'_>, call: &Invocation<'_>) -> Result<Vec<u8>> {
    let out = oracle.run_under(&Invocation { raw: true, ..*call })?;
    Ok(handed_back(out))
}

/// What upstream's `system_with_timeout` hands back of a run, the test's
/// program and its `SKIPIF` alike: what it wrote, as it wrote it, and a
/// `Termsig=` line where a signal ended it or it exited 129 to 159
/// (run-tests.php:1235-1239), which upstream trims with the rest. So a
/// php that `_exit(137)`s at its hard memory limit
/// (`zend_mm_hard_limit_error`) reads `Termsig=9`, as an expectation that
/// ends in it wants, and a `SKIPIF` that crashed having written nothing
/// borks. Upstream's `SKIPIF` command line opens with
/// `unset`s (run-tests.php:2123), so the shell forks php and exits with
/// 128 and the signal that ended it: the oracle's `proc_get_status` over
/// that shape with a php that killed itself with 11 answers `signaled`
/// false and an exit of 139, and the line reads `Termsig=11`, as it does
/// here for a php a signal ended.
fn handed_back(out: Observation) -> Vec<u8> {
    let mut said = out.stdout;
    let termsig = match out.exit {
        Ended::Signal(n) => Some(n),
        Ended::Status(n @ 129..=159) => Some(n - 128),
        Ended::Status(_) => None,
    };
    if let Some(n) = termsig {
        said.extend_from_slice(format!("\nTermsig={n}\n").as_bytes());
    }
    said
}

/// The reason upstream's `pattern` reads out of what a `SKIPIF` said, its
/// one group, none where it does not match: `regex_lite`'s `\s` and `.`
/// are PCRE's, the ASCII spaces and anything but a newline.
fn reason(said: &str, pattern: &str) -> Option<String> {
    let found = Regex::new(pattern).ok()?.captures(said)?;
    Some(found.get(1)?.as_str().to_owned())
}

/// What php's `trim` and `ltrim` take off: `" \t\n\r\0\x0B"`.
pub(super) const TRIM: [char; 6] = [' ', '\t', '\n', '\r', '\0', '\x0B'];

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
    fn a_skipif_is_read_as_upstream_reads_it_and_a_warning_before_its_skip_borks() {
        let skip = |why: &str| Said::Skip(why.to_owned());
        assert_eq!(Said::of("skip no pcre\n"), skip("no pcre"));
        assert_eq!(Said::of("\n SKIP  no pcre\nmore"), skip("no pcre"));
        assert_eq!(Said::of("skip"), skip(""));
        assert_eq!(Said::of("xfail known"), Said::XFail("known".to_owned()));
        for runs in [
            "",
            " \n",
            "nocache",
            "nocache junk",
            "info x",
            "xleak",
            "flaky y",
        ] {
            assert_eq!(Said::of(runs), Said::Run, "{runs:?}");
        }
        // mcast_ipv6_recv.phpt's SKIPIF where the group joins and the
        // send fails: its unsilenced warning stands before the skip.
        let warned = "\nWarning: socket_sendto(): Unable to write to socket [65]: No route to host in /t/skip.php on line 24\nskip unable to send multicast packet.";
        assert_eq!(
            Said::of("warn\n  slow\nhere"),
            Said::Warn("slow".to_owned())
        );
        // php's `trim` takes a NUL off and leaves a form feed.
        assert_eq!(Said::of("\0skip x\0\x0B"), skip("x"));
        // PCRE's `\s+(.+)` gives back a space for the group, and `.` takes
        // a carriage return.
        assert_eq!(Said::of("warn \x0C"), Said::Warn("\x0C".to_owned()));
        assert_eq!(Said::of("skip a\rb\nc"), skip("a\rb"));
        for borks in [
            warned,
            "info",
            "warn",
            "warnx",
            "warn\x0C",
            "Warning: x",
            "nothing",
            "NOCACHE",
            "\x0Cskip",
        ] {
            assert_eq!(
                Said::of(borks),
                Said::Bork(borks.trim_matches(TRIM).to_owned()),
                "{borks:?}"
            );
        }
    }

    /// A run a signal ended, or one that exited as a shell says a signal
    /// ended its child, reads as upstream's `Termsig=` line after what it
    /// wrote, the test's program as its `SKIPIF`, and a `SKIPIF` borks
    /// where it wrote nothing (run-tests.php:1235-1239).
    #[test]
    fn a_run_a_signal_ended_is_handed_back_with_upstreams_termsig_line() {
        let back = |stdout: &str, exit| {
            let out = handed_back(Observation {
                stdout: stdout.into(),
                stderr: Vec::new(),
                exit,
                wall: std::time::Duration::ZERO,
            });
            String::from_utf8(out).unwrap()
        };
        assert_eq!(back("", Ended::Signal(11)), "\nTermsig=11\n");
        assert_eq!(back("skip x", Ended::Status(139)), "skip x\nTermsig=11\n");
        // php's `_exit(137)` at its hard memory limit, the FILE run's case:
        // the line's own newline is kept, so trimmed with the rest it
        // stands a blank line above `Termsig=9`, as the expectation has it.
        let fatal = "Hard memory limit of 2097152 bytes exhausted in x on line 3\n";
        assert_eq!(
            back(fatal, Ended::Status(137)),
            format!("{fatal}\nTermsig=9\n")
        );
        assert_eq!(back("x", Ended::Status(129)), "x\nTermsig=1\n");
        assert_eq!(back("x", Ended::Status(159)), "x\nTermsig=31\n");
        for plain in [0, 1, 128, 160, 255] {
            assert_eq!(back("x", Ended::Status(plain)), "x", "{plain}");
        }
        let said = Said::of(&back("", Ended::Signal(9)));
        assert_eq!(said, Said::Bork("Termsig=9".to_owned()));
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
