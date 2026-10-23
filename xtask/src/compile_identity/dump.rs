//! How one file of the corpus is judged: its program written where the
//! pinned oracle compiles it from, dumped once by php and once by the
//! compiler under measurement, and the two held to byte-identity through
//! [`oracle::compare`], so the registry decides what a difference means
//! (harness §4.3). The tally, the corpus and the reading are the lane's;
//! this is the per-file judge it contributes.
//!
//! **Which compiler dumps.** A binary, one process per program under the
//! same recipe as php: the engine's own, which honours php's dump flags,
//! or the pinned oracle standing in as the lane's control. The harness
//! links no compiler of its own (harness §8, revision 84; #2166).
//!
//! **Why one program under one name.** php prints the name it was given
//! into each block of the dump, so two programs under two names would
//! differ there before an opline was compared. One program, under one
//! name, the absolute path the process adapter runs a file by
//! ([`oracle::run_by`], #331), in one directory, under one recipe: what
//! is left between the two dumps is the compiler.
//!
//! **What each binary folds from its own build.** Both compilers fold
//! `PHP_BINARY` and `PHP_BUILD_DATE` into the op array, as php's
//! `zend_try_ct_eval_const` does with a persistent internal constant, so
//! each dump carries the path its own binary runs from and the day it was
//! built, and the two differ by construction. Before the two dumps are
//! compared, the lane asks each binary once per run for those two values,
//! under the recipe and regimes the dump runs under, and puts one fixed
//! placeholder per constant, `<PHP_BINARY>` and `<PHP_BUILD_DATE>`, where
//! exactly that binary's own value appears in a string literal of its
//! dump: the whole literal, `string("<PHP_BINARY>")`, or the part a
//! folded concatenation took from it, `string("<PHP_BINARY> -S …")`
//! ([`Folded`], as #2692 ruled). A registry row would accept the whole
//! file, since the registry is looked up per file, and hide any later
//! difference in it; the placeholder hides the value and nothing else, so
//! a compiler that folds a value other than its own, or folds where php
//! does not, still differs.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::{Corpus, INSTRUMENT, Judged, Judgement, LINT};
use crate::divergence::Registry;
use crate::oracle::{
    self, Ended, Invocation, Observation, Oracle, PastLimit, Program, Verdict as Compared,
};
use crate::phpt::file::Phpt;
use crate::scratch::Tree;

/// The two dumps of one run: the oracle and the registry, the binary
/// whose dump is held to the oracle's, and the directory the programs are
/// written in, which is taken away when the run ends.
pub(super) struct Dumps<'a> {
    oracle: &'a Oracle<'a>,
    registry: &'a Registry,
    compiler: &'a Path,
    dir: Programs,
    /// `-l`, the one flag before the program, held as the invocation takes
    /// it.
    flags: [String; 1],
    /// The regimes both binaries dump under, `dump` first.
    regimes: Vec<&'a str>,
    /// What the oracle folds from its own build, asked once for the run;
    /// none where it did not say.
    oracle_folds: Option<Folded>,
    /// What the compiler under measurement folds from its own.
    compiler_folds: Option<Folded>,
}

impl<'a> Dumps<'a> {
    pub(super) fn new(
        oracle: &'a Oracle<'a>,
        registry: &'a Registry,
        compiler: &'a Path,
        regimes: &[&'a str],
    ) -> Result<Dumps<'a>> {
        Ok(Dumps {
            oracle,
            registry,
            compiler,
            dir: Programs::new()?,
            flags: [LINT.to_owned()],
            regimes: regimes.to_vec(),
            oracle_folds: Folded::asked(oracle, &oracle.pin().oracle.path, regimes),
            compiler_folds: Folded::asked(oracle, compiler, regimes),
        })
    }

    /// One file, end to end: its program, the two dumps, the verdict.
    pub(super) fn judge(&self, corpus: &Corpus, file: &Path) -> Result<Judged> {
        let id = corpus.id(file);
        let name = corpus.name(file);
        let body = match held(file) {
            Held::Program(body) => body,
            Held::NoProgram(why) => return Ok(Judged::new(&id, &name, Judgement::Skip, why)),
            Held::Unreadable(why) => return Ok(Judged::new(&id, &name, Judgement::Bork, why)),
        };
        let program = self.dir.write(file, &body)?;
        let oracle = &self.oracle.pin().oracle.path;
        let expected = self
            .dump(oracle, &program)
            .with_context(|| format!("dumping {} under {}", program.display(), oracle.display()))?;
        let actual = self.dump(self.compiler, &program);
        fs::remove_file(&program).ok();
        let actual = match engine_side(actual) {
            Ok(dumped) => dumped,
            Err((judgement, why)) => return Ok(Judged::new(&id, &name, judgement, why)),
        };
        Ok(verdict(
            self.registry,
            &id,
            &name,
            &Dumped::from(expected).placed(self.oracle_folds.as_ref()),
            &actual.placed(self.compiler_folds.as_ref()),
        ))
    }

