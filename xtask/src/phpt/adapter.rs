//! The fresh-process adapter of design §8's `ScriptExec` seam: how one
//! `.phpt` is judged. The test's program is written where upstream writes
//! it, beside the test in the tree the subset stands in, the run's copy
//! of the pinned one (#1183), and run through the binary under test by its absolute path, as
//! upstream hands it (#331), with the sections that shape one run
//! (`--INI--`, `--ARGS--`, `--ENV--`, `--STDIN--`, `--CAPTURE_STDIO--`),
//! its `SKIPIF` and `CLEAN` sections run beside it under the oracle by the
//! same shape of path, and what it wrote held to its `--EXPECT*--` section
//! with php's normalisation. The verdict goes through [`oracle::verdict`], so
//! the registry decides what a difference means (harness §4.3). The tally,
//! the subsets and the reading are `phpt`'s; this is the per-item judge it
//! contributes.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::num::NonZeroUsize;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use regex_lite::Regex;

use super::file::{self, Phpt};
use super::{Judged, Judgement, Lane, Subset};
use crate::divergence::Registry;
use crate::oracle::expect::Expectation;
use crate::oracle::{
    self, Invocation, Observation, Oracle, PastLimit, Pin, Program, Verdict as Compared,
};
use crate::tally::{Apart, Within};

/// The fresh-process adapter of design §8's `ScriptExec` seam, bound to
/// one run: the oracle and the registry, the binary it runs, the tier lane
/// it runs under, and the extensions that binary loads, which
/// `--EXTENSIONS--` is answered from.
pub(super) struct Adapter<'a> {
    oracle: &'a Oracle<'a>,
    registry: &'a Registry,
    binary: &'a Path,
    lane: Option<Lane>,
    loaded: Vec<String>,
}

