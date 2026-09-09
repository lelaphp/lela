//! The fresh-process adapter of design §8's `ScriptExec` seam: how one
//! `.phpt` is judged. The test's program is written where upstream writes
//! it and run through the binary under test with the sections that shape
//! one run (`--INI--`, `--ARGS--`, `--ENV--`, `--STDIN--`,
//! `--CAPTURE_STDIO--`), its `SKIPIF` and `CLEAN` sections run beside it
//! under the oracle, and its output held to its `--EXPECT*--` section with
//! php's normalisation. The verdict goes through [`oracle::verdict`], so
//! the registry decides what a difference means (harness §4.3). The lane,
//! the subsets and the reading are `phpt`'s; this is the per-item judge it
//! contributes.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use regex_lite::Regex;

use super::file::Phpt;
use super::{Judged, Judgement, Lane, Subset};
use crate::divergence::Registry;
use crate::oracle::expect::Expectation;
use crate::oracle::{self, Invocation, Oracle, Output, Pin, Program, Verdict as Compared};

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
        let mut expected_to_fail = phpt.text("XFAIL").map(|why| why.trim().to_owned());
        if let Some(body) = phpt.filled("SKIPIF") {
            let out = self.run_section(test, "skip", body)?;
            let said = String::from_utf8_lossy(&out.stdout).into_owned();
            if let Some(reason) = reason_after("skip", &said) {
                return Ok(Judged::new(&id, &name, Judgement::Skip, reason));
            }
            if let Some(reason) = reason_after("xfail", &said) {
                expected_to_fail = Some(reason);
            }
        }

        let actual = match self.run_program(&phpt, test, &ini)? {
            Ok(actual) => actual,
            Err(borked) => return Ok(Judged::new(&id, &name, Judgement::Bork, borked)),
        };
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
    /// left. A file already standing where the program goes borks the
    /// test rather than being overwritten.
    fn run_program(
        &self,
        phpt: &Phpt,
        test: &Path,
        ini: &[String],
    ) -> Result<Result<Vec<u8>, String>> {
        let program = test.with_extension("php");
        if program.exists() {
            return Ok(Err(format!(
                "a file already stands where the test's program goes, {}",
                program.display()
            )));
        }
        let body = phpt.section("FILE").unwrap_or_default().to_vec();
        let written = Written::new(&program, &body)?;
        let args = words(&phpt.text("ARGS").unwrap_or_default());
        let mut env = executable(self.binary);
        env.extend(env_of(phpt, test));
        let output = self
            .oracle
            .run_under(&Invocation {
                ini,
                args: &args,
                env: &env,
                stdin: phpt.section("STDIN"),
                merge_streams: merges_streams(phpt),
                ..Invocation::new(self.binary, Program::File(&program))
            })
            .with_context(|| format!("running {}", test.display()))?;
        drop(written);
        if let Some(body) = phpt.filled("CLEAN") {
            self.run_section(test, "clean", body)?;
        }
        Ok(Ok(output.stdout))
    }

    /// A section that runs beside the test rather than as it: `SKIPIF`,
    /// which decides whether the test applies at all, and `CLEAN`, which
    /// takes away what it left. Neither is given the test's own `--INI--`,
    /// as upstream gives neither. Both run under the oracle, not the
    /// engine: what they answer must be true of the corpus whatever the
    /// engine can do yet, and a young engine must not be trusted with
    /// cleanup. Upstream runs them under the binary it is testing, so a
    /// `SKIPIF` that asks what the engine has is answered here by php; it
    /// is the runner's one known difference from upstream's verdicts.
    fn run_section(&self, test: &Path, kind: &str, body: &[u8]) -> Result<Output> {
        let program = test.with_extension(format!("{kind}.php"));
        let written = Written::new(&program, body)?;
        let out = self
            .oracle
            .run(Program::File(&program), None)
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
}

/// The extensions a binary loads under the recipe, as upstream asks for
/// them: `Core` dropped, `Zend OPcache` mapped to `opcache` and the rest
/// lowered (run-tests.php:2908-2916).
fn loaded_extensions(oracle: &Oracle<'_>, binary: &Path) -> Result<Vec<String>> {
    let snippet = r#"echo implode("\n", get_loaded_extensions());"#;
    let out = oracle.run_under(&Invocation::new(binary, Program::Snippet(snippet)))?;
    Ok(String::from_utf8_lossy(&out.stdout)
        .lines()
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

/// The recipe's normalisation over a section, which is what the output it
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
        .map(str::trim)
        .filter(|line| line.contains('='))
        .map(str::to_owned)
        .collect())
}

/// The binary under test, under the two names the corpus spawns it by
/// (run-tests.php:670-673). The recipe's `TEST_PHP_EXECUTABLE` names the
/// pinned oracle, which is right for a witness and wrong for a phpt lane:
/// what a test spawns is the binary being measured, so these two stand
/// over the recipe's and under the test's own `--ENV--`.
fn executable(binary: &Path) -> Vec<(String, String)> {
    let path = binary.to_string_lossy().into_owned();
    let escaped = format!("'{}'", path.replace('\'', r"'\''"));
    vec![
        ("TEST_PHP_EXECUTABLE".to_owned(), path),
        ("TEST_PHP_EXECUTABLE_ESCAPED".to_owned(), escaped),
    ]
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
/// upstream's `/^skip\s*(.+)/i` over the section's output.
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
/// after, which is where upstream puts a test's program too.
struct Written(PathBuf);

impl Written {
    fn new(path: &Path, body: &[u8]) -> Result<Written> {
        fs::write(path, body).with_context(|| format!("writing {}", path.display()))?;
        Ok(Written(path.to_path_buf()))
    }
}

impl Drop for Written {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
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
    fn the_streams_are_one_unless_capture_stdio_says_otherwise() {
        let phpt = |text: &str| Phpt::parse(Path::new("x.phpt"), text.as_bytes()).unwrap();
        let plain = phpt("--TEST--\nn\n--FILE--\nx\n--EXPECT--\n1\n");
        assert!(merges_streams(&plain));
        let one =
            phpt("--TEST--\nn\n--CAPTURE_STDIO--\nSTDIN STDOUT\n--FILE--\nx\n--EXPECT--\n1\n");
        assert!(!merges_streams(&one));
    }
}
