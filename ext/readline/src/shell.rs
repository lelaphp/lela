//! `readline_cli.c`: the CLI's interactive shell, `php -a`, which the
//! extension provides and a host that carries php's shell callbacks
//! serves ([`php_ext::abi::Shell`]). The loop reads a line with the
//! prompt `cli.prompt` renders, gathers lines until the code they make
//! could end, and serves it as `zend_eval_stringl` does; a line of the
//! form `#name=value` sets an ini entry. What the request writes is
//! seen first by [`write`], which keeps the last byte for the line end
//! the loop adds, what the prompt's code prints, and what `cli.pager`
//! pipes to.

use std::io::{BufWriter, Write as _};
use std::process::{ChildStdin, Command, Stdio};

use php_ext::call::{autoload, call, lookup_function};
use php_ext::coerce::to_callable;
use php_ext::raising::{Level, clear, leave_pending, uncaught_at, warning};
use php_ext::request::Request;
use php_ext::values::{Key, PhpArray, PhpString, Value, walk};

use crate::ffi::{self, Completion, Matches};

/// `cli.pager`: the command the request's bytes are piped to, beside
/// the terminal.
pub(crate) const PAGER: &str = "cli.pager";
/// `cli.prompt`: the prompt's template.
pub(crate) const PROMPT: &str = "cli.prompt";
/// `DEFAULT_PROMPT`.
pub(crate) const DEFAULT_PROMPT: &[u8] = b"\\b \\> ";

/// What the shell keeps while the request runs: `php_last_char`, the
/// bytes the prompt's code writes while it runs (`CLIR_G(prompt_str)`), and the
/// pager's input (`pager_pipe`), buffered as php's `FILE` is.
#[derive(Default)]
pub(crate) struct State {
    last: u8,
    capture: Option<Vec<u8>>,
    pager: Option<BufWriter<ChildStdin>>,
}

fn state(req: &mut Request) -> &mut State {
    &mut req.state::<crate::State>().shell
}

/// `readline_shell_ub_write` then `readline_shell_write`: the last byte
/// kept; the bytes added to the prompt being rendered, or written to the
/// pager, started on the first write that finds `cli.pager` set, at most
/// 16 KiB of them as php's `fwrite` takes.
pub(crate) fn write(req: &mut Request, bytes: &[u8]) {
    let pager = req.ini_get(PAGER.as_bytes()).unwrap_or_default().to_vec();
    let state = state(req);
    if let Some(&last) = bytes.last() {
        state.last = last;
    }
    if let Some(capture) = &mut state.capture {
        capture.extend_from_slice(bytes);
        return;
    }
    if !pager.is_empty() && state.pager.is_none() {
        // `popen(pager, "w")`: `/bin/sh -c`, its input a pipe.
        state.pager = Command::new("/bin/sh")
            .arg("-c")
            .arg(String::from_utf8_lossy(&pager).as_ref())
            .stdin(Stdio::piped())
            .spawn()
            .ok()
            .and_then(|mut child| child.stdin.take())
            .map(BufWriter::new);
    }
    if let Some(pager) = &mut state.pager {
        pager.write_all(&bytes[..bytes.len().min(16384)]).ok();
    }
}

/// `fclose(pager_pipe)`, as the loop closes it: the buffer flushed and
/// the pipe closed, the pager left to finish on its own.
pub(crate) fn close_pager(req: &mut Request) {
    if let Some(mut pager) = state(req).pager.take() {
        pager.flush().ok();
    }
}

/// `cli_get_prompt`: `cli.prompt` rendered, `\b` the block, `\>` the
/// prompt character, `\v` the version, `\e` escape, `\n`, `\t`, `` \` ``
/// and `\\`, and code between backticks served for what it prints; a byte
/// above 0x7F is `?`, with php's warning once.
fn prompt(req: &mut Request, block: &[u8], prompt: u8) -> Vec<u8> {
    let spec = match req.ini_get(PROMPT.as_bytes()) {
        Some(spec) => spec.to_vec(),
        None => DEFAULT_PROMPT.to_vec(),
    };
    let mut out = Vec::new();
    let mut warned = false;
    let mut at = 0;
    while at < spec.len() {
        let byte = spec[at];
        if byte == b'\\' {
            let next = spec.get(at + 1).copied();
            let taken: Option<&[u8]> = match next {
                Some(b'\\') => Some(b"\\"),
                Some(b'n') => Some(b"\n"),
                Some(b't') => Some(b"\t"),
                Some(b'e') => Some(b"\x1b"),
                Some(b'v') => Some(php_ext::request::PHP_VERSION.as_bytes()),
                Some(b'b') => Some(block),
                Some(b'>') => Some(std::slice::from_ref(&prompt)),
                Some(b'`') => Some(b"`"),
                _ => None,
            };
            if let Some(taken) = taken {
                out.extend_from_slice(taken);
                at += 1;
            } else {
                out.push(b'\\');
            }
        } else if byte == b'`' {
            if let Some(len) = spec[at + 1..].iter().position(|&b| b == b'`') {
                let code = spec[at + 1..at + 1 + len].to_vec();
                state(req).capture = Some(Vec::new());
                // php leaves what the code threw pending; it is given up.
                if let Err(thrown) = req.eval_stringl(&code, b"php prompt code") {
                    clear(req, thrown).ok();
                }
                if let Some(captured) = state(req).capture.take() {
                    out.extend_from_slice(&captured);
                }
                at += len + 1;
            }
        } else if byte & 0x80 == 0 {
            out.push(byte);
        } else {
            if !warned {
                warning(req, "prompt contains unsupported unicode characters");
                warned = true;
            }
            out.push(b'?');
        }
        at += 1;
    }
    out
}