impl<'a> Adapter<'a> {
    pub(super) fn new(
        oracle: &'a Oracle<'a>,
        registry: &'a Registry,
        binary: &'a Path,
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
            lane,
            loaded,
        }
    }

    /// One test, end to end.
    pub(super) fn judge(&self, subset: &Subset, test: &Path) -> Result<Judged> {
        let id = subset.id(test);
        let name = subset.name(test);
        let phpt = match Phpt::load(test) {
            Ok(phpt) => phpt,
            Err(borked) => return Ok(Judged::new(&id, &name, Judgement::Bork, borked.0)),
        };
        if let Some(why) = unhosted(&phpt) {
            return Ok(Judged::new(&id, &name, Judgement::Skip, why));
        }
        if let Some(why) = self.missing(&phpt) {
            return Ok(Judged::new(&id, &name, Judgement::Skip, why));
        }
        let mut ini = match ini_of(&phpt, test) {
            Ok(ini) => ini,
            Err(why) => return Ok(Judged::new(&id, &name, Judgement::Skip, why)),
        };
        ini.extend(self.lane.map(Lane::ini));
        let env = self.environment(&phpt, test, &ini, &subset.root);
        let mut expected_to_fail = phpt.text("XFAIL").map(|why| why.trim().to_owned());
        if let Some(body) = phpt.filled("SKIPIF") {
            let out = self.run_section(subset, test, &env, "skip", body)?;
            let said = String::from_utf8_lossy(&out.stdout).into_owned();
            if let Some(reason) = reason_after("skip", &said) {
                return Ok(Judged::new(&id, &name, Judgement::Skip, reason));
            }
            if let Some(reason) = reason_after("xfail", &said) {
                expected_to_fail = Some(reason);
            }
        }

        let actual = match self.run_program(subset, &phpt, test, &ini, &env)? {
            Ok(actual) => actual,
            Err((judgement, why)) => return Ok(Judged::new(&id, &name, judgement, why)),
        };
        if let Some(construct) = unread(&actual) {
            return Ok(Judged::new(&id, &name, Judgement::Unread, construct));
        }
        let Some(expectation) = expectation_of(self.oracle.pin(), &phpt) else {
            return Ok(Judged::new(
                &id,
                &name,
                Judgement::Bork,
                "missing section --EXPECT--, --EXPECTF-- or --EXPECTREGEX--",
            ));
        };
        let (met, trouble) = match expectation.met(&actual) {
            Ok(met) => (met, String::new()),
            Err(e) => (false, format!("{e:#}\n")),
        };
        let compared = oracle::verdict(self.registry, &id, met, expectation.text(), &actual);
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
            ),
        })
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
    fn run_program(
        &self,
        subset: &Subset,
        phpt: &Phpt,
        test: &Path,
        ini: &[String],
        env: &[(String, String)],
    ) -> Result<Result<Vec<u8>, (Judgement, String)>> {
        let program = beside(test, None);
        if program.exists() {
            return Ok(Err((
                Judgement::Bork,
                format!(
                    "a file already stands where the test's program goes, {}",
                    program.display()
                ),
            )));
        }
        let body = phpt.section("FILE").unwrap_or_default().to_vec();
        let written = Written::new(&program, &body)?;
        let args = words(&phpt.text("ARGS").unwrap_or_default());
        let ran = self.oracle.run_under(&Invocation {
            ini,
            args: &args,
            env,
            stdin: phpt.section("STDIN"),
            merge_streams: merges_streams(phpt),
            cwd: Some(&subset.root),
            ..Invocation::new(self.binary, Program::Beside(&program))
        });
        drop(written);
        let output = match ran {
            Ok(output) => Ok(output.stdout),
            Err(e) => match e.downcast_ref::<PastLimit>() {
                Some(past) => Err((Judgement::Fail, past.to_string())),
                None => return Err(e.context(format!("running {}", test.display()))),
            },
        };
        if let Some(body) = phpt.filled("CLEAN") {
            self.run_section(subset, test, env, "clean", body)?;
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
    /// can do asks the binary under test, as upstream's asks it.
    fn run_section(
        &self,
        subset: &Subset,
        test: &Path,
        env: &[(String, String)],
        kind: &str,
        body: &[u8],
    ) -> Result<Observation> {
        let program = beside(test, Some(kind));
        let written = Written::new(&program, body)?;
        self.oracle.asserted()?;
        let pinned = &self.oracle.pin().oracle.path;
        let out = self
            .oracle
            .run_under(&Invocation {
                env,
                cwd: Some(&subset.root),
                ..Invocation::new(pinned, Program::Beside(&program))
            })
            .with_context(|| format!("running the {kind} section of {}", test.display()))?;
        drop(written);
        Ok(out)
    }

    /// The `--EXTENSIONS--` the runner's binary does not load, in
    /// upstream's words.
    fn missing(&self, phpt: &Phpt) -> Option<String> {
        let wanted = phpt.text("EXTENSIONS").unwrap_or_default().into_owned();
        let missing: Vec<&str> = wanted
            .split_whitespace()
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
    /// recipe's, as upstream hands it to the `SKIPIF`, the `FILE` and the
    /// `CLEAN` alike: the binary under test by the names the corpus spawns
    /// it by (run-tests.php:670-677), the test's own `--ENV--`
    /// (run-tests.php:2009-2018), and last `TEST_PHP_EXTRA_ARGS`, which is
    /// the recipe's flags and every ini entry the test runs under
    /// (run-tests.php:2115). `TEST_PHP_SRCDIR` names `root`, the tree the
    /// programs run in, as the variable names upstream's own
    /// (run-tests.php:179-183), and never the pinned tree, which a test
    /// does not write in (#1183). The recipe's `TEST_PHP_EXECUTABLE`
    /// names the pinned php, which is right for a witness and wrong here:
    /// what a test spawns is the binary being measured. No CGI or debugger
    /// host stands beside that binary, so their two names are empty, as
    /// upstream's `get_binary` leaves them when it finds none
    /// (run-tests.php:1003-1019).
    fn environment(
        &self,
        phpt: &Phpt,
        test: &Path,
        ini: &[String],
        root: &Path,
    ) -> Vec<(String, String)> {
        let binary = self.binary.to_string_lossy().into_owned();
        let recipe = &self.oracle.pin().recipe;
        let mut env = vec![
            ("TEST_PHP_EXECUTABLE_ESCAPED".to_owned(), escaped(&binary)),
            ("TEST_PHP_EXECUTABLE".to_owned(), binary),
            (
                oracle::TREE_VARIABLE.to_owned(),
                root.to_string_lossy().into_owned(),
            ),
        ];
        for host in ["TEST_PHP_CGI_EXECUTABLE", "TEST_PHPDBG_EXECUTABLE"] {
            env.push((host.to_owned(), String::new()));
            env.push((format!("{host}_ESCAPED"), escaped("")));
        }
        env.extend(env_of(phpt, test));
        env.push((
            "TEST_PHP_EXTRA_ARGS".to_owned(),
            extra_args(&recipe.args, recipe.ini.iter().chain(ini)),
        ));
        env
    }
}

/// Where a test's run writes a file beside it, as upstream does: its
/// program at `x.php`, and a section run beside it at `x.skip.php` or
/// `x.clean.php`.
fn beside(test: &Path, kind: Option<&str>) -> PathBuf {
    test.with_extension(kind.map_or_else(|| "php".to_owned(), |kind| format!("{kind}.php")))
}

/// What a test may not run beside in its run: its conflict keys and the
/// three files its run writes beside it, so no two tests whose files would
/// stand at one path run at once; alone where its conflict keys name
/// `all`, as upstream has it (run-tests.php:1365-1370). Another run is in
/// a copy of its own and holds nothing against this one (#1183).
pub(super) fn apart(test: &Path, dirs: &mut BTreeMap<PathBuf, Vec<String>>) -> Apart {
    let mut keys = file::conflicts(test, dirs);
    let within = if keys.iter().any(|key| key == "all") {
        Within::Alone
    } else {
        Within::Batch
    };
    keys.retain(|key| key != "all");
    keys.extend(
        [None, Some("skip"), Some("clean")].map(|kind| beside(test, kind).display().to_string()),
    );
    Apart { within, keys }
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
/// them: `Core` dropped, `Zend OPcache` mapped to `opcache` and the rest
/// lowered (run-tests.php:2908-2916). Upstream asks a program,
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
        .map(|name| {
            if name == "Zend OPcache" {
                "opcache".to_owned()
            } else {
                name.to_lowercase()
            }
        })
        .collect())
}

/// Why this runner cannot host a test: it serves the command line, and the
/// three shapes below are another host's or another program's.
fn unhosted(phpt: &Phpt) -> Option<String> {
    if phpt.has("REDIRECTTEST") {
        return Some("a --REDIRECTTEST--; this runner runs a subset, not a redirected set".into());
    }
    if phpt.is_cgi() {
        return Some("a CGI test; this runner hosts the command line".into());
    }
    if phpt.has("PHPDBG") {
        return Some("a --PHPDBG-- test; this engine has no debugger host".into());
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

/// Whether the two streams are captured as one, which is upstream's
/// default and what `--CAPTURE_STDIO--` can narrow.
fn merges_streams(phpt: &Phpt) -> bool {
    match phpt.text("CAPTURE_STDIO") {
        Some(capture) => {
            let capture = capture.to_uppercase();
            capture.contains("STDOUT") && capture.contains("STDERR")
        }
        None => true,
    }
}

/// The expectation the test carries, normalised as php normalises it.
fn expectation_of(pin: &Pin, phpt: &Phpt) -> Option<Expectation> {
    if let Some(body) = phpt.section("EXPECT") {
        return Some(Expectation::Exact(trimmed(pin, body)));
    }
    if let Some(body) = phpt.text("EXPECTF") {
        return Some(Expectation::Format(trimmed_text(pin, &body)));
    }
    phpt.text("EXPECTREGEX")
        .map(|body| Expectation::Pattern(trimmed_text(pin, &body)))
}

/// The recipe's normalisation over a section, which is what the stdout it
/// is compared with has been through; the oracle module owns the rule.
fn trimmed(pin: &Pin, body: &[u8]) -> Vec<u8> {
    oracle::normalise(&pin.recipe.normalise, body.to_vec())
}

/// The same over a section a runner reads as words.
fn trimmed_text(pin: &Pin, text: &str) -> String {
    String::from_utf8_lossy(&trimmed(pin, text.as_bytes())).into_owned()
}

/// A test's `--INI--`, one `-d` line each, with upstream's four
/// substitutions; an unset `{ENV:NAME}` is a skip with upstream's reason.
/// Each line is `name=value` with both halves trimmed, as
/// `settings2array` (run-tests.php:2950) splits and trims them, so an
/// `input_encoding = ISO-8859-1` is the entry and not a name php knows
/// nothing of.
fn ini_of(phpt: &Phpt, test: &Path) -> Result<Vec<String>, String> {
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

/// php's `escapeshellarg` on a POSIX host: the word in single quotes,
/// each quote inside closed, escaped and reopened.
fn escaped(word: &str) -> String {
    format!("'{}'", word.replace('\'', r"'\''"))
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
        let _ = write!(rendered, " {flag}");
    }
    rendered.push(' ');
    for (name, value) in settings {
        let _ = write!(rendered, " -d \"{name}={}\"", addslashes(value));
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

/// A test's `--ENV--`, one `NAME=value` a line.
fn env_of(phpt: &Phpt, test: &Path) -> Vec<(String, String)> {
    substituted(&phpt.text("ENV").unwrap_or_default(), test)
        .lines()
        .filter_map(|line| line.split_once('='))
        .map(|(name, value)| (name.trim().to_owned(), value.trim().to_owned()))
        .filter(|(name, _)| !name.is_empty())
        .collect()
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
fn reason_after(word: &str, said: &str) -> Option<String> {
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
struct Written {
    path: PathBuf,
}

impl Written {
    fn new(path: &Path, body: &[u8]) -> Result<Written> {
        fs::write(path, body).with_context(|| format!("writing {}", path.display()))?;
        Ok(Written {
            path: path.to_path_buf(),
        })
    }
}

impl Drop for Written {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
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
        assert_eq!(escaped("/a/it's/php"), r"'/a/it'\''s/php'");
    }

    #[test]
    fn the_streams_are_one_unless_capture_stdio_says_otherwise() {
        let phpt = |text: &str| Phpt::parse(Path::new("x.phpt"), text.as_bytes()).unwrap();
        let plain = phpt("--TEST--\nn\n--FILE--\nx\n--EXPECT--\n1\n");
        assert!(merges_streams(&plain));
        let one =
            phpt("--TEST--\nn\n--CAPTURE_STDIO--\nSTDIN STDOUT\n--FILE--\nx\n--EXPECT--\n1\n");
        assert!(!merges_streams(&one));
    }
}
