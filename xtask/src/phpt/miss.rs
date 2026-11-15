//! What a miss keeps: for each test that counts against its subset or
//! that the compiler refused, the first line where what ran and what the
//! test expects part, and a [`Signature`] that names the cause.
//! Both are decoded once, where the adapter reaches the verdict, carried
//! on the verdict and kept on the run, so learning why a test missed never
//! means running it again, and the census groups by the signature the run
//! kept rather than working one out afresh.
//!
//! **The first differing lines.** The expectation is aligned with what
//! ran line by line ([`oracle::align`]), a line of an `--EXPECTF--` held
//! to what it matches rather than to its text, so a `%s` standing for a
//! path is no difference. A line that may stand for many (`%a`, `%A`) takes
//! the lines after it that nothing else stands for. What is left is the
//! first line the engine wrote that the test does not expect, and the
//! first line the test expects that the engine did not write.
//!
//! **The signature.** Named by what is missing where it can be (an
//! undefined function, method or constant, a class-like not found), masked
//! of strings, paths and numbers where the line is a diagnostic, and
//! `output differs` where it is neither. Where the engine wrote nothing the
//! test does not expect, the cause is what php wrote and the engine did
//! not, `php only:` and the first such diagnostic, where a compile or link
//! fatal php raises lands. `.claude/skills/specs/first-failures.awk` read
//! the same off a printed run; this is that rule, kept on the run.
//!
//! **How the program died.** Where a signal ended the test's program, or
//! it exited 129 to 159 as a shell's child a signal ended does, the miss
//! keeps that ending and the last of what it wrote on stderr ([`Died`]),
//! which upstream's `Termsig=` line among the lines does not tell apart
//! and which no rerun is sure to show again.

use std::fmt;
use std::sync::LazyLock;

use regex_lite::Regex;
use serde::{Deserialize, Serialize};

use crate::oracle::expect::{self, Expectation};
use crate::oracle::{self, Ended, Step};

/// The longest a masked line is kept, in characters.
const MASKED: usize = 100;

/// The words that make a line a diagnostic php writes.
const DIAGNOSTICS: [&str; 5] = [
    "Fatal error",
    "Parse error",
    "Warning",
    "Deprecated",
    "Notice",
];

/// What one missed test keeps on the run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Miss {
    /// The first line the test expects that the engine did not write.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected: Option<String>,
    /// The first line the engine wrote that the test does not expect.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actual: Option<String>,
    pub signature: Signature,
    /// How the program died, where a signal ended it: none for a run
    /// that exited as a run does, and for a run kept before the field was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub died: Option<Died>,
}

/// How a test's program died, which upstream's `Termsig=` line says too
/// little of to tell what ended it: whether a signal ended it or
/// it exited with a status a shell gives a child a signal ended, and the
/// last of what it wrote on stderr.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Died {
    /// The signal, or the status from 129 to 159.
    pub ended: Ended,
    /// The last [`TAIL`] lines of its stderr, the stream its stdout is
    /// too where the test captures both as one (upstream's `2>&1`); empty
    /// where it wrote none.
    pub stderr: String,
}

/// The most lines of stderr a [`Died`] keeps.
pub(crate) const TAIL: usize = 20;

impl Died {
    /// How a run that ended as `ended` died, `stderr` being its stderr,
    /// or none where it exited as a run does: a status of 128 or under,
    /// or past 159, which no shell gives a child a signal ended.
    pub(crate) fn of(ended: Ended, stderr: &[u8]) -> Option<Died> {
        if !matches!(ended, Ended::Signal(_) | Ended::Status(129..=159)) {
            return None;
        }
        let text = String::from_utf8_lossy(stderr);
        let lines: Vec<&str> = text.lines().collect();
        let tail = &lines[lines.len().saturating_sub(TAIL)..];
        Some(Died {
            ended,
            stderr: tail.join("\n"),
        })
    }

    /// What a failure's note ends with of how its program died: the
    /// death on a line of its own, and nothing where it did not die.
    pub(crate) fn noted(died: Option<&Died>) -> String {
        died.map_or_else(String::new, |died| format!("\n{died}"))
    }
}

