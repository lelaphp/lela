//! A shell line read as the argv of each program it runs, and the
//! manifest's `with`, `deny_argv` and `require_argv` read over one argv
//! (#1001). One place for the idea: every rule that names a command's
//! words is judged here, and nowhere else.
//!
//! The line is split into simple commands at `;`, `&`, `&&`, `|`, `||`,
//! newlines and the parentheses of a subshell, and each command's words
//! are what the shell hands the program: quotes removed and never split
//! on, a redirection and its target dropped, a heredoc's body skipped as
//! text, the words of a `$(…)` or a backquote, and of what a shell runs
//! with `-c` or `eval` runs, read as commands of their own. A heredoc a
//! shell reads on its stdin (`bash <<EOF`, `cat <<EOF | sh`) is no text:
//! its lines are the commands the shell runs, read as a line of their
//! own. Leading assignments, reserved words (`if`, `{`, `!`) and a few
//! programs that run the rest of their words (`env`, `command`, `nohup`)
//! are peeled, so the argv is the program's that runs; a variable the line
//! assigned, standing as the program, is read as its words. xtask's argv
//! is decoded once, however it is run (the cargo alias, `cargo run -p
//! xtask`, a path), as its name and the words it receives, so `with =
//! "xtask phpt"` names every form (#1860). What a shell can hide beyond
//! that (a variable the line does not assign, an alias, a function) is
//! not read; pre-push and CI run `done` behind it.
//!
//! The same reading gives a `deny_pattern` its text ([`Line::text`]): the
//! line with the heredoc bodies it carries as text removed, so a pattern
//! judges what a command runs and not what it quotes (#1771, #1832), as
//! an argv rule does. One lexer decides what a body is for both.

use std::collections::BTreeMap;
use std::ops::Range;

use crate::manifest::OneOrMany;

/// One simple command's words as the program that runs receives them,
/// its name first.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Argv(Vec<String>);

/// Every simple command the line runs, in the order the lexer closes them.
pub(super) fn commands(line: &str) -> Vec<Argv> {
    Lexer::read(line).commands
}

/// A shell line as permit reads it: the simple commands it runs, its text
/// less the heredoc bodies it carries as text, those bodies, and the files
/// its redirections write.
#[derive(Debug)]
pub(super) struct Line {
    commands: Vec<Argv>,
    text: String,
    bodies: Vec<Heretext>,
    written: Vec<String>,
}

/// One heredoc body its command reads as text, whole, beside the commands
/// of the line that opened it, by their place in [`Line::commands`]: the
/// program an interpreter on that line runs, or a message `cat` writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Heretext {
    pub(super) commands: Range<usize>,
    pub(super) text: String,
}

impl Line {
    pub(super) fn read(line: &str) -> Line {
        let lexer = Lexer::read(line);
        let mut text = String::with_capacity(line.len());
        let mut at = 0;
        for body in &lexer.text_bodies {
            text.extend(&lexer.chars[at..body.start]);
            at = body.end;
        }
        text.extend(&lexer.chars[at..]);
        Line {
            commands: lexer.commands,
            text,
            bodies: lexer.bodies,
            written: lexer.written,
        }
    }

    /// Every simple command the line runs ([`commands`]).
    pub(super) fn commands(&self) -> &[Argv] {
        &self.commands
    }

    /// What a `deny_pattern` reads: the line with every heredoc body line
    /// that is text to its command removed, the line ending with it. A
    /// body a shell reads on its stdin stays, being commands, and so does
    /// a line of an unquoted body that runs a `$(…)` or a backquote. A
    /// heredoc inside a body a shell runs, a backquote or a `-c` argument
    /// stays too, being read by a lexer of its own: the text is then
    /// stricter than the argv, never looser.
    pub(super) fn text(&self) -> &str {
        &self.text
    }

    /// Every heredoc body a command reads as text, a nested line's
    /// included, each with the commands of its line.
    pub(super) fn bodies(&self) -> &[Heretext] {
        &self.bodies
    }

    /// The files the line's redirections write (`>`, `>>`, `>|`, `&>`,
    /// `<>`), a nested line's included, as the line spells them.
    pub(super) fn written(&self) -> &[String] {
        &self.written
    }
}

/// A heredoc whose body starts at the next newline.
struct Heredoc {
    delimiter: String,
    strip_tabs: bool,
    /// An unquoted delimiter leaves `$(…)` in the body running.
    expands: bool,
}

/// What a heredoc's body is to the call, which the line that opened it
/// decides: whether a shell on it reads its stdin.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Body {
    /// Text its command reads, a message, a file's content or another
    /// program's source; under an unquoted delimiter its `$(…)` and
    /// backquotes still run as the shell reads it.
    Text,
    /// The commands a shell on the line reads on its stdin.
    Commands,
}

/// What the next word of a command is for.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Next {
    Argument,
    /// The file a redirection names, which is no word of the argv;
    /// `writes` for one the redirection writes (`>`, `>>`, `<>`, `&>`).
    Target {
        writes: bool,
    },
    Delimiter {
        strip_tabs: bool,
    },
}

/// A word being read: its text and whether any of it was quoted.
struct Word {
    text: String,
    quoted: bool,
}

/// The command being read.
struct Command {
    words: Vec<String>,
    word: Option<Word>,
    next: Next,
}

impl Command {
    fn new() -> Command {
        Command {
            words: Vec::new(),
            word: None,
            next: Next::Argument,
        }
    }

    fn push(&mut self, c: char) {
        self.word
            .get_or_insert_with(|| Word {
                text: String::new(),
                quoted: false,
            })
            .text
            .push(c);
    }

    /// A quote or a substitution opens a word even when it adds no text.
    fn open(&mut self, quoted: bool) {
        let word = self.word.get_or_insert_with(|| Word {
            text: String::new(),
            quoted,
        });
        word.quoted |= quoted;
    }
}

struct Lexer {
    chars: Vec<char>,
    at: usize,
    commands: Vec<Argv>,
    heredocs: Vec<Heredoc>,
    /// Where in `commands` the line being read starts, so the heredocs it
    /// opened can ask what its commands are.
    line_start: usize,
    /// The heredoc body lines skipped as text, each with its line ending,
    /// as ranges of `chars` in order.
    text_bodies: Vec<Range<usize>>,
    /// The variables the line has assigned so far ([`Assigned`]).
    assigned: Assigned,
    /// The files the line's redirections write, in order.
    written: Vec<String>,
    /// The heredoc bodies the line's commands read as text ([`Heretext`]).
    bodies: Vec<Heretext>,
}

impl Lexer {
    fn new(line: &str) -> Lexer {
        Lexer {
            chars: line.chars().collect(),
            at: 0,
            commands: Vec::new(),
            heredocs: Vec::new(),
            line_start: 0,
            text_bodies: Vec::new(),
            assigned: Assigned::new(),
            written: Vec::new(),
            bodies: Vec::new(),
        }
    }

