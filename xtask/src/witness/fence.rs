//! The fence grammar of a witness (design §8), and what carries one.
//! `xtask witness` is this grammar's parser and rustdoc is not: the `=` on
//! an `expect` fence line trips `rustdoc::invalid_codeblock_attributes`, so
//! a crate that carries witnesses allows that lint at its root and its
//! doctests still run (harness-design.md §13 D.3).
//!
//! One block, as the design writes it:
//!
//! ````text
//! /// ```php
//! /// set_time_limit(1); for (;;) {} ?>never reached
//! /// ```
//! /// ```expect exit=N wall=1s
//! /// Fatal error: Maximum execution time of 1 second exceeded in witness.php on line 2
//! /// ```
//! ````
//!
//! The `php` fence is the program the oracle's recipe runs as `witness.php`,
//! with `<?php` on line 1 and the snippet on line 2, so php's own error text
//! matches. The `expect` fence's body is the pinned first stream; `exit=N`
//! is the pinned status, `signal=N` the signal pinned in its place where the
//! oracle's run ends on one (#3545), an absent `exit` asserts zero, and a
//! bare `exit` is the opt-in `xtask witness --accept` fills with how the
//! oracle's run ended. A
//! `wall=D` is disclosure and gates nothing. A `stderr` fence, when one
//! follows, is the pinned second stream; without one, an empty stream is
//! asserted.
//!
//! A `regime=<name>` names one `[regime.<name>]` of the pin, whose ini
//! stands after the recipe's for the oracle run and for the linked engine
//! alike (harness-design.md §8, ledger 55), and whose arguments, where it
//! carries any, stand after the recipe's on a binary's command line and
//! so serve only a fence marked `process` (#1780): the way to pin a behaviour the
//! recipe's own ini holds off, such as what `<?` opens while
//! `short_open_tag` is on. The name is all a fence carries; the values are
//! the pin's, written by a person, so a fence that would set an ini of its
//! own is an attribute this grammar has no meaning for and the block is
//! refused.
//!
//! A `request` fence between the two describes a request of the web, in
//! the sections a CGI-shaped `.phpt` carries (`--GET--`, `--POST--`,
//! `--POST_RAW--`, `--COOKIE--`, `--ENV--`): the snippet is served by php-cgi beside
//! the pinned php for the oracle and by the engine's `php-cgi` for the
//! engine (`sapi/cgi`, ADR-0008), each in a process of its own, and the `expect` fence
//! pins what php-cgi writes, its head and its body (#1197). A `request fpm`
//! fence is the same request handed to a php-fpm as `FastCGI` records, the
//! oracle's pinned php-fpm and the engine's `php-fpm` each over a pool of
//! one child, for the natives php-fpm's own module declares (#1291). A
//! `request phpdbg` fence hands the snippet to phpdbg as a `--PHPDBG--`
//! test is handed (`-qIb -f`), the pinned phpdbg for the oracle and the
//! engine's `phpdbg` for the engine, the fence's body the commands the
//! prompt reads, for the natives phpdbg's own module declares (#1348). A
//! `request makala` fence hands the request to the production server over
//! HTTP for the engine, and to the pinned php-fpm under the pin's
//! `[fastcgi]` mapping for the oracle, each response read as a client of
//! nginx in front of php-fpm reads it (#2347).
//!
//! A `process` says the snippet changes the process it runs in: it forks,
//! sends or takes a signal, or sets the process's ids, session or limits.
//! Such a fence is served by the `php` binary in a process of its own, a
//! single-threaded one as design revision 35 requires of pcntl, and never
//! by the engine linked into the harness, whose process runs the lane on
//! threads (harness-design.md §8, revision 61; #1062). What the oracle
//! does is not moved by it, so it is no part of the divergence key.
//!
//! A `mysql` says the snippet needs the `MySQL` server the recipe's
//! `PDO_MYSQL_TEST_DSN` names (`ci/mysql/compose.yaml`): a crate whose
//! fences carry it is refused, not judged, where the server does not
//! answer, as `phpt` refuses a `MySQL` subset (#1366).
//!
//! A `capture` says the fence's streams are the oracle's to write: `xtask
//! witness --accept --streams` writes the oracle's stdout into the
//! `expect` fence and its stderr into a `stderr` fence, after the recipe's
//! normalisation, and leaves every fence without it as its author wrote it
//! (harness-design.md §8, #801). The marker is a word and not an empty
//! `expect` fence, since an empty fence is what a snippet that prints
//! nothing pins; and it stays on the fence once written, so the text under
//! it is known to be the oracle's and the next capture, after an oracle
//! bump, writes it again. It is no part of the divergence key.
//!
//! A `locale=<name>`, which may stand more than once, says the snippet
//! needs the host to know that locale, as a `.phpt` whose `--SKIPIF--`
//! dies on `!setlocale(LC_CTYPE, "de_DE")` does: where neither the oracle
//! nor the linked engine can `setlocale(LC_ALL, <name>)` for one of them,
//! the witness is a skip that names the locale and is counted, never a
//! pass or a fail (#1678). Where either can, it runs and is held as any
//! other. The name is letters, digits and `_ . - @`, so it stands in the
//! probe's string as written. What the snippet runs is not moved by it,
//! so it is no part of the divergence key.
//!
//! The scan is textual, as `vocab`'s is, not a parse. A doc line is one
//! beginning `///` or `//!` with one leading space dropped from its body; a
//! block ends at the first item line, which is a line at indentation zero
//! that declares one. A `///` block's witnesses belong to that item, a
//! `//!` block's to the file.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::git;
use crate::oracle::Ended;

/// The info string of the fence that carries the program.
const PHP: &str = "php";
/// The info string of the fence that pins what the program prints.
const EXPECT: &str = "expect";
/// The info string of the optional fence that pins the second stream.
const STDERR: &str = "stderr";
/// The info string of the optional fence between the two that describes
/// the request a CGI host serves the program as.
const REQUEST: &str = "request";
/// The word after `request` that hands the request to a php-fpm.
const FPM: &str = "fpm";
/// The word after `request` that hands the program to phpdbg, the fence's
/// body the commands its prompt reads.
const PHPDBG: &str = "phpdbg";
/// The word after `request` that hands the request to the production
/// server over HTTP, and to php-fpm under the pin's mapping for the
/// oracle (#2347).
const MAKALA: &str = "makala";

