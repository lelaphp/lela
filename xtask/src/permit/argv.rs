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
//! not read; pre-push and CI run `done` behind it. The parentheses of
//! an array assignment, `P=(a b)`, open no subshell: its elements are
//! words of the assignment (#3165).
//!
//! The same reading gives a `deny_pattern` what it reads, the line's
//! commands ([`Line::subject`]): the line with the heredoc bodies it
//! carries as text removed, and a quoted argument read as words and never
//! as a command position, so a pattern judges what a command runs and not
//! what it quotes (#1771, #1832, #3635), as an argv rule does. One lexer
//! decides what a body and a quote are for both.

use std::collections::BTreeMap;
use std::ops::Range;

use regex_lite::Regex;

use crate::manifest::OneOrMany;

/// One simple command's words as the program that runs receives them,
/// its name first, and where its own words and those it runs under move
/// it apart from its shell ([`Move`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Argv {
    words: Vec<String>,
    moves: Vec<Move>,
    /// Its words past the program as the shell expands them as far as the
    /// line says ([`Argv::named`]).
    named: Vec<String>,
}

/// A directory a command's words move its program to, or reach, apart
/// from where its shell stands, each read from where the one before it
/// left the program (#2995).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Move {
    /// It runs there: `env -C`, git's `-C`, `patch -d`.
    Into(String),
    /// It reaches there besides: `GIT_WORK_TREE`, prefixed or assigned by
    /// the line, git's `--work-tree`, the target directory of `cp`, `mv`,
    /// `install` or `ln`, and the directory of each operand of `ln`, a
    /// link through which a later write lands there.
    Reaches(String),
    /// git's repository is there: `GIT_DIR`, prefixed or assigned by the
    /// line, and git's `--git-dir`. Every reader takes it as a place the
    /// program reaches; the one asking which repository git reads takes it
    /// in place of where the program runs (#3271).
    GitDir(String),
    /// Somewhere its words do not say: `find -execdir`, or a variable the
    /// line did not assign.
    Unsaid,
}

/// The separator on one side of a simple command, as its shell reads the
/// list the command stands in (#2995).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(super) enum Joint {
    /// `;`, a newline, the line's edge or a subshell's close: what
    /// follows may run whether this succeeded or not.
    #[default]
    Then,
    /// `||`: what follows runs only where this failed, and begins a chain
    /// as `;` does (#3257).
    Or,
    /// `&&`: what follows runs only where this succeeded.
    And,
    /// `|` or `|&`: both sides run at once, each in a shell of its own.
    Pipe,
    /// A lone `&`: the `&&` list it ends runs in a shell of its own, in
    /// the background.
    Background,
}

/// The separators either side of one command; a subshell's, a
/// substitution's or a nested line's first command is joined as the
/// command it stands in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Joints {
    pub(super) before: Joint,
    pub(super) after: Joint,
}

/// Every simple command the line runs, in the order the lexer closes them.
pub(super) fn commands(line: &str) -> Vec<Argv> {
    Lexer::read(line).commands
}

/// A shell line as permit reads it: the simple commands it runs, its text
/// less the heredoc bodies it carries as text, those bodies, and the files
/// its redirections write, each with the command it stands on.
#[derive(Debug)]
pub(super) struct Line {
    commands: Vec<Argv>,
    shells: Vec<Vec<u32>>,
    joints: Vec<Joints>,
    text: String,
    subject: String,
    /// Whether a quote holds each byte of `subject`.
    quoted: Vec<bool>,
    bodies: Vec<Heretext>,
    redirects: Vec<Redirect>,
}