    /// What one binary prints compiling the program: the pin's recipe, its
    /// `dump` regime and any regime after it, and `-l`, which compiles the
    /// file and does not run it. php prints its op arrays on the second stream, so the dump is
    /// what that stream carries; the first holds `-l`'s own sentence,
    /// which is no op array and is not compared.
    fn dump(&self, binary: &Path, program: &Path) -> Result<Observation> {
        self.oracle.run_under(&Invocation {
            regimes: &self.regimes,
            flags: &self.flags,
            ..Invocation::new(binary, Program::File(program))
        })
    }
}

/// The engine's side of one file: its dump, or, where it finished none,
/// the file's verdict, and never the lane's end (design §8). A dump past
/// the recipe's limit, or one a signal ended, is the compiler's failure on
/// the file as a wrong dump is, named (#699, #1069), and a binary that
/// would not run at all is the file's bork; the rest of the corpus is
/// judged either way.
pub(super) fn engine_side(run: Result<Observation>) -> Result<Dumped, (Judgement, String)> {
    match run {
        Ok(Observation {
            exit: Ended::Signal(signal),
            ..
        }) => Err((
            Judgement::Fail,
            format!("the engine's binary died on signal {signal} dumping it"),
        )),
        Ok(observed) => Ok(observed.into()),
        Err(trouble) => Err(match trouble.downcast_ref::<PastLimit>() {
            Some(past) => (Judgement::Fail, past.to_string()),
            None => (
                Judgement::Bork,
                format!("the engine's side did not dump it: {trouble:#}"),
            ),
        }),
    }
}

impl From<Observation> for Dumped {
    fn from(out: Observation) -> Dumped {
        Dumped {
            arrays: out.stderr,
            said: out.stdout,
        }
    }
}

/// What one binary printed compiling one program: the op arrays it dumped,
/// and what it said beside them. php prints its op arrays on the second
/// stream and keeps the first for `-l`'s own sentence and for a compile
/// error, so `said` is never an op array and is never compared; it is the
/// reason a file that compiled to nothing carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Dumped {
    pub arrays: Vec<u8>,
    pub said: Vec<u8>,
}

impl Dumped {
    /// A dump of these op arrays and nothing said: what a test pins.
    #[cfg(test)]
    pub(super) fn of(arrays: &[u8]) -> Dumped {
        Dumped {
            arrays: arrays.to_vec(),
            said: Vec::new(),
        }
    }

    /// These op arrays with the binary's own folded values written as
    /// their constants' placeholders, and as they stand where the binary
    /// did not say its values.
    pub(super) fn placed(self, folded: Option<&Folded>) -> Dumped {
        match folded {
            Some(folded) => Dumped {
                arrays: folded.put_out(&self.arrays),
                said: self.said,
            },
            None => self,
        }
    }

    /// What the binary said, as a note's one line.
    fn line(&self) -> String {
        String::from_utf8_lossy(&self.said)
            .lines()
            .next()
            .unwrap_or_default()
            .to_owned()
    }
}