/// `php_code_type`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Body,
    Single,
    Double,
    SingleEscape,
    DoubleEscape,
    LineComment,
    BlockComment,
    HeredocStart,
    Heredoc,
    Outside,
}

/// `cli_is_valid_code`'s state over the code read so far.
struct Scan<'a> {
    code: &'a [u8],
    short_tags: bool,
    valid_end: bool,
    last_valid_end: bool,
    brackets: u32,
    braces: u32,
    kind: Kind,
    tag: Option<usize>,
    tag_len: usize,
}

impl Scan<'_> {
    fn at(&self, i: usize) -> u8 {
        self.code.get(i).copied().unwrap_or(0)
    }

    /// The byte at `i` read in the state the scan is in; the index the
    /// next byte is at.
    fn step(&mut self, i: usize) -> usize {
        let c = self.at(i);
        match self.kind {
            Kind::Single | Kind::Double => {
                let single = self.kind == Kind::Single;
                if c == b'\\' {
                    self.kind = if single {
                        Kind::SingleEscape
                    } else {
                        Kind::DoubleEscape
                    };
                } else if c == if single { b'\'' } else { b'"' } {
                    self.kind = Kind::Body;
                }
            }
            Kind::SingleEscape => self.kind = Kind::Single,
            Kind::DoubleEscape => self.kind = Kind::Double,
            Kind::LineComment if c == b'\n' => self.kind = Kind::Body,
            Kind::LineComment => {}
            Kind::BlockComment => {
                if i > 0 && self.at(i - 1) == b'*' && c == b'/' {
                    self.kind = Kind::Body;
                    self.valid_end = self.last_valid_end;
                }
            }
            Kind::HeredocStart => match c {
                b' ' | b'\t' | b'\'' | b'"' => {}
                b'\r' | b'\n' => {
                    self.kind = if self.tag.is_some() {
                        Kind::Heredoc
                    } else {
                        Kind::Body
                    };
                }
                _ => {
                    self.tag.get_or_insert(i);
                    self.tag_len += 1;
                }
            },
            Kind::Heredoc => self.heredoc(i),
            Kind::Outside => {
                let code = self.code;
                if (self.short_tags && i >= 1 && code.get(i - 1..=i) == Some(b"<?"))
                    || (i > 3 && code.get(i - 4..=i) == Some(b"<?php"))
                {
                    self.kind = Kind::Body;
                }
            }
            Kind::Body => return self.body(i, c),
        }
        i + 1
    }

    /// A heredoc ends where its label stands alone after the blanks that
    /// open its line.
    fn heredoc(&mut self, i: usize) {
        let (Some(tag), len) = (self.tag, self.tag_len) else {
            return;
        };
        if i + 1 < len || self.code.get(i + 1 - len..=i) != self.code.get(tag..tag + len) {
            return;
        }
        let next = self.at(i + 1);
        if next.is_ascii_alphanumeric() || next == b'_' || next >= 0x80 {
            return;
        }
        let mut p = i.checked_sub(len);
        while let Some(q) = p
            && matches!(self.at(q), b' ' | b'\t')
        {
            p = q.checked_sub(1);
        }
        if p.is_some_and(|q| self.at(q) == b'\n') {
            self.kind = Kind::Body;
        }
    }

    /// A byte of code outside strings and comments.
    fn body(&mut self, mut i: usize, c: u8) -> usize {
        let next = self.at(i + 1);
        match c {
            b'{' => {
                self.brackets += 1;
                self.valid_end = false;
            }
            b'}' => {
                self.brackets = self.brackets.saturating_sub(1);
                self.valid_end = self.brackets == 0;
            }
            b'(' => {
                self.braces += 1;
                self.valid_end = false;
            }
            b')' => {
                self.braces = self.braces.saturating_sub(1);
                self.valid_end = false;
            }
            b';' => self.valid_end = self.braces == 0 && self.brackets == 0,
            b' ' | b'\r' | b'\n' | b'\t' => {}
            b'\'' => self.kind = Kind::Single,
            b'"' => self.kind = Kind::Double,
            b'#' if next == b'[' => self.valid_end = false,
            b'#' => self.kind = Kind::LineComment,
            b'/' if next == b'/' => {
                i += 1;
                self.kind = Kind::LineComment;
            }
            b'/' if next == b'*' => {
                self.last_valid_end = self.valid_end;
                self.valid_end = false;
                self.kind = Kind::BlockComment;
                i += 1;
            }
            b'?' if next == b'>' => {
                i += 1;
                self.kind = Kind::Outside;
            }
            b'<' => {
                self.valid_end = false;
                if i + 2 < self.code.len() && next == b'<' && self.at(i + 2) == b'<' {
                    i += 2;
                    self.kind = Kind::HeredocStart;
                    self.tag = None;
                    self.tag_len = 0;
                }
            }
            _ => self.valid_end = false,
        }
        i + 1
    }
}