impl fmt::Display for Died {
    /// The words a failure's note gives it: the ending on its own line,
    /// then the stderr's tail indented under it, a line a reader of the
    /// printed run never takes for a test's header or its difference.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.ended {
            Ended::Signal(n) => write!(f, "the program was ended by signal {n}")?,
            Ended::Status(n) => write!(
                f,
                "the program exited with status {n}, as a shell says signal {} ended its child",
                n - 128
            )?,
        }
        if self.stderr.is_empty() {
            return f.write_str(", having written nothing on stderr");
        }
        f.write_str(", and its stderr ended:")?;
        for line in self.stderr.lines() {
            write!(f, "\n    {line}")?;
        }
        Ok(())
    }
}

/// Why a test missed, as the census groups it: one variant per kind of
/// cause, so a group is a value and never a spelling.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Signature {
    /// The compiler refused the program, in its own sentence.
    Unread(String),
    /// The engine's first line of its own names what it lacks.
    Undefined(Symbol),
    /// The engine's first line of its own is a diagnostic, masked.
    Diagnostic(String),
    /// The engine's first line of its own is neither, or the two part
    /// where no single line shows it.
    Differs,
    /// The engine wrote nothing the test does not expect: the first
    /// diagnostic php wrote and the engine did not, masked, or none where
    /// what php alone wrote holds no diagnostic.
    PhpOnly(Option<String>),
    /// The program ran past the recipe's limit.
    PastLimit,
    /// The runner could not read the test, masked.
    Bork(String),
}

/// What an engine's diagnostic says it has not got.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Symbol {
    Function(String),
    /// `Class::method`, as php names it.
    Method(String),
    Constant(String),
    /// A class, interface, trait or enum, by the word php names it with.
    ClassLike {
        kind: String,
        name: String,
    },
}

impl fmt::Display for Signature {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Signature::Unread(construct) => f.write_str(construct),
            Signature::Undefined(Symbol::Function(name)) => write!(f, "undefined function {name}"),
            Signature::Undefined(Symbol::Method(name)) => write!(f, "undefined method {name}"),
            Signature::Undefined(Symbol::Constant(name)) => write!(f, "undefined constant {name}"),
            Signature::Undefined(Symbol::ClassLike { kind, name }) => {
                write!(f, "{kind} \"{name}\" not found")
            }
            Signature::Diagnostic(line) => f.write_str(line),
            Signature::Differs => f.write_str("output differs"),
            Signature::PhpOnly(Some(line)) => write!(f, "php only: {line}"),
            Signature::PhpOnly(None) => f.write_str("php only: no diagnostic"),
            Signature::PastLimit => f.write_str("ran past the recipe's limit"),
            Signature::Bork(why) => write!(f, "bork: {why}"),
        }
    }
}

impl Miss {
    /// A test the compiler refused, by the construct its refusal names.
    pub(crate) fn unread(construct: &str) -> Miss {
        Miss::without_lines(Signature::Unread(construct.to_owned()))
    }

    /// A test whose program ran past the recipe's limit.
    pub(crate) fn past_limit() -> Miss {
        Miss::without_lines(Signature::PastLimit)
    }

    /// A test the runner could not read, by the first line of why.
    pub(crate) fn bork(why: &str) -> Miss {
        Miss::without_lines(Signature::Bork(masked(why.lines().next().unwrap_or(""))))
    }

    fn without_lines(signature: Signature) -> Miss {
        Miss {
            expected: None,
            actual: None,
            signature,
            died: None,
        }
    }

    /// A test whose program wrote `actual`, normalised, where the test
    /// expects `expectation`: the first line each has that the other has
    /// not, and the signature they come to.
    pub(crate) fn of(expectation: &Expectation, actual: &[u8]) -> Miss {
        let lines = expectation.lines();
        let ran: Vec<&[u8]> = expect::lines(actual).collect();
        let (texts, shown): (Vec<_>, Vec<_>) = (
            lines.iter().map(expect::Line::text).collect(),
            ran.iter()
                .map(|line| String::from_utf8_lossy(line))
                .collect(),
        );
        let mut expected: Vec<&str> = Vec::new();
        let mut written: Vec<&str> = Vec::new();
        let mut spanning = false;
        for step in oracle::align(lines.len(), ran.len(), |i, j| lines[i].meets(ran[j])) {
            match step {
                Step::Both(i, _) => spanning = lines[i].spans(),
                Step::Expected(i) => {
                    spanning = lines[i].spans();
                    if !spanning {
                        expected.push(&texts[i]);
                    }
                }
                Step::Actual(j) if !spanning => written.push(&shown[j]),
                Step::Actual(_) => {}
            }
        }
        let written = first(&written);
        Miss {
            expected: first(&expected).map(str::to_owned),
            actual: written.map(str::to_owned),
            signature: signature(written, &expected),
            died: None,
        }
    }