/// The verdict on one file's two dumps, the oracle's and the engine's,
/// through [`oracle::compare`], so the registry is what accepts a
/// difference. Byte-identity is the whole standard: a dump that differs
/// from the oracle's by one line is a failure with the difference as its
/// note, and the fixture under `xtask/fixtures/compile-identity` is the
/// negative control that says so.
///
/// **A file php compiled nothing from is no measurement.** php dumps
/// nothing for a program it could not compile: the parse error goes on the
/// first stream and the second stays empty, which hundreds of the corpus's
/// own tests are written to provoke. Two empty dumps are not identity,
/// they are two compilers that both stopped, and a pass there would count
/// every such file as green against an engine that compiles nothing at
/// all. The file is skipped with what php said as its reason, and counted
/// as skipped in the reading. An engine that dumps where php dumped
/// nothing still differs, and is still a failure.
pub(super) fn verdict(
    registry: &Registry,
    id: &str,
    name: &str,
    expected: &Dumped,
    actual: &Dumped,
) -> Judged {
    if expected.arrays.is_empty() && actual.arrays.is_empty() {
        let said = expected.line();
        let why = if said.is_empty() {
            "php dumped no op array for it".to_owned()
        } else {
            format!("php dumped no op array for it; it said: {said}")
        };
        return Judged::new(id, name, Judgement::Skip, why);
    }
    match oracle::compare(registry, id, &expected.arrays, &actual.arrays) {
        Compared::Same => Judged::new(id, name, Judgement::Pass, ""),
        Compared::Diverged(row) => Judged::new(id, name, Judgement::Diverged, row.reason),
        Compared::Moved(moved) => Judged::new(id, name, Judgement::Fail, moved.to_string()),
        Compared::Differs(difference) => Judged::new(
            id,
            name,
            Judgement::Fail,
            format!("the dump is not the oracle's:\n{difference}"),
        ),
    }
}

/// The program that asks a binary what it folds: the two values, one NUL
/// apart, as each would fold into a literal of its dump.
const ASK: &str = r#"echo PHP_BINARY, "\0", PHP_BUILD_DATE;"#;

/// The two values one binary folds from its own build where a program
/// names `PHP_BINARY` or `PHP_BUILD_DATE`, as the binary itself says them:
/// never computed or guessed by the lane (#2692, #2697).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Folded {
    /// `PHP_BINARY`: the path the binary runs from.
    binary: Vec<u8>,
    /// `PHP_BUILD_DATE`: the day and time it was built.
    build_date: Vec<u8>,
}

impl Folded {
    /// What `binary` says it folds, asked once under the recipe and the
    /// regimes the dump runs under. A binary that does not answer with two
    /// values, a stand-in or one that cannot run the program, has its dump
    /// compared with its literals as they stand, and the run says so.
    fn asked(oracle: &Oracle<'_>, binary: &Path, regimes: &[&str]) -> Option<Folded> {
        let seen = oracle.run_under(&Invocation {
            regimes,
            raw: true,
            ..Invocation::new(binary, Program::Snippet(ASK))
        });
        let folded = match &seen {
            Ok(seen) if seen.exit == Ended::Status(0) => Folded::said(&seen.stdout),
            _ => None,
        };
        if folded.is_none() {
            let why = match seen {
                Ok(seen) => format!("it wrote {:?}", String::from_utf8_lossy(&seen.stdout)),
                Err(trouble) => format!("{trouble:#}"),
            };
            println!(
                "{INSTRUMENT}: {} did not say its own PHP_BINARY and PHP_BUILD_DATE ({why}); its dump's literals are compared as they stand",
                binary.display()
            );
        }
        folded
    }

    /// The two values out of a binary's answer, and none for an answer
    /// that is not two values one NUL apart. An empty value says nothing:
    /// `string("")` is every empty string of the dump, not the binary's.
    pub(super) fn said(answer: &[u8]) -> Option<Folded> {
        let mut values = answer.split(|&byte| byte == 0);
        let (Some(binary), Some(build_date), None) = (values.next(), values.next(), values.next())
        else {
            return None;
        };
        if binary.is_empty() || build_date.is_empty() {
            return None;
        }
        Some(Folded {
            binary: binary.to_vec(),
            build_date: build_date.to_vec(),
        })
    }

    /// `arrays` with each of the two values, as php's dump prints it, written
    /// as its constant's placeholder wherever it stands inside a string
    /// literal: a literal the value is the whole of, and one a folded
    /// concatenation made of it and more. The dump is walked a token at a
    /// time, an escape being one, so a value matches only where it starts
    /// on a token of the literal; outside a literal nothing is touched.
    fn put_out(&self, arrays: &[u8]) -> Vec<u8> {
        let values = [
            (escaped(&self.binary), BINARY),
            (escaped(&self.build_date), BUILD_DATE),
        ];
        let mut out = Vec::with_capacity(arrays.len());
        let mut at = 0;
        let mut inside = false;
        while at < arrays.len() {
            let rest = &arrays[at..];
            let step = if !inside {
                inside = rest.starts_with(OPENS);
                if inside { OPENS.len() } else { 1 }
            } else if let Some((value, placeholder)) =
                values.iter().find(|(value, _)| rest.starts_with(value))
            {
                out.extend_from_slice(placeholder);
                at += value.len();
                continue;
            } else {
                match rest {
                    [b'"', ..] => {
                        inside = false;
                        1
                    }
                    [b'\\', b'x', ..] => 4,
                    [b'\\', ..] => 2,
                    _ => 1,
                }
            };
            let step = step.min(rest.len());
            out.extend_from_slice(&rest[..step]);
            at += step;
        }
        out
    }
}