/// What the `exit` attribute of an `expect` fence says.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Exit {
    /// No `exit` attribute at all: zero is asserted.
    #[default]
    Absent,
    /// `exit=N`: the number a run of `--accept` captured.
    Pinned(i32),
    /// `signal=N`: the signal a run of `--accept` captured, where the run
    /// ended on one and left no status, as php's `abort()` does (#3545).
    Signal(i32),
    /// A bare `exit`: the opt-in that fails the run until `--accept` writes
    /// how the oracle's run ended in its place.
    Bare,
}

impl Exit {
    /// How the fence asserts a run ends; nothing for the bare opt-in,
    /// which asserts nothing until the oracle's is captured.
    pub fn asserted(self) -> Option<Ended> {
        match self {
            Exit::Absent => Some(Ended::Status(0)),
            Exit::Pinned(status) => Some(Ended::Status(status)),
            Exit::Signal(signal) => Some(Ended::Signal(signal)),
            Exit::Bare => None,
        }
    }

    /// What the fence pins once `--accept` captured `ended`.
    pub fn of(ended: Ended) -> Exit {
        match ended {
            Ended::Status(status) => Exit::Pinned(status),
            Ended::Signal(signal) => Exit::Signal(signal),
        }
    }
}

/// What serves a witness's snippet beside the oracle.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Served {
    /// The engine linked into the harness, in this process (harness §7,
    /// revision 48).
    #[default]
    Linked,
    /// The `php` binary, in a process of its own: the fence carries
    /// `process` (harness §8, revision 61).
    Binary,
    /// The engine's `php-cgi`, beside php-cgi for the
    /// oracle: a `request` fence describes what the CGI host is handed
    /// (#1197).
    Cgi,
    /// The engine's `php-fpm`, beside php-fpm for the
    /// oracle: the fence is `request fpm` (#1291).
    Fpm,
    /// The engine's `phpdbg`, beside phpdbg for the
    /// oracle: the fence is `request phpdbg`, its body the commands the
    /// prompt reads (#1348).
    Phpdbg,
    /// The production server's `makala` over HTTP, beside php-fpm under
    /// the pin's `[fastcgi]` mapping for the oracle: the fence is
    /// `request makala` (#2347).
    Makala,
}

/// One witness: the program, what it prints, and where it stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Witness {
    /// Relative to the workspace root.
    pub file: PathBuf,
    /// The item the block documents, as the line writes it (`pub fn
    /// extension`); empty for a witness in a file's own doc.
    pub subject: String,
    /// The name of the declaration the `///` block stands on, at any
    /// indentation, so a method's fences are its own (`add`); what
    /// `xtask witness --item` names. Nothing for a file's own doc.
    pub item: Option<String>,
    /// 1-based, the `php` fence's line: where a person looks.
    pub line: usize,
    /// 1-based, the `expect` fence's line: the first `--accept` rewrites.
    pub fence: usize,
    /// The block's lines from the `expect` fence's through the line that
    /// closes the last fence, as they stand: what `--accept` rewrites, the
    /// first alone for a status and all of them for the streams.
    pub fences: Vec<String>,
    /// The program, without the `<?php` the recipe's preamble puts on line 1.
    pub snippet: String,
    /// The pinned first stream.
    pub stdout: String,
    /// The pinned second stream; `None` asserts an empty one.
    pub stderr: Option<String>,
    pub exit: Exit,
    /// The `[regime.<name>]` of the pin the fence names, whose ini and
    /// arguments stand after the recipe's for both runs; nothing where it names none, and
    /// then the recipe alone is what the snippet runs under.
    pub regime: Option<String>,
    /// The declared duration, disclosure only; the instrument sums them.
    pub wall: Option<Duration>,
    /// What serves the snippet beside the oracle.
    pub served: Served,
    /// `capture`: the streams are the oracle's, written by `--accept
    /// --streams` (#801).
    pub capture: bool,
    /// `mysql`: the snippet needs the `MySQL` server.
    pub server: bool,
    /// The locales the snippet needs, each a `locale=<name>` in the order
    /// written: the witness is a skip where the host knows one of them to
    /// neither run (#1678).
    pub locales: Vec<String>,
    /// The `request` fence's body: the `.phpt` sections of a CGI-shaped
    /// test (`--GET--`, `--POST--`, `--POST_RAW--`, `--COOKIE--`) the CGI
    /// host is handed, read by the phpt lane's one translation of them.
    pub request: Option<String>,
    /// The bundle feature the `php` serving a `process` fence is built
    /// with: its crate's, where the binary under test does not turn it on
    /// (#2721). The crate's, never the grammar's: [`super::extract`] sets
    /// it.
    pub feature: Option<String>,
}

impl Witness {
    /// The divergence key (harness-design.md §4.3): `witness:` and a content
    /// hash of what runs, so a registry row names the program and not the
    /// file it stands in. The program is the snippet where the fence names
    /// no regime, and the regime's name ahead of it where it names one,
    /// since the same snippet under two regimes is two runs and a person's
    /// row on one of them is not a row on the other.
    pub fn id(&self) -> String {
        let mut program = match &self.regime {
            None => self.snippet.clone(),
            Some(regime) => format!("{regime}\n{}", self.snippet),
        };
        if let Some(request) = &self.request {
            program = format!("{request}\n{program}");
        }
        match self.served {
            Served::Fpm => program = format!("{FPM}\n{program}"),
            Served::Phpdbg => program = format!("{PHPDBG}\n{program}"),
            Served::Makala => program = format!("{MAKALA}\n{program}"),
            _ => {}
        }
        format!("witness:{}", git::blob_id(program.as_bytes()))
    }

    /// Where it stands, as a person reads a printed line.
    pub fn at(&self) -> String {
        format!("{}:{}", self.file.display(), self.line)
    }
}

/// One kernel or native the coverage rule counts: a function item at
/// indentation zero, visible past its own module (`pub` or `pub(crate)`).
/// A private function beside one is not counted; an
/// associated function is not a kernel either, which is why the scan reads
/// indentation zero alone. The definition is textual and sharpens when
/// `crates/php_engine/src/ops` exists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Subject {
    pub file: PathBuf,
    pub line: usize,
    /// The item's head as the line writes it, `pub fn preg_match`.
    pub name: String,
    /// Whether a witness in its own doc block covers it.
    pub witnessed: bool,
}

impl Subject {
    pub fn at(&self) -> String {
        format!("{}:{} {}", self.file.display(), self.line, self.name)
    }
}

/// A block this grammar cannot read: a `php` fence without its `expect`
/// fence, an unclosed one, or an attribute the grammar has no meaning for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    pub file: PathBuf,
    pub line: usize,
    pub what: String,
}