/// `cli_is_valid_code`: whether `code` could end where it does, its
/// brackets closed and its last statement ended, and the prompt for the
/// line after it.
fn is_valid_code(req: &mut Request, code: &[u8]) -> (bool, Vec<u8>) {
    let mut scan = Scan {
        code,
        short_tags: req
            .ini_get(b"short_open_tag")
            .is_none_or(Request::ini_parse_bool),
        valid_end: true,
        last_valid_end: true,
        brackets: 0,
        braces: 0,
        kind: Kind::Body,
        tag: None,
        tag_len: 0,
    };
    let mut i = 0;
    while i < code.len() {
        i = scan.step(i);
    }
    let next = match scan.kind {
        Kind::Single | Kind::SingleEscape => prompt(req, b"php", b'\''),
        Kind::Double | Kind::DoubleEscape => prompt(req, b"php", b'"'),
        Kind::BlockComment => prompt(req, b"/* ", b'>'),
        Kind::Heredoc => prompt(req, b"<<<", b'>'),
        Kind::Outside => prompt(req, b"   ", b'>'),
        _ if scan.braces > 0 => prompt(req, b"php", b'('),
        _ if scan.brackets > 0 => prompt(req, b"php", b'{'),
        _ => prompt(req, b"php", b'>'),
    };
    (scan.valid_end && scan.brackets == 0, next)
}

/// `cli_completion_generator` walked to its end: for `$` the request's
/// global variables, for `#` the ini entries, and otherwise the
/// functions, then the constants, then the classes whose names start
/// with the word, or a class's methods and then its constants after
/// `Class::`, which php compares with as many bytes of the whole word as
/// the member has; each with the character the library appends after
/// it, which is the last one's.
pub(crate) fn completion(req: &mut Request, text: &[u8]) -> Matches {
    let mut found: Vec<Vec<u8>> = Vec::new();
    let mut append = None;
    if let Some(name) = text.strip_prefix(b"$") {
        for var in req.variables_mut().global_names() {
            if var.starts_with(name) {
                found.push([b"$".as_slice(), &var].concat());
                append = Some(0);
            }
        }
    } else if let Some(name) = text.strip_prefix(b"#").filter(|n| n.first() != Some(&b'[')) {
        for entry in req.ini_entries() {
            if entry.decl.name.0.starts_with(name) {
                found.push([b"#".as_slice(), &entry.decl.name.0].concat());
                append = Some(b'=');
            }
        }
    } else if let Some(split) = text.windows(2).position(|w| w == b"::") {
        // `zend_lookup_class`: what an autoloader threw is left in
        // `EG(exception)`, as php's completion leaves it.
        let class = match autoload(req, &text[..split].to_ascii_lowercase()) {
            Ok(Some(class)) => class,
            Ok(None) => return Matches::None,
            Err(thrown) => {
                leave_pending(req, thrown);
                return Matches::None;
            }
        };
        let member = text[split + 2..].to_ascii_lowercase();
        let Some(decl) = req.class_decl(class) else {
            return Matches::None;
        };
        let class_name = decl.name.0.to_vec();
        let mut members: Vec<(Vec<u8>, u8)> = Vec::new();
        for method in &decl.methods {
            if method
                .function
                .name
                .0
                .to_ascii_lowercase()
                .starts_with(&member)
            {
                members.push((method.function.name.0.to_vec(), b'('));
            }
        }
        let compared = &text[..member.len()];
        for constant in &decl.constants {
            if constant.name.0.starts_with(compared) {
                members.push((constant.name.0.to_vec(), 0));
            }
        }
        for (name, end) in members {
            found.push([class_name.as_slice(), b"::", &name].concat());
            append = Some(end);
        }
    } else {
        let lowered = text.to_ascii_lowercase();
        if let Ok(functions) = req.defined_functions() {
            for (_, group) in walk::rows(req, *functions) {
                let group = group.deref();
                let Some(group) = group.as_array() else {
                    continue;
                };
                for name in names(req, *group) {
                    if name.starts_with(&lowered) {
                        // `func->common.function_name`, as declared.
                        let declared = lookup_function(req, &name)
                            .and_then(|f| req.function_decl(f))
                            .map_or(name, |decl| decl.name.0.to_vec());
                        found.push(declared);
                        append = Some(b'(');
                    }
                }
            }
            functions.give_up(req);
        }
        for name in constant_names(req) {
            if name.starts_with(text) {
                found.push(name);
                append = Some(0);
            }
        }
        for class in req.declared_classes() {
            if let Some(decl) = req.class_decl(class)
                && decl.name.0.to_ascii_lowercase().starts_with(&lowered)
            {
                found.push(decl.name.0.to_vec());
                append = Some(0);
            }
        }
    }
    if let Some(append) = append {
        ffi::set_completion_append_character(append);
    }
    if found.is_empty() {
        return Matches::None;
    }
    Matches::Candidates(found)
}