/// Where php's dump opens a string literal, `zend_dump_const`; inside it
/// every `"` is escaped, so the first one unescaped closes it.
const OPENS: &[u8] = b"string(\"";
/// What stands in a literal where the binary's own `PHP_BINARY` stood.
const BINARY: &[u8] = b"<PHP_BINARY>";
/// What stands in a literal where its own `PHP_BUILD_DATE` stood.
const BUILD_DATE: &[u8] = b"<PHP_BUILD_DATE>";

/// Bytes as php's dump prints them inside a string literal,
/// `zend_dump_const`: escaped as `smart_str_append_escaped` escapes them,
/// with `"` escaped after.
fn escaped(value: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(value.len());
    for &byte in value {
        match byte {
            b'"' => out.extend_from_slice(b"\\\""),
            b'\\' => out.extend_from_slice(b"\\\\"),
            b'\n' => out.extend_from_slice(b"\\n"),
            b'\r' => out.extend_from_slice(b"\\r"),
            b'\t' => out.extend_from_slice(b"\\t"),
            0x0c => out.extend_from_slice(b"\\f"),
            0x0b => out.extend_from_slice(b"\\v"),
            0x1b => out.extend_from_slice(b"\\e"),
            32..=126 => out.push(byte),
            _ => out.extend_from_slice(format!("\\x{byte:02X}").as_bytes()),
        }
    }
    out
}

/// What a file of the corpus holds to dump.
pub(super) enum Held {
    Program(Vec<u8>),
    /// The file holds no program this lane can dump, and why.
    NoProgram(String),
    /// The file is not one this lane can read, in upstream's words.
    Unreadable(String),
}

/// The program one file of the corpus holds: a `.phpt`'s `--FILE--`
/// section, read as upstream's runner reads it, and a `.php` whole. A
/// test that redirects to another, or that hosts a debugger rather than
/// the command line, carries no program of its own and is skipped with
/// its reason, as the phpt lane skips it.
pub(super) fn held(file: &Path) -> Held {
    if file.extension().is_some_and(|e| e == "php") {
        return match fs::read(file) {
            Ok(body) => Held::Program(body),
            Err(e) => Held::Unreadable(format!("cannot read [{}]: {e}", file.display())),
        };
    }
    let phpt = match Phpt::load(file) {
        Ok(phpt) => phpt,
        Err(borked) => return Held::Unreadable(borked.0),
    };
    if phpt.has("REDIRECTTEST") {
        return Held::NoProgram("the test redirects to another and holds no program".to_owned());
    }
    if phpt.has("PHPDBG") {
        return Held::NoProgram("the test hosts phpdbg, not the command line".to_owned());
    }
    match phpt.section("FILE") {
        Some(body) => Held::Program(body.to_vec()),
        None => Held::NoProgram("the test has no --FILE-- section".to_owned()),
    }
}

/// Where the programs of one run are written: a directory of this run's
/// own, taken away when the run ends, so nothing of the pinned tree is
/// written to and no file of the corpus is compiled where it stands. The
/// oracle module keeps a directory of its own for the streams of one
/// program; this one holds the program both binaries compile.
struct Programs(Tree);

impl Programs {
    fn new() -> Result<Programs> {
        // One directory per run of the lane, never one per process: two
        // runs in one process, which is what the tests are, must not
        // write over or take away each other's programs. The counter in
        // a scratch tree's name is what keeps them apart.
        Ok(Programs(Tree::new(super::INSTRUMENT)?))
    }

    /// The program of one file, written under the stem the file has in the
    /// corpus, so the path the dump prints is legible in a difference.
    /// Both binaries compile this one file, and the judge takes it away
    /// once they have.
    fn write(&self, file: &Path, body: &[u8]) -> Result<PathBuf> {
        let name = file
            .file_stem()
            .map_or_else(|| PathBuf::from("program"), PathBuf::from)
            .with_extension("php");
        let path = self.0.join(name);
        crate::write(&path, body)?;
        Ok(path)
    }
}