impl Problem {
    fn at(file: &Path, line: usize, what: impl Into<String>) -> Problem {
        Problem {
            file: file.to_path_buf(),
            line: line + 1,
            what: what.into(),
        }
    }

    pub fn at_line(&self) -> String {
        format!("{}:{}: {}", self.file.display(), self.line, self.what)
    }
}

/// What one file holds.
#[derive(Debug, Clone, Default)]
pub struct Extracted {
    pub witnesses: Vec<Witness>,
    pub subjects: Vec<Subject>,
    pub problems: Vec<Problem>,
    /// The natives the attributes declare, where the crate's covered
    /// sources carry one: coverage is then what the fences' runs entered
    /// (harness §4.5 condition 2, revision 58), and `subjects` holds the
    /// crate's kernels alone (#1264).
    pub(crate) anchored: Option<super::anchor::Coverage>,
}

impl Extracted {
    /// Take another file's into this one.
    pub fn take(&mut self, other: Extracted) {
        self.witnesses.extend(other.witnesses);
        self.subjects.extend(other.subjects);
        self.problems.extend(other.problems);
    }
}

/// Every witness, subject and problem in one source file.
pub fn extract(file: &Path, text: &str) -> Extracted {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Extracted::default();
    // The witnesses read since the last item line, which that item's line
    // will name; a `//!` block's belong to the file and wait for nothing.
    let mut waiting: Vec<usize> = Vec::new();
    // The same witnesses, waiting for the next declaration at any
    // indentation, which names their item.
    let mut unnamed: Vec<usize> = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        if let Some((on_an_item, body)) = doc_of(lines[index]) {
            if fence(body) == Some(PHP) {
                match block_at(file, &lines, index) {
                    Ok((witness, next)) => {
                        if on_an_item {
                            waiting.push(out.witnesses.len());
                            unnamed.push(out.witnesses.len());
                        }
                        out.witnesses.push(witness);
                        index = next;
                        continue;
                    }
                    Err(problem) => out.problems.push(problem),
                }
            }
            index += 1;
            continue;
        }
        // An attribute, a comment or a blank line stands between a doc
        // block and its declaration; any other line ends the wait.
        let line = lines[index].trim_start();
        if !(line.is_empty() || line.starts_with("#[") || line.starts_with("//")) {
            let name = item_of(line).map(|item| item.name);
            for at in unnamed.drain(..) {
                out.witnesses[at].item.clone_from(&name);
            }
        }
        if let Some(item) = item_of(lines[index]) {
            let witnessed = !waiting.is_empty();
            for at in waiting.drain(..) {
                out.witnesses[at].subject.clone_from(&item.head);
            }
            if item.counted {
                out.subjects.push(Subject {
                    file: file.to_path_buf(),
                    line: index + 1,
                    name: item.head,
                    witnessed,
                });
            }
        }
        index += 1;
    }
    out
}

/// The block whose `php` fence opens at `start`, and the index after its
/// last line.
fn block_at(file: &Path, lines: &[&str], start: usize) -> Result<(Witness, usize), Problem> {
    let (snippet, mut after) = body_of(lines, start)
        .ok_or_else(|| Problem::at(file, start, "a php fence no line of the block closes"))?;
    // The request a CGI host serves the snippet as, where a `request`
    // fence stands between the two.
    let mut request = None;
    let mut host = Served::Cgi;
    if let Some((at, info, body, next)) = follows(file, lines, after, REQUEST)? {
        match info.split_whitespace().nth(1) {
            None => {}
            Some(FPM) => host = Served::Fpm,
            Some(PHPDBG) => host = Served::Phpdbg,
            Some(MAKALA) => host = Served::Makala,
            Some(_) => {
                return Err(Problem::at(
                    file,
                    at,
                    "a request fence names `fpm`, `phpdbg` or `makala` after `request`, or nothing",
                ));
            }
        }
        request = Some(body);
        after = next;
    }
    let opened = next_fence(lines, after).ok_or_else(|| {
        Problem::at(
            file,
            start,
            "a php fence without its expect fence; a witness is the pair (design §8)",
        )
    })?;
    let info = info_at(lines, opened).unwrap_or_default();
    if first_word(info) != EXPECT {
        return Err(Problem::at(
            file,
            opened,
            format!(
                "a php fence followed by a `{info}` fence; a witness's second fence is `expect`"
            ),
        ));
    }
    let attributes = Attributes::of(info);
    if let Some(unknown) = attributes.unknown.first() {
        return Err(Problem::at(
            file,
            opened,
            format!(
                "`{unknown}` on an expect fence; the attributes are `exit`, `exit=<status>`, `signal=<n>`, `wall=<duration>`, `regime=<name>`, `locale=<name>`, `process`, `mysql` and `capture`, and the ini a regime carries is the pin's"
            ),
        ));
    }
    let (stdout, after) = body_of(lines, opened)
        .ok_or_else(|| Problem::at(file, opened, "an expect fence no line of the block closes"))?;
    let (stderr, after) = match follows(file, lines, after, STDERR)? {
        Some((.., body, after)) => (Some(body), after),
        None => (None, after),
    };
    Ok((
        Witness {
            file: file.to_path_buf(),
            subject: String::new(),
            item: None,
            line: start + 1,
            fence: opened + 1,
            fences: lines[opened..after].iter().map(|&l| l.to_owned()).collect(),
            snippet,
            stdout,
            stderr,
            exit: attributes.exit,
            wall: attributes.wall,
            regime: attributes.regime,
            served: if request.is_some() {
                host
            } else {
                attributes.served
            },
            capture: attributes.capture,
            server: attributes.server,
            locales: attributes.locales,
            request,
            feature: None,
        },
        after,
    ))
}

/// The fence that opens next after `after` where its first word is `word`:
/// where it opens, its info, its body and the index after it; none where
/// the next fence is another or there is none.
fn follows<'a>(
    file: &Path,
    lines: &[&'a str],
    after: usize,
    word: &str,
) -> Result<Option<(usize, &'a str, String, usize)>, Problem> {
    let Some(at) = next_fence(lines, after) else {
        return Ok(None);
    };
    let info = info_at(lines, at).unwrap_or_default();
    if first_word(info) != word {
        return Ok(None);
    }
    let closes = || {
        Problem::at(
            file,
            at,
            format!("a {word} fence no line of the block closes"),
        )
    };
    let (body, next) = body_of(lines, at).ok_or_else(closes)?;
    Ok(Some((at, info, body, next)))
}