    /// The whole line read.
    fn read(line: &str) -> Lexer {
        let mut lexer = Lexer::new(line);
        lexer.list(None);
        lexer
    }

    /// A line the line runs in turn, read by a lexer of its own: its
    /// commands, the files its redirections write and its heredoc bodies
    /// join this one's.
    fn nested(&mut self, line: &str) {
        let nested = Lexer::read(line);
        let offset = self.commands.len();
        self.commands.extend(nested.commands);
        self.written.extend(nested.written);
        self.bodies
            .extend(nested.bodies.into_iter().map(|body| Heretext {
                commands: body.commands.start + offset..body.commands.end + offset,
                text: body.text,
            }));
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.chars.get(self.at).copied();
        self.at += 1;
        c
    }

    fn eat(&mut self, c: char) -> bool {
        let here = self.chars.get(self.at) == Some(&c);
        self.at += usize::from(here);
        here
    }

    /// Commands until `close`, the `)` of a subshell or a `$(`, or the end.
    fn list(&mut self, close: Option<char>) {
        let mut command = Command::new();
        while let Some(c) = self.bump() {
            match c {
                c if Some(c) == close => break,
                '\\' => match self.bump() {
                    Some('\n') | None => {}
                    Some(c) => {
                        command.open(true);
                        command.push(c);
                    }
                },
                '\'' => {
                    command.open(true);
                    while let Some(c) = self.bump().filter(|c| *c != '\'') {
                        command.push(c);
                    }
                }
                '"' => {
                    command.open(true);
                    self.expand(&mut command, Some('"'));
                }
                '`' => {
                    command.open(false);
                    self.backquote();
                }
                '$' if self.eat('(') => {
                    command.open(false);
                    self.list(Some(')'));
                }
                '#' if command.word.is_none() => {
                    while self.chars.get(self.at).is_some_and(|c| *c != '\n') {
                        self.at += 1;
                    }
                }
                '<' | '>' => self.redirection(&mut command, c),
                '&' if self.eat('>') => {
                    self.end_word(&mut command);
                    self.eat('>');
                    command.next = Next::Target { writes: true };
                }
                '(' => {
                    self.end(&mut command);
                    self.list(Some(')'));
                }
                ';' | '&' | '|' | ')' | '\n' => {
                    self.end(&mut command);
                    if c == '\n' {
                        self.skip_heredoc_bodies();
                    }
                }
                c if c.is_whitespace() => self.end_word(&mut command),
                c => command.push(c),
            }
        }
        self.end(&mut command);
    }

    /// The text of a double-quoted word up to `close`, or of a heredoc
    /// body line to its end: `\` escapes only what it escapes there, and a
    /// substitution's commands are read.
    fn expand(&mut self, command: &mut Command, close: Option<char>) {
        while let Some(c) = self.bump() {
            match c {
                c if Some(c) == close => return,
                '\\' => match self.bump() {
                    Some('\n') | None => {}
                    Some(c @ ('"' | '\\' | '$' | '`')) => command.push(c),
                    Some(c) => {
                        command.push('\\');
                        command.push(c);
                    }
                },
                '`' => self.backquote(),
                '$' if self.eat('(') => self.list(Some(')')),
                c => command.push(c),
            }
        }
    }

    /// A backquoted substitution: its text with the backquote's escapes
    /// removed, read as a line of its own.
    fn backquote(&mut self) {
        let mut inner = String::new();
        while let Some(c) = self.bump().filter(|c| *c != '`') {
            match c {
                '\\' => match self.bump() {
                    Some(c @ ('`' | '\\' | '$')) => inner.push(c),
                    Some(c) => {
                        inner.push('\\');
                        inner.push(c);
                    }
                    None => {}
                },
                c => inner.push(c),
            }
        }
        self.nested(&inner);
    }

    /// `<`, `>`, `>>`, `>&`, `>|`, `<&`, `<>`, `<<<`, `<<` and `<<-`, with
    /// the file-descriptor number standing against one dropped as well.
    fn redirection(&mut self, command: &mut Command, c: char) {
        if command
            .word
            .as_ref()
            .is_some_and(|w| !w.quoted && w.text.bytes().all(|b| b.is_ascii_digit()))
        {
            command.word = None;
        }
        self.end_word(command);
        command.next = Next::Target { writes: c == '>' };
        if c == '<' && self.eat('<') {
            if !self.eat('<') {
                command.next = Next::Delimiter {
                    strip_tabs: self.eat('-'),
                };
            }
        } else if c == '<' && self.eat('>') {
            command.next = Next::Target { writes: true };
        } else if self.eat('&') {
            // `>&1` and `>&-` name a descriptor, not a file.
            let descriptor = self
                .chars
                .get(self.at)
                .is_some_and(|c| c.is_ascii_digit() || *c == '-');
            command.next = Next::Target {
                writes: c == '>' && !descriptor,
            };
        } else {
            let _ = self.eat('>') || self.eat('|');
        }
    }

    fn end_word(&mut self, command: &mut Command) {
        let Some(word) = command.word.take() else {
            return;
        };
        match std::mem::replace(&mut command.next, Next::Argument) {
            Next::Argument => command.words.push(word.text),
            Next::Target { writes: true } => self.written.push(word.text),
            Next::Target { writes: false } => {}
            Next::Delimiter { strip_tabs } => self.heredocs.push(Heredoc {
                delimiter: word.text,
                strip_tabs,
                expands: !word.quoted,
            }),
        }
    }

    fn end(&mut self, command: &mut Command) {
        self.end_word(command);
        command.next = Next::Argument;
        let words = std::mem::take(&mut command.words);
        self.assigned.read(&words);
        if let Some(argv) = Argv::running(words, &self.assigned) {
            let nested = argv.nested_line();
            self.commands.push(argv);
            if let Some(line) = nested {
                self.nested(&line);
            }
        }
    }