/// `EG(zend_constants)`'s names in its order, as
/// `get_defined_constants()` lists them.
fn constant_names(req: &mut Request) -> Vec<Vec<u8>> {
    let Ok(name) = PhpString::new(req, b"get_defined_constants") else {
        return Vec::new();
    };
    let called =
        to_callable(req, &Value::string(*name)).and_then(|callable| call(req, &callable, &[]));
    name.give_up(req);
    let Ok(all) = called else {
        return Vec::new();
    };
    let held = all.deref();
    let names = held.as_array().map_or_else(Vec::new, |constants| {
        walk::rows(req, *constants)
            .filter_map(|(key, _)| match key {
                Key::Str(name) => Some(name.as_bytes().to_vec()),
                Key::Int(_) => None,
            })
            .collect()
    });
    all.give_up(req);
    names
}

/// The strings of a list.
fn names(req: &Request, list: PhpArray) -> Vec<Vec<u8>> {
    walk::rows(req, list)
        .filter_map(|(_, name)| name.deref().as_str_bytes().map(<[u8]>::to_vec))
        .collect()
}

/// `readline_shell_run`: the history read from `PHP_HISTFILE`, or
/// `~/.php_history`, and written back after each block that added to it;
/// the shell's completion unless the program named one; then a block of
/// code at a time, `exit` or `quit` alone on a line, or the end of input,
/// leaving; the status the request's.
pub(crate) fn run(req: &mut Request) -> i32 {
    let history = req.env(b"PHP_HISTFILE").map_or_else(
        || {
            let home = req.env(b"HOME").unwrap_or(b"(null)");
            [home, b"/.php_history"].concat()
        },
        <[u8]>::to_vec,
    );
    if !ffi::completes_with_user() {
        ffi::complete_with(Completion::Shell);
    }
    ffi::special_prefixes_dollar();
    ffi::read_history(Some(&history));
    let mut code: Vec<u8> = Vec::new();
    let mut next = prompt(req, b"php", b'>');
    let mut unwritten = 0;
    while let Some(line) = ffi::readline(req, Some(&next)) {
        if line == b"exit" || line == b"quit" {
            break;
        }
        if code.is_empty() && line.is_empty() {
            continue;
        }
        if line.first() == Some(&b'#')
            && line.get(1) != Some(&b'[')
            && let Some(eq) = line.iter().position(|&b| b == b'=')
        {
            req.ini_set(&line[1..eq], &line[eq + 1..]).ok();
            ffi::add_history(&line);
            next = prompt(req, b"php", b'>');
            continue;
        }
        code.extend_from_slice(&line);
        code.push(b'\n');
        if !line.is_empty() {
            ffi::add_history(&line);
            unwritten += 1;
        }
        let (valid, after) = is_valid_code(req, &code);
        next = after;
        if !valid {
            continue;
        }
        if unwritten > 0 {
            ffi::write_history(Some(&history));
            unwritten = 0;
        }
        let left = req.eval_stringl(&code, b"php shell code");
        code.clear();
        let shell = state(req);
        if shell.pager.is_none() && shell.last != 0 && shell.last != b'\n' {
            req.write(b"\n");
        }
        if let Err(thrown) = left {
            uncaught_at(req, thrown, Level::Warning).ok();
        }
        close_pager(req);
        state(req).last = 0;
    }
    req.diagnostics().exit_status()
}