/// The body of the fence opened at `start`, and the index after the line
/// that closes it; nothing when no doc line of the block does.
fn body_of(lines: &[&str], start: usize) -> Option<(String, usize)> {
    let mut body: Vec<&str> = Vec::new();
    for (index, line) in lines.iter().enumerate().skip(start + 1) {
        let (_, text) = doc_of(line)?;
        if fence(text).is_some() {
            return Some((body.join("\n"), index + 1));
        }
        body.push(text.trim_end());
    }
    None
}

/// The next doc line at or after `from` that opens a fence, empty doc lines
/// passed over; nothing when prose or a line that is no doc comes first.
fn next_fence(lines: &[&str], from: usize) -> Option<usize> {
    for (index, line) in lines.iter().enumerate().skip(from) {
        let (_, text) = doc_of(line)?;
        if fence(text).is_some() {
            return Some(index);
        }
        if !text.trim().is_empty() {
            return None;
        }
    }
    None
}

/// The info string of the fence a line opens.
fn info_at<'a>(lines: &[&'a str], index: usize) -> Option<&'a str> {
    fence(doc_of(lines.get(index)?)?.1)
}

/// The rustdoc line's body, and whether it documents the item below it
/// (`///`) rather than the file it stands in (`//!`); one leading space is
/// the marker's and is dropped.
fn doc_of(line: &str) -> Option<(bool, &str)> {
    let line = line.trim_start();
    for (marker, on_an_item) in [("///", true), ("//!", false)] {
        if let Some(rest) = line.strip_prefix(marker) {
            return Some((on_an_item, rest.strip_prefix(' ').unwrap_or(rest)));
        }
    }
    None
}

/// The info string of a fence line, empty for the line that closes one;
/// nothing when the line opens no fence.
fn fence(body: &str) -> Option<&str> {
    body.trim_start().strip_prefix("```").map(str::trim)
}

/// The first word of an info string.
fn first_word(info: &str) -> &str {
    info.split_whitespace().next().unwrap_or_default()
}

/// The `expect` fence's attributes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Attributes {
    exit: Exit,
    wall: Option<Duration>,
    /// The name of the pin's regime the run stands under.
    regime: Option<String>,
    /// `process`: served by the binary.
    served: Served,
    /// `capture`: the streams are the oracle's to write.
    capture: bool,
    /// `mysql`: the snippet needs the `MySQL` server.
    server: bool,
    /// Each `locale=<name>`, in the order written.
    locales: Vec<String>,
    /// Attributes this grammar has no meaning for, in the order written.
    unknown: Vec<String>,
}

impl Attributes {
    /// The attributes an `expect` fence's info string carries, its own name
    /// aside.
    fn of(info: &str) -> Attributes {
        let mut out = Attributes::default();
        for word in info.split_whitespace().skip(1) {
            if word == "exit" {
                out.exit = Exit::Bare;
            } else if word == "process" {
                out.served = Served::Binary;
            } else if word == "mysql" {
                out.server = true;
            } else if word == "capture" {
                out.capture = true;
            } else if let Some(status) = word.strip_prefix("exit=") {
                match status.parse() {
                    Ok(status) => out.exit = Exit::Pinned(status),
                    Err(_) => out.unknown.push(word.to_owned()),
                }
            } else if let Some(signal) = word.strip_prefix("signal=") {
                match signal.parse() {
                    Ok(signal) => out.exit = Exit::Signal(signal),
                    Err(_) => out.unknown.push(word.to_owned()),
                }
            } else if let Some(wall) = word.strip_prefix("wall=") {
                match duration(wall) {
                    Some(wall) => out.wall = Some(wall),
                    None => out.unknown.push(word.to_owned()),
                }
            } else if let Some(regime) = word.strip_prefix("regime=") {
                // The name alone, and a name at all; whether the pin
                // carries it is the instrument's to say, and it names the
                // regimes the pin has.
                if regime.is_empty() {
                    out.unknown.push(word.to_owned());
                } else {
                    out.regime = Some(regime.to_owned());
                }
            } else if let Some(locale) = word.strip_prefix("locale=") {
                // A name the probe can carry as written, and a name at
                // all; whether the host knows it is the instrument's to
                // ask, at the run.
                if locale_name(locale) {
                    out.locales.push(locale.to_owned());
                } else {
                    out.unknown.push(word.to_owned());
                }
            } else {
                out.unknown.push(word.to_owned());
            }
        }
        out
    }
}

/// Whether a `locale=` names a locale as a C library spells one:
/// letters, digits and `_ . - @`, and at least one of them.
fn locale_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-' | '@'))
}

/// A declared duration: a number and `ms` or `s`. Nothing longer, since
/// the recipe's own limit ends a run first.
fn duration(text: &str) -> Option<Duration> {
    let at = text.find(char::is_alphabetic)?;
    let (number, unit) = text.split_at(at);
    let value: f64 = number.parse().ok()?;
    let seconds = match unit {
        "ms" => value / 1000.0,
        "s" => value,
        _ => return None,
    };
    Duration::try_from_secs_f64(seconds).ok()
}

/// The item a line declares, when it declares one at indentation zero.
struct Item {
    /// The head as the line writes it, `pub fn extension`.
    head: String,
    /// The name it declares, `extension`.
    name: String,
    /// Whether the coverage rule counts it: a function visible past its
    /// own module.
    counted: bool,
}

/// The item keywords a line may open with, `fn` aside.
const KEYWORDS: [&str; 10] = [
    "struct",
    "enum",
    "union",
    "trait",
    "type",
    "mod",
    "const",
    "static",
    "impl",
    "macro_rules!",
];

/// The item a line at indentation zero declares.
fn item_of(line: &str) -> Option<Item> {
    if line.starts_with(char::is_whitespace) {
        return None;
    }
    let (visible, rest) = if let Some(rest) = line.strip_prefix("pub(crate)") {
        (true, rest.trim_start())
    } else if let Some(rest) = line.strip_prefix("pub(") {
        (
            false,
            rest.split_once(')').map_or(rest, |(_, r)| r).trim_start(),
        )
    } else if let Some(rest) = line.strip_prefix("pub ") {
        (true, rest.trim_start())
    } else {
        (false, line)
    };
    let mut words = rest.split_whitespace().peekable();
    let mut keyword = None;
    while let Some(word) = words.next() {
        match word {
            "unsafe" | "async" | "extern" => {}
            w if w.starts_with('"') => {} // the ABI of `extern "C"`
            "const" if words.peek() == Some(&"fn") => {}
            "fn" => {
                keyword = Some("fn");
                break;
            }
            w if KEYWORDS.contains(&w) => {
                keyword = Some("item");
                break;
            }
            _ => return None,
        }
    }
    let words = words.collect::<Vec<_>>().join(" ");
    let name = identifier(&words).unwrap_or_default().to_owned();
    let function = keyword? == "fn";
    let head = if function {
        format!("{}fn {name}", if visible { "pub " } else { "" })
    } else {
        line.trim_end().trim_end_matches(['{', ' ']).to_owned()
    };
    Some(Item {
        counted: function && visible,
        head,
        name,
    })
}