    /// The bodies of the heredocs the line just ended opened: skipped as
    /// text, but for a `$(…)` an unquoted body still runs, and read as a
    /// line of its own where a shell on the line reads it on its stdin.
    fn skip_heredoc_bodies(&mut self) {
        let body = if self.commands[self.line_start..]
            .iter()
            .any(Argv::reads_its_stdin)
        {
            Body::Commands
        } else {
            Body::Text
        };
        let line_commands = self.line_start..self.commands.len();
        for heredoc in std::mem::take(&mut self.heredocs) {
            let mut stdin = String::new();
            let mut whole = String::new();
            while self.at < self.chars.len() {
                let start = self.at;
                let end = self.chars[start..]
                    .iter()
                    .position(|c| *c == '\n')
                    .map_or(self.chars.len(), |n| start + n);
                self.at = end + 1;
                let line: String = self.chars[start..end].iter().collect();
                let text = if heredoc.strip_tabs {
                    line.trim_start_matches('\t')
                } else {
                    &line
                };
                if text == heredoc.delimiter {
                    break;
                }
                // Under an unquoted delimiter the shell runs a line's
                // substitutions as it reads the body, whoever reads it next.
                let mut substituted = Vec::new();
                if heredoc.expands {
                    let mut read = Lexer::new(&line);
                    read.expand(&mut Command::new(), None);
                    substituted = read.commands;
                }
                match body {
                    Body::Text if substituted.is_empty() => {
                        self.text_bodies.push(start..self.at.min(self.chars.len()));
                        whole.push_str(text);
                        whole.push('\n');
                    }
                    Body::Text => {
                        whole.push_str(text);
                        whole.push('\n');
                    }
                    Body::Commands => {
                        stdin.push_str(text);
                        stdin.push('\n');
                    }
                }
                self.commands.append(&mut substituted);
            }
            if body == Body::Commands {
                self.nested(&stdin);
            } else {
                self.bodies.push(Heretext {
                    commands: line_commands.clone(),
                    text: whole,
                });
            }
        }
        self.line_start = self.commands.len();
    }
}

/// Builtins whose operands are assignments to the shell's own variables.
const DECLARING: [&str; 5] = ["export", "readonly", "declare", "typeset", "local"];

/// The variables a line assigns in a command of their own, `X=v` or
/// `export X=v`, by name, each its last value: what `$X` or `${X}`
/// standing as a later command's program runs, and the directory a later
/// `cd`, `pushd` or git `-C` names by it ([`Argv::directories`]). An
/// assignment in front of a program is that program's environment and
/// not the shell's, which expands the line before it assigns, so it is
/// none of these. One made in a subshell or a substitution is read as
/// the line's: the reading is then stricter than the shell, never looser.
struct Assigned(BTreeMap<String, String>);

impl Assigned {
    fn new() -> Assigned {
        Assigned(BTreeMap::new())
    }

    /// The assignments a command's words make, where they are the whole
    /// command or a declaring builtin's operands.
    fn read(&mut self, words: &[String]) {
        let operands = match words.split_first() {
            Some((first, rest)) if DECLARING.contains(&first.as_str()) => rest,
            _ if words.iter().all(|w| is_assignment(w)) => words,
            _ => return,
        };
        for (name, value) in operands.iter().filter_map(|w| w.split_once('=')) {
            if is_name(name) {
                self.0.insert(name.to_owned(), value.to_owned());
            }
        }
    }

    /// The words `$X` or `${X}` stands for, split as an unquoted expansion
    /// is; `None` for any other word, or a variable the line never
    /// assigned.
    fn expand(&self, word: &str) -> Option<Vec<String>> {
        let name = word.strip_prefix('$')?;
        let name = name
            .strip_prefix('{')
            .and_then(|n| n.strip_suffix('}'))
            .unwrap_or(name);
        let value = self.0.get(name)?;
        Some(value.split_whitespace().map(str::to_owned).collect())
    }

    /// The directory a word names that starts with a variable the line
    /// assigned, `$X`, `${X}`, `$X/sub` or `${X}/sub`, its value whole,
    /// since a directory operand is one word whether quoted or not; `None`
    /// for any other word, or a variable the line never assigned.
    fn directory(&self, word: &str) -> Option<String> {
        let rest = word.strip_prefix('$')?;
        let (name, tail) = match rest.strip_prefix('{') {
            Some(braced) => braced.split_once('}')?,
            None => rest.split_at(
                rest.find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
                    .unwrap_or(rest.len()),
            ),
        };
        Some(format!("{}{tail}", self.0.get(name)?))
    }
}

/// Cargo's options that take the word after them as their value, before
/// its subcommand and among `cargo run`'s own.
const CARGO_VALUED: [&str; 18] = [
    "--color",
    "--explain",
    "--config",
    "-Z",
    "-C",
    "-p",
    "--package",
    "--bin",
    "--example",
    "-F",
    "--features",
    "-j",
    "--jobs",
    "--profile",
    "--target",
    "--target-dir",
    "--manifest-path",
    "--message-format",
];

/// Where the words xtask receives start, when `words` run it: after its
/// own name, by any path; after cargo's `xtask` alias, past cargo's own
/// options; or after `cargo run -p xtask`'s options and its `--`, if any
/// (`cargo r`, `--bin xtask` alike).
/// `None` where they run anything else. The alias is `run --package xtask
/// --` (.cargo/config.toml).
fn xtask_own(words: &[String]) -> Option<usize> {
    match basename(words.first()?) {
        "xtask" => Some(1),
        "cargo" => {
            let at = past_options(words, 1);
            match words.get(at)?.as_str() {
                "xtask" => Some(at + 1),
                "run" | "r" => run_of_xtask(words, at + 1),
                _ => None,
            }
        }
        _ => None,
    }
}

/// Past cargo's options standing from `at`, each with its value where it
/// takes one ([`CARGO_VALUED`]), and a `+toolchain`; never past `--`.
fn past_options(words: &[String], mut at: usize) -> usize {
    while let Some(word) = words
        .get(at)
        .filter(|w| w.starts_with(['-', '+']) && *w != "--")
    {
        at += if CARGO_VALUED.contains(&word.as_str()) {
            2
        } else {
            1
        };
    }
    at.min(words.len())
}

/// Where the binary's words start in a `cargo run` whose own words start
/// at `at`, when what it runs is xtask (`-p xtask`, or `--bin xtask`):
/// after its `--`, or at its first operand, every word from which `run`
/// hands the binary.
fn run_of_xtask(words: &[String], at: usize) -> Option<usize> {
    let end = past_options(words, at);
    let options = &words[at.min(end)..end];
    let xtask = options.iter().enumerate().any(|(i, w)| match w.as_str() {
        "-p" | "--package" | "--bin" => options.get(i + 1).is_some_and(|p| p == "xtask"),
        w => matches!(w, "--package=xtask" | "-pxtask" | "--bin=xtask"),
    });
    let args = if words.get(end).is_some_and(|w| w == "--") {
        end + 1
    } else {
        end
    };
    xtask.then_some(args)
}

/// Words a command starts with that are the shell's and not the program's.
const RESERVED: [&str; 10] = [
    "!", "{", "}", "if", "then", "elif", "else", "do", "while", "until",
];