    /// The miss with how its program died, where it did.
    pub(crate) fn dying(self, died: Option<Died>) -> Miss {
        Miss { died, ..self }
    }
}

/// The first of the lines that says something: a blank line is the
/// first only where every line is blank.
fn first<'a>(lines: &[&'a str]) -> Option<&'a str> {
    lines
        .iter()
        .find(|line| !line.trim().is_empty())
        .or_else(|| lines.first())
        .copied()
}

/// The signature of a difference, the one place it is decided: the
/// engine's first line of its own, `written`, named by what it lacks or
/// masked where it is a diagnostic; else the first diagnostic of the
/// lines php alone wrote, `php_only`; else the two differ.
fn signature(written: Option<&str>, php_only: &[&str]) -> Signature {
    match written {
        Some(line) => match undefined(line) {
            Some(symbol) => Signature::Undefined(symbol),
            None if is_diagnostic(line) => Signature::Diagnostic(masked(line)),
            None => Signature::Differs,
        },
        None if php_only.is_empty() => Signature::Differs,
        None => Signature::PhpOnly(
            php_only
                .iter()
                .find(|line| is_diagnostic(line))
                .map(|line| masked(&unwildcarded(line))),
        ),
    }
}

/// What a line says the engine has not got, in php's four sentences for
/// it.
fn undefined(line: &str) -> Option<Symbol> {
    static SENTENCES: LazyLock<[Regex; 4]> = LazyLock::new(|| {
        [
            r"Call to undefined function ([A-Za-z0-9_\\]+)",
            r"Call to undefined method ([A-Za-z0-9_\\:]+)",
            r#"(Class|Interface|Trait|Enum) "([^"]+)" not found"#,
            r#"Undefined constant "([^"]+)""#,
        ]
        .map(fixed)
    });
    let [function, method, class_like, constant] = &*SENTENCES;
    if let Some(found) = function.captures(line) {
        return Some(Symbol::Function(found[1].to_owned()));
    }
    if let Some(found) = method.captures(line) {
        return Some(Symbol::Method(found[1].to_owned()));
    }
    if let Some(found) = class_like.captures(line) {
        return Some(Symbol::ClassLike {
            kind: found[1].to_owned(),
            name: found[2].to_owned(),
        });
    }
    constant
        .captures(line)
        .map(|found| Symbol::Constant(found[1].to_owned()))
}

fn is_diagnostic(line: &str) -> bool {
    DIAGNOSTICS.iter().any(|word| line.contains(word))
}

/// A line with its strings, its paths and its numbers each written `_`,
/// and cut at [`MASKED`] characters, so two tests that part at one
/// diagnostic about different names group together.
pub(crate) fn masked(line: &str) -> String {
    static MASKS: LazyLock<[Regex; 3]> =
        LazyLock::new(|| [r#""[^"]*""#, r"[^ ]+\.(?:php|inc)", r"[0-9]+"].map(fixed));
    let line = MASKS.iter().fold(line.to_owned(), |line, mask| {
        mask.replace_all(&line, "_").into_owned()
    });
    line.chars().take(MASKED).collect()
}

/// An expected line with each of `--EXPECTF--`'s wildcards written `_`,
/// as [`masked`] writes what they stand for in what ran.
fn unwildcarded(line: &str) -> String {
    static WILDCARD: LazyLock<Regex> = LazyLock::new(|| fixed(r"%r.*?%r|%[esSaAwidxfc0]"));
    WILDCARD.replace_all(line, "_").into_owned()
}

/// A pattern written here, which compiles or is a defect of this file.
fn fixed(pattern: &str) -> Regex {
    Regex::new(pattern).unwrap_or_else(|err| unreachable!("the pattern {pattern}: {err}"))
}

#[cfg(test)]
mod tests;