/// A file one redirection writes (`>`, `>>`, `>|`, `&>`, `<>`): its target
/// as the shell expands it as far as the line says, a variable the line
/// assigned read as its value and `~/` as the home ([`Assigned::target`],
/// #4250), and the command it stands on by its place in
/// [`Line::commands`]; `None` for one on no command, a redirection alone or
/// one after a subshell's `)`, which the line's shell makes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Redirect {
    pub(super) target: String,
    pub(super) command: Option<usize>,
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
        let (subject, quoted) = lexer.subject();
        Line {
            commands: lexer.commands,
            shells: lexer.shells,
            joints: lexer.joints,
            text,
            subject,
            quoted,
            bodies: lexer.bodies,
            redirects: lexer.redirects,
        }
    }

    /// Every simple command the line runs ([`commands`]).
    pub(super) fn commands(&self) -> &[Argv] {
        &self.commands
    }

    /// The shells each of [`Line::commands`] runs in, outermost first:
    /// one id per subshell, substitution or child shell holding it, none
    /// for the line's own, so a `cd` in one moves the commands that share
    /// its shells alone (#2764).
    pub(super) fn shells(&self) -> &[Vec<u32>] {
        &self.shells
    }

    /// How each of [`Line::commands`] is joined to its neighbours in its
    /// shell ([`Joints`], #2995).
    pub(super) fn joints(&self) -> &[Joints] {
        &self.joints
    }

    /// The line as it is spelled less the heredoc body lines that are text
    /// to their command, each with its line ending. A body a shell reads on
    /// its stdin stays, being commands, and so does a line of an unquoted
    /// body that runs a `$(…)` or a backquote. A heredoc inside a body a
    /// shell runs, a backquote or a `-c` argument stays too, being read by
    /// a lexer of its own.
    pub(super) fn text(&self) -> &str {
        &self.text
    }

    /// What a `deny_pattern` and its `unless_pattern` read, the line's
    /// commands (#3635): [`Line::text`] with each `;`, `&`, `|` and newline
    /// a quote holds read as a space, since the shell reads none of them as
    /// a separator, and after it, a line each, what a line the line runs in
    /// turn reads, a `-c` argument, `eval`'s words, a backquote or a body a
    /// shell reads on its stdin, so a command a quote holds and a shell
    /// runs is read where it runs.
    pub(super) fn subject(&self) -> &str {
        &self.subject
    }

    /// Each hit of `regex` over [`Line::subject`] that starts outside a
    /// quote: a quoted argument is never a command position, nor a
    /// redirection, so no hit starts in one (#3635). Each search resumes
    /// just past the last hit's start, so a hit passed over hides none.
    pub(super) fn hits(&self, regex: &Regex) -> Vec<Range<usize>> {
        let (mut hits, mut at) = (Vec::new(), 0);
        while let Some(hit) = regex.find_at(&self.subject, at) {
            if !self.quoted.get(hit.start()).copied().unwrap_or_default() {
                hits.push(hit.range());
            }
            let next = self.subject[hit.start()..].chars().next();
            at = hit.start() + next.map_or(1, char::len_utf8);
            if at > self.subject.len() {
                break;
            }
        }
        hits
    }

    /// Every heredoc body a command reads as text, a nested line's
    /// included, each with the commands of its line.
    pub(super) fn bodies(&self) -> &[Heretext] {
        &self.bodies
    }

    /// The files the line's redirections write, a nested line's included,
    /// each target as [`Redirect`] expands it.
    pub(super) fn written(&self) -> impl Iterator<Item = &String> {
        self.redirects.iter().map(|r| &r.target)
    }

    /// Each redirection that writes a file, with the command it stands on
    /// ([`Redirect`]).
    pub(super) fn redirects(&self) -> &[Redirect] {
        &self.redirects
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
#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum Next {
    #[default]
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
#[derive(Default)]
struct Word {
    text: String,
    quoted: bool,
}

/// The command being read.
#[derive(Default)]
struct Command {
    words: Vec<String>,
    word: Option<Word>,
    next: Next,
    /// Whether it carries a redirection, so a command of no words still
    /// runs one.
    redirected: bool,
    /// The index in `words` of each array assignment that word text
    /// follows straight after its `)` (#3198): zsh ends the word there,
    /// and bash reads the word after as more of a scalar assignment.
    joined: Vec<usize>,
    /// The files its redirections write, expanded ([`Redirect`]).
    targets: Vec<String>,
}

impl Command {
    fn push(&mut self, c: char) {
        self.word.get_or_insert_with(Word::default).text.push(c);
    }

    /// Whether a `(` here opens an array assignment (#3165): the word
    /// being read is an unquoted `NAME=` or `NAME+=`, standing where an
    /// assignment does, among the command's first words or after a
    /// declaring builtin.
    fn opens_array(&self) -> bool {
        let Some(word) = self.word.as_ref().filter(|w| !w.quoted) else {
            return false;
        };
        let at_assignment = match self.words.first() {
            Some(first) if DECLARING.contains(&first.as_str()) => true,
            _ => self.words.iter().all(|w| is_assignment(w)),
        };
        at_assignment && word.text.strip_suffix('=').and_then(assigns).is_some()
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

#[derive(Default)]
struct Lexer {
    chars: Vec<char>,
    at: usize,
    commands: Vec<Argv>,
    /// The shells each command runs in ([`Line::shells`]), the ones open
    /// now, and how many this lexer has opened, each one's id.
    shells: Vec<Vec<u32>>,
    open: Vec<u32>,
    opened: u32,
    /// How each command is joined ([`Line::joints`]), and the separator
    /// before the command being read.
    joints: Vec<Joints>,
    before: Joint,
    heredocs: Vec<Heredoc>,
    /// Where in `commands` the line being read starts, so the heredocs it
    /// opened can ask what its commands are.
    line_start: usize,
    /// The heredoc body lines skipped as text, each with its line ending,
    /// as ranges of `chars` in order.
    text_bodies: Vec<Range<usize>>,
    /// The variables the line has assigned so far ([`Assigned`]).
    assigned: Assigned,
    /// The files the line's redirections write, in order ([`Redirect`]).
    redirects: Vec<Redirect>,
    /// The heredoc bodies the line's commands read as text ([`Heretext`]).
    bodies: Vec<Heretext>,
    /// Whether a quote or an escape holds each of `chars`, as text.
    quoted: Vec<bool>,
    /// What each line this line runs in turn reads ([`Lexer::subject`]).
    nested: Vec<(String, Vec<bool>)>,
}

impl Lexer {
    fn new(line: &str) -> Lexer {
        let chars: Vec<char> = line.chars().collect();
        Lexer {
            quoted: vec![false; chars.len()],
            chars,
            ..Lexer::default()
        }
    }

    /// The char just read is text a quote or an escape holds.
    fn held(&mut self) {
        if let Some(held) = self.quoted.get_mut(self.at.saturating_sub(1)) {
            *held = true;
        }
    }

    /// What a `deny_pattern` reads of what this lexer read
    /// ([`Line::subject`]), and whether a quote holds each byte of it.
    fn subject(&self) -> (String, Vec<bool>) {
        let (mut text, mut quoted) = (String::new(), Vec::new());
        for (at, &c) in self.chars.iter().enumerate() {
            if self.text_bodies.iter().any(|body| body.contains(&at)) {
                continue;
            }
            let held = self.quoted.get(at).copied().unwrap_or_default();
            let blank = held && matches!(c, ';' | '&' | '|' | '\n');
            text.push(if blank { ' ' } else { c });
            quoted.resize(text.len(), held);
        }
        for (line, held) in &self.nested {
            text.push('\n');
            text.push_str(line);
            quoted.push(false);
            quoted.extend(held);
        }
        (text, quoted)
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
        let mut nested = Lexer::new(line);
        nested.before = self.before;
        nested.list(None);
        self.nested.push(nested.subject());
        let offset = self.commands.len();
        self.adopt(&mut nested);
        self.redirects
            .extend(nested.redirects.into_iter().map(|r| Redirect {
                target: r.target,
                command: r.command.map(|at| at + offset),
            }));
        self.bodies
            .extend(nested.bodies.into_iter().map(|body| Heretext {
                commands: body.commands.start + offset..body.commands.end + offset,
                text: body.text,
            }));
    }

    /// Commands another lexer read, run from the shells open here, each
    /// in the shells that lexer opened, joined as it read them.
    fn adopt(&mut self, other: &mut Lexer) {
        let first = self.opened;
        self.opened += other.opened;
        self.commands.append(&mut other.commands);
        self.joints.append(&mut other.joints);
        let open = &self.open;
        let shells = std::mem::take(&mut other.shells).into_iter().map(|inner| {
            let ids = inner.into_iter().map(|id| first + id);
            open.iter().copied().chain(ids).collect()
        });
        self.shells.extend(shells);
    }

    /// What `read` reads run in a shell of its own, its first command
    /// joined as the one it stands in.
    fn child(&mut self, read: impl FnOnce(&mut Lexer)) {
        let before = self.before;
        self.open.push(self.opened);
        self.opened += 1;
        read(self);
        self.open.pop();
        self.before = before;
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
        let mut command = Command::default();
        while let Some(c) = self.bump() {
            if self.quote(&mut command, c) {
                continue;
            }
            match c {
                c if Some(c) == close => break,
                '#' if command.word.is_none() => self.comment(),
                '<' | '>' => self.redirection(&mut command, c),
                '&' if self.eat('>') => {
                    self.end_word(&mut command);
                    self.eat('>');
                    command.next = Next::Target { writes: true };
                }
                '(' if command.opens_array() => self.array(&mut command),
                '(' => {
                    self.end(&mut command, Joint::Then);
                    self.child(|lexer| lexer.list(Some(')')));
                    self.before = Joint::Then;
                }
                ';' | '&' | '|' | ')' | '\n' => {
                    let after = match c {
                        '&' if self.eat('&') => Joint::And,
                        '&' => Joint::Background,
                        '|' if !self.eat('|') => {
                            self.eat('&');
                            Joint::Pipe
                        }
                        '|' => Joint::Or,
                        _ => Joint::Then,
                    };
                    self.end(&mut command, after);
                    if c == '\n' {
                        self.skip_heredoc_bodies();
                    }
                }
                c if c.is_whitespace() => self.end_word(&mut command),
                c => command.push(c),
            }
        }
        self.end(&mut command, Joint::Then);
    }

    /// An escape, a quote or a substitution `c` opens, read into the word
    /// being read; `false` where `c` opens none.
    fn quote(&mut self, command: &mut Command, c: char) -> bool {
        match c {
            '\\' => match self.bump() {
                Some('\n') | None => {}
                Some(c) => {
                    self.held();
                    command.open(true);
                    command.push(c);
                }
            },
            '\'' => {
                command.open(true);
                while let Some(c) = self.bump().filter(|c| *c != '\'') {
                    self.held();
                    command.push(c);
                }
            }
            '"' => {
                command.open(true);
                self.expand(command, Some('"'));
            }
            '`' => {
                command.open(false);
                self.backquote();
            }
            '$' if self.eat('(') => {
                command.open(false);
                self.child(|lexer| lexer.list(Some(')')));
            }
            _ => return false,
        }
        true
    }

    /// A comment, up to the newline that ends it.
    fn comment(&mut self) {
        while self.chars.get(self.at).is_some_and(|c| *c != '\n') {
            self.at += 1;
        }
    }

    /// The elements of an array assignment, `NAME=(…)` or `NAME+=(…)`, up
    /// to the `)` that matches its `(` (#3165): they are words of the
    /// assignment and no commands, a `$(…)` among them still running, and
    /// the word ends at the `)` as zsh ends it, word text straight after
    /// it read as bash's too ([`Lexer::end`]). A process substitution
    /// among them runs too, `<(…)`, `>(…)` or zsh's `=(…)`, and a newline
    /// ends the line a heredoc's body follows. A separator among them is
    /// one more space, as zsh reads `;` there and bash refuses the line;
    /// a parenthesis nested in an element is its text, a zsh pattern. The
    /// value is the elements joined by spaces, as `$P` expands in zsh, and
    /// an append's is led by one, so it extends the value as `+=` does.
    fn array(&mut self, command: &mut Command) {
        let mut elements = Command::default();
        let mut depth = 0_usize;
        while let Some(c) = self.bump() {
            if self.quote(&mut elements, c) {
                continue;
            }
            match c {
                ')' if depth == 0 => break,
                '#' if elements.word.is_none() => self.comment(),
                '<' | '>' | '=' if self.eat('(') => {
                    elements.open(false);
                    self.child(|lexer| lexer.list(Some(')')));
                }
                '\n' => {
                    self.end_word(&mut elements);
                    self.skip_heredoc_bodies();
                }
                c if c.is_whitespace() || matches!(c, ';' | '&' | '|') => {
                    self.end_word(&mut elements);
                }
                c => {
                    match c {
                        '(' => depth += 1,
                        ')' => depth -= 1,
                        _ => {}
                    }
                    elements.push(c);
                }
            }
        }
        self.end_word(&mut elements);
        let value = elements.words.join(" ");
        if let Some(word) = command.word.as_mut() {
            if word.text.ends_with("+=") && !value.is_empty() {
                word.text.push(' ');
            }
            word.text.push_str(&value);
        }
        let word = command.words.len();
        let listed = command.next == Next::Argument;
        self.end_word(command);
        let next = self.chars.get(self.at);
        if listed && next.is_some_and(|c| !c.is_whitespace() && !";&|()<>".contains(*c)) {
            command.joined.push(word);
            command.open(false);
        }
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
                    Some(c @ ('"' | '\\' | '$' | '`')) => {
                        self.held();
                        command.push(c);
                    }
                    Some(c) => {
                        self.held();
                        command.push('\\');
                        command.push(c);
                    }
                },
                '`' => self.backquote(),
                '$' if self.eat('(') => self.child(|lexer| lexer.list(Some(')'))),
                c => {
                    self.held();
                    command.push(c);
                }
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
        self.child(|lexer| lexer.nested(&inner));
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
            // A digit straight after an array's `)` is zsh's descriptor
            // and bash's more of the assignment: neither runs it.
            if command
                .joined
                .last()
                .is_some_and(|at| at + 1 == command.words.len())
            {
                command.joined.pop();
            }
        }
        self.end_word(command);
        command.redirected = true;
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
            Next::Target { writes: true } => command.targets.push(self.assigned.target(&word.text)),
            Next::Target { writes: false } => {}
            Next::Delimiter { strip_tabs } => self.heredocs.push(Heredoc {
                delimiter: word.text,
                strip_tabs,
                expands: !word.quoted,
            }),
        }
    }

    /// The command read so far, ended by `after`. Where it has no words,
    /// a `;` or a newline leaves the separator before it standing, so a
    /// line continued after `&&` or `|`, or opening a subshell after one,
    /// joins what follows as that separator does ([`Joint`]). Where it
    /// runs no program, an assignment or a redirection alone, the `&&` or
    /// `|` after it joins what follows to the chain it stands in, so after
    /// a command and a `;`, `||` or `&` it is joined as by that separator:
    /// `cd /wt; P=x && git add` runs `git add` wherever the `cd` left the
    /// shell (#3165).
    ///
    /// Where word text follows an array's `)` straight after, `P=(a)x git
    /// commit`, the command is read as zsh runs it, `x` the program, and
    /// then as bash runs it, `P=(a)x` an assignment and `git` the program,
    /// both joined as the command stands (#3198). What the line assigns
    /// is zsh's reading's, as before: bash's would replace a program the
    /// line runs by `$P` with a word no rule names, `P=(git)Q=1; $P
    /// commit` running `gitQ=1`.
    fn end(&mut self, command: &mut Command, after: Joint) {
        self.end_word(command);
        command.next = Next::Argument;
        let words = std::mem::take(&mut command.words);
        let redirected = std::mem::take(&mut command.redirected);
        let joined = std::mem::take(&mut command.joined);
        let targets = std::mem::take(&mut command.targets);
        let mut bash = words.clone();
        for &at in joined.iter().rev() {
            if at + 1 < bash.len() {
                let rest = bash.remove(at + 1);
                bash[at].push_str(&rest);
            }
        }
        let before = self.before;
        self.run(words, redirected, targets, after);
        if !joined.is_empty() {
            let zsh = std::mem::replace(&mut self.before, before);
            let assigned = self.assigned.0.clone();
            self.run(bash, redirected, Vec::new(), after);
            self.assigned.0 = assigned;
            self.before = zsh;
        }
    }

    /// One reading of a command's words, ended by `after`, with the files
    /// its redirections write, which stand on the program it runs, or on
    /// none where it runs none.
    fn run(&mut self, words: Vec<String>, redirected: bool, targets: Vec<String>, after: Joint) {
        self.assigned.read(&words);
        let empty = words.is_empty() && !redirected;
        let argv = Argv::running(words, &self.assigned);
        let command = argv.as_ref().map(|_| self.commands.len());
        let targets = targets
            .into_iter()
            .map(|target| Redirect { target, command });
        self.redirects.extend(targets);
        let Some(argv) = argv else {
            let begins = !self.commands.is_empty()
                && matches!(self.before, Joint::Then | Joint::Or | Joint::Background);
            self.before = match after {
                Joint::Then | Joint::Or if empty => self.before,
                Joint::And | Joint::Pipe if !empty && begins => Joint::Then,
                _ => after,
            };
            return;
        };
        let nested = argv.nested_line();
        let eval = argv.program() == "eval";
        self.commands.push(argv);
        self.shells.push(self.open.clone());
        let before = self.before;
        self.joints.push(Joints { before, after });
        match nested {
            Some(line) if eval => self.nested(&line),
            Some(line) => self.child(|lexer| lexer.nested(&line)),
            None => {}
        }
        self.before = after;
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
                let mut read = Lexer::new(&line);
                if heredoc.expands {
                    read.expand(&mut Command::default(), None);
                }
                match body {
                    Body::Text if read.commands.is_empty() => {
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
                self.adopt(&mut read);
            }
            if body == Body::Commands {
                self.child(|lexer| lexer.nested(&stdin));
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
/// `cd` or `pushd` ([`Argv::directory`]) or a program's own option
/// ([`Move`]) names by it. An
/// assignment in front of a program is that program's environment and
/// not the shell's, which expands the line before it assigns, so it is
/// none of these. One made in a subshell or a substitution is read as
/// the line's: the reading is then stricter than the shell, never looser.
#[derive(Default)]
struct Assigned(BTreeMap<String, String>);

impl Assigned {
    /// The assignments a command's words make, where they are the whole
    /// command or a declaring builtin's operands.
    fn read(&mut self, words: &[String]) {
        let operands = match words.split_first() {
            Some((first, rest)) if DECLARING.contains(&first.as_str()) => rest,
            _ if words.iter().all(|w| is_assignment(w)) => words,
            _ => return,
        };
        for (set, value) in operands.iter().filter_map(|w| w.split_once('=')) {
            let Some((name, append)) = assigns(set) else {
                continue;
            };
            // A value led by a variable the line assigned is that value,
            // as the shell expands it: `S=/x; R=$S/repo` names `/x/repo`
            // (#3625).
            let led = self.directory(value);
            let value = led.as_deref().unwrap_or(value);
            let prior = self.0.get(name).filter(|_| append);
            let value = format!("{}{value}", prior.map_or("", String::as_str));
            self.0.insert(name.to_owned(), value);
        }
    }

    /// The words `$X`, `${X}` or an array's `${X[@]}` or `${X[*]}` stands
    /// for, split as an unquoted expansion is; `None` for any other word,
    /// or a variable the line never assigned.
    fn expand(&self, word: &str) -> Option<Vec<String>> {
        let name = word.strip_prefix('$')?;
        let name = name
            .strip_prefix('{')
            .and_then(|n| n.strip_suffix('}'))
            .unwrap_or(name);
        let name = ["[@]", "[*]"]
            .iter()
            .find_map(|all| name.strip_suffix(all))
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

    /// A word as the shell expands it as far as the line says (#4250): led
    /// by a variable the line assigned, its value ([`Assigned::directory`]),
    /// and led by `~/`, the home; any other word as it is spelled, one led
    /// by a variable the line never assigned among them, which says no
    /// place.
    fn target(&self, word: &str) -> String {
        if let Some(expanded) = self.directory(word) {
            return expanded;
        }
        match (word.strip_prefix("~/"), std::env::var("HOME")) {
            (Some(rest), Ok(home)) => format!("{home}/{rest}"),
            _ => word.to_owned(),
        }
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
        // The option, and its value where it takes one.
        at += 1 + usize::from(CARGO_VALUED.contains(&word.as_str()));
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
const RESERVED: [&str; 11] = [
    "!", "{", "}", "if", "then", "elif", "else", "do", "done", "while", "until",
];

/// Programs that run the rest of their words as a command, with the
/// options each reads a value after and the operands it reads before the
/// command: `timeout`'s duration (#2668), and `caffeinate`'s, which keeps
/// the machine awake while its command runs (#2990), and `xargs`'s, whose
/// command may run a line a quote holds, `xargs sh -c '…'` (#3635).
const WRAPPERS: [(&str, &[&str], usize); 10] = [
    ("xargs", &XARGS_VALUED, 0),
    ("command", &[], 0),
    ("builtin", &[], 0),
    ("exec", &["-a"], 0),
    ("nohup", &[], 0),
    ("time", &[], 0),
    ("env", &["-u", "--unset", "-C", "--chdir", "-S"], 0),
    ("nice", &["-n"], 0),
    ("timeout", &["-s", "--signal", "-k", "--kill-after"], 1),
    ("caffeinate", &["-t", "-w"], 0),
];

/// `xargs`'s options, BSD's and GNU's, that read a value after them.
const XARGS_VALUED: [&str; 11] = [
    "-E", "-I", "-J", "-L", "-n", "-P", "-R", "-S", "-s", "-a", "-d",
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
    word.split_once('=')
        .is_some_and(|(set, _)| assigns(set).is_some())
}

/// The name the left side of an assignment sets, `NAME` or `NAME+`, and
/// whether it appends to the value; `None` where it names no variable.
fn assigns(set: &str) -> Option<(&str, bool)> {
    let (name, append) = set.strip_suffix('+').map_or((set, false), |n| (n, true));
    is_name(name).then_some((name, append))
}

/// A shell variable's name: a letter or `_`, then letters, digits and `_`.
fn is_name(name: &str) -> bool {
    name.chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The options of the programs named that move one into a directory
/// ([`Move::Into`]), name one it reaches besides ([`Move::Reaches`]) or
/// its repository ([`Move::GitDir`]), or
/// take a value and move nothing (`None`), each as [`valued`] reads it;
/// git's stand before its subcommand, so a `-C` after it is the
/// subcommand's own (`git commit -C HEAD`).
const MOVING: [(&[&str], &str, Moving); 12] = [
    (&["env"], "-C", Some(Move::Into)),
    (&["env"], "--chdir", Some(Move::Into)),
    (&["git"], "-C", Some(Move::Into)),
    (&["git"], "--git-dir", Some(Move::GitDir)),
    (&["git"], "--work-tree", Some(Move::Reaches)),
    (&["git"], "-c", None),
    (&["git"], "--namespace", None),
    (&["git"], "--config-env", None),
    (&["patch"], "-d", Some(Move::Into)),
    (&["patch"], "--directory", Some(Move::Into)),
    (TARGETS, "-t", Some(Move::Reaches)),
    (TARGETS, "--target-directory", Some(Move::Reaches)),
];

/// The move an option of [`MOVING`] makes of its value, or none.
type Moving = Option<fn(String) -> Move>;

/// The programs whose `-t` names the directory they write into.
const TARGETS: &[&str] = &["cp", "mv", "install", "ln"];

/// The value `word` gives `option`: empty where it is the option alone,
/// or a cluster of short options ending in it (`-iC`), its value then
/// being the next word; the rest of the word where the value is attached
/// (`-C/wt`, `--chdir=/wt`); `None` where the word is not the option.
fn valued<'w>(word: &'w str, option: &str) -> Option<&'w str> {
    if option.starts_with("--") {
        let rest = word.strip_prefix(option)?;
        return (rest.is_empty() || rest.starts_with('=')).then(|| rest.trim_start_matches('='));
    }
    let cluster = word.strip_prefix('-').filter(|c| !c.starts_with('-'))?;
    cluster
        .split_once(option.trim_start_matches('-'))
        .map(|(_, value)| value)
}

impl Move {
    /// The moves `program`'s own words make, in order: its options
    /// ([`MOVING`]), `GIT_DIR` or `GIT_WORK_TREE` the line assigned for
    /// git, each of `ln`'s operands, a link through which a later write
    /// lands there, and `find -execdir`'s, which its words do not say.
    fn of(program: &str, words: &[String], assigned: &Assigned) -> Vec<Move> {
        let set = assigned.0.iter().filter(|_| program == "git");
        let mut moves: Vec<Move> = set
            .filter_map(|(name, value)| Move::git_tree(name, value, assigned))
            .collect();
        let mut words = words.iter().map(String::as_str);
        while let Some(word) = words.next() {
            let mut own = MOVING
                .iter()
                .filter(|(programs, ..)| programs.contains(&program));
            let option = own.find_map(|(_, option, make)| Some((valued(word, option)?, make)));
            if let Some((value, make)) = option {
                let value = Some(value)
                    .filter(|v| !v.is_empty())
                    .or_else(|| words.next());
                let to = make
                    .zip(value)
                    .map(|(make, value)| Move::to(value, make, assigned));
                moves.extend(to);
            } else if program == "git" && !word.starts_with('-') {
                break;
            } else if program == "ln" && !word.starts_with('-') {
                moves.push(Move::to(word, Move::Reaches, assigned));
            } else if program == "find" && matches!(word, "-execdir" | "-okdir") {
                moves.push(Move::Unsaid);
            }
        }
        moves
    }

    /// `make`'s move to the directory `word` names, a variable the line
    /// assigned read as its value; one it did not is [`Move::Unsaid`].
    fn to(word: &str, make: fn(String) -> Move, assigned: &Assigned) -> Move {
        let plain = (!word.starts_with('$')).then(|| word.to_owned());
        assigned
            .directory(word)
            .or(plain)
            .map_or(Move::Unsaid, make)
    }

    /// The tree git reaches by an assignment of `value` to `name`: its
    /// repository for `GIT_DIR`, a place it reaches for `GIT_WORK_TREE`.
    fn git_tree(name: &str, value: &str, assigned: &Assigned) -> Option<Move> {
        let make: fn(String) -> Move = match name {
            "GIT_DIR" => Move::GitDir,
            "GIT_WORK_TREE" => Move::Reaches,
            _ => return None,
        };
        Some(Move::to(value, make, assigned))
    }
}

impl Argv {
    /// The argv of the program a command's words run, past its leading
    /// assignments, reserved words and wrappers, with a variable the line
    /// [`Assigned`] read as its words where it stands as the program, and
    /// xtask's argv, its name and the words it receives, however it is run
    /// ([`xtask_own`]); `None` when they run none. What moves the program
    /// is read as it is peeled ([`Move`]): `GIT_DIR` or `GIT_WORK_TREE`
    /// among the assignments, `env`'s `-C`, and the program's own options.
    fn running(mut words: Vec<String>, assigned: &Assigned) -> Option<Argv> {
        let mut at = 0;
        // The shell expands a word once: what `$X` stands for is never
        // expanded again, so `X='$X'; $X` runs the program `$X`.
        let mut expanded = false;
        let mut moves = Vec::new();
        loop {
            let word = words.get(at)?;
            // A `for` loop's head, `for n in a b`, names a variable and the
            // words it takes, and runs nothing; one the shell reads as a
            // reserved word stands first or after another (#4162).
            let reserved = |w: &String| RESERVED.contains(&w.as_str());
            if word == "for" && !expanded && words[..at].iter().all(reserved) {
                return None;
            }
            if reserved(word) || is_assignment(word) {
                // `GIT_DIR+=…` extends a value the line may not say.
                let set = word.split_once('=');
                let set = set.and_then(|(set, value)| Some((assigns(set)?, value)));
                moves.extend(set.and_then(|((name, append), value)| {
                    let tree = Move::git_tree(name, value, assigned)?;
                    Some(if append { Move::Unsaid } else { tree })
                }));
                at += 1;
                continue;
            }
            if let Some(value) = assigned.expand(word).filter(|_| !expanded) {
                words.splice(at..=at, value);
                expanded = true;
                continue;
            }
            let Some((name, valued, operands)) = WRAPPERS.iter().find(|w| w.0 == basename(word))
            else {
                break;
            };
            let options = at + 1;
            at = options;
            while let Some(option) = words.get(at).filter(|w| w.starts_with('-')) {
                let takes = valued.iter().any(|v| self::valued(option, v) == Some(""));
                at += 1 + usize::from(takes);
            }
            let own = words.get(options..at).unwrap_or_default();
            moves.extend(Move::of(name, own, assigned));
            at += operands;
        }
        words.drain(..at);
        if let Some(own) = xtask_own(&words) {
            words.splice(..own, ["xtask".to_owned()]);
        }
        let (program, rest) = words.split_first_mut()?;
        let program = basename(program);
        if matches!(program, "cd" | "pushd")
            && let Some(word) = rest.iter_mut().find(|w| !w.starts_with('-'))
            && let Some(dir) = assigned.directory(word)
        {
            *word = dir;
        }
        moves.extend(Move::of(program, rest, assigned));
        let named = rest.iter().map(|w| assigned.target(w)).collect();
        Some(Argv {
            words,
            moves,
            named,
        })
    }

    /// The directory a `cd` or `pushd` names to move into: its first word
    /// past its options, a variable the line assigned read as its value
    /// ([`Assigned`]); one it did not stays as written, `$X`.
    pub(super) fn directory(&self) -> Option<&str> {
        let (change, rest) = self.words.split_first()?;
        let operand = rest.iter().find(|w| !w.starts_with('-'));
        operand
            .filter(|_| matches!(change.as_str(), "cd" | "pushd"))
            .map(String::as_str)
    }

    /// Where the program runs or reaches apart from its shell, in order
    /// ([`Move`]).
    pub(super) fn moves(&self) -> &[Move] {
        &self.moves
    }

    /// The line this command runs in turn: a shell's `-c` argument, or the
    /// words `eval` joins.
    fn nested_line(&self) -> Option<String> {
        let (program, args) = self.words.split_first()?;
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
        let Some((program, args)) = self.words.split_first() else {
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
        &self.words
    }

    /// The words past the program as the shell expands them as far as the
    /// line says, a variable the line assigned read as its value and `~/`
    /// as the home ([`Assigned::target`]), one for each of
    /// [`Argv::words`] past the first: the names a program the owner lock
    /// places by what it names lands on (#4250).
    pub(super) fn named(&self) -> &[String] {
        &self.named
    }

    /// The program's name without its directory; empty for no words.
    pub(super) fn program(&self) -> &str {
        self.words.first().map_or("", |program| basename(program))
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

    /// The words after `phrase` where it names this command, its own
    /// options and operands ([`Argv::phrase_end`]); `None` where it does not.
    pub(super) fn after(&self, phrase: &str) -> Option<&[String]> {
        self.phrase_end(phrase).map(|end| &self.words[end..])
    }

    /// Whether the command asks for its help, `-h` or `--help` among its
    /// words before a `--`, and so runs nothing else.
    pub(super) fn asks_help(&self) -> bool {
        asks_help(self.words.get(1..).unwrap_or_default())
    }

    /// Whether one of the phrases names this command; none names every one.
    pub(super) fn invokes(&self, with: &OneOrMany) -> bool {
        self.invocations(with).next().is_some()
    }

    /// `deny_argv` with `with`: one of the phrases names this command and
    /// one of the denied elements stands in it (see [`Element`]), save an
    /// element `unless` excuses here: the phrase its excuse names runs,
    /// bounded by the excuse's option as [`Argv::unbounded`] reads one,
    /// `xtask phpt --jit force --subset Zend/tests` (#3068).
    pub(super) fn denies(
        &self,
        denied: &[String],
        with: &OneOrMany,
        unless: Option<&BTreeMap<String, String>>,
    ) -> bool {
        let excused = |element: &String| {
            unless
                .and_then(|u| u.get(element))
                .and_then(|e| e.rsplit_once(' '))
                .is_some_and(|(phrase, option)| {
                    self.runs(phrase) && !self.unbounded(option, &OneOrMany::One(phrase.to_owned()))
                })
        };
        self.invocations(with).any(|invocation| {
            denied
                .iter()
                .filter(|d| !excused(d))
                .filter_map(|d| Element::parse(d))
                .any(|element| invocation.carries(&element))
        })
    }

    /// `require_argv` with `with`: one of the phrases names this command
    /// and its own words hold no `separator`, or nothing after it, or
    /// after it a name for the tree rather than for files (`.`, `:/`, a
    /// `*` the shell left unexpanded). The separator is `--` for git, after
    /// which every word is a path, so `git add -A -- src` is bounded to
    /// `src` and passes; an option separator bounds the one value it
    /// takes, after `=` or as its next word, `--subset=ext/pcre`, so a
    /// substitution further along the command, `--against $(git
    /// merge-base HEAD main)`, is no path of it (#3619). A command that asks for its help (`-h`,
    /// `--help` among its options) runs nothing and is never unbounded.
    pub(super) fn unbounded(&self, separator: &str, with: &OneOrMany) -> bool {
        self.invocations(with).any(|invocation| {
            let own = invocation.own();
            if asks_help(own) {
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
            let values = if separator == "--" { usize::MAX } else { 1 };
            let paths: Vec<String> = joined(&own[at])
                .into_iter()
                .chain(own[at + 1..].iter().take(values).cloned())
                .take(values)
                .collect();
            paths.is_empty() || paths.iter().any(|p| names_the_tree(p))
        })
    }

    /// Where each phrase that names this command stands in it.
    fn invocations<'a>(&'a self, with: &'a OneOrMany) -> impl Iterator<Item = Invocation<'a>> {
        let every = with.is_empty().then_some(1);
        let named = with.iter().filter_map(|phrase| self.phrase_end(phrase));
        every.into_iter().chain(named).map(|own| Invocation {
            words: &self.words,
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
        let program = basename(self.words.first()?);
        match words.as_slice() {
            [first, rest @ ..] if *first == program => follows(&self.words, rest),
            [subcommand] => follows(&self.words, &[subcommand]),
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

/// Whether `-h` or `--help` stands among `words` before a `--`.
fn asks_help(words: &[String]) -> bool {
    let options = words.iter().take_while(|w| *w != "--");
    options.into_iter().any(|w| w == "-h" || w == "--help")
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
        commands(line).into_iter().map(|argv| argv.words).collect()
    }

    /// #2764: each command names the shells it runs in, a sibling
    /// subshell its own, `eval` the shell that runs it and a shell's `-c`
    /// a child of it.
    #[test]
    fn each_command_names_the_subshells_it_runs_in() {
        let line = Line::read("(cd a; x $(y)); (cd b); eval cd c; bash -c 'cd d; z'; w");
        let (none, first, inner, second, child) = (vec![], vec![0], vec![0, 1], vec![2], vec![3]);
        let expected = [
            &first, &inner, &first, &second, &none, &none, &none, &child, &child, &none,
        ];
        assert_eq!(line.shells().iter().collect::<Vec<_>>(), expected);
        assert_eq!(line.shells().len(), line.commands().len());
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

    /// #3635: a quoted argument is read as words and never as a command
    /// position, and a line a shell runs is read where it runs.
    #[test]
    fn a_lines_subject_reads_no_quote_as_a_command_position() {
        let subject = |line: &str| Line::read(line).subject().to_owned();
        assert_eq!(
            subject("printf '%s\\n' 'a && git merge x\ngit merge y' > f; ls"),
            "printf '%s\\n' 'a    git merge x git merge y' > f; ls"
        );
        assert_eq!(subject("echo \"a;b\" a\\;b"), "echo \"a b\" a\\ b");
        // A line a shell runs is read again after the line, a line of its own.
        assert_eq!(
            subject("sh -c 'cd x; git merge y'"),
            "sh -c 'cd x  git merge y'\ncd x; git merge y"
        );
        // A heredoc body that is text is no more read there than here.
        assert_eq!(subject("cat <<'EOF'\ngit merge y\nEOF"), "cat <<'EOF'\nEOF");
        let merge = Regex::new(r"(?m)(\A|[|;&(\n]\s*)git\s+merge").unwrap();
        let hits = |line: &str| Line::read(line).hits(&merge).len();
        assert_eq!(hits("echo 'x\ngit merge y'"), 0);
        assert_eq!(hits("echo '(git merge y'"), 0);
        assert_eq!(hits("echo 'x' && git merge y"), 1);
        assert_eq!(hits("bash -c 'ls; git merge y'"), 1);
        // A hit passed over in a quote hides none after it.
        let redirect = Regex::new(r">\s*seams\.toml").unwrap();
        let hits = Line::read("echo '> seams.toml' > seams.toml").hits(&redirect);
        assert_eq!(hits, vec![20..32]);
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
        // #4162: a `for` loop's head and its `done` run nothing; what its
        // list substitutes, and its body, run.
        assert_eq!(
            words("for n in a $(git stash); do gh issue view $n; done"),
            vec![vec!["git", "stash"], vec!["gh", "issue", "view", "$n"]]
        );
        assert_eq!(words("xargs for n"), vec![vec!["for", "n"]]);
        assert_eq!(words("A=1"), Vec::<Vec<String>>::new());
        assert_eq!(
            words("CARGO_BUILD_JOBS=2 caffeinate -i git merge main"),
            vec![vec!["git", "merge", "main"]]
        );
        assert_eq!(
            words("caffeinate -t 60 -i git commit"),
            vec![vec!["git", "commit"]]
        );
    }

    /// #2607: the directory a `cd` or `pushd` names by a variable the
    /// line assigned earlier is the variable's value, whole; one the line
    /// did not assign, or assigned only in the command's own environment,
    /// stays as written.
    #[test]
    fn a_directory_named_by_a_variable_the_line_assigned_is_its_value() {
        let dirs = |line: &str| -> Vec<String> {
            let commands = commands(line);
            let dirs = commands.iter().filter_map(Argv::directory);
            dirs.map(str::to_owned).collect()
        };
        assert_eq!(dirs("W='/a b' && cd \"$W\""), ["/a b"]);
        assert_eq!(dirs("export W=/a; cd ${W}/sub"), ["/a/sub"]);
        assert_eq!(dirs("W=/a; pushd -q $W"), ["/a"]);
        assert_eq!(dirs("cd \"$W\""), ["$W"]);
        assert_eq!(dirs("W=/a cd $W"), ["$W"]);
        assert_eq!(dirs("cd -"), Vec::<String>::new());
        assert_eq!(dirs("touch $W"), Vec::<String>::new());
    }

    /// #2995: what moves a program apart from its shell is read as it is
    /// peeled: `env -C`, git's own options before its subcommand and the
    /// `GIT_DIR` or `GIT_WORK_TREE` it runs under, `patch -d`, a target
    /// directory, the directories `ln` names, and `find -execdir` or a
    /// variable the line did not assign as somewhere unsaid.
    #[test]
    fn what_moves_a_program_is_read_as_it_is_peeled() {
        let moves = |line: &str| -> Vec<Vec<Move>> {
            commands(line).into_iter().map(|argv| argv.moves).collect()
        };
        let (into, reaches, git_dir) = (
            |d: &str| Move::Into(d.into()),
            |d: &str| Move::Reaches(d.into()),
            |d: &str| Move::GitDir(d.into()),
        );
        assert_eq!(moves("env -C /wt git add x"), [[into("/wt")]]);
        assert_eq!(
            moves("env --chdir=/wt A=1 git -C sub --git-dir=.git add -C x"),
            [[into("/wt"), into("sub"), git_dir(".git")]]
        );
        assert_eq!(
            moves("W=/a && git -c x=y -C $W -C sub commit -C HEAD"),
            [[into("/a"), into("sub")]]
        );
        assert_eq!(moves("GIT_WORK_TREE=/wt git checkout"), [[reaches("/wt")]]);
        assert_eq!(moves("GIT_DIR+=/x git status"), [[Move::Unsaid]]);
        assert_eq!(
            moves("export GIT_DIR=/wt/.git; git status"),
            [vec![], vec![git_dir("/wt/.git")]]
        );
        assert_eq!(
            moves("patch -d /wt -p1 < f; cp -t /wt a; ln -sf -- /m/seams.toml ."),
            [
                vec![into("/wt")],
                vec![reaches("/wt")],
                vec![reaches("/m/seams.toml"), reaches(".")]
            ]
        );
        // A short option in a cluster, or with its value attached.
        assert_eq!(
            moves("env -iC /wt ls; patch -d/wt; cp -vt /wt a"),
            [[into("/wt")], [into("/wt")], [reaches("/wt")]]
        );
        assert_eq!(
            moves("find . -execdir rm {} +; env -C \"$X\" ls"),
            [[Move::Unsaid], [Move::Unsaid]]
        );
    }

    /// #2995: each command records the separators either side of it; a
    /// line continued after `&&`, a subshell's first command and a
    /// substitution's are joined as the separator before them.
    #[test]
    fn each_command_is_joined_by_the_separators_either_side_of_it() {
        use Joint::{And, Background, Or, Pipe, Then};
        let joints = |line: &str| -> Vec<(Joint, Joint)> {
            let line = Line::read(line);
            line.joints().iter().map(|j| (j.before, j.after)).collect()
        };
        assert_eq!(
            joints("a && b || c | d & e; f\ng"),
            [
                (Then, And),
                (And, Or),
                (Or, Pipe),
                (Pipe, Background),
                (Background, Then),
                (Then, Then),
                (Then, Then)
            ]
        );
        assert_eq!(
            joints("a &&\n(b; c) | d $(e)"),
            [
                (Then, And),
                (And, Then),
                (Then, Then),
                (Pipe, Then),
                (Pipe, Then)
            ]
        );
        let line = Line::read("x && bash -c 'cd /wt; y'");
        assert_eq!(line.joints().len(), line.commands().len());
        assert_eq!(line.joints()[1].before, And);
    }

    /// #3165: a `(` straight after `NAME=` or `NAME+=` where an
    /// assignment stands opens an array, its elements words of the
    /// assignment, a parenthesis, a quoted or escaped `)`, a comment and a
    /// separator among them alike, so the line reads as it does with a
    /// scalar; a `$(…)` among them runs, the word ends at the `)`, and a
    /// `(` anywhere else opens a subshell.
    #[test]
    fn an_array_assignment_is_one_word_and_opens_no_subshell() {
        use Joint::{And, Then};
        type Read = (Vec<Vec<String>>, Vec<Vec<u32>>, Vec<(Joint, Joint)>);
        let read = |line: &str| -> Read {
            let line = Line::read(line);
            let words = line.commands().iter().map(|a| a.words.clone());
            let joints = line.joints().iter().map(|j| (j.before, j.after));
            (words.collect(), line.shells().to_vec(), joints.collect())
        };
        let (words_of, shells, joints) = read("P=(a b) && git add -- \"${P[@]}\"");
        assert_eq!(words_of, [["git", "add", "--", "${P[@]}"]]);
        assert_eq!((shells, joints), (vec![vec![]], vec![(And, Then)]));
        for (array, scalar) in [
            ("P=(a (x) \")\" b\\) # c)\n d) && git add", "P=a && git add"),
            ("A=1 P+=(x) && git add", "A=1 P+=x && git add"),
            ("P=() && git add", "P= && git add"),
            (
                "typeset -a P=(x; y) && git add",
                "typeset -a P='x y' && git add",
            ),
        ] {
            assert_eq!(read(array), read(scalar), "{array}");
        }
        assert_eq!(
            words("P=(a $(git rev-parse HEAD)) && git add"),
            [vec!["git", "rev-parse", "HEAD"], vec!["git", "add"]]
        );
        for substituted in ["<(git add)", ">(git add)", "=(git add)"] {
            let line = format!("P=(a {substituted}) && x");
            assert_eq!(words(&line), [vec!["git", "add"], vec!["x"]], "{line}");
        }
        assert_eq!(
            words("cat <<EOF && P=(x\nEOF\n)\ngit commit -n\nEOF"),
            [vec!["cat"], vec!["git", "commit", "-n"], vec!["EOF"]]
        );
        assert_eq!(
            words("P=(git commit -n); \"${P[@]}\"; ${P[*]}"),
            [["git", "commit", "-n"], ["git", "commit", "-n"]]
        );
        assert_eq!(words("P=(a)b"), [["b"]]);
        assert_eq!(
            words("P=(git commit); P+=(-n); $P"),
            [["git", "commit", "-n"]]
        );
        for chained in [
            "cd /wt; P=(a) && git add",
            "cd /wt; P=a | git add",
            "cd /wt; >x && git add",
        ] {
            assert_eq!(read(chained).2, [(Then, Then), (Then, Then)], "{chained}");
        }
        assert_eq!(
            read("cd /wt && P=a && git add").2,
            [(Then, And), (And, Then)]
        );
        for subshell in ["P= (git add)", "\"P\"=(git add)", "echo P=(git add)"] {
            assert!(read(subshell).1.contains(&vec![0]), "{subshell}");
        }
    }

    /// #3198: word text straight after an array's `)` is zsh's next word
    /// and bash's more of the assignment, so the command is read both
    /// ways, each joined as it stands; a space after the `)`, or nothing
    /// after the text, reads as before.
    #[test]
    fn word_text_after_an_array_is_read_as_zsh_and_as_bash_run_it() {
        use Joint::{And, Then};
        let line = Line::read("cd /wt && P=(a)x git commit -n; y");
        let read: Vec<_> = line.commands().iter().map(|a| a.words.clone()).collect();
        assert_eq!(
            read,
            [
                vec!["cd", "/wt"],
                vec!["x", "git", "commit", "-n"],
                vec!["git", "commit", "-n"],
                vec!["y"]
            ]
        );
        let joints: Vec<_> = line.joints().iter().map(|j| (j.before, j.after)).collect();
        assert_eq!(
            joints,
            [(Then, And), (And, Then), (And, Then), (Then, Then)]
        );
        assert!(names("P=(a)x git commit", "git commit"));
        assert!(names("P=(a)x git commit", "x"));
        assert_eq!(
            words("P=(a)\"x\"#y git add; P=(a)Q=(b)$(z)y git add"),
            [
                vec!["x#y", "git", "add"],
                vec!["git", "add"],
                vec!["z"],
                vec!["y", "git", "add"],
                vec!["git", "add"]
            ]
        );
        assert_eq!(words("P=(a) git commit"), [["git", "commit"]]);
        assert_eq!(words("P=(a)b"), [["b"]]);
        assert_eq!(words("P=(a)>f git commit"), [["git", "commit"]]);
        assert_eq!(words("P=(a)2>f git commit"), [["git", "commit"]]);
        assert_eq!(words("P=(git)Q=1; $P commit"), [["git", "commit"]]);
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

    /// `lanes.nightly-flags` with the excuse #3052 ruled, `{ "--jit" =
    /// "xtask phpt --subset" }`: the tier's lane passes on `xtask phpt`
    /// bounded by one subset, before or after `--jit`, and on nothing else
    /// (#3068).
    #[test]
    fn a_subset_excuses_the_tier_s_lane_on_phpt_alone() {
        let lane = ["--full".to_owned(), "--jit".to_owned()];
        let with = OneOrMany::Many(vec!["xtask phpt".to_owned(), "xtask bench".to_owned()]);
        let unless = BTreeMap::from([("--jit".to_owned(), "xtask phpt --subset".to_owned())]);
        let denied = |line: &str| {
            commands(line)
                .iter()
                .any(|a| a.denies(&lane, &with, Some(&unless)))
        };
        for line in [
            "cargo xtask phpt --jit force --full",
            "cargo xtask phpt --jit force",
            "cargo xtask phpt --jit=exit-fuzz --subset",
            "cargo xtask phpt --subset . --jit force",
            "cargo xtask phpt --full --subset Zend/tests",
            "cargo xtask bench --jit force --subset Zend/tests",
            "cargo xtask phpt --subset Zend/tests; cargo xtask phpt --jit force",
        ] {
            assert!(denied(line), "{line}");
        }
        for line in [
            "cargo xtask phpt --jobs 3 --jit force --subset Zend/tests --engine target/featured/jit/release-da/php",
            "cargo run -q -p xtask -- phpt --subset=Zend/tests --jit exit-fuzz",
            "target/release/xtask phpt --jit=force --subset tests",
        ] {
            assert!(!denied(line), "{line}");
        }
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
            .any(|argv| argv.denies(&denied, &with, None))
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