/// Programs that run the rest of their words as a command, with the
/// options each reads a value after and the operands it reads before the
/// command: `timeout`'s duration (#2668).
const WRAPPERS: [(&str, &[&str], usize); 8] = [
    ("command", &[], 0),
    ("builtin", &[], 0),
    ("exec", &["-a"], 0),
    ("nohup", &[], 0),
    ("time", &[], 0),
    ("env", &["-u", "-C", "-S"], 0),
    ("nice", &["-n"], 0),
    ("timeout", &["-s", "--signal", "-k", "--kill-after"], 1),
];

/// Programs that read commands, from a `-c` argument, a file or stdin.
const SHELLS: [&str; 5] = ["sh", "bash", "zsh", "dash", "ksh"];

/// A shell's options that take the word after them as their value.
const SHELL_VALUED: [&str; 4] = ["-o", "+o", "-O", "+O"];

/// A program's name without its directory: `/usr/bin/git` is `git`.
fn basename(word: &str) -> &str {
    word.rsplit('/').next().unwrap_or(word)
}

fn is_assignment(word: &str) -> bool {
    word.split_once('=').is_some_and(|(name, _)| is_name(name))
}

/// A shell variable's name: a letter or `_`, then letters, digits and `_`.
fn is_name(name: &str) -> bool {
    name.chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

impl Argv {
    /// The argv of the program a command's words run, past its leading
    /// assignments, reserved words and wrappers, with a variable the line
    /// [`Assigned`] read as its words where it stands as the program, and
    /// xtask's argv, its name and the words it receives, however it is run
    /// ([`xtask_own`]); `None` when they run none.
    fn running(mut words: Vec<String>, assigned: &Assigned) -> Option<Argv> {
        let mut at = 0;
        // The shell expands a word once: what `$X` stands for is never
        // expanded again, so `X='$X'; $X` runs the program `$X`.
        let mut expanded = false;
        loop {
            let word = words.get(at)?;
            if RESERVED.contains(&word.as_str()) || is_assignment(word) {
                at += 1;
                continue;
            }
            if let Some(value) = assigned.expand(word).filter(|_| !expanded) {
                words.splice(at..=at, value);
                expanded = true;
                continue;
            }
            let Some((_, valued, operands)) = WRAPPERS.iter().find(|(w, ..)| *w == basename(word))
            else {
                break;
            };
            at += 1;
            while let Some(option) = words.get(at).filter(|w| w.starts_with('-')) {
                at += if valued.contains(&option.as_str()) {
                    2
                } else {
                    1
                };
            }
            at += operands;
        }
        words.drain(..at);
        if let Some(own) = xtask_own(&words) {
            words.splice(..own, ["xtask".to_owned()]);
        }
        let mut argv = Argv(words);
        for at in argv.directory_operands() {
            if let Some(word) = argv.0.get_mut(at)
                && let Some(dir) = assigned.directory(word)
            {
                *word = dir;
            }
        }
        Some(argv)
    }

    /// Where the words naming a directory the command moves into stand: a
    /// `cd`'s or `pushd`'s operand, its first word past its options, and
    /// each git `-C` among the options before its subcommand, as git reads
    /// them; a `-C` after the subcommand is the subcommand's own (`git
    /// commit -C HEAD`).
    fn directory_operands(&self) -> Vec<usize> {
        let words = &self.0;
        match words.first().map(String::as_str) {
            Some("cd" | "pushd") => {
                return words
                    .iter()
                    .skip(1)
                    .position(|w| !w.starts_with('-'))
                    .map(|at| at + 1)
                    .into_iter()
                    .collect();
            }
            _ if self.program() != "git" => return Vec::new(),
            _ => {}
        }
        let mut operands = Vec::new();
        let mut at = 1;
        while let Some(word) = words.get(at) {
            match word.as_str() {
                "-C" => {
                    operands.extend((at + 1 < words.len()).then_some(at + 1));
                    at += 2;
                }
                "-c" | "--git-dir" | "--work-tree" | "--namespace" | "--config-env" => at += 2,
                option if option.starts_with('-') => at += 1,
                _ => break,
            }
        }
        operands
    }

    /// The directories the command names to move into, in order
    /// ([`Argv::directory_operands`]): a `cd`'s or `pushd`'s operand, or
    /// git's `-C`s, each relative to the one before. A variable the line
    /// assigned is read as its value ([`Assigned`]); one it did not stays
    /// as written, `$X`.
    pub(super) fn directories(&self) -> impl Iterator<Item = &str> {
        self.directory_operands()
            .into_iter()
            .filter_map(|at| self.0.get(at).map(String::as_str))
    }

    /// The line this command runs in turn: a shell's `-c` argument, or the
    /// words `eval` joins.
    fn nested_line(&self) -> Option<String> {
        let (program, args) = self.0.split_first()?;
        match basename(program) {
            "eval" => Some(args.join(" ")),
            shell if SHELLS.contains(&shell) => args
                .iter()
                .position(|a| is_command_option(a))
                .and_then(|at| args.get(at + 1))
                .cloned(),
            _ => None,
        }
    }

    /// Whether this command is a shell reading the commands it runs on its
    /// stdin: one with neither a `-c` nor a file to read them from, one
    /// told so with `-s`, whose words after it are its arguments, or one
    /// whose file is stdin (`-`, `/dev/stdin`).
    fn reads_its_stdin(&self) -> bool {
        let Some((program, args)) = self.0.split_first() else {
            return false;
        };
        if !SHELLS.contains(&basename(program)) || args.iter().any(|a| is_command_option(a)) {
            return false;
        }
        let mut words = args.iter();
        while let Some(word) = words.next() {
            if word == "-s" {
                return true;
            }
            if word == "--" {
                return words.next().is_none_or(|file| reads_stdin(file));
            }
            if SHELL_VALUED.contains(&word.as_str()) {
                words.next();
            } else if word == "-" || !word.starts_with(['-', '+']) {
                return reads_stdin(word);
            }
        }
        true
    }

    /// The words, program first.
    pub(super) fn words(&self) -> &[String] {
        &self.0
    }

    /// The program's name without its directory; empty for no words.
    pub(super) fn program(&self) -> &str {
        self.0.first().map_or("", |program| basename(program))
    }

    /// Whether `phrase` names this command: a program of one word by its
    /// name, `cat`, or a program and its subcommand, `git diff`, with only
    /// options between them ([`Argv::phrase_end`]).
    pub(super) fn runs(&self, phrase: &str) -> bool {
        if phrase.contains(' ') {
            self.phrase_end(phrase).is_some()
        } else {
            self.program() == phrase
        }
    }

    /// Whether one of the phrases names this command; none names every one.
    pub(super) fn invokes(&self, with: &OneOrMany) -> bool {
        self.invocations(with).next().is_some()
    }

    /// `deny_argv` with `with`: one of the phrases names this command and
    /// one of the denied elements stands in it (see [`Element`]).
    pub(super) fn denies(&self, denied: &[String], with: &OneOrMany) -> bool {
        self.invocations(with).any(|invocation| {
            denied
                .iter()
                .filter_map(|d| Element::parse(d))
                .any(|element| invocation.carries(&element))
        })
    }

    /// `require_argv` with `with`: one of the phrases names this command
    /// and its own words hold no `separator`, or nothing after it, or
    /// after it a name for the tree rather than for files (`.`, `:/`, a
    /// `*` the shell left unexpanded). The separator is `--` for git, after
    /// which every word is a path, so `git add -A -- src` is bounded to
    /// `src` and passes; an option separator may carry its value after
    /// `=`, `--subset=ext/pcre`. A command that asks for its help (`-h`,
    /// `--help` among its options) runs nothing and is never unbounded.
    pub(super) fn unbounded(&self, separator: &str, with: &OneOrMany) -> bool {
        self.invocations(with).any(|invocation| {
            let own = invocation.own();
            let options = own.iter().take_while(|w| *w != "--");
            if options.into_iter().any(|w| w == "-h" || w == "--help") {
                return false;
            }
            let joined = |word: &String| {
                word.strip_prefix(separator)
                    .and_then(|rest| rest.strip_prefix('='))
                    .map(str::to_owned)
            };
            let Some(at) = own
                .iter()
                .position(|w| w == separator || joined(w).is_some())
            else {
                return true;
            };
            let paths: Vec<String> = joined(&own[at])
                .into_iter()
                .chain(own[at + 1..].iter().cloned())
                .collect();
            paths.is_empty() || paths.iter().any(|p| names_the_tree(p))
        })
    }

    /// Where each phrase that names this command stands in it.
    fn invocations<'a>(&'a self, with: &'a OneOrMany) -> impl Iterator<Item = Invocation<'a>> {
        let every = with.is_empty().then_some(1);
        let named = with.iter().filter_map(|phrase| self.phrase_end(phrase));
        every.into_iter().chain(named).map(|own| Invocation {
            words: &self.0,
            own,
        })
    }

    /// Where a phrase's last word stands, plus one. A phrase is the program
    /// and the subcommands it names, `git commit`; a phrase of one word is
    /// the program, or a subcommand under whatever program stands before
    /// it, `commit`. The words stand in order with only options and their
    /// values between them, so `git -C dir commit` is `git commit` and
    /// `git log --grep commit` is not, `log` being neither.
    fn phrase_end(&self, phrase: &str) -> Option<usize> {
        let words: Vec<&str> = phrase.split_whitespace().collect();
        let program = basename(self.0.first()?);
        match words.as_slice() {
            [first, rest @ ..] if *first == program => follows(&self.0, rest),
            [subcommand] => follows(&self.0, &[subcommand]),
            _ => None,
        }
    }
}

/// Where the words stand after `tokens[0]` in order, with only an option
/// (`-x`, `--x`) or the token after one between two of them: the index
/// after the last, or `None`.
fn follows(tokens: &[String], words: &[&str]) -> Option<usize> {
    let mut at = 1;
    for word in words {
        loop {
            let token = tokens.get(at)?;
            if token == word {
                at += 1;
                break;
            }
            let option = |i: usize| tokens.get(i).is_some_and(|t| t.starts_with('-'));
            if !option(at) && !option(at - 1) {
                return None;
            }
            at += 1;
        }
    }
    Some(at)
}

/// One matched phrase: the argv, and where the command's own words start.
struct Invocation<'a> {
    words: &'a [String],
    own: usize,
}