/// The identifier at the start of `text`, if there is one; a raw one,
/// `r#mod`, as it is written, since php's name for the kernel is a
/// keyword here.
pub(super) fn identifier(text: &str) -> Option<&str> {
    let text = text.trim_start();
    let raw = text.strip_prefix("r#").map_or(0, |_| 2);
    let end = text[raw..]
        .find(|c: char| !(c.is_alphanumeric() || c == '_'))
        .map_or(text.len(), |at| raw + at);
    (end > raw).then(|| &text[..end])
}

/// The `expect` fence line with how a run `ended` pinned on it, `exit=N`
/// or `signal=N`: the one thing `--accept` writes, and the reason neither
/// is ever authored (design §8). The line's own indentation, marker and
/// other attributes stand.
pub fn pinned(line: &str, ended: Ended) -> String {
    let Some(at) = line.find("```") else {
        return line.to_owned();
    };
    let (head, rest) = line.split_at(at);
    let info = rest.trim_start_matches('`');
    let mut words: Vec<String> = info.split_whitespace().map(str::to_owned).collect();
    let pin = match ended {
        Ended::Status(status) => format!("exit={status}"),
        Ended::Signal(signal) => format!("signal={signal}"),
    };
    match words
        .iter()
        .position(|w| w == "exit" || w.starts_with("exit=") || w.starts_with("signal="))
    {
        Some(at) => words[at] = pin,
        None => words.insert(1.min(words.len()), pin),
    }
    format!("{head}```{}", words.join(" "))
}

/// What a verdict `--accept` wrote is known by: the block from the `php`
/// fence that opens it through the close of the `expect` fence at `line`
/// (0-based), and of the `stderr` fence after it where one follows, each
/// line's body as [`doc_of`] reads it, as a git blob id. The block is the
/// verdict and the program it is of, and nothing else of the file, so prose
/// edited around it, an indentation changed or a merge that moves the file
/// on keep it; a status, a program or a stream typed differently is a block
/// no `--accept` wrote. Nothing for a line that opens no `expect` fence
/// under a `php` one.
pub(crate) fn verdict(text: &str, line: usize) -> Option<String> {
    let lines: Vec<&str> = text.lines().collect();
    if first_word(info_at(&lines, line)?) != EXPECT {
        return None;
    }
    let opens = (0..line)
        .rev()
        .take_while(|at| doc_of(lines[*at]).is_some())
        .find(|at| info_at(&lines, *at).map(first_word) == Some(PHP))?;
    let (_, mut end) = body_of(&lines, line)?;
    if let Some(next) = next_fence(&lines, end)
        && info_at(&lines, next).map(first_word) == Some(STDERR)
    {
        end = body_of(&lines, next)?.1;
    }
    let body: Vec<&str> = lines[opens..end]
        .iter()
        .map(|l| doc_of(l).map_or("", |(_, text)| text.trim_end()))
        .collect();
    Some(crate::git::blob_id(body.join("\n").as_bytes()))
}

