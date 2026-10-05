//! How one file of the corpus is judged: its program written where the
//! pinned oracle compiles it from, dumped once by php and once by the
//! compiler under measurement, and the two held to byte-identity through
//! [`oracle::compare`], so the registry decides what a difference means
//! (harness §4.3). The tally, the corpus and the reading are the lane's;
//! this is the per-file judge it contributes.
//!
//! **Which compiler dumps.** Design §6.1: the lane compiles in this
//! process, through `php_bundle`, and [`Compiler::Linked`] is that
//! adapter; [`Compiler::Binary`] is the rung above it, a binary standing
//! where the engine's will once it honours php's dump flags, one process
//! per program under the same recipe. Both answer one [`Dumped`], so the
//! verdict never learns which of the two printed it.
//!
//! **Why one program under one name.** php prints the name it was given
//! into each block of the dump, so two programs under two names would
//! differ there before an opline was compared. One program, under one
//! name, the absolute path the process adapter runs a file by
//! ([`oracle::run_by`], #331), in one directory, under one recipe: what
//! is left between the two dumps is the compiler.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::{Corpus, Judged, Judgement, LINT, linked};
use crate::divergence::Registry;
use crate::oracle::{self, Invocation, Oracle, Program, Verdict as Compared};
use crate::phpt::file::Phpt;
use crate::scratch::Tree;

/// What takes the engine's side of one dump.
pub(super) enum Compiler<'a> {
    /// The compiler linked into this process, reached through
    /// `php_bundle` (design §6.1): what the lane stands on from day 1,
    /// with no binary built.
    Linked(php_bundle::test::Engine),
    /// A binary of its own, one process per program under the pin's
    /// recipe: the pinned oracle standing in as the lane's control, and
    /// the engine's own binary once it honours php's dump flags.
    Binary(&'a Path),
}

/// The two dumps of one run: the oracle and the registry, the compiler
/// whose dump is held to the oracle's, and the directory the programs are
/// written in, which is taken away when the run ends.
pub(super) struct Dumps<'a> {
    oracle: &'a Oracle<'a>,
    registry: &'a Registry,
    compiler: Compiler<'a>,
    dir: Programs,
    /// `-l`, the one flag before the program, held as the invocation takes
    /// it.
    flags: [String; 1],
    /// The regimes both binaries dump under, `dump` first.
    regimes: Vec<&'a str>,
}

impl<'a> Dumps<'a> {
    pub(super) fn new(
        oracle: &'a Oracle<'a>,
        registry: &'a Registry,
        compiler: Compiler<'a>,
        regimes: &[&'a str],
    ) -> Result<Dumps<'a>> {
        Ok(Dumps {
            oracle,
            registry,
            compiler,
            dir: Programs::new()?,
            flags: [LINT.to_owned()],
            regimes: regimes.to_vec(),
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
        let expected = self.dump(&self.oracle.pin().oracle.path, &program)?;
        let actual = self.engine_dump(&program, &body);
        let _ = fs::remove_file(&program);
        // Trouble on the engine's side is one file's and never the
        // lane's, whichever compiler it was: a panic inside the linked
        // one (design §8) as much as a binary that would not start. The
        // rest of the corpus is still measured and this file is counted
        // against the reading.
        let actual = match actual {
            Ok(dumped) => dumped,
            Err(trouble) => {
                let why = format!("the engine's side did not dump it: {trouble:#}");
                return Ok(Judged::new(&id, &name, Judgement::Bork, why));
            }
        };
        Ok(verdict(self.registry, &id, &name, &expected, &actual))
    }

    /// The engine's side of one dump: the linked compiler over the
    /// program's own bytes, under the path php was given for them, or the
    /// binary compiling the file php compiled.
    fn engine_dump(&self, program: &Path, body: &[u8]) -> Result<Dumped> {
        match &self.compiler {
            Compiler::Linked(engine) => {
                linked::dump(engine, &self.oracle.pin().recipe.normalise, body, program)
            }
            Compiler::Binary(binary) => self.dump(binary, program),
        }
    }

    /// What one binary prints compiling the program: the pin's recipe, its
    /// `dump` regime and any regime after it, and `-l`, which compiles the
    /// file and does not run it. php prints its op arrays on the second stream, so the dump is
    /// what that stream carries; the first holds `-l`'s own sentence,
    /// which is no op array and is not compared.
    fn dump(&self, binary: &Path, program: &Path) -> Result<Dumped> {
        let out = self
            .oracle
            .run_under(&Invocation {
                regimes: &self.regimes,
                flags: &self.flags,
                ..Invocation::new(binary, Program::File(program))
            })
            .with_context(|| format!("dumping {} under {}", program.display(), binary.display()))?;
        Ok(Dumped {
            arrays: out.stderr,
            said: out.stdout,
        })
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
        Compared::Differs(difference) => Judged::new(
            id,
            name,
            Judgement::Fail,
            format!("the dump is not the oracle's:\n{difference}"),
        ),
    }
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
        fs::write(&path, body).with_context(|| format!("writing {}", path.display()))?;
        Ok(path)
    }
}