impl Invocation<'_> {
    /// The words after the phrase: the command's own options and operands.
    fn own(&self) -> &[String] {
        &self.words[self.own..]
    }

    fn carries(&self, element: &Element) -> bool {
        let options = |words: &[String]| -> Vec<String> {
            words.iter().take_while(|w| *w != "--").cloned().collect()
        };
        match *element {
            Element::Option(option) => options(self.own())
                .iter()
                .any(|word| option_matches(option, word)),
            Element::Valued { option, name } => {
                let words = options(&self.words[1..]);
                words.iter().enumerate().any(|(at, word)| {
                    let value = if word == option {
                        words.get(at + 1).map(String::as_str)
                    } else {
                        word.strip_prefix(option)
                            .filter(|_| option.starts_with("--"))
                            .and_then(|rest| rest.strip_prefix('='))
                    };
                    value.is_some_and(|value| {
                        let set = value.split_once('=').map_or(value, |(set, _)| set);
                        set.eq_ignore_ascii_case(name)
                    })
                })
            }
            Element::Subcommand(word) => follows(&self.words[self.own - 1..], &[word]).is_some(),
        }
    }
}

/// One `deny_argv` element, by its shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Element<'a> {
    /// `-n`, `--no-verify`: an option among the command's own words, as a
    /// word, as a letter in a cluster of short options (`-anm`) or as
    /// `--name=value`.
    Option(&'a str),
    /// `-c core.hooksPath`: an option and the name its value sets
    /// (`-c core.hooksPath=/dev/null`, `--config-env=core.hooksPath=X`),
    /// wherever the program or the phrase reads options; the name compares
    /// without ASCII case, as git reads the key.
    Valued { option: &'a str, name: &'a str },
    /// `close`: the subcommand standing after the phrase, `gh issue close`.
    Subcommand(&'a str),
}

impl<'a> Element<'a> {
    /// The element a `deny_argv` string writes; `None` for a shape that is
    /// none of the three, which the manifest refuses.
    pub(crate) fn parse(text: &'a str) -> Option<Element<'a>> {
        let mut words = text.split_whitespace();
        match (words.next()?, words.next(), words.next()) {
            (option, None, None) if option.starts_with('-') => Some(Element::Option(option)),
            (word, None, None) => Some(Element::Subcommand(word)),
            (option, Some(name), None) if option.starts_with('-') && !name.starts_with('-') => {
                Some(Element::Valued { option, name })
            }
            _ => None,
        }
    }
}

/// A file operand that names stdin.
fn reads_stdin(file: &str) -> bool {
    matches!(file, "-" | "/dev/stdin")
}

/// `-c`, or a cluster carrying it (`-lc`): a shell's commands from the
/// word after it.
fn is_command_option(word: &str) -> bool {
    is_short_cluster(word) && word.contains('c')
}

/// `-anm`: one `-` and letters only.
fn is_short_cluster(word: &str) -> bool {
    word.strip_prefix('-')
        .is_some_and(|c| !c.is_empty() && c.chars().all(|c| c.is_ascii_alphabetic()))
}

fn option_matches(option: &str, word: &str) -> bool {
    if word == option {
        return true;
    }
    if option.starts_with("--") {
        return word
            .strip_prefix(option)
            .is_some_and(|rest| rest.starts_with('='));
    }
    let mut letters = option.chars().skip(1);
    match (letters.next(), letters.next()) {
        (Some(letter), None) => is_short_cluster(word) && word[1..].contains(letter),
        _ => false,
    }
}

/// `.`, `..`, `./`, `../`, a `:` pathspec magic (`:/`) or a glob the
/// shell left unexpanded (`'*'`, `'./*'`): a name for the tree, or for
/// whatever is in it, rather than for files the agent wrote.
fn names_the_tree(path: &str) -> bool {
    let path = path.trim_end_matches('/');
    let path = path.strip_prefix("./").unwrap_or(path);
    path.is_empty() || path == "." || path == ".." || path.starts_with(':') || path.contains('*')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(line: &str) -> Vec<Vec<String>> {
        commands(line).into_iter().map(|argv| argv.0).collect()
    }

    #[test]
    fn a_line_splits_into_simple_commands_with_quotes_removed_and_never_split_on() {
        assert_eq!(
            words("git add -A && git commit -m \"fix -n handling\"; echo 'a b' | cat"),
            vec![
                vec!["git", "add", "-A"],
                vec!["git", "commit", "-m", "fix -n handling"],
                vec!["echo", "a b"],
                vec!["cat"],
            ]
        );
        assert_eq!(
            words("git commit -m ''"),
            vec![vec!["git", "commit", "-m", ""]]
        );
        assert_eq!(
            words("echo \"a;b|c\" x\\ y"),
            vec![vec!["echo", "a;b|c", "x y"]]
        );
        assert_eq!(
            words("(cd x && git status) || true"),
            vec![vec!["cd", "x"], vec!["git", "status"], vec!["true"],]
        );
    }

    #[test]
    fn a_redirection_and_its_target_are_no_words_of_the_argv() {
        assert_eq!(
            words("cargo test 2>&1 >/tmp/out &>> log < in | tail -n 5"),
            vec![vec!["cargo", "test"], vec!["tail", "-n", "5"]]
        );
        assert_eq!(words("echo a>b"), vec![vec!["echo", "a"]]);
    }

    #[test]
    fn a_heredoc_body_is_text_and_what_follows_it_is_read() {
        let line = "cat > /tmp/m <<'EOF'\ngit commit -n\n$(git commit -n)\nEOF\ngit status";
        assert_eq!(words(line), vec![vec!["cat"], vec!["git", "status"]]);
        let tabs = "cat <<-END; echo after\n\tgit commit -n\n\tEND\ngit log";
        assert_eq!(
            words(tabs),
            vec![vec!["cat"], vec!["echo", "after"], vec!["git", "log"]]
        );
        // Under an unquoted delimiter a substitution in the body still runs.
        let unquoted = "cat <<EOF\ntext $(git commit -n) text\nEOF";
        assert_eq!(
            words(unquoted),
            vec![vec!["cat"], vec!["git", "commit", "-n"]]
        );
        assert_eq!(words("cat <<< 'git commit -n'"), vec![vec!["cat"]]);
    }

    #[test]
    fn a_heredoc_a_shell_reads_on_its_stdin_is_the_commands_it_runs() {
        let fed = |line: &str| {
            words(line)
                .into_iter()
                .any(|w| w == ["git", "commit", "-n"])
        };
        assert!(fed("bash <<'EOF'\ncd x\ngit commit -n\nEOF"));
        assert!(fed("cat <<'EOF' | sh\ngit commit -n\nEOF"));
        assert!(fed("bash -s -- a b <<'EOF'\ngit commit -n\nEOF"));
        assert!(fed("bash -o pipefail <<'EOF'\ngit commit -n\nEOF"));
        assert!(fed("zsh -- <<'EOF'\ngit commit -n\nEOF"));
        assert!(fed("bash /dev/stdin <<'EOF'\ngit commit -n\nEOF"));
        assert!(fed("sh - <<'EOF'\ngit commit -n\nEOF"));
        // Under an unquoted delimiter the outer shell runs a substitution
        // before the shell that reads the body sees its quotes.
        assert!(fed("bash <<EOF\necho '$(git commit -n)'\nEOF"));
        assert!(fed("sudo=1 /bin/sh <<-EOF\n\tgit commit -n\n\tEOF"));
        // A shell with its commands elsewhere reads the heredoc as text.
        assert!(!fed("bash -c 'cat' <<'EOF'\ngit commit -n\nEOF"));
        assert!(!fed("bash run.sh <<'EOF'\ngit commit -n\nEOF"));
        assert!(!fed("python3 - <<'EOF'\ngit commit -n\nEOF"));
        // The body ends at its delimiter, and a later line is no shell's.
        assert!(!fed("bash <<'EOF'\necho\nEOF\ncat <<'X'\ngit commit -n\nX"));
    }

    #[test]
    fn a_lines_text_is_without_the_heredoc_bodies_it_carries_as_text() {
        let text = |line: &str| Line::read(line).text().to_owned();
        assert_eq!(
            text("cat > /tmp/m <<'EOF'\nquoted -- one\n  two\nEOF\ngit status"),
            "cat > /tmp/m <<'EOF'\nEOF\ngit status"
        );
        assert_eq!(text("cat <<-END\n\tquoted\n\tEND"), "cat <<-END\n\tEND");
        // A body never closed is text to the line's end.
        assert_eq!(text("cat <<EOF\nquoted"), "cat <<EOF\n");
        // Two on one line, each to its own delimiter.
        assert_eq!(
            text("cat <<A; cat <<'B'\none\nA\ntwo\nB\nls"),
            "cat <<A; cat <<'B'\nA\nB\nls"
        );
        // A line of an unquoted body that runs a substitution stays, and
        // so does a body a shell runs.
        assert_eq!(
            text("cat <<EOF\nplain\nran $(date) here\nEOF"),
            "cat <<EOF\nran $(date) here\nEOF"
        );
        assert_eq!(
            text("cat <<'EOF'\nran $(date) here\nEOF"),
            "cat <<'EOF'\nEOF"
        );
        let shell = "bash <<'EOF'\nls\nEOF";
        assert_eq!(text(shell), shell);
        // A herestring and a line with no heredoc are what they are.
        assert_eq!(text("cat <<< 'quoted'"), "cat <<< 'quoted'");
        assert_eq!(text("echo é; ls"), "echo é; ls");
    }

    #[test]
    fn a_substitution_and_a_nested_line_are_commands_of_their_own() {
        let inner = |line: &str| {
            words(line)
                .into_iter()
                .any(|w| w == ["git", "commit", "-n"])
        };
        assert!(inner("echo $(git commit -n)"));
        assert!(inner("echo \"$(git commit -n)\""));
        assert!(inner("echo `git commit -n`"));
        assert!(inner("bash -c 'git commit -n'"));
        assert!(inner("sh -lc \"cd x && git commit -n\""));
        assert!(inner("eval git commit -n"));
        assert!(!inner("echo '$(git commit -n)'"));
    }

    #[test]
    fn assignments_reserved_words_and_wrappers_are_peeled_off_the_program() {
        assert_eq!(
            words("GIT_EDITOR=true git commit"),
            vec![vec!["git", "commit"]]
        );
        assert_eq!(
            words("env -u X A=1 nohup git commit"),
            vec![vec!["git", "commit"]]
        );
        assert_eq!(
            words("if git diff; then git commit; fi"),
            vec![vec!["git", "diff"], vec!["git", "commit"], vec!["fi"],]
        );
        assert_eq!(words("{ git commit; }"), vec![vec!["git", "commit"]]);
        assert_eq!(words("A=1"), Vec::<Vec<String>>::new());
    }

    /// #2607: the directory a `cd`, `pushd` or git `-C` names by a
    /// variable the line assigned earlier is the variable's value, whole;
    /// one the line did not assign, or assigned only in the command's own
    /// environment, stays as written.
    #[test]
    fn a_directory_named_by_a_variable_the_line_assigned_is_its_value() {
        let dirs = |line: &str| -> Vec<Vec<String>> {
            commands(line)
                .iter()
                .map(|argv| argv.directories().map(str::to_owned).collect())
                .filter(|dirs: &Vec<String>| !dirs.is_empty())
                .collect()
        };
        assert_eq!(dirs("W='/a b' && cd \"$W\""), vec![vec!["/a b"]]);
        assert_eq!(dirs("export W=/a; cd ${W}/sub"), vec![vec!["/a/sub"]]);
        assert_eq!(dirs("W=/a; pushd -q $W"), vec![vec!["/a"]]);
        assert_eq!(
            dirs("W=/a && git -c x=y -C $W -C sub commit -C HEAD"),
            vec![vec!["/a", "sub"]]
        );
        assert_eq!(dirs("cd \"$W\""), vec![vec!["$W"]]);
        assert_eq!(dirs("W=/a cd $W"), vec![vec!["$W"]]);
        assert_eq!(dirs("cd -"), Vec::<Vec<String>>::new());
        assert_eq!(dirs("touch $W"), Vec::<Vec<String>>::new());
    }

    /// Whether a command of the line is the one `phrase` names.
    fn names(line: &str, phrase: &str) -> bool {
        let with = OneOrMany::One(phrase.to_owned());
        commands(line).iter().any(|argv| argv.invokes(&with))
    }

    /// Every way xtask is run names `xtask phpt` (#1860): the cargo alias,
    /// `cargo run` on the package, the binary by any path, and a variable
    /// the line assigned one of those, in an assignment of its own or an
    /// `export`. Where the words are quoted text or another program's,
    /// none does.
    #[test]
    fn an_xtask_invocation_is_read_however_xtask_is_run() {
        for line in [
            "cargo xtask phpt --full",
            "cargo +nightly -q xtask phpt",
            "cargo --color never xtask --root /tmp/x phpt",
            "cargo run -p xtask -- phpt --full",
            "cargo run -q --release --package xtask -- phpt",
            "cargo run --package=xtask -- phpt",
            "cargo -q run -pxtask -- phpt",
            "cargo run -p xtask phpt --full",
            "cargo run -j 4 -p xtask --bin xtask -- phpt",
            "cargo r --bin xtask -- phpt",
            "target/release/xtask phpt --full",
            "./xtask phpt",
            "cd x && /Users/m/dax-php/target/debug/xtask phpt > lane.out",
            "XTASK=target/release/xtask; $XTASK phpt --full",
            "X='cargo xtask'\n${X} phpt",
            "export X=\"cargo run -q -p xtask --\" && A=1 $X phpt",
            "X=target/debug/xtask Y=1; \"$X\" phpt",
        ] {
            assert!(names(line, "xtask phpt"), "{line}: {:?}", words(line));
        }
        for line in [
            "git commit -m \"cargo xtask phpt --full\" -- a.yml",
            "gh issue create --title t --body 'run cargo xtask phpt --full nightly'",
            "echo cargo xtask phpt",
            "cargo run -p other -- phpt",
            "cargo test -p xtask phpt",
            "cargo run -p xtask",
            "cargo xtask bench",
            "$XTASK phpt",
            "X=target/release/xtask $X phpt",
            "X='$X'; $X phpt",
            "X='$Y'; Y='cargo xtask'; $X phpt",
            "X=cargo; echo $X xtask phpt",
            "rg 'xtask phpt' xtask/src",
        ] {
            assert!(!names(line, "xtask phpt"), "{line}: {:?}", words(line));
        }
        // xtask's argv is the words it receives, its own name first.
        assert_eq!(
            words("cargo run -q -p xtask -- phpt --subset ext/pcre"),
            vec![vec!["xtask", "phpt", "--subset", "ext/pcre"]]
        );
        assert_eq!(
            words("X='cargo xtask'; $X phpt"),
            vec![vec!["xtask", "phpt"]]
        );
    }

    /// `lanes.nightly-flags` as `deny_argv` reads them: among the words
    /// xtask's instrument receives, however xtask is run, and never where a
    /// message cites them or another instrument carries them.
    #[test]
    fn the_nightly_flags_are_read_among_the_instruments_own_words() {
        let with = "xtask phpt";
        let lane = ["--full", "--jit"];
        assert!(denies("cargo xtask phpt --full || true", &lane, with));
        assert!(denies(
            "cargo run -p xtask -- phpt --jit force",
            &lane,
            with
        ));
        assert!(denies(
            "target/release/xtask phpt --jit=exit-fuzz",
            &lane,
            with
        ));
        assert!(denies(
            "X=target/release/xtask; $X phpt --full",
            &lane,
            with
        ));
        assert!(!denies("cargo xtask phpt --subset ext/pcre", &lane, with));
        assert!(!denies(
            "git commit -m 'cargo xtask phpt --full' -- a.yml",
            &lane,
            with
        ));
        assert!(!denies("cargo xtask bench --full", &lane, with));
        assert!(denies(
            "cargo xtask bench --jit force",
            &lane,
            "xtask bench"
        ));
        assert!(!denies("cargo xtask phpt -- --full", &lane, with));
    }

    fn denies(line: &str, denied: &[&str], with: &str) -> bool {
        let denied: Vec<String> = denied.iter().map(|d| (*d).to_owned()).collect();
        let with = if with.is_empty() {
            OneOrMany::None
        } else {
            OneOrMany::One(with.to_owned())
        };
        commands(line)
            .iter()
            .any(|argv| argv.denies(&denied, &with))
    }

    #[test]
    fn a_denied_option_is_one_of_the_commands_own_words_after_the_phrase() {
        let denied = ["--no-verify", "-n"];
        for with in ["commit", "git commit"] {
            assert!(denies("git commit -n -m x", &denied, with));
            assert!(denies("git commit -anm x", &denied, with));
            assert!(denies(
                "git -C /tmp/x --no-pager commit --no-verify",
                &denied,
                with
            ));
            assert!(denies("cd x && /usr/bin/git commit -n", &denied, with));
            assert!(denies("(cd x; git commit -n)", &denied, with));
            // A quoted word is the word the program receives.
            assert!(denies("git commit '--no-verify'", &denied, with));
            assert!(!denies("git log -n 3", &denied, with));
            assert!(!denies("grep -n commit file", &denied, with));
            assert!(!denies("sed -n '/commit/p' file", &denied, with));
            assert!(!denies("git commit -m 'no -n here'", &denied, with));
            assert!(!denies("git commit --no-verify-x", &denied, with));
            assert!(!denies("git commit -m x -- -n", &denied, with));
            assert!(!denies("echo git commit -n", &denied, "git commit"));
        }
        assert!(denies("git commit -n", &denied, ""));
    }

    #[test]
    fn a_valued_element_is_an_option_and_the_name_its_value_sets() {
        let denied = ["-c core.hooksPath", "--config-env core.hooksPath"];
        assert!(denies(
            "git -c core.hooksPath=/dev/null commit",
            &denied,
            "git commit"
        ));
        assert!(denies(
            "git -c core.hookspath=x commit -m y",
            &denied,
            "git commit"
        ));
        assert!(denies(
            "git --config-env=core.hooksPath=H commit",
            &denied,
            "git commit"
        ));
        assert!(denies(
            "git --config-env core.hooksPath=H commit",
            &denied,
            "git commit"
        ));
        assert!(!denies("git -c user.name=x commit", &denied, "git commit"));
        assert!(!denies(
            "git -c core.hooksPath=/dev/null log",
            &denied,
            "git commit"
        ));
    }

    #[test]
    fn a_denied_word_is_the_subcommand_standing_after_the_phrase() {
        let denied = ["close", "reopen"];
        assert!(denies(
            "gh issue close 28 --comment done",
            &denied,
            "gh issue"
        ));
        assert!(denies(
            "gh -R o/r issue --repo o/r reopen 28",
            &denied,
            "gh issue"
        ));
        assert!(!denies(
            "gh issue edit 28 --title close",
            &denied,
            "gh issue"
        ));
        assert!(!denies("gh issue list --state closed", &denied, "gh issue"));
        assert!(!denies("cargo xtask ticket close 28", &denied, "gh issue"));
        assert!(!denies("gh pr close 3", &denied, "gh issue"));
    }

    #[test]
    fn an_element_is_an_option_a_valued_option_or_a_subcommand() {
        assert_eq!(Element::parse("-n"), Some(Element::Option("-n")));
        assert_eq!(Element::parse("close"), Some(Element::Subcommand("close")));
        assert_eq!(
            Element::parse("-c core.hooksPath"),
            Some(Element::Valued {
                option: "-c",
                name: "core.hooksPath"
            })
        );
        assert_eq!(Element::parse("git commit"), None);
        assert_eq!(Element::parse("-c -n"), None);
        assert_eq!(Element::parse("-c a b"), None);
        assert_eq!(Element::parse(" "), None);
    }

    #[test]
    fn a_phrase_is_its_words_in_order_with_only_options_between_them() {
        let git_add = OneOrMany::Many(vec!["git add".to_owned(), "git commit".to_owned()]);
        let carries = |line: &str| commands(line)[0].invokes(&git_add);
        assert!(carries("git add -- src"));
        assert!(carries("git -C /tmp/x add -- src"));
        assert!(carries("git --no-pager -c user.name=x commit -- src"));
        assert!(!carries("git log --grep add"));
        assert!(!carries("git worktree add -b x dir"));
        assert!(!carries("cargo add serde"));
        assert!(carries("git commit -m x"));
    }

    /// `lanes.builder-subset` as `require_argv = "--subset"` under `with =
    /// "xtask phpt"`: the subset named, as a word or after `=`, passes,
    /// and so does a call for the instrument's help; the lane run bare or
    /// with `--census`, however xtask is run, is unbounded, and a message
    /// that cites it is no xtask.
    #[test]
    fn a_subset_bounds_the_phpt_lane() {
        let with = OneOrMany::One("xtask phpt".to_owned());
        let bare = |line: &str| {
            commands(line)
                .iter()
                .any(|a| a.unbounded("--subset", &with))
        };
        for line in [
            "cargo xtask phpt",
            "cargo xtask phpt --census --against e7f3b27",
            "cargo run -q -p xtask -- phpt --jobs 4",
            "X=target/release/xtask; $X phpt",
            "cargo xtask phpt; grep -h x file",
            "cargo xtask phpt --subset",
        ] {
            assert!(bare(line), "{line}");
        }
        for line in [
            "cargo xtask phpt --subset ext/zlib",
            "cargo xtask phpt --subset=ext/zlib --census",
            "target/debug/xtask phpt --jobs 2 --subset Zend/tests",
            "cargo xtask phpt --help | grep -E '^ +--'",
            "cargo xtask phpt -h",
            "git commit -m 'the merge check runs cargo xtask phpt --census' -- a.rs",
            "cargo xtask phpt-census",
            "cargo xtask bench",
        ] {
            assert!(!bare(line), "{line}");
        }
    }

    #[test]
    fn the_separator_is_required_and_what_follows_it_names_files() {
        let with = OneOrMany::Many(vec!["git add".to_owned(), "git commit".to_owned()]);
        let missing = |line: &str| commands(line).iter().any(|a| a.unbounded("--", &with));
        for line in [
            "git add -A",
            "git add .",
            "git add -u",
            "git add -- .",
            "git add -- ':/'",
            "git add -- ./",
            "git add -- ../",
            "git add -- './*'",
            "git add -- 'xtask/*'",
            "git add --",
            "git commit -m x",
            "git commit --amend --no-edit",
            "git commit -am x",
            "cargo fmt && git add -A && git commit -m x",
        ] {
            assert!(missing(line), "{line}");
        }
        for line in [
            "git add -- xtask/src/permit/rules.rs",
            "git add -A -- xtask/src",
            "git commit -F /tmp/msg -- xtask/src AGENTS.md",
            "git commit --amend -F /tmp/msg -- xtask/src",
            "git commit -m 'add -- nothing' -- AGENTS.md",
            "git status && git diff --cached -- xtask",
            "git log --grep add",
        ] {
            assert!(!missing(line), "{line}");
        }
    }
}