/// The block's fences as `--accept --streams` writes them (#801): `head`,
/// the `expect` fence's line as the status leaves it, then `stdout`, and a
/// `stderr` fence over `stderr` where it is not empty; each line under the
/// head's indentation and marker, an empty one under the marker alone. A
/// text the grammar would not read back as written, a line that closes a
/// fence or ends in a space the reading drops, is refused with the first
/// line that does not, since the capture writes nothing it cannot hold.
pub fn streams(head: &str, stdout: &str, stderr: &str) -> Result<Vec<String>, String> {
    let prefix = head.find("```").map_or(head, |at| &head[..at]);
    let line = |text: &str| match text {
        "" => prefix.trim_end().to_owned(),
        text => format!("{prefix}{text}"),
    };
    let close = format!("{prefix}```");
    let mut out = vec![head.to_owned()];
    out.extend(stdout.lines().map(line));
    out.push(close.clone());
    if !stderr.is_empty() {
        out.push(format!("{prefix}```{STDERR}"));
        out.extend(stderr.lines().map(line));
        out.push(close);
    }
    let block = format!(
        "{prefix}```{PHP}\n{prefix}x\n{prefix}```\n{}\n",
        out.join("\n")
    );
    let found = extract(Path::new(""), &block);
    let Some(read) = found.witnesses.first() else {
        return Err(format!("`{head}` opens no fence the grammar reads"));
    };
    let reread = read.stderr.as_deref().unwrap_or_default();
    for (stream, wrote, read) in [
        ("stdout", stdout, read.stdout.as_str()),
        ("stderr", stderr, reread),
    ] {
        if wrote == read {
            continue;
        }
        let (mut wrote, mut read) = (wrote.split('\n'), read.split('\n'));
        let mut at = 1;
        let (one, two) = loop {
            match (wrote.next(), read.next()) {
                (one, two) if one == two && one.is_some() => at += 1,
                (one, two) => break (one.unwrap_or_default(), two.unwrap_or_default()),
            }
        };
        return Err(format!(
            "{stream} line {at}, `{one}`, reads back from the fence as `{two}`"
        ));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn php(file: &str) -> Extracted {
        extract(Path::new("ext/fixture/src/lib.rs"), file)
    }

    #[test]
    fn a_pair_of_fences_is_one_witness_on_the_item_below_it() {
        let found = php(concat!(
            "/// The native.\n",
            "/// ```php\n",
            "/// echo 1;\n",
            "/// ```\n",
            "/// ```expect\n",
            "/// 1\n",
            "/// ```\n",
            "pub fn ctype_digit() {}\n"
        ));
        assert!(found.problems.is_empty(), "{:#?}", found.problems);
        let [witness] = &found.witnesses[..] else {
            panic!("{found:#?}")
        };
        assert_eq!(witness.snippet, "echo 1;");
        assert_eq!(witness.stdout, "1");
        assert_eq!(witness.stderr, None);
        assert_eq!(witness.exit, Exit::Absent);
        assert_eq!(witness.line, 2);
        assert_eq!(witness.subject, "pub fn ctype_digit");
        let [subject] = &found.subjects[..] else {
            panic!("{found:#?}")
        };
        assert!(subject.witnessed);
        assert_eq!(subject.name, "pub fn ctype_digit");
    }

    #[test]
    fn a_block_s_item_is_the_declaration_below_it_at_any_indentation() {
        let found = php(concat!(
            "//! ```php\n//! x\n//! ```\n//! ```expect\n//! ```\n",
            "impl List {\n",
            "    /// ```php\n    /// x\n    /// ```\n    /// ```expect\n    /// ```\n",
            "    #[php(tentative)]\n",
            "    fn add() {}\n",
            "}\n",
            "/// ```php\n/// y\n/// ```\n/// ```expect\n/// ```\n",
            "// a comment\n",
            "pub(crate) fn r#mod() {}\n",
        ));
        let items: Vec<_> = found.witnesses.iter().map(|w| w.item.as_deref()).collect();
        assert_eq!(items, [None, Some("add"), Some("r#mod")]);
    }

    #[test]
    fn a_raw_identifier_names_the_kernel_as_it_is_written() {
        let found = php(concat!(
            "/// ```php\n/// echo 1 % 0;\n/// ```\n",
            "/// ```expect\n/// x\n/// ```\n",
            "pub(crate) fn r#mod() {}\n"
        ));
        assert_eq!(found.witnesses[0].subject, "pub fn r#mod");
        assert_eq!(found.subjects[0].name, "pub fn r#mod");
    }

    #[test]
    fn the_attributes_are_the_status_and_the_declared_duration() {
        let fence = "/// ```expect exit=255 wall=1s\n";
        let found = php(&format!(
            "/// ```php\n/// x\n/// ```\n{fence}/// boom\n/// ```\npub fn kernel() {{}}\n"
        ));
        let [witness] = &found.witnesses[..] else {
            panic!("{found:#?}")
        };
        assert_eq!(witness.exit, Exit::Pinned(255));
        assert_eq!(witness.wall, Some(Duration::from_secs(1)));
        assert_eq!(witness.exit.asserted(), Some(Ended::Status(255)));
    }

    /// `signal=N` pins a run that ended on a signal where a status would
    /// stand, and a word after it that is no number is refused (#3545).
    #[test]
    fn a_signal_is_pinned_where_a_status_would_stand() {
        let found =
            php("/// ```php\n/// x\n/// ```\n/// ```expect signal=6\n/// ```\npub fn k() {}\n");
        assert_eq!(found.witnesses[0].exit, Exit::Signal(6));
        assert_eq!(found.witnesses[0].exit.asserted(), Some(Ended::Signal(6)));
        assert_eq!(Exit::of(Ended::Signal(6)), Exit::Signal(6));
        let refused =
            php("/// ```php\n/// x\n/// ```\n/// ```expect signal=abrt\n/// ```\npub fn k() {}\n");
        assert!(refused.witnesses.is_empty(), "{refused:#?}");
    }

    #[test]
    fn a_bare_exit_asserts_no_status_and_an_absent_one_asserts_zero() {
        let found = php("/// ```php\n/// x\n/// ```\n/// ```expect exit\n/// ```\npub fn k() {}\n");
        assert_eq!(found.witnesses[0].exit, Exit::Bare);
        assert_eq!(found.witnesses[0].exit.asserted(), None);
        assert_eq!(Exit::Absent.asserted(), Some(Ended::Status(0)));
    }

    #[test]
    fn a_stderr_fence_is_the_pinned_second_stream() {
        let found = php(concat!(
            "/// ```php\n/// x\n/// ```\n",
            "/// ```expect\n/// out\n/// ```\n",
            "/// ```stderr\n/// err\n/// ```\n",
            "pub fn k() {}\n"
        ));
        assert!(found.problems.is_empty(), "{:#?}", found.problems);
        assert_eq!(found.witnesses[0].stderr.as_deref(), Some("err"));
    }

    #[test]
    fn a_php_fence_without_its_expect_fence_is_a_problem() {
        let found = php("/// ```php\n/// x\n/// ```\n/// prose\npub fn k() {}\n");
        assert!(found.witnesses.is_empty(), "{found:#?}");
        assert_eq!(found.problems.len(), 1, "{found:#?}");
        assert!(
            found.problems[0].what.contains("without its expect fence"),
            "{:?}",
            found.problems[0]
        );
        // The item is still counted, and nothing witnesses it.
        assert_eq!(found.subjects.len(), 1);
        assert!(!found.subjects[0].witnessed);
    }

    #[test]
    fn a_regime_attribute_names_one_block_of_the_pin_and_carries_no_ini() {
        let found = php(concat!(
            "/// ```php\n/// x\n/// ```\n",
            "/// ```expect regime=short-tags exit=0\n/// 1\n/// ```\n",
            "pub fn k() {}\n"
        ));
        assert!(found.problems.is_empty(), "{:#?}", found.problems);
        let [witness] = &found.witnesses[..] else {
            panic!("{found:#?}")
        };
        assert_eq!(witness.regime.as_deref(), Some("short-tags"));
        assert_eq!(witness.exit, Exit::Pinned(0));
    }

    #[test]
    fn a_locale_attribute_repeats_and_is_no_part_of_the_key() {
        let marked = php(concat!(
            "/// ```php\n/// echo 1;\n/// ```\n",
            "/// ```expect locale=de_DE locale=fr_FR.ISO8859-1 locale=sr_RS@latin\n/// 1\n/// ```\n",
            "pub fn k() {}\n"
        ));
        assert!(marked.problems.is_empty(), "{:#?}", marked.problems);
        let plain = php("/// ```php\n/// echo 1;\n/// ```\n/// ```expect\n/// 1\n/// ```\n");
        assert_eq!(
            marked.witnesses[0].locales,
            ["de_DE", "fr_FR.ISO8859-1", "sr_RS@latin"]
        );
        assert!(plain.witnesses[0].locales.is_empty());
        assert_eq!(marked.witnesses[0].id(), plain.witnesses[0].id());
    }

    #[test]
    fn a_locale_attribute_without_a_name_the_probe_can_carry_is_a_problem() {
        for word in ["locale=", "locale=de_DE'", "locale=de DE"] {
            let found = php(&format!(
                "/// ```php\n/// x\n/// ```\n/// ```expect {word}\n/// ```\npub fn k() {{}}\n"
            ));
            if word == "locale=de DE" {
                // `DE` is a word of its own, which the grammar has no
                // meaning for.
                assert_eq!(found.problems.len(), 1, "{found:#?}");
                continue;
            }
            assert!(found.witnesses.is_empty(), "{word}: {found:#?}");
            assert!(
                found.problems[0].what.contains("`locale=<name>`"),
                "{:?}",
                found.problems[0]
            );
        }
    }

    #[test]
    fn a_process_fence_is_served_by_the_binary_under_the_same_key() {
        let marked =
            php("/// ```php\n/// echo 1;\n/// ```\n/// ```expect exit=0 process\n/// 1\n/// ```\n");
        let plain = php("/// ```php\n/// echo 1;\n/// ```\n/// ```expect exit=0\n/// 1\n/// ```\n");
        assert!(marked.problems.is_empty());
        assert_eq!(marked.witnesses[0].served, Served::Binary);
        assert_eq!(plain.witnesses[0].served, Served::Linked);
        assert_eq!(marked.witnesses[0].id(), plain.witnesses[0].id());
        assert_eq!(
            pinned("/// ```expect exit process", Ended::Status(255)),
            "/// ```expect exit=255 process"
        );
    }

    /// A `mysql` fence needs the server and is keyed as the same fence
    /// without it (#1366).
    #[test]
    fn a_mysql_fence_needs_the_server_under_the_same_key() {
        let marked = php("/// ```php\n/// echo 1;\n/// ```\n/// ```expect mysql\n/// 1\n/// ```\n");
        let plain = php("/// ```php\n/// echo 1;\n/// ```\n/// ```expect\n/// 1\n/// ```\n");
        assert!(marked.problems.is_empty(), "{:#?}", marked.problems);
        assert!(marked.witnesses[0].server);
        assert!(!plain.witnesses[0].server);
        assert_eq!(marked.witnesses[0].id(), plain.witnesses[0].id());
    }

    /// A `request` fence between the two is the request a CGI host serves
    /// the snippet as, and part of what the key names (#1197).
    #[test]
    fn a_request_fence_serves_the_snippet_as_a_request_of_the_web() {
        let request = php(
            "/// ```php\n/// echo $_GET['q'];\n/// ```\n/// ```request\n/// --GET--\n/// q=1\n/// ```\n/// ```expect\n/// 1\n/// ```\n",
        );
        let plain =
            php("/// ```php\n/// echo $_GET['q'];\n/// ```\n/// ```expect\n/// 1\n/// ```\n");
        assert!(request.problems.is_empty(), "{request:#?}");
        let witness = &request.witnesses[0];
        assert_eq!(witness.served, Served::Cgi);
        assert_eq!(witness.request.as_deref(), Some("--GET--\nq=1"));
        assert_eq!(witness.stdout, "1");
        assert_ne!(witness.id(), plain.witnesses[0].id());
    }

    /// `request fpm` hands the same request to a php-fpm, and is a run of
    /// its own under the key (#1291).
    #[test]
    fn a_request_fpm_fence_serves_the_snippet_by_a_process_manager() {
        let block = |info: &str| {
            php(&format!(
                "/// ```php\n/// echo $_GET['q'];\n/// ```\n/// ```{info}\n/// --GET--\n/// q=1\n/// ```\n/// ```expect\n/// 1\n/// ```\n"
            ))
        };
        let (fpm, cgi, other) = (
            block("request fpm"),
            block("request"),
            block("request apache"),
        );
        assert!(fpm.problems.is_empty(), "{fpm:#?}");
        assert_eq!(fpm.witnesses[0].served, Served::Fpm);
        assert_ne!(fpm.witnesses[0].id(), cgi.witnesses[0].id());
        assert_eq!(other.problems.len(), 1, "{other:#?}");
        let phpdbg = block("request phpdbg");
        assert_eq!(phpdbg.witnesses[0].served, Served::Phpdbg);
        assert_ne!(phpdbg.witnesses[0].id(), fpm.witnesses[0].id());
        let makala = block("request makala");
        assert_eq!(makala.witnesses[0].served, Served::Makala);
        assert_ne!(makala.witnesses[0].id(), fpm.witnesses[0].id());
    }

    #[test]
    fn a_fence_that_names_no_regime_stands_under_the_recipe_alone() {
        let found = php("/// ```php\n/// x\n/// ```\n/// ```expect\n/// ```\npub fn k() {}\n");
        assert_eq!(found.witnesses[0].regime, None);
    }

    /// A fence carries a regime's name and never its values: an ini on the
    /// fence would let the fence approve itself against a pin nobody wrote,
    /// so it is an attribute the grammar has no meaning for, and so is a
    /// `regime=` that names none (#610).
    #[test]
    fn a_fence_that_sets_its_own_ini_is_a_problem_naming_the_attributes() {
        for word in ["ini=short_open_tag=1", "-d", "regime="] {
            let found = php(&format!(
                "/// ```php\n/// x\n/// ```\n/// ```expect {word}\n/// ```\npub fn k() {{}}\n"
            ));
            assert!(found.witnesses.is_empty(), "{word}: {found:#?}");
            assert!(
                found.problems[0].what.contains(word)
                    && found.problems[0].what.contains("`regime=<name>`"),
                "{word}: {:?}",
                found.problems[0]
            );
        }
    }

    /// The same snippet under two regimes is two runs, so a person's row on
    /// one of them is not a row on the other; a fence naming none keys as it
    /// keyed before the attribute existed.
    #[test]
    fn the_id_is_a_content_hash_of_the_program_and_the_regime_it_runs_under() {
        let keyed = |fences: &str| {
            php(&format!(
                "/// ```php\n/// echo 1;\n/// ```\n{fences}pub fn a() {{}}\n"
            ))
            .witnesses[0]
                .id()
        };
        let plain = keyed("/// ```expect\n/// 1\n/// ```\n");
        assert_eq!(plain, format!("witness:{}", git::blob_id(b"echo 1;")));
        assert_ne!(
            plain,
            keyed("/// ```expect regime=short-tags\n/// 1\n/// ```\n")
        );
        assert_ne!(
            keyed("/// ```expect regime=short-tags\n/// 1\n/// ```\n"),
            keyed("/// ```expect regime=dump\n/// 1\n/// ```\n")
        );
    }

    #[test]
    fn an_attribute_the_grammar_has_no_meaning_for_is_a_problem() {
        let found =
            php("/// ```php\n/// x\n/// ```\n/// ```expect status\n/// ```\npub fn k() {}\n");
        assert!(found.witnesses.is_empty(), "{found:#?}");
        assert!(
            found.problems[0].what.contains("`status`"),
            "{:?}",
            found.problems[0]
        );
    }

    #[test]
    fn a_module_doc_carries_a_witness_of_the_file_and_no_item_claims_it() {
        let found = php("//! ```php\n//! x\n//! ```\n//! ```expect\n//! ```\npub fn k() {}\n");
        assert_eq!(found.witnesses.len(), 1, "{found:#?}");
        assert_eq!(found.witnesses[0].subject, "");
        assert!(!found.subjects[0].witnessed, "{found:#?}");
    }

    #[test]
    fn a_subject_is_a_function_visible_past_its_module_at_indentation_zero() {
        let counted = |line: &str| php(&format!("{line}\n")).subjects.len();
        assert_eq!(counted("pub fn preg_match() {}"), 1);
        assert_eq!(counted("pub(crate) fn add() {}"), 1);
        assert_eq!(counted(&format!("pub {} fn go() {{}}", "unsafe")), 1);
        assert_eq!(counted("pub const fn zero() -> u8 { 0 }"), 1);
        assert_eq!(counted("fn helper() {}"), 0);
        assert_eq!(counted("pub(super) fn narrow() {}"), 0);
        assert_eq!(counted("    pub fn associated() {}"), 0);
        assert_eq!(counted("pub struct Handle;"), 0);
    }

    #[test]
    fn the_id_is_a_content_hash_of_the_snippet_and_not_of_what_it_pins() {
        let one =
            php("/// ```php\n/// echo 1;\n/// ```\n/// ```expect\n/// 1\n/// ```\npub fn a() {}\n");
        let two =
            php("/// ```php\n/// echo 1;\n/// ```\n/// ```expect\n/// 2\n/// ```\npub fn b() {}\n");
        assert_eq!(one.witnesses[0].id(), two.witnesses[0].id());
        assert!(one.witnesses[0].id().starts_with("witness:"));
    }

    #[test]
    fn a_verdict_is_known_by_its_block_and_by_nothing_else_of_the_file() {
        let file = |prose: &str, indent: &str, status: &str, program: &str| {
            format!(
                "{indent}/// {prose}\n{indent}///\n{indent}/// ```php\n{indent}/// {program}\n{indent}/// ```\n{indent}/// ```expect {status}\n{indent}/// 1\n{indent}/// ```\n{indent}pub fn f() {{}}\n"
            )
        };
        let at = 5;
        let one = verdict(&file("Before.", "", "exit=3", "echo 1;"), at).unwrap();
        assert_eq!(
            verdict(&file("After.", "    ", "exit=3", "echo 1;"), at),
            Some(one.clone())
        );
        assert_ne!(
            verdict(&file("Before.", "", "exit=4", "echo 1;"), at),
            Some(one.clone())
        );
        assert_ne!(
            verdict(&file("Before.", "", "exit=3", "echo 2;"), at),
            Some(one)
        );
        assert_eq!(verdict(&file("Before.", "", "exit=3", "echo 1;"), 2), None);
        let with_stderr = "/// ```php\n/// x\n/// ```\n/// ```expect exit=1\n/// ```\n/// ```stderr\n/// a\n/// ```\n";
        let other = with_stderr.replace("/// a\n", "/// b\n");
        assert_ne!(verdict(with_stderr, 3), verdict(&other, 3));
    }

    #[test]
    fn accept_writes_the_status_onto_the_fence_line_and_leaves_the_rest() {
        assert_eq!(
            pinned("/// ```expect exit wall=1s", Ended::Status(255)),
            "/// ```expect exit=255 wall=1s"
        );
        let status = |code| Ended::Status(code);
        assert_eq!(
            pinned("    /// ```expect", status(3)),
            "    /// ```expect exit=3"
        );
        assert_eq!(
            pinned("/// ```expect exit=1", status(0)),
            "/// ```expect exit=0"
        );
        let abort = Ended::Signal(6);
        assert_eq!(
            pinned("/// ```expect exit", abort),
            "/// ```expect signal=6"
        );
        assert_eq!(
            pinned("/// ```expect exit=255", abort),
            "/// ```expect signal=6"
        );
        assert_eq!(
            pinned("/// ```expect signal=6", status(255)),
            "/// ```expect exit=255"
        );
    }

    /// `capture` opts a fence into `--accept --streams` and is no part of
    /// the key; the fences a block spans are what the capture rewrites.
    #[test]
    fn a_capture_fence_spans_its_expect_and_stderr_fences_under_the_same_key() {
        let marked = php(concat!(
            "/// ```php\n/// echo 1;\n/// ```\n",
            "/// ```expect capture\n/// 1\n/// ```\n/// ```stderr\n/// e\n/// ```\n",
            "pub fn k() {}\n"
        ));
        let plain = php("/// ```php\n/// echo 1;\n/// ```\n/// ```expect\n/// 1\n/// ```\n");
        assert!(marked.problems.is_empty(), "{:#?}", marked.problems);
        let witness = &marked.witnesses[0];
        assert!(witness.capture && !plain.witnesses[0].capture);
        assert_eq!(witness.id(), plain.witnesses[0].id());
        assert_eq!(witness.fence, 4);
        assert_eq!(
            witness.fences,
            [
                "/// ```expect capture",
                "/// 1",
                "/// ```",
                "/// ```stderr",
                "/// e",
                "/// ```"
            ]
        );
    }

    /// What `--accept --streams` writes reads back as the streams it was
    /// given, under the head's indentation and marker, an empty line under
    /// the marker alone, and a `stderr` fence only for a stream that is not
    /// empty; a line the grammar would read otherwise is refused by name.
    #[test]
    fn the_streams_are_written_as_the_grammar_reads_them_back() {
        assert_eq!(
            streams("    /// ```expect capture exit=255", "a\n\n b", "").unwrap(),
            [
                "    /// ```expect capture exit=255",
                "    /// a",
                "    ///",
                "    ///  b",
                "    /// ```"
            ]
        );
        assert_eq!(
            streams("//! ```expect capture", "", "e").unwrap(),
            [
                "//! ```expect capture",
                "//! ```",
                "//! ```stderr",
                "//! e",
                "//! ```"
            ]
        );
        let trailing = streams("/// ```expect capture", "a\nb ", "").unwrap_err();
        assert!(trailing.contains("stdout line 2, `b `"), "{trailing}");
        let closing = streams("/// ```expect capture", "", "```").unwrap_err();
        assert!(closing.contains("stderr line 1"), "{closing}");
    }

    #[test]
    fn a_declared_duration_is_a_number_and_a_unit() {
        assert_eq!(duration("1s"), Some(Duration::from_secs(1)));
        assert_eq!(duration("250ms"), Some(Duration::from_millis(250)));
        assert_eq!(duration("1.5s"), Some(Duration::from_millis(1500)));
        assert_eq!(duration("2m"), None);
        assert_eq!(duration("later"), None);
    }
}
