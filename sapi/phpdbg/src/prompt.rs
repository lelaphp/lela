//! The prompt (`Z/sapi/phpdbg/phpdbg_prompt.c`, `phpdbg_cmd.c`): a line
//! read, cut into commands, each resolved against php's table by its
//! name, an abbreviation of it or its one-letter alias, its parameters
//! held to the command's specification, and run. A command that goes
//! on with the program ends the prompt; one that fails drops the rest of
//! its line, the error it failed with written unless the line named a
//! registered function, which is called instead.

use std::collections::BTreeSet;

use php_ext::abi::AfterLog;
use php_ext::request::Opcode;
use php_sapi::tier::{self, Argument, FrameRef, Request};

use crate::bp::{Kind, OpArray, Resolution};
use crate::cmd::{self, Command, Param};
use crate::input::{self, Line};
use crate::opcodes;
use crate::out;
use crate::read;
use crate::session::{Context, Execution, Origin, Seek, SeekKind, Session, Stop};

/// Where the prompt stands.
pub enum Here<'a> {
    /// A run stopped between two handlers: `frame` is the one the
    /// commands read, php's `EG(current_execute_data)`, the frame the run
    /// stopped in unless `frame n` switched it.
    Paused {
        req: &'a mut Request,
        frame: FrameRef,
        switched: Option<Switched>,
    },
    /// The execution context compiled before `run`, with nothing
    /// executing: php's `PHPDBG_G(ops)` with `EG(current_execute_data)`
    /// `NULL`.
    Held { req: &'a mut Request },
    /// A request with no program of the context's, with nothing
    /// executing: the prompt's where no context is held.
    Idle { req: &'a mut Request },
    /// No request standing.
    Outside,
}

/// `PHPDBG_G(frame)` while `frame n` has switched the frame the
/// commands read: its number, counted from the frame the run stopped in,
/// and that frame, which the prompt goes back to before the run goes on
/// (`phpdbg_restore_frame`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Switched {
    num: u32,
    stopped: FrameRef,
}

impl<'a> Here<'a> {
    /// The prompt of a run stopped at `frame`.
    pub fn paused(req: &'a mut Request, frame: FrameRef) -> Here<'a> {
        Here::Paused {
            req,
            frame,
            switched: None,
        }
    }
}

impl Here<'_> {
    /// `phpdbg_restore_frame`: the frame the run stopped in read again.
    fn restore_frame(&mut self) {
        if let Here::Paused {
            frame, switched, ..
        } = self
            && let Some(Switched { stopped, .. }) = switched.take()
        {
            *frame = stopped;
        }
    }

    /// `PHPDBG_FRAME(num)`: how many frames below the one the run stopped
    /// in the commands read.
    pub fn frame_num(&self) -> u32 {
        match self {
            Here::Paused {
                switched: Some(Switched { num, .. }),
                ..
            } => *num,
            _ => 0,
        }
    }

    /// The request the prompt stands in, where one stands.
    pub fn request(&mut self) -> Option<&mut Request> {
        match self {
            Here::Paused { req, .. } | Here::Held { req } | Here::Idle { req } => Some(&mut **req),
            Here::Outside => None,
        }
    }

    /// The request and the frame a run stopped in, `EG(current_execute_data)`,
    /// where one is executing.
    pub fn standing(&mut self) -> Option<(&mut Request, FrameRef)> {
        match self {
            Here::Paused { req, frame, .. } => Some((&mut **req, *frame)),
            Here::Held { .. } | Here::Idle { .. } | Here::Outside => None,
        }
    }

    /// The request the prompt has serve code of its own, an `ev` or a
    /// registered function, and what that code stands on: the frame a
    /// run stopped in, or nothing, as php's `EG(current_execute_data)` is
    /// `NULL` at a prompt no run stands under.
    fn serving(&mut self) -> Option<(&mut Request, tier::Under)> {
        match self {
            Here::Paused { req, frame, .. } => Some((&mut **req, tier::Under::Frame(*frame))),
            Here::Held { req } | Here::Idle { req } => Some((&mut **req, tier::Under::Nothing)),
            Here::Outside => None,
        }
    }

    /// `PHPDBG_G(in_execution)` with `EG(current_execute_data)` standing:
    /// a frame the program is stopped in.
    pub fn executing(&self) -> bool {
        matches!(self, Here::Paused { .. })
    }

    /// `PHPDBG_G(ops)`: the context is compiled and standing.
    fn compiled(&self) -> bool {
        matches!(self, Here::Paused { .. } | Here::Held { .. })
    }
}

/// `PHPDBG_G(in_execution)`: a frame stands, or the session takes
/// something to be executing where none does.
fn in_execution(s: &Session, here: &Here<'_>) -> bool {
    here.executing() || s.execution != Execution::Not
}

/// Whether the user answered `question` with a yes; the input ending
/// first ends the session, as php's read bails out quitting.
fn permitted(s: &mut Session, question: &[u8]) -> bool {
    match input::ask(s, question) {
        input::Answer::Yes => true,
        input::Answer::No => false,
        input::Answer::End => {
            s.stopped = Some(Stop::Quit);
            false
        }
    }
}

/// `run`'s question where php takes something to be executing.
pub fn restart_permitted(s: &mut Session) -> bool {
    permitted(s, b"Do you really want to restart execution?")
}

/// How the prompt ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The program goes on.
    Resume,
    /// A command that goes on with the program where none is running:
    /// `[Not running]` written and `PHPDBG_NEXT` answered, which the
    /// prompt php stands at inside its log leaves by, and every other
    /// prompt stands at again.
    NotRunning,
    /// The request ends: the session's `stopped` says why.
    Stopped,
    /// A refusal at the prompt inside the log bailed out of the log, past
    /// what `php_error_cb` writes after it.
    LeftLog,
}

impl Outcome {
    /// How the host's log a prompt inside it stood in ends, the session
    /// `stopped` or not: bailed out, past what `php_error_cb` writes
    /// after it, where the prompt stopped the session or a refusal at it
    /// bailed out; else returned, for `php_error_cb` to go on.
    pub fn after_log(self, stopped: bool) -> AfterLog {
        match self {
            Outcome::Stopped | Outcome::LeftLog => AfterLog::BailedOut,
            Outcome::Resume | Outcome::NotRunning if stopped => AfterLog::BailedOut,
            Outcome::Resume | Outcome::NotRunning => AfterLog::Returned,
        }
    }
}

/// What one command asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ret {
    Success,
    Failure,
    /// `PHPDBG_NEXT` and its kin: the program goes on.
    Resume,
    /// `run` in the held request, which goes on as the run.
    Run,
}

/// `phpdbg_interactive`: lines read and run until one goes on with the
/// program or ends the request; the commands left of a line after a
/// `run` wait in the session for the run to end.
pub fn interactive(s: &mut Session, here: &mut Here<'_>) -> Outcome {
    let outcome = prompt_loop(s, here);
    here.restore_frame();
    outcome
}

/// The prompt's commands, read and run until one ends it.
fn prompt_loop(s: &mut Session, here: &mut Here<'_>) -> Outcome {
    loop {
        if s.stopped.is_some() {
            return Outcome::Stopped;
        }
        let commands = if s.queued.is_empty() {
            let prompt = [s.prompt.as_slice(), b" "].concat();
            match input::read_input(s, &prompt) {
                Line::End => {
                    s.stopped = Some(Stop::Quit);
                    return Outcome::Stopped;
                }
                Line::Read(mut line) => {
                    // `phpdbg_do_parse`'s `PHPDBG_G(cur_command)`.
                    s.command_line = Some(line.clone());
                    match cmd::parse(&mut line) {
                        Ok(commands) => commands,
                        Err(cmd::Malformed(message)) => {
                            s.out.error(format!("Parse Error: {message}").as_bytes());
                            continue;
                        }
                    }
                }
            }
        } else {
            std::mem::take(&mut s.queued)
        };
        if commands.is_empty() {
            s.out.error(b"The stack contains nothing !");
            continue;
        }
        let mut rest = commands.into_iter();
        while let Some(command) = rest.next() {
            let asked = execute(s, here, &command);
            if std::mem::take(&mut s.left_log) {
                return Outcome::LeftLog;
            }
            if s.stopped.is_some() {
                s.queued = rest.collect();
                return Outcome::Stopped;
            }
            match asked {
                Ret::Success => {}
                Ret::Failure => break,
                Ret::Run => {
                    s.queued = rest.collect();
                    return Outcome::Resume;
                }
                Ret::Resume => {
                    if here.executing() {
                        return Outcome::Resume;
                    }
                    if !in_execution(s, here) {
                        s.out.error(b"Not running");
                    }
                    return Outcome::NotRunning;
                }
            }
        }
    }
}

/// The commands a run left, run where the run ended, outside a request.
pub fn run_queued(s: &mut Session) {
    let mut here = Here::Outside;
    let mut rest = std::mem::take(&mut s.queued).into_iter();
    while let Some(command) = rest.next() {
        let asked = execute(s, &mut here, &command);
        if s.stopped.is_some() {
            s.queued = rest.collect();
            return;
        }
        match asked {
            Ret::Success => {}
            Ret::Failure | Ret::Run => return,
            Ret::Resume => {
                s.out.error(b"Not running");
                return;
            }
        }
    }
}

/// How one line of an init file ended.
pub enum InitLine {
    /// Its commands ran, or one failed and wrote its own error.
    Done,
    /// A command ended the request: the session's `stopped` says why.
    Stopped,
    /// `run` runs the program inline, and the reading goes on after it.
    Ran,
    /// A command named nothing php's table resolves, or what it named was
    /// held to the wrong parameters, and no registered function took the
    /// line: the error, which `phpdbg_line_init` writes with the line.
    Unrecognized(String),
}

/// One line of an init file, `phpdbg_line_init`'s `phpdbg_stack_execute`:
/// its commands in turn until one does not succeed. What resolving and
/// verifying a command writes is held back in php's error buffer and
/// handed to the caller instead; a handler's own error is written, and a
/// line that fails in its handler or goes on with the program ends
/// silently, as php's buffer is let go before the handler runs.
pub fn init_line(s: &mut Session, here: &mut Here<'_>, commands: Vec<Command>) -> InitLine {
    if commands.is_empty() {
        return InitLine::Unrecognized(String::from("The stack contains nothing !"));
    }
    for command in commands {
        let asked = match &command {
            Command::Params(params) => {
                let resolved = resolve(&ROOT, None, params).and_then(|(def, rest)| {
                    verify(def, rest)?;
                    Ok((def, rest))
                });
                match resolved {
                    Ok((def, rest)) => (def.handler)(s, here, rest),
                    Err(error) => {
                        if call_registered(s, here, params) {
                            return InitLine::Done;
                        }
                        return error.map_or(InitLine::Done, InitLine::Unrecognized);
                    }
                }
            }
            Command::Eval(_) | Command::Run(_) | Command::Shell(_) => execute(s, here, &command),
        };
        if s.stopped.is_some() {
            return InitLine::Stopped;
        }
        if s.inline_run {
            return InitLine::Ran;
        }
        match asked {
            Ret::Success => {}
            Ret::Failure | Ret::Run | Ret::Resume => return InitLine::Done,
        }
    }
    InitLine::Done
}

/// The command a run left that needs a request standing, served by an
/// idle request of its own once the run's has ended.
pub fn idle_job(s: &mut Session, req: &mut Request, command: &Command) {
    let mut here = Here::Idle { req };
    let _ = execute(s, &mut here, command);
}

/// `phpdbg_internal_stack_execute`.
fn execute(s: &mut Session, here: &mut Here<'_>, command: &Command) -> Ret {
    match command {
        Command::Eval(code) => ev(s, here, code),
        Command::Run(args) => run(s, here, args.as_deref()),
        Command::Shell(_) => Ret::Success,
        Command::Params(params) => {
            let resolved = resolve(&ROOT, None, params).and_then(|(def, rest)| {
                verify(def, rest)?;
                Ok((def, rest))
            });
            match resolved {
                Ok((def, rest)) => (def.handler)(s, here, rest),
                Err(error) => {
                    if !call_registered(s, here, params)
                        && let Some(error) = error
                    {
                        s.out.error(error.as_bytes());
                    }
                    Ret::Failure
                }
            }
        }
    }
}

/// A handler.
type Handler = fn(&mut Session, &mut Here<'_>, &[Param]) -> Ret;

/// A row of php's command tables.
struct Def {
    name: &'static str,
    alias: Option<u8>,
    args: &'static str,
    subs: &'static [Def],
    /// A sub-command's `parent`: the row of `phpdbg_prompt_commands` its
    /// table points at, which is not its own command's row but a fixed
    /// one each table names, and which its errors name it by.
    parent: Option<&'static Def>,
    handler: Handler,
}

const fn def(name: &'static str, alias: Option<u8>, args: &'static str, handler: Handler) -> Def {
    Def {
        name,
        alias,
        args,
        subs: &[],
        parent: None,
        handler,
    }
}

const fn sub(
    parent: &'static Def,
    name: &'static str,
    alias: Option<u8>,
    args: &'static str,
    handler: Handler,
) -> Def {
    Def {
        name,
        alias,
        args,
        subs: &[],
        parent: Some(parent),
        handler,
    }
}

/// A row of `phpdbg_watch_commands`.
const fn watch_row(
    name: &'static str,
    alias: Option<u8>,
    args: &'static str,
    handler: Handler,
) -> Def {
    Def {
        name,
        alias,
        args,
        subs: ROOT.split_at(24).1,
        parent: None,
        handler,
    }
}

const fn with_subs(
    name: &'static str,
    alias: Option<u8>,
    args: &'static str,
    subs: &'static [Def],
    handler: Handler,
) -> Def {
    Def {
        name,
        alias,
        args,
        subs,
        parent: None,
        handler,
    }
}

/// `phpdbg_prompt_commands`.
static ROOT: [Def; 27] = [
    def("exec", Some(b'e'), "s", exec),
    def("stdin", None, "s", stdin),
    def("step", Some(b's'), "", step),
    def("continue", Some(b'c'), "", resume),
    def("run", Some(b'r'), "|s", run_command),
    def("ev", None, "i", ev_command),
    def("until", Some(b'u'), "", until),
    def("finish", Some(b'F'), "", finish),
    def("leave", Some(b'L'), "", leave),
    def("generator", Some(b'g'), "|n", unsupported),
    with_subs("print", Some(b'p'), "|*c", PRINT, print),
    with_subs("break", Some(b'b'), "|*c", BREAK, breakpoint),
    def("back", Some(b't'), "|n", back),
    def("frame", Some(b'f'), "|n", frame),
    with_subs("list", Some(b'l'), "*", LIST, list),
    with_subs("info", Some(b'i'), "|s", INFO, info_overview),
    def("clean", Some(b'X'), "", clean),
    def("clear", Some(b'C'), "", clear),
    def("help", Some(b'h'), "|s", help),
    with_subs("set", Some(b'S'), "s", SET, set),
    def("register", Some(b'R'), "s", register),
    def("source", Some(b'<'), "s", unsupported),
    def("export", Some(b'>'), "s", unsupported),
    def("sh", None, "i", unsupported),
    def("quit", Some(b'q'), "", quit),
    with_subs("watch", Some(b'w'), "|ss", WATCH, crate::watch::watch),
    def("next", Some(b'n'), "", next),
];

/// `phpdbg_print_commands`, whose parent is php's row 8, `leave`
/// (`Z/sapi/phpdbg/phpdbg_print.c:29`).
static PRINT: &[Def] = &[
    sub(&ROOT[8], "exec", Some(b'e'), "", print_exec),
    sub(&ROOT[8], "opline", Some(b'o'), "", print_opline),
    sub(&ROOT[8], "class", Some(b'c'), "s", print_class),
    sub(&ROOT[8], "method", Some(b'm'), "m", print_method),
    sub(&ROOT[8], "func", Some(b'f'), "s", print_func),
    sub(&ROOT[8], "stack", Some(b's'), "", print_stack),
];

/// `phpdbg_break_commands`, whose parent is php's row 9, `generator`
/// (`Z/sapi/phpdbg/phpdbg_break.c:29`).
static BREAK: &[Def] = &[
    sub(&ROOT[9], "at", Some(b'@'), "*c", unsupported),
    sub(&ROOT[9], "del", Some(b'~'), "n", break_del),
];

/// `phpdbg_list_commands`, whose parent is php's row 12, `back`
/// (`Z/sapi/phpdbg/phpdbg_list.c:37`).
static LIST: &[Def] = &[
    sub(&ROOT[12], "lines", Some(b'l'), "l", list),
    sub(&ROOT[12], "class", Some(b'c'), "s", unsupported),
    sub(&ROOT[12], "method", Some(b'm'), "m", unsupported),
    sub(&ROOT[12], "func", Some(b'f'), "s", unsupported),
];

/// `phpdbg_info_commands`, whose parent is php's row 13, `frame`
/// (`Z/sapi/phpdbg/phpdbg_info.c:29`).
static INFO: &[Def] = &[
    sub(&ROOT[13], "break", Some(b'b'), "", crate::info::breaks),
    sub(&ROOT[13], "files", Some(b'F'), "", crate::info::files),
    sub(&ROOT[13], "classes", Some(b'c'), "", crate::info::classes),
    sub(&ROOT[13], "funcs", Some(b'f'), "", crate::info::funcs),
    sub(&ROOT[13], "error", Some(b'e'), "", crate::info::error),
    sub(
        &ROOT[13],
        "constants",
        Some(b'd'),
        "",
        crate::info::constants,
    ),
    sub(&ROOT[13], "vars", Some(b'v'), "", crate::info::vars),
    sub(&ROOT[13], "globals", Some(b'g'), "", crate::info::globals),
    sub(&ROOT[13], "literal", Some(b'l'), "", crate::info::literal),
    sub(&ROOT[13], "memory", Some(b'm'), "", unsupported),
];

/// `phpdbg_set_commands`, whose parent is php's row 17, `clear`
/// (`Z/sapi/phpdbg/phpdbg_set.c:28-46`).
static SET: &[Def] = &[
    sub(&ROOT[17], "prompt", Some(b'p'), "|s", set_prompt),
    sub(&ROOT[17], "pagination", Some(b'P'), "|b", unsupported),
    sub(&ROOT[17], "color", Some(b'c'), "ss", set_color),
    sub(&ROOT[17], "colors", Some(b'C'), "|b", set_colors),
    sub(&ROOT[17], "break", Some(b'b'), "l|b", set_break),
    sub(&ROOT[17], "breaks", Some(b'B'), "|b", set_breaks),
    sub(&ROOT[17], "quiet", Some(b'q'), "|b", set_quiet),
    sub(&ROOT[17], "stepping", Some(b's'), "|s", unsupported),
    sub(&ROOT[17], "refcount", Some(b'r'), "|b", unsupported),
    sub(&ROOT[17], "lines", Some(b'l'), "|l", unsupported),
];

/// `phpdbg_watch_commands` (`Z/sapi/phpdbg/phpdbg_watch.c:124-129`):
/// `PHPDBG_COMMAND_D_EX` names no parent, so an error names the row
/// alone, and the row php's line passes where the others pass a parent
/// is its sub-commands, `phpdbg_prompt_commands` from row 24, `quit`, on.
static WATCH: &[Def] = &[
    watch_row("array", Some(b'a'), "s", crate::watch::array),
    watch_row("delete", Some(b'd'), "n", crate::watch::delete),
    watch_row("recursive", Some(b'r'), "s", crate::watch::recursive),
];

/// `phpdbg_stack_resolve`: the command the first parameter names, and
/// the parameters after it; an error, where there is one to write, when
/// none matches or several do. A sub-command that matches none is its
/// parent's, which runs with the name as its first parameter.
fn resolve<'p>(
    table: &'static [Def],
    parent: Option<&'static Def>,
    params: &'p [Param],
) -> Result<(&'static Def, &'p [Param]), Option<String>> {
    let Some((Param::Str(name), rest)) = params.split_first() else {
        if params.is_empty() {
            return Err(Some(String::from("The stack contains nothing !")));
        }
        return Err(Some(String::from("The first parameter makes no sense !")));
    };
    let mut matched: Vec<&'static Def> = Vec::new();
    for def in table {
        if name.len() != 1 && def.name.len() < name.len() {
            continue;
        }
        if let (Some(alias), 1) = (def.alias, name.len()) {
            if alias == name[0] {
                matched.push(def);
            }
            continue;
        }
        if def.name.len() >= name.len()
            && def.name.as_bytes()[..name.len()].eq_ignore_ascii_case(name)
        {
            if matched.len() < 3
                && ((name.len() != def.name.len() && def.alias.is_some())
                    || name.len() == def.name.len())
            {
                matched.push(def);
            }
            if name.len() == def.name.len() {
                break;
            }
        }
    }
    let text = String::from_utf8_lossy(name).into_owned();
    match matched.as_slice() {
        [] => match parent {
            Some(parent) => Ok((parent, params)),
            None => Err(Some(format!("The command \"{text}\" could not be found"))),
        },
        [one] => {
            if !one.subs.is_empty()
                && let Some(Param::Str(_)) = rest.first()
            {
                return resolve(one.subs, Some(one), rest);
            }
            Ok((one, rest))
        }
        several => {
            let names: Vec<&str> = several.iter().map(|def| def.name).collect();
            Err(Some(format!(
                "The command \"{text}\" is ambiguous, matching {} commands ({})",
                several.len(),
                names.join(", ")
            )))
        }
    }
}

/// `phpdbg_command_name`: a sub-command under the row its table names
/// as its parent, `back` for `list`'s, and a watch row alone.
///
/// ```php
/// $a = 1;
/// echo "end\n";
/// ```
/// ```request phpdbg
/// b 3
/// r
/// l f
/// w a
/// c
/// q
/// ```
/// ```expect
/// [Successful compilation of %SCRATCH%/witness.php]
/// prompt> [Breakpoint #0 added at %SCRATCH%/witness.php:3]
/// prompt> [Breakpoint #0 at %SCRATCH%/witness.php:3, hits: 1]
/// >00003: echo "end\n";
/// prompt> [The command "back func" expected string and got nothing at parameter 1]
/// prompt> [The command "array" expected string and got nothing at parameter 1]
/// prompt> end
/// [Script ended normally]
/// prompt>
/// ```
fn command_name(def: &Def) -> String {
    match def.parent {
        Some(parent) => format!("{} {}", parent.name, def.name),
        None => def.name.to_owned(),
    }
}

/// Whether an argument is of the kind a letter of a command's spec names.
type Fits = fn(&Param) -> bool;

/// `phpdbg_stack_verify`: each letter of the command's spec checks the
/// argument it stands for, and `*` checks none, so `list` with nothing
/// after it is refused by the count of arguments it needs, not by the
/// argument missing.
///
/// ```php
/// $a = 1;
/// echo "end\n";
/// ```
/// ```request phpdbg
/// b 3
/// r
/// list
/// c
/// q
/// ```
/// ```expect
/// [Successful compilation of %SCRATCH%/witness.php]
/// prompt> [Breakpoint #0 added at %SCRATCH%/witness.php:3]
/// prompt> [Breakpoint #0 at %SCRATCH%/witness.php:3, hits: 1]
/// >00003: echo "end\n";
/// prompt> [The command "list" expected at least 1 arguments (*) and received 0]
/// prompt> end
/// [Script ended normally]
/// prompt>
/// ```
fn verify(def: &Def, params: &[Param]) -> Result<(), Option<String>> {
    let name = command_name(def);
    if def.args.is_empty() {
        if params.is_empty() {
            return Ok(());
        }
        return Err(Some(format!(
            "The command \"{name}\" expected no arguments"
        )));
    }
    let least = def.args.bytes().take_while(|b| *b != b'|').count();
    let mut optional = false;
    let mut received = 0;
    let mut current = 0;
    let mut at = params.iter();
    let mut top = at.next();
    for spec in def.args.bytes() {
        if spec == b'|' {
            optional = true;
            continue;
        }
        current += 1;
        // `*` and a letter php's switch does not name are checked
        // against nothing: a missing argument ends the loop, and the
        // count after it answers.
        let kind: Option<(&str, Fits)> = match spec {
            b'i' => Some(("raw input", |p| matches!(p, Param::Str(_)))),
            b's' => Some(("string", |p| matches!(p, Param::Str(_)))),
            b'n' => Some(("number", |p| matches!(p, Param::Num(_)))),
            b'm' => Some(("method", |p| matches!(p, Param::Method { .. }))),
            b'a' => Some(("address", |p| matches!(p, Param::Addr(_)))),
            b'f' => Some(("file:line", |p| matches!(p, Param::File { .. }))),
            b'c' => Some(("condition", |p| matches!(p, Param::Cond(_)))),
            b'o' => Some(("opcode", |p| matches!(p, Param::Op(_)))),
            b'b' => Some(("boolean", |p| matches!(p, Param::Num(_)))),
            _ => None,
        };
        match (top, kind) {
            (None, Some((expected, _))) if !optional => {
                return Err(Some(format!(
                    "The command \"{name}\" expected {expected} and got nothing at parameter {current}"
                )));
            }
            (None, _) => break,
            (Some(param), Some((expected, fits))) if !fits(param) => {
                return Err(Some(format!(
                    "The command \"{name}\" expected {expected} and got {} at parameter {current}",
                    param.kind()
                )));
            }
            (Some(_), _) => {}
        }
        top = at.next();
        received += 1;
    }
    if received < least {
        return Err(Some(format!(
            "The command \"{name}\" expected at least {least} arguments ({}) and received {received}",
            def.args
        )));
    }
    Ok(())
}

/// `phpdbg_call_register`: a line whose first word is a registered
/// function calls it with the rest as its arguments, then writes a
/// newline, but where the call ended in a fatal or an `exit`: with
/// nothing executing, what it leaves uncaught is rendered as a fatal.
/// Where `log_errors` logs that fatal, `php_sapi_phpdbg_log_message`
/// writes it and follows it with the listing where the request stands,
/// `[no active file]` where nothing does, and a prompt
/// ([`crate::debugger::fatal_logged`]). The status is the 255
/// `php_error_cb` writes after the log, which a `quit` at that prompt
/// bails out before, and which the display regime, logging nothing,
/// writes at once.
///
/// ```php
/// $a = 1;
/// ```
/// ```request phpdbg
/// register intdiv
/// intdiv 1
/// q
/// ```
/// ```expect exit=255
/// [Successful compilation of %SCRATCH%/witness.php]
/// prompt> [Registered intdiv]
/// prompt>%s
/// Fatal error: Uncaught ArgumentCountError: intdiv() expects exactly 2 arguments, 1 given in [no active file]:0
/// Stack trace:
/// #0 [internal function]: intdiv(1)
/// #1 {main}
///   thrown in [no active file] on line 0
/// prompt>
/// ```
///
/// ```php
/// $a = 1;
/// ```
/// ```request phpdbg
/// ev ini_set("display_errors", 0)
/// ev ini_set("log_errors", 1)
/// register intdiv
/// intdiv 1
/// q
/// ```
/// ```expect
/// [Successful compilation of %SCRATCH%/witness.php]
/// prompt> 1
/// prompt> 0
/// prompt> [Registered intdiv]
/// prompt> [PHP Fatal error:  Uncaught ArgumentCountError: intdiv() expects exactly 2 arguments, 1 given in [no active file]:0
/// Stack trace:
/// #0 [internal function]: intdiv(1)
/// #1 {main}
///   thrown in [no active file] on line 0]
/// [Could not find information about included file...]
/// prompt>
/// ```
fn call_registered(s: &mut Session, here: &mut Here<'_>, params: &[Param]) -> bool {
    let Some((Param::Str(name), rest)) = params.split_first() else {
        return false;
    };
    let lowered = name.to_ascii_lowercase();
    if !s.registered.contains(&lowered) {
        return false;
    }
    let arguments: Vec<Argument> = rest
        .iter()
        .filter_map(|param| match param {
            Param::Num(n) => Some(Argument::Int(*n)),
            Param::Str(_)
            | Param::Op(_)
            | Param::Cond(_)
            | Param::Method { .. }
            | Param::NumericMethod { .. }
            | Param::NumericFunction { .. }
            | Param::File { .. } => Some(Argument::Str(param.text())),
            Param::NumericFile { name, line } => Some(Argument::Str(
                [name.as_slice(), format!(":#{line}").as_bytes()].concat(),
            )),
            Param::Addr(_) => None,
        })
        .collect();
    let Some((req, under)) = here.serving() else {
        crate::host::idle(s, Command::Params(params.to_vec()));
        return true;
    };
    match crate::session::lend(req, s, |req| tier::call(req, under, &lowered, &arguments)) {
        tier::Called::Returned => s.out.out(b"\n"),
        tier::Called::Threw => {
            if let tier::Under::Frame(frame) = under {
                s.thrown_into = Some(frame);
            }
            s.out.out(b"\n");
        }
        tier::Called::Exited => bailed_out(s),
        tier::Called::Rendered => {}
    }
    true
}

/// An `exit` served where nothing was executing: php's loop bailed out
/// of `phpdbg_execute_ex` with `in_execution` set, so `run` asks before
/// it restarts what php takes to be executing, and the input ending
/// before an answer ends the session.
///
/// ```php
/// $a = 1;
/// $b = 2;
/// $c = 3;
/// echo "x\n";
/// ```
/// ```request phpdbg
/// ev exit(3)
/// r
/// q
/// ```
/// ```expect exit=0
/// [Successful compilation of %SCRATCH%/witness.php]
/// prompt> prompt> Do you really want to restart execution? (type y or n): Please enter either y (yes) or n (no):
/// ```
///
/// ```php
/// $a = 1;
/// $b = 2;
/// $c = 3;
/// echo "x\n";
/// ```
/// ```request phpdbg
/// ev exit(3)
/// c
/// r
/// y
/// q
/// ```
/// ```expect exit=0
/// [Successful compilation of %SCRATCH%/witness.php]
/// prompt> prompt> prompt> Do you really want to restart execution? (type y or n): x
/// [Script ended normally]
/// prompt>
/// ```
pub fn bailed_out(s: &mut Session) {
    if s.execution == Execution::Not {
        s.execution = Execution::BailedOut;
    }
}

/// What `zend_get_executed_filename` names where no frame stands.
pub const NO_ACTIVE_FILE: &[u8] = b"[no active file]";

// ------------------------------------------------------------ commands

fn unsupported(_: &mut Session, _: &mut Here<'_>, _: &[Param]) -> Ret {
    Ret::Success
}

/// `PHPDBG_COMMAND(exec)`.
fn exec(s: &mut Session, here: &mut Here<'_>, params: &[Param]) -> Ret {
    let Some(Param::Str(path)) = params.first() else {
        return Ret::Failure;
    };
    let shown = String::from_utf8_lossy(path).into_owned();
    let os = std::path::Path::new(std::ffi::OsStr::from_bytes(path));
    let Ok(meta) = std::fs::metadata(os) else {
        s.out
            .error(format!("Cannot stat {shown}, ensure the file exists").as_bytes());
        return Ret::Success;
    };
    if !meta.is_file() {
        s.out.error(
            format!("Cannot use {shown} as execution context, not a valid file or symlink")
                .as_bytes(),
        );
        return Ret::Success;
    }
    let resolved = crate::host::real_path(path);
    if s.exec.as_deref() == Some(resolved.as_slice()) {
        s.out.notice(b"Execution context not changed");
        return Ret::Success;
    }
    if in_execution(s, here)
        && !permitted(
            s,
            b"Do you really want to stop execution to set a new execution context?",
        )
    {
        return Ret::Failure;
    }
    if let Some(old) = &s.exec {
        s.out.notice(
            [&b"Unsetting old execution context: "[..], old]
                .concat()
                .as_slice(),
        );
    }
    if here.compiled() {
        s.out.notice(b"Destroying compiled opcodes");
    }
    if let Some(dir) = std::path::Path::new(std::ffi::OsStr::from_bytes(&resolved)).parent() {
        std::env::set_current_dir(dir).ok();
    }
    s.out.notice(
        [&b"Set execution context: "[..], &resolved]
            .concat()
            .as_slice(),
    );
    s.exec = Some(resolved);
    s.stdin_code = None;
    s.exec_in_log = !in_execution(s, here) && s.out.logging();
    s.context = if in_execution(s, here) {
        Context::Announced
    } else {
        Context::Unannounced
    };
    // `phpdbg_clean(.., 0)` lets the line the cycle parsed last go.
    s.command_line = None;
    s.stopped = Some(Stop::Exec);
    Ret::Success
}

use std::os::unix::ffi::OsStrExt as _;

/// `PHPDBG_COMMAND(stdin)`: lines up to one that is the delimiter, as
/// the execution context.
fn stdin(s: &mut Session, here: &mut Here<'_>, params: &[Param]) -> Ret {
    let Some(Param::Str(delimiter)) = params.first() else {
        return Ret::Failure;
    };
    let mut code = Vec::new();
    loop {
        match input::consume_line(s) {
            Line::End => break,
            Line::Read(line) => {
                let bare = line.strip_suffix(b"\n").unwrap_or(&line);
                let bare = bare.strip_suffix(b"\r").unwrap_or(bare);
                if bare == delimiter.as_slice() {
                    break;
                }
                code.extend_from_slice(&line);
            }
        }
    }
    if here.executing()
        && !permitted(
            s,
            b"Do you really want to stop execution to set a new execution context?",
        )
    {
        return Ret::Failure;
    }
    s.exec = Some(b"Standard input code".to_vec());
    s.stdin_code = Some(code);
    s.context = Context::Unannounced;
    s.stopped = Some(Stop::Exec);
    Ret::Success
}

/// `PHPDBG_COMMAND(step)`.
fn step(s: &mut Session, here: &mut Here<'_>, _: &[Param]) -> Ret {
    if here.executing() {
        s.stepping = true;
    }
    Ret::Resume
}

/// `PHPDBG_COMMAND(continue)`.
fn resume(_: &mut Session, _: &mut Here<'_>, _: &[Param]) -> Ret {
    Ret::Resume
}

/// `run` as a word, with its one parameter, where the lexer did not
/// read it as `run`'s raw form.
fn run_command(s: &mut Session, here: &mut Here<'_>, params: &[Param]) -> Ret {
    let args = match params.first() {
        Some(Param::Str(args)) => Some(args.as_slice()),
        _ => None,
    };
    run(s, here, args)
}

/// `PHPDBG_COMMAND(run)`.
fn run(s: &mut Session, here: &mut Here<'_>, args: Option<&[u8]>) -> Ret {
    if s.exec.is_none() {
        s.out.error(b"Nothing to execute!");
        return Ret::Success;
    }
    let args = args.filter(|args| !args.is_empty()).map(<[u8]>::to_vec);
    // A yes in the init files, after the run among them, has php count
    // a startup run and clean the cycle, and the reading goes on.
    if in_execution(s, here) {
        if restart_permitted(s) {
            if s.initializing {
                s.startup_run = true;
            } else {
                // php keeps this line for the next cycle to resubmit,
                // which is this `run` again, the restart's.
                s.command_line = None;
                s.stopped = Some(Stop::Restart(args));
            }
        }
        return Ret::Success;
    }
    // `phpdbg_compile` before anything of the run: the compiler refuses
    // the context again, in its log, and bails out of the run.
    if s.context == Context::Refused && !here.compiled() {
        refused_again(s, here);
        return Ret::Success;
    }
    // In the init files, php's `run` compiles the context and runs it
    // there and then, `PHPDBG_IS_INITIALIZING` keeping the loop from
    // stopping but for a throwable nothing catches, and the reading goes
    // on after it.
    // With arguments, it rebuilds `argv` in the request first, a `<file`
    // among them opened as the program's stdin there, and one that does
    // not open refusing the run.
    if s.initializing && s.program_held {
        compiled_in_init(s);
        if let Some(args) = &args {
            crate::host::set_arguments(s, args);
            if let Some(path) = s.stdin_file.take() {
                let Ok(file) = std::fs::File::open(&path) else {
                    s.out.error(
                        format!("Could not open '{}' for reading from stdin", path.display())
                            .as_bytes(),
                    );
                    return Ret::Success;
                };
                if let Some(req) = here.request() {
                    tier::set_stdin(req, file.into());
                }
            }
            if let Some(req) = here.request() {
                let mut words = vec![s.exec.clone().unwrap_or_default()];
                words.extend(s.args.iter().cloned());
                tier::set_argv(req, words);
            }
        }
        s.inline_run = true;
        return Ret::Success;
    }
    match here {
        Here::Paused { .. } => Ret::Success,
        Here::Held { .. } if args.is_none() => {
            begin_run(s);
            Ret::Run
        }
        Here::Held { .. } | Here::Outside | Here::Idle { .. } => {
            s.stopped = Some(Stop::RunWith(args));
            Ret::Success
        }
    }
}

/// What a run starts from: the hits at zero, no seek, no throwable seen
/// (`PHPDBG_COMMAND(run)`'s clean up from last execution).
pub fn begin_run(s: &mut Session) {
    s.breaks.reset_hits();
    s.seek = None;
    s.handled = None;
    s.stepping = false;
    s.last_line = 0;
    s.execution = Execution::Run;
}

/// `ev` as a word with its raw parameter.
fn ev_command(s: &mut Session, here: &mut Here<'_>, params: &[Param]) -> Ret {
    let Some(Param::Str(code)) = params.first() else {
        return Ret::Failure;
    };
    let code = code.clone();
    ev(s, here, &code)
}

/// `PHPDBG_COMMAND(ev)`: the code evaluated under `PHPDBG_IN_EVAL`, with
/// the step put off while it runs and set again after where it stood. At
/// the held prompt nothing stands under it, so what it leaves uncaught
/// traces to `{main}`, and the status is the 255 `php_error_cb` wrote;
/// the `%s` is the space after the prompt, which no doc line keeps.
///
/// ```php
/// $a = 1;
/// ```
/// ```request phpdbg
/// ev intdiv(1)
/// q
/// ```
/// ```expect exit=255
/// [Successful compilation of %SCRATCH%/witness.php]
/// prompt>%s
/// Fatal error: Uncaught ArgumentCountError: intdiv() expects exactly 2 arguments, 1 given in eval()'d code:1
/// Stack trace:
/// #0 eval()'d code(1): intdiv(1)
/// #1 {main}
///   thrown in eval()'d code on line 1
/// prompt>
/// ```
///
/// An `exit` it serves leaves the status 0, as `ev`'s `zend_catch`
/// writes it, and stays pending, so the next `ev` serves nothing:
/// `zend_execute` refuses to start with something pending, and what is
/// pending is rendered, which for an exit is nothing.
///
/// ```php
/// $a = 1;
/// ```
/// ```request phpdbg
/// ev exit(3)
/// ev 4
/// ev 5
/// q
/// ```
/// ```expect exit=0
/// [Successful compilation of %SCRATCH%/witness.php]
/// prompt> prompt> prompt> 5
/// prompt>
/// ```
///
/// Where `log_errors` logs it, the log is `php_sapi_phpdbg_log_message`'s,
/// through `phpdbg_error`, and under `PHPDBG_IN_EVAL` nothing follows.
///
/// ```php
/// $a = 1;
/// ```
/// ```request phpdbg
/// ev ini_set("display_errors", 0)
/// ev ini_set("log_errors", 1)
/// ev intdiv(1)
/// q
/// ```
/// ```expect exit=255
/// [Successful compilation of %SCRATCH%/witness.php]
/// prompt> 1
/// prompt> 0
/// prompt> [PHP Fatal error:  Uncaught ArgumentCountError: intdiv() expects exactly 2 arguments, 1 given in eval()'d code:1
/// Stack trace:
/// #0 eval()'d code(1): intdiv(1)
/// #1 {main}
///   thrown in eval()'d code on line 1]
/// prompt>
/// ```
///
/// Code that does not compile writes nothing: `zend_eval_stringl`
/// answers `FAILURE` with the `ParseError` left pending, which the next
/// `ev`, serving nothing while it stands, renders; `run` serves nothing
/// either and reports it uncaught.
///
/// ```php
/// $a = 1;
/// $b = 2;
/// echo "x\n";
/// ```
/// ```request phpdbg
/// ev 1 +
/// ev 2
/// ev 1 +
/// r
/// q
/// ```
/// ```expect exit=255
/// [Successful compilation of %SCRATCH%/witness.php]
/// prompt> prompt>%s
/// Parse error: syntax error, unexpected token ";" in eval()'d code on line 1
/// prompt> prompt> [Uncaught ParseError in eval()'d code on line 1]
/// ParseError: syntax error, unexpected token ";" in eval()'d code:1
/// Stack trace:
/// #0 {main}
/// [Script ended normally]
/// prompt>
/// ```
///
/// Thrown with a frame executing, it leaves the frame at its exception
/// op, so the run goes on from there with nothing pending and the frame
/// returns nothing.
///
/// ```php
/// $a = 1;
/// $b = 2;
/// echo "x\n";
/// ```
/// ```request phpdbg
/// b 3
/// r
/// ev 1 +
/// ev 2
/// ev 3
/// c
/// q
/// ```
/// ```expect exit=255
/// [Successful compilation of %SCRATCH%/witness.php]
/// prompt> [Breakpoint #0 added at %SCRATCH%/witness.php:3]
/// prompt> [Breakpoint #0 at %SCRATCH%/witness.php:3, hits: 1]
/// >00003: $b = 2;
///  00004: echo "x\n";
/// prompt> prompt>%s
/// Parse error: syntax error, unexpected token ";" in eval()'d code on line 1
/// prompt> 3
/// prompt> [Script ended normally]
/// prompt>
/// ```
///
/// A fatal raised inside what it serves bails out to its `zend_catch`,
/// which writes the status 0: the request goes on and the next `ev` is
/// served. Where nothing was executing, the bailout passed the
/// `phpdbg_execute_ex` that set `in_execution`, so `run` asks before it
/// restarts.
///
/// ```php
/// $a = 1;
/// $b = 2;
/// echo "x\n";
/// ```
/// ```request phpdbg
/// ev ini_set("memory_limit","2M")
/// ev str_repeat("x", 4<<20)
/// ev 5
/// q
/// ```
/// ```expect exit=0
/// [Successful compilation of %SCRATCH%/witness.php]
/// prompt> 128M
/// prompt>%s
/// Fatal error: Allowed memory size of 2097152 bytes exhausted (tried to allocate 4210688 bytes) in eval()'d code on line 1
/// prompt> prompt>
/// ```
///
/// ```php
/// $a = 1;
/// $b = 2;
/// echo "x\n";
/// ```
/// ```request phpdbg
/// ev ini_set("memory_limit","2M")
/// ev str_repeat("x", 4<<20)
/// r
/// q
/// ```
/// ```expect exit=0
/// [Successful compilation of %SCRATCH%/witness.php]
/// prompt> 128M
/// prompt>%s
/// Fatal error: Allowed memory size of 2097152 bytes exhausted (tried to allocate 4210688 bytes) in eval()'d code on line 1
/// prompt> Do you really want to restart execution? (type y or n): Please enter either y (yes) or n (no):
/// ```
fn ev(s: &mut Session, here: &mut Here<'_>, code: &[u8]) -> Ret {
    // `phpdbg_execute_ex` bails out at its entry while the cycle is being
    // cleaned, and `ev`'s `zend_catch` writes nothing.
    if s.init_cleaning {
        return Ret::Success;
    }
    let Some((req, under)) = here.serving() else {
        crate::host::idle(s, Command::Eval(code.to_vec()));
        return Ret::Success;
    };
    let stepping = std::mem::take(&mut s.stepping);
    crate::watch::freeze(s, req);
    s.origin = Origin::Eval;
    let evaluated = crate::session::lend(req, s, |req| tier::evaluate(req, under, code));
    // An exit or a fatal served where nothing was executing bailed out of
    // the `phpdbg_execute_ex` that `zend_execute` entered.
    if under == tier::Under::Nothing
        && (read::exiting(req) || evaluated == tier::Evaluated::BailedOut)
    {
        bailed_out(s);
    }
    s.origin = Origin::Program;
    crate::watch::thaw(s);
    s.stepping |= stepping;
    match evaluated {
        // `zend_print_zval_r` writes through the program's output, which
        // `PHPDBG_DISCARD_OUTPUT` does not hold; the newline is phpdbg's.
        tier::Evaluated::Printed(printed) => {
            s.out.program(&printed);
            s.out.out(b"\n");
        }
        // The compiler threw with the frame executing, which
        // `zend_throw_exception_internal` leaves at `EG(exception_op)`.
        tier::Evaluated::NotCompiled => {
            if let tier::Under::Frame(frame) = under {
                s.thrown_into = Some(frame);
            }
        }
        tier::Evaluated::Rendered | tier::Evaluated::BailedOut => {}
    }
    Ret::Success
}

/// The oplines of the frame the prompt stands in, and the frame.
fn seek_over(here: &mut Here<'_>, keep: impl Fn(Opcode, u32, u32) -> bool) -> Option<Seek> {
    let (req, frame) = here.standing()?;
    let position = read::position(req, frame)?;
    let oplines: BTreeSet<u32> = read::oplines(req, frame)
        .into_iter()
        .enumerate()
        .filter(|(_, (opcode, line))| keep(*opcode, *line, position.line))
        .filter_map(|(at, _)| u32::try_from(at).ok())
        .collect();
    Some(Seek {
        kind: SeekKind::Until,
        frame: frame.0,
        oplines,
    })
}

/// `phpdbg_skip_line_helper`.
fn skip_line(s: &mut Session, here: &mut Here<'_>) -> Ret {
    s.seek = seek_over(here, |opcode, line, current| {
        line != current || opcodes::is(opcode, opcodes::ENDS)
    });
    Ret::Resume
}

/// `PHPDBG_COMMAND(until)`.
fn until(s: &mut Session, here: &mut Here<'_>, _: &[Param]) -> Ret {
    if !here.executing() {
        s.out.error(b"Not executing");
        return Ret::Success;
    }
    skip_line(s, here)
}

/// `PHPDBG_COMMAND(next)`.
fn next(s: &mut Session, here: &mut Here<'_>, _: &[Param]) -> Ret {
    if !here.executing() {
        s.out.error(b"Not executing");
        return Ret::Success;
    }
    s.stepping = true;
    skip_line(s, here)
}

/// Whether the frame stands at an opline where its function ends, the
/// seek to it set where it does not.
fn seek_to_end(s: &mut Session, here: &mut Here<'_>, kind: SeekKind) -> bool {
    let Some(mut seek) = seek_over(here, |opcode, _, _| opcodes::is(opcode, opcodes::ENDS)) else {
        return true;
    };
    let at = here
        .standing()
        .and_then(|(req, frame)| read::position(req, frame))
        .map_or(0, |position| position.opline);
    if seek.oplines.contains(&at) {
        s.seek = None;
        return true;
    }
    seek.kind = kind;
    s.seek = Some(seek);
    false
}

/// `PHPDBG_COMMAND(finish)`.
fn finish(s: &mut Session, here: &mut Here<'_>, _: &[Param]) -> Ret {
    if !here.executing() {
        s.out.error(b"Not executing");
        return Ret::Success;
    }
    seek_to_end(s, here, SeekKind::Finish);
    Ret::Resume
}

/// `PHPDBG_COMMAND(leave)`.
fn leave(s: &mut Session, here: &mut Here<'_>, _: &[Param]) -> Ret {
    if !here.executing() {
        s.out.error(b"Not executing");
        return Ret::Success;
    }
    if seek_to_end(s, here, SeekKind::Leave) {
        s.out.notice(b"Already at the end of the function");
        return Ret::Success;
    }
    Ret::Resume
}

/// `PHPDBG_COMMAND(print)`: the stack with nothing, a function or a
/// method by name.
fn print(s: &mut Session, here: &mut Here<'_>, params: &[Param]) -> Ret {
    match params.first() {
        None => print_stack(s, here, params),
        Some(Param::Str(_)) => print_func(s, here, params),
        Some(Param::Method { .. }) => print_method(s, here, params),
        Some(_) => {
            s.out.error(
                b"Invalid arguments to print, expected nothing, function name or method name",
            );
            Ret::Success
        }
    }
}

fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// `PHPDBG_PRINT(exec)`. In the init files, before php's cycle has
/// compiled the context, `phpdbg_compile` compiles it here first and
/// writes its notice, once a cycle.
fn print_exec(s: &mut Session, here: &mut Here<'_>, _: &[Param]) -> Ret {
    let Some(exec) = s.exec.clone() else {
        s.out.error(b"No execution context set");
        return Ret::Success;
    };
    // The compiler refuses the context again, its log written again.
    if s.context == Context::Refused {
        refused_again(s, here);
        return Ret::Success;
    }
    let Some(req) = here.request() else {
        // After a `run` in the init files, no request stands, and
        // `phpdbg_compile` compiles the context again.
        if s.init_cleaning
            && let Some(code) = s.ran_code.clone()
        {
            compiled_in_init(s);
            s.out
                .notice(format!("Context {} ({} ops)", lossy(&exec), code.ops.len()).as_bytes());
            out::dump(&code.dump);
        }
        return Ret::Success;
    };
    compiled_in_init(s);
    let entry = tier::entry(req);
    let main = main_frame(req, entry);
    if let Some(code) = read::frame_code(req, main) {
        s.out
            .notice(format!("Context {} ({} ops)", lossy(&exec), code.ops.len()).as_bytes());
        out::dump(&code.dump);
    }
    Ret::Success
}

/// `phpdbg_compile` where the compiler refuses the context: the context
/// compiled again in the request the prompt stands in ([`tier::compile`]),
/// its diagnostics displayed and logged, and where its fatal was logged,
/// the listing where nothing executes and, once the session is fully
/// started, a prompt inside the log, under which nothing more is logged;
/// and the status 255. What the request's earlier compiles left stands
/// for it as php's one request keeps it: the record a refusal leaves
/// standing, emitted ahead of the next one's fatal, and the functions it
/// had declared, which the next meets as `Cannot redeclare`. Inside the
/// log already, nothing is logged, and the refusal bails out of the log
/// with its guard left set, and of the prompt inside it.
///
/// ```php
/// $a = 1;
/// $b =
/// ```
/// ```request phpdbg
/// print exec
/// c
/// print exec
/// ev 1
/// c
/// q
/// ```
/// ```expect exit=255
/// Parse error: syntax error, unexpected end of file in %SCRATCH%/witness.php on line 3
/// prompt>%s
/// Parse error: syntax error, unexpected end of file in %SCRATCH%/witness.php on line 3
/// prompt> [Not running]
/// prompt>%s
/// Parse error: syntax error, unexpected end of file in %SCRATCH%/witness.php on line 3
/// prompt> 1
/// prompt> [Not running]
/// prompt>
/// ```
pub fn refused_again(s: &mut Session, here: &mut Here<'_>) {
    let (Some(exec), Some(req)) = (s.exec.clone(), here.request()) else {
        return;
    };
    // A refusal inside the log bails out of it, past the line that
    // clears `PG(in_error_log)`, and of the prompt inside it.
    let logging = s.out.logging();
    let in_log_prompt = s.out.in_log_prompt();
    let code = s.stdin_code.clone();
    // A file is not opened while the init files leave something pending,
    // so nothing compiles and nothing is displayed but phpdbg's error.
    let compiled = if code.is_none() && read::open_refused(req) {
        tier::Compiled::Unopened
    } else {
        crate::session::lend(req, s, |req| tier::compile(req, &exec, code.as_deref()))
    };
    match compiled {
        tier::Compiled::Refused if logging => {
            s.left_log = in_log_prompt;
            s.out.stick_log();
        }
        tier::Compiled::Refused => {}
        tier::Compiled::Compiled => crate::debugger::announce(s),
        tier::Compiled::Unopened => s
            .out
            .error(format!("Could not open file {}", String::from_utf8_lossy(&exec)).as_bytes()),
    }
}

/// `phpdbg_compile`'s notice where the init files compile the context,
/// `PHPDBG_G(ops)` being empty until they have: once a cycle, whatever
/// the cycle's own compile writes after them.
pub fn compiled_in_init(s: &mut Session) {
    if !s.initializing || std::mem::replace(&mut s.init_compiled, true) {
        return;
    }
    if let Some(exec) = &s.exec {
        let message = if s.stdin_code.is_some() {
            b"Successful compilation of stdin input".to_vec()
        } else {
            [&b"Successful compilation of "[..], exec].concat()
        };
        s.out.notice(&message);
    }
}

/// The bottom frame of the chain from `frame`, past any native between
/// two of its frames: the main op array's, `PHPDBG_G(ops)`, which `print
/// exec` names while the run stands in a callback `array_map` called.
///
/// ```php
/// function cb($x) {
///   return $x;
/// }
/// array_map("cb", [1]);
/// ```
/// ```request phpdbg
/// b cb
/// r
/// print exec
/// q
/// ```
/// ```expect
/// [Successful compilation of %SCRATCH%/witness.php]
/// prompt> [Breakpoint #0 added at cb]
/// prompt> [Breakpoint #0 in cb() at %SCRATCH%/witness.php:3, hits: 1]
/// >00003:   return $x;
///  00004: }
///  00005: array_map("cb", [1]);
/// prompt> [Context %SCRATCH%/witness.php (5 ops)]
/// prompt>
/// ```
/// ```stderr
///
/// $_main:
///      ; (lines=5, args=0, vars=0, tmps=1)
///      ; %SCRATCH%/witness.php:1-5
/// L0005 0000 INIT_FCALL 2 112 string("array_map")
/// L0005 0001 SEND_VAL string("cb") 1
/// L0005 0002 SEND_VAL array(...) 2
/// L0005 0003 DO_ICALL
/// L0005 0004 RETURN int(1)
/// ```
fn main_frame(req: &Request, frame: FrameRef) -> FrameRef {
    let mut at = frame;
    while let Some(below) = tier::below(req, at) {
        at = below;
    }
    at
}

/// `PHPDBG_PRINT(opline)`: the opline the run stands at, as
/// `zend_dump_op_line` writes it, to stderr.
///
/// ```php
/// function f($x) {
///   $y = $x + 1;
///   return $y;
/// }
/// echo f(1), "\n";
/// echo "a\n";
/// ```
/// ```request phpdbg
/// p o
/// b f
/// r
/// p o
/// q
/// ```
/// ```expect
/// [Successful compilation of %SCRATCH%/witness.php]
/// prompt> [Not Executing!]
/// prompt> [Breakpoint #0 added at f]
/// prompt> [Breakpoint #0 in f() at %SCRATCH%/witness.php:3, hits: 1]
/// >00003:   $y = $x + 1;
///  00004:   return $y;
///  00005: }
/// prompt> prompt>
/// ```
/// ```stderr
/// L0003 0001 T2 = ADD CV0($x) int(1)
/// ```
fn print_opline(s: &mut Session, here: &mut Here<'_>, _: &[Param]) -> Ret {
    let executing = here.executing();
    match here.standing() {
        Some((req, frame)) if executing => print_opline_ex(req, frame),
        _ => s.out.error(b"Not Executing!"),
    }
    Ret::Success
}

/// `phpdbg_print_opline`'s `zend_dump_op_line`: the opline `frame`
/// stands at, numbered and with its line, written to stderr, where
/// `print opline` asks for it and where the loop reaches it stepping
/// with quietness off.
pub fn print_opline_ex(req: &Request, frame: FrameRef) {
    if let (Some(code), Some(position)) = (read::frame_code(req, frame), read::position(req, frame))
        && let Some(line) = opline_line(&code.dump, position.opline)
    {
        out::dump(&[line, b"\n"].concat());
    }
}

/// The dump's line for the opline numbered `at`, `L0015 0001 ...`.
fn opline_line(dump: &[u8], at: u32) -> Option<&[u8]> {
    let wanted = format!(" {at:04} ");
    dump.split(|b| *b == b'\n')
        .find(|line| line.first() == Some(&b'L') && line.get(5..11) == Some(wanted.as_bytes()))
}

/// `PHPDBG_PRINT(class)`.
fn print_class(s: &mut Session, here: &mut Here<'_>, params: &[Param]) -> Ret {
    let Some(Param::Str(name)) = params.first() else {
        return Ret::Failure;
    };
    let class = here.request().and_then(|req| read::class_named(req, name));
    let Some(class) = class else {
        s.out
            .error(format!("The class {} could not be found", lossy(name)).as_bytes());
        return Ret::Success;
    };
    let shape = match class.shape {
        read::Shape::Interface => "Interface",
        read::Shape::AbstractClass => "Abstract Class",
        read::Shape::Class => "Class",
    };
    s.out.notice(
        format!(
            "{} {shape}: {} ({} methods)",
            if class.user { "User" } else { "Internal" },
            lossy(&class.name),
            class.methods.len()
        )
        .as_bytes(),
    );
    for method in &class.methods {
        print_function(s, method);
    }
    Ret::Success
}

/// `phpdbg_print_function_helper`.
fn print_function(s: &mut Session, code: &read::Code) {
    if code.user {
        out::dump(&code.dump);
        return;
    }
    let line = match &code.class {
        Some(class) => format!("\tInternal {}::{}()", lossy(class), lossy(&code.name)),
        None => format!("\tInternal {}()", lossy(&code.name)),
    };
    s.out.writeln(line.as_bytes());
}

/// `PHPDBG_PRINT(method)`.
fn print_method(s: &mut Session, here: &mut Here<'_>, params: &[Param]) -> Ret {
    let Some(Param::Method { class, name }) = params.first() else {
        return Ret::Failure;
    };
    let found = here.request().map_or(read::Named::NoClass, |req| {
        read::method_named(req, class, name)
    });
    match found {
        read::Named::Code(code) => {
            s.out.notice(
                format!(
                    "{} Method {} ({} ops)",
                    if code.user { "User" } else { "Internal" },
                    lossy(&code.name),
                    code.ops.len()
                )
                .as_bytes(),
            );
            print_function(s, &code);
        }
        read::Named::NoFunction => s.out.error(
            format!(
                "The method {}::{} could not be found",
                lossy(class),
                lossy(name)
            )
            .as_bytes(),
        ),
        read::Named::NoClass => {
            s.out
                .error(format!("The class {} could not be found", lossy(class)).as_bytes());
        }
    }
    Ret::Success
}

/// `PHPDBG_PRINT(func)`.
fn print_func(s: &mut Session, here: &mut Here<'_>, params: &[Param]) -> Ret {
    let Some(Param::Str(name)) = params.first() else {
        return Ret::Failure;
    };
    let found = here.request().map_or(read::Named::NoFunction, |req| {
        read::function_named(req, name)
    });
    match found {
        read::Named::Code(code) => {
            s.out.notice(
                format!(
                    "{} {} {} ({} ops)",
                    if code.user { "User" } else { "Internal" },
                    if code.class.is_some() {
                        "Method"
                    } else {
                        "Function"
                    },
                    lossy(&code.name),
                    code.ops.len()
                )
                .as_bytes(),
            );
            print_function(s, &code);
        }
        read::Named::NoFunction | read::Named::NoClass => {
            s.out
                .error(format!("The function {} could not be found", lossy(name)).as_bytes());
        }
    }
    Ret::Success
}

/// `PHPDBG_PRINT(stack)`.
fn print_stack(s: &mut Session, here: &mut Here<'_>, _: &[Param]) -> Ret {
    let executing = here.executing();
    let Some((req, frame)) = here.standing().filter(|_| executing) else {
        s.out.error(b"Not Executing!");
        return Ret::Success;
    };
    let Some(code) = read::frame_code(req, frame) else {
        return Ret::Success;
    };
    let title = match (&code.class, code.name.is_empty()) {
        (_, true) => format!("Stack in {} ({} ops)", lossy(&code.file), code.ops.len()),
        (Some(class), false) => format!(
            "Stack in {}::{}() ({} ops)",
            lossy(class),
            lossy(&code.name),
            code.ops.len()
        ),
        (None, false) => format!("Stack in {}() ({} ops)", lossy(&code.name), code.ops.len()),
    };
    s.out.notice(title.as_bytes());
    print_function(s, &code);
    Ret::Success
}

/// `PHPDBG_COMMAND(break)`. With nothing named, the line standing, which
/// at the held prompt, where nothing does, is `[no active file]`'s line
/// zero, pending.
///
/// ```php
/// $a = 1;
/// ```
/// ```request phpdbg
/// b
/// b
/// q
/// ```
/// ```expect
/// [Successful compilation of %SCRATCH%/witness.php]
/// prompt> [Pending breakpoint #0 added at [no active file]:0]
/// prompt> [Breakpoint at [no active file]:0 exists]
/// prompt>
/// ```
fn breakpoint(s: &mut Session, here: &mut Here<'_>, params: &[Param]) -> Ret {
    let executing = here.executing();
    let current = here
        .standing()
        .filter(|_| executing)
        .and_then(|(req, frame)| read::position(req, frame))
        .map(|position| (position.file, position.line));
    match params.first() {
        None => match (&s.exec, current) {
            (Some(_), Some((file, line))) => {
                s.breaks
                    .set_file(&mut s.out, Some(file), &[], i64::from(line));
            }
            (Some(_), None) => crate::host::file_break(s, NO_ACTIVE_FILE, 0),
            (None, _) => s.out.error(b"Execution context not set!"),
        },
        Some(Param::Num(line)) => match &s.exec {
            Some(exec) => {
                let file = current.map_or_else(|| exec.clone(), |(file, _)| file);
                s.breaks.set_file(&mut s.out, Some(file), &[], *line);
            }
            None => s.out.error(b"Execution context not set!"),
        },
        Some(Param::File { name, line }) => crate::host::file_break(s, name, *line),
        Some(Param::Method { class, name }) => s.breaks.set_method(&mut s.out, class, name),
        Some(Param::NumericMethod { class, name, num }) => opline_break(
            s,
            here,
            Kind::MethodOpline {
                class: class.clone(),
                name: name.clone(),
                opline: opline_of(*num),
            },
        ),
        Some(Param::NumericFunction { name, num }) => opline_break(
            s,
            here,
            Kind::FunctionOpline {
                name: name.clone(),
                opline: opline_of(*num),
            },
        ),
        Some(Param::NumericFile { name, line }) => opline_break(
            s,
            here,
            Kind::FileOpline {
                file: name.clone(),
                opline: opline_of(*line),
            },
        ),
        Some(Param::Str(name)) => s.breaks.set_symbol(&mut s.out, name),
        Some(Param::Op(name)) => s.breaks.set_opcode(&mut s.out, name),
        Some(_) => {}
    }
    Ret::Success
}

/// The opline a `#N` names, as php's `zend_ulong` holds it.
fn opline_of(num: i64) -> u32 {
    u32::try_from(num).unwrap_or(u32::MAX)
}

/// `phpdbg_set_breakpoint_function_opline`, `_method_opline` and
/// `_file_opline`: the breakpoint's number taken, then
/// `phpdbg_resolve_opline_break`, which refuses it with its number spent,
/// resolves it, or leaves it pending; then the breakpoint set
/// ([`crate::bp::Breakpoints::set_named_opline`]). A file's is resolved
/// at once against the frames executing, its own code's under the name
/// as typed, and is pending where nothing executes, as php's compare
/// with the compiled context never matches.
///
/// ```php
/// function f() {
///   return 1;
/// }
/// f();
/// ```
/// ```request phpdbg
/// b witness.php:#0
/// b f
/// r
/// b witness.php:#0
/// b f#9
/// b f#0
/// q
/// ```
/// ```expect
/// [Successful compilation of %SCRATCH%/witness.php]
/// prompt> [Pending breakpoint #0 at witness.php:0]
/// prompt> [Breakpoint #1 added at f]
/// prompt> [Breakpoint #1 in f() at %SCRATCH%/witness.php:3, hits: 1]
/// >00003:   return 1;
///  00004: }
///  00005: f();
/// prompt> [Pending breakpoint #2 at witness.php:0]
/// [Breakpoint already exists for witness.php:0]
/// prompt> [There are only 2 oplines in function f (breaking at opline 9 impossible)]
/// prompt> [Breakpoint #3 added at f#0]
/// prompt>
/// ```
fn opline_break(s: &mut Session, here: &mut Here<'_>, kind: Kind) {
    let id = s.breaks.next_id();
    let resolved = match &kind {
        Kind::FileOpline { file, opline } => match executing_code(here, file) {
            Some(last) if last <= *opline => Err(format!(
                "There are only {last} oplines in file {} (breaking at opline {opline} impossible)",
                lossy(file)
            )),
            Some(_) => {
                let op = OpArray {
                    file,
                    function: None,
                    class: None,
                };
                Ok(Resolution::At(crate::bp::address(op, *opline)))
            }
            None => Ok(Resolution::Pending),
        },
        Kind::FunctionOpline { name, opline } => named_opline(here, None, name, *opline),
        Kind::MethodOpline {
            class,
            name,
            opline,
        } => named_opline(here, Some(class), name, *opline),
        _ => return,
    };
    match resolved {
        Ok(found) => s.breaks.set_named_opline(&mut s.out, id, kind, found),
        Err(refused) => s.out.error(refused.as_bytes()),
    }
}

/// `phpdbg_resolve_opline_break`'s walk for a file: the frames from the
/// one executing down to the first serving the code of the file `name`,
/// and how many oplines that code holds; none where nothing executes or
/// no frame serves it.
fn executing_code(here: &mut Here<'_>, name: &[u8]) -> Option<u32> {
    let (req, frame) = here.standing()?;
    let mut at = Some(frame);
    while let Some(frame) = at {
        if read::serves(req, frame) == read::Serves::Code
            && let Some(position) = read::position(req, frame)
            && position.file == name
        {
            return Some(position.oplines);
        }
        at = tier::below(req, frame);
    }
    None
}

/// `phpdbg_resolve_opline_break` for a function or a method: the opline
/// where the function exists, pending where it does not yet, and the
/// error where it is refused.
fn named_opline(
    here: &mut Here<'_>,
    class: Option<&Vec<u8>>,
    name: &[u8],
    opline: u32,
) -> Result<Resolution, String> {
    let found = match here.request() {
        Some(req) => match class {
            Some(class) => read::method_named(req, class, name),
            None => read::function_named(req, name),
        },
        None => read::Named::NoClass,
    };
    let message = match found {
        read::Named::Code(code) if !code.user => match class {
            Some(class) => format!(
                "{}::{} is not a user defined method, no oplines exist",
                lossy(class),
                lossy(name)
            ),
            None => format!(
                "{} is not a user defined function, no oplines exist",
                lossy(name)
            ),
        },
        read::Named::Code(code) if code.ops.len() <= opline as usize => match class {
            Some(class) => format!(
                "There are only {} oplines in method {}::{} (breaking at opline {opline} impossible)",
                code.ops.len(),
                lossy(class),
                lossy(name)
            ),
            None => format!(
                "There are only {} oplines in function {} (breaking at opline {opline} impossible)",
                code.ops.len(),
                lossy(name)
            ),
        },
        read::Named::Code(code) => {
            let op = OpArray {
                file: &code.file,
                function: Some(&code.name),
                class: code.class.as_deref(),
            };
            return Ok(Resolution::At(crate::bp::address(op, opline)));
        }
        read::Named::NoFunction if class.is_some() => format!(
            "Method {} doesn't exist in class {}",
            lossy(name),
            class.map(|c| lossy(c)).unwrap_or_default()
        ),
        read::Named::NoFunction | read::Named::NoClass => return Ok(Resolution::Pending),
    };
    Err(message)
}

/// `PHPDBG_BREAK(del)`.
fn break_del(s: &mut Session, _: &mut Here<'_>, params: &[Param]) -> Ret {
    if let Some(Param::Num(id)) = params.first() {
        s.breaks.delete(&mut s.out, *id);
    }
    Ret::Success
}

/// `PHPDBG_COMMAND(back)`: from the frame the commands read.
fn back(s: &mut Session, here: &mut Here<'_>, params: &[Param]) -> Ret {
    let executing = here.executing();
    let above = here.frame_num();
    let Some((req, frame)) = here.standing().filter(|_| executing) else {
        s.out.error(b"Not executing!");
        return Ret::Success;
    };
    let limit = match params.first() {
        Some(Param::Num(n)) => usize::try_from(*n).unwrap_or(0),
        _ => 0,
    };
    crate::info::backtrace(&mut s.out, req, frame, above, limit);
    Ret::Success
}

/// `PHPDBG_COMMAND(frame)`: with no number, the frame the commands read;
/// with one, `phpdbg_switch_frame`: the frame that many below the one the
/// run stopped in made the one `ev`, `info vars`, `back` and the listings
/// read, said so with its call and listed, until the run goes on.
///
/// ```php
/// function g() {
///     return 1;
/// }
/// function m() {
///     $local = 2;
///     g();
///     echo $local, "\n";
/// }
/// m();
/// ```
/// ```request phpdbg
/// b g
/// r
/// frame 1
/// ev $local = 5
/// c
/// q
/// ```
/// ```expect
/// [Successful compilation of %SCRATCH%/witness.php]
/// prompt> [Breakpoint #0 added at g]
/// prompt> [Breakpoint #0 in g() at %SCRATCH%/witness.php:3, hits: 1]
/// >00003:     return 1;
///  00004: }
///  00005: function m() {
/// prompt> [Switched to frame #1: m() at %SCRATCH%/witness.php:7]
/// >00007:     g();
///  00008:     echo $local, "\n";
///  00009: }
/// prompt> 5
/// prompt> 5
/// [Script ended normally]
/// prompt>
/// ```
///
/// The frame of a file's code is named by its number alone, a native's
/// call between two frames is passed over, and the count is from the
/// frame the run stopped in whichever one the commands read.
///
/// ```php
/// function g($x) {
///     return $x;
/// }
/// array_map(function ($v) { return g($v); }, [3]);
/// ```
/// ```request phpdbg
/// b g
/// r
/// frame
/// frame 2
/// frame
/// frame 1
/// frame 3
/// frame 0
/// frame 0
/// q
/// ```
/// ```expect
/// [Successful compilation of %SCRATCH%/witness.php]
/// prompt> [Breakpoint #0 added at g]
/// prompt> [Breakpoint #0 in g() at %SCRATCH%/witness.php:3, hits: 1]
/// >00003:     return $x;
///  00004: }
///  00005: array_map(function ($v) { return g($v); }, [3]);
/// prompt> [Currently in frame #0]
/// prompt> [Switched to frame #2]
/// >00005: array_map(function ($v) { return g($v); }, [3]);
/// prompt> [Currently in frame #2]
/// prompt> [Switched to frame #1: {closure:%SCRATCH%/witness.php:5}(v=3) at %SCRATCH%/witness.php:5]
/// >00005: array_map(function ($v) { return g($v); }, [3]);
/// prompt> [No frame #3]
/// prompt> [Switched to frame #0: g(x=3) at %SCRATCH%/witness.php:3]
/// >00003:     return $x;
///  00004: }
///  00005: array_map(function ($v) { return g($v); }, [3]);
/// prompt> [Already in frame #0]
/// prompt>
/// ```
///
/// `info vars` and `back` read the frame switched to, and a step goes
/// on from the frame the run stopped in, the frame read again.
///
/// ```php
/// function g() {
///     return 1;
/// }
/// function m() {
///     $v = 4;
///     g();
///     echo "m\n";
/// }
/// m();
/// ```
/// ```request phpdbg
/// b g
/// r
/// frame 1
/// i v
/// back
/// s
/// frame
/// c
/// q
/// ```
/// ```expect
/// [Successful compilation of %SCRATCH%/witness.php]
/// prompt> [Breakpoint #0 added at g]
/// prompt> [Breakpoint #0 in g() at %SCRATCH%/witness.php:3, hits: 1]
/// >00003:     return 1;
///  00004: }
///  00005: function m() {
/// prompt> [Switched to frame #1: m() at %SCRATCH%/witness.php:7]
/// >00007:     g();
///  00008:     echo "m\n";
///  00009: }
/// prompt> [Variables in m() (1)]
/// Address            Refs    Type      Variable
/// 0x%s1       int       $v
/// int (4)
/// prompt> frame #0: m() at %SCRATCH%/witness.php:7
/// frame #1: {main} at %SCRATCH%/witness.php:10
/// prompt> >00008:     echo "m\n";
///  00009: }
///  00010: m();
/// prompt> [Currently in frame #0]
/// prompt> m
/// [Script ended normally]
/// prompt>
/// ```
fn frame(s: &mut Session, here: &mut Here<'_>, params: &[Param]) -> Ret {
    let num = here.frame_num();
    let Some(Param::Num(asked)) = params.first() else {
        s.out
            .notice(format!("Currently in frame #{num}").as_bytes());
        return Ret::Success;
    };
    if i64::from(num) == *asked {
        s.out
            .notice(format!("Already in frame #{asked}").as_bytes());
        return Ret::Success;
    }
    let Here::Paused {
        req,
        frame,
        switched,
    } = here
    else {
        s.out.error(format!("No frame #{asked}").as_bytes());
        return Ret::Success;
    };
    let stopped = switched.map_or(*frame, |switched| switched.stopped);
    let target = u32::try_from(*asked).ok().and_then(|to| {
        let mut at = Some(stopped);
        for _ in 0..to {
            at = tier::below(req, at?);
        }
        at.map(|at| (to, at))
    });
    let Some((to, target)) = target else {
        s.out.error(format!("No frame #{asked}").as_bytes());
        return Ret::Success;
    };
    *frame = target;
    *switched = (to > 0).then_some(Switched { num: to, stopped });
    let switched = format!("Switched to frame #{to}");
    match crate::info::stackframe(req, target, to) {
        Some(call) => s.out.notice(format!("{switched}: {call}").as_bytes()),
        None => s.out.notice(switched.as_bytes()),
    }
    if let Some(position) = read::position(req, target) {
        crate::list::code(s, &position.file, position.line);
    }
    Ret::Success
}

/// `PHPDBG_COMMAND(list)` and `PHPDBG_LIST(lines)`.
fn list(s: &mut Session, here: &mut Here<'_>, params: &[Param]) -> Ret {
    let executing = here.executing();
    let current = here
        .standing()
        .and_then(|(req, frame)| read::position(req, frame));
    let (file, line) = if let (Some(position), true) = (&current, executing) {
        (position.file.clone(), position.line)
    } else {
        let Some(exec) = &s.exec else {
            s.out.error(b"Not executing, and execution context not set");
            return Ret::Success;
        };
        (exec.clone(), 0)
    };
    match params.first() {
        None => crate::list::file(s, &file, 0, i64::from(line), 0),
        Some(Param::Num(n)) => {
            let count = if *n < 0 { 1 - *n } else { *n };
            let offset = if *n < 0 { *n } else { 0 } + i64::from(line);
            crate::list::file(s, &file, count, offset, 0);
        }
        Some(Param::File { name, line }) => {
            crate::list::file(s, &crate::host::real_path(name), *line, 0, 0);
        }
        Some(_) => {}
    }
    Ret::Success
}

/// `PHPDBG_COMMAND(info)` with no sub-command: the execution context's
/// overview (`Z/sapi/phpdbg/phpdbg_prompt.c:1070-1109`), the build's
/// line editor as the oracle's (libedit, no readline), the main op
/// array's sizes where it is compiled, and the tables' sizes as the
/// engine holds them.
///
/// ```php
/// function f() {}
/// $a = 1;
/// f();
/// ```
/// ```request phpdbg
/// b f
/// r
/// info
/// q
/// ```
/// ```expect
/// [Successful compilation of %SCRATCH%/witness.php]
/// prompt> [Breakpoint #0 added at f]
/// prompt> [Breakpoint #0 in f() at %SCRATCH%/witness.php:2, hits: 1]
/// >00002: function f() {}
///  00003: $a = 1;
///  00004: f();
/// prompt> Execution Context Information
///
/// Readline   no
/// Libedit    yes
/// Exec       %SCRATCH%/witness.php
/// Compiled   yes
/// Stepping   off
/// Quietness  on
/// Opcodes    4
/// Variables  0
/// Executing  yes
/// VM Return  1
/// Classes    %d
/// Functions  %d
/// Constants  %d
/// Included   0
/// prompt>
/// ```
fn info_overview(s: &mut Session, here: &mut Here<'_>, _: &[Param]) -> Ret {
    s.out.out(b"Execution Context Information\n\n");
    s.out.writeln(b"Readline   no");
    s.out.writeln(b"Libedit    yes");
    let exec = s
        .exec
        .as_deref()
        .map_or_else(|| String::from("none"), lossy);
    s.out.writeln(format!("Exec       {exec}").as_bytes());
    let main = if here.compiled() {
        here.request().and_then(|req| {
            let entry = tier::entry(req);
            read::frame_code(req, main_frame(req, entry))
        })
    } else {
        None
    };
    let on = |on: bool| if on { "on" } else { "off" };
    let yes = |yes: bool| if yes { "yes" } else { "no" };
    s.out
        .writeln(format!("Compiled   {}", yes(main.is_some())).as_bytes());
    s.out
        .writeln(format!("Stepping   {}", on(s.stepping)).as_bytes());
    s.out
        .writeln(format!("Quietness  {}", on(s.quiet)).as_bytes());
    if let Some(main) = &main {
        s.out
            .writeln(format!("Opcodes    {}", main.ops.len()).as_bytes());
        s.out
            .writeln(format!("Variables  {}", main.vars.saturating_sub(1)).as_bytes());
    }
    let executing = in_execution(s, here);
    s.out
        .writeln(format!("Executing  {}", yes(executing)).as_bytes());
    if executing {
        s.out.writeln(format!("VM Return  {}", s.vmret).as_bytes());
    }
    let [classes, functions, constants, included] =
        here.request().map_or([0; 4], |req| read::table_sizes(req));
    s.out.writeln(format!("Classes    {classes}").as_bytes());
    s.out.writeln(format!("Functions  {functions}").as_bytes());
    s.out.writeln(format!("Constants  {constants}").as_bytes());
    s.out.writeln(format!("Included   {included}").as_bytes());
    Ret::Success
}

/// `PHPDBG_COMMAND(clean)`.
fn clean(s: &mut Session, here: &mut Here<'_>, _: &[Param]) -> Ret {
    if in_execution(s, here)
        && !permitted(s, b"Do you really want to clean your current environment?")
    {
        return Ret::Success;
    }
    s.out.out(b"Cleaning Execution Environment\n");
    let [classes, functions, constants, includes] =
        here.request().map_or([0; 4], |req| read::table_sizes(req));
    s.out.writeln(format!("Classes    {classes}").as_bytes());
    s.out.writeln(format!("Functions  {functions}").as_bytes());
    s.out.writeln(format!("Constants  {constants}").as_bytes());
    s.out.writeln(format!("Includes   {includes}").as_bytes());
    // `phpdbg_clean(1, 0)` lets the line the cycle parsed last go.
    s.command_line = None;
    s.stopped = Some(Stop::Clean);
    Ret::Success
}

/// `PHPDBG_COMMAND(clear)`.
fn clear(s: &mut Session, _: &mut Here<'_>, _: &[Param]) -> Ret {
    s.breaks.clear(&mut s.out);
    Ret::Success
}

/// `PHPDBG_COMMAND(help)`, as far as a topic nobody has.
fn help(s: &mut Session, _: &mut Here<'_>, params: &[Param]) -> Ret {
    if let Some(Param::Str(topic)) = params.first()
        && !ROOT.iter().any(|def| {
            def.name.as_bytes().starts_with(topic.as_slice())
                || (topic.len() == 1 && def.alias == Some(topic[0]))
        })
    {
        s.out
            .error(format!("No help topic found for {}", lossy(topic)).as_bytes());
    }
    Ret::Success
}

/// `PHPDBG_COMMAND(set)`: a word no row of `set` names.
fn set(s: &mut Session, _: &mut Here<'_>, _: &[Param]) -> Ret {
    s.out.error(b"No set command selected!");
    Ret::Success
}

/// `PHPDBG_SET(prompt)`: the prompt as the prompt shows it, or its text
/// from here, a byte past ASCII shown as `?` as `phpdbg_prompt()` sets it.
///
/// ```php
/// echo "x\n";
/// ```
/// ```request phpdbg
/// set prompt
/// set prompt dbg>
/// r
/// q
/// ```
/// ```expect
/// [Successful compilation of %SCRATCH%/witness.php]
/// prompt> Current prompt: prompt>%s
/// prompt> dbg> x
/// [Script ended normally]
/// dbg>
/// ```
fn set_prompt(s: &mut Session, _: &mut Here<'_>, params: &[Param]) -> Ret {
    match params.first() {
        Some(Param::Str(text)) => {
            s.prompt = text
                .iter()
                .map(|b| if *b & 0x80 == 0 { *b } else { b'?' })
                .collect();
        }
        _ => s
            .out
            .writeln(&[&b"Current prompt: "[..], &s.prompt, b" "].concat()),
    }
    Ret::Success
}

/// `PHPDBG_SET(color)`: the element's colour by the table's name for it,
/// the notice naming both.
///
/// ```php
/// echo "x\n";
/// ```
/// ```request phpdbg
/// set color error nope
/// set color foo red
/// set colors on
/// set color notice red
/// r
/// q
/// ```
/// ```expect
/// [Successful compilation of %SCRATCH%/witness.php]
/// prompt> [Failed to find the requested color (nope)]
/// prompt> [Failed to find the requested element (foo)]
/// prompt> prompt> %s[setting notice color to red (0;31)]%s
/// prompt> x
/// %s0;31m[Script ended normally]%s
/// prompt>
/// ```
fn set_color(s: &mut Session, _: &mut Here<'_>, params: &[Param]) -> Ret {
    let [Param::Str(element), Param::Str(name), ..] = params else {
        return Ret::Failure;
    };
    let Some(colour) = out::find_colour(name) else {
        s.out
            .error(format!("Failed to find the requested color ({})", lossy(name)).as_bytes());
        return Ret::Success;
    };
    let Some(element) = out::Element::named(element) else {
        s.out
            .error(format!("Failed to find the requested element ({})", lossy(element)).as_bytes());
        return Ret::Success;
    };
    let named = match element {
        out::Element::Prompt => "prompt",
        out::Element::Error => "error",
        out::Element::Notice => "notice",
    };
    s.out
        .notice(format!("setting {named} color to {} ({})", colour.name, colour.code).as_bytes());
    s.out.set_colour(element, colour);
    Ret::Success
}

/// `PHPDBG_SET(colors)`: whether notices and errors are coloured, or
/// which they are.
///
/// ```php
/// echo "x\n";
/// ```
/// ```request phpdbg
/// set colors
/// set colors on
/// set colors
/// set colors off
/// r
/// q
/// ```
/// ```expect
/// [Successful compilation of %SCRATCH%/witness.php]
/// prompt> Colors off
/// prompt> prompt> Colors on
/// prompt> prompt> x
/// [Script ended normally]
/// prompt>
/// ```
fn set_colors(s: &mut Session, _: &mut Here<'_>, params: &[Param]) -> Ret {
    match params.first() {
        None => {
            let shown = match s.out.colours() {
                out::Colours::On => "on",
                out::Colours::Off => "off",
            };
            s.out.writeln(format!("Colors {shown}").as_bytes());
        }
        Some(Param::Num(on)) => s.out.set_colours(if *on != 0 {
            out::Colours::On
        } else {
            out::Colours::Off
        }),
        Some(_) => s
            .out
            .error(b"set colors used incorrectly: set colors <on|off>"),
    }
    Ret::Success
}

/// `PHPDBG_SET(break)`: one breakpoint by its number enabled or
/// disabled, or whether it is. A disabled one is listed so and stops
/// nothing; the `break` lines a cycle's end exports it as say nothing
/// of that, so the next cycle has it enabled again.
///
/// ```php
/// function foo() { echo "foo\n"; }
/// foo();
/// echo "end\n";
/// ```
/// ```request phpdbg
/// b foo
/// set break 0
/// set break 0 off
/// set break 0
/// set break 5
/// set break 5 off
/// set break x
/// info break
/// r
/// info break
/// r
/// q
/// ```
/// ```expect
/// [Successful compilation of %SCRATCH%/witness.php]
/// prompt> [Breakpoint #0 added at foo]
/// prompt> Breakpoint #0 on
/// prompt> prompt> Breakpoint #0 off
/// prompt> [Failed to find breakpoint #5]
/// prompt> prompt> [set break used incorrectly: set break [id] <on|off>]
/// prompt> ------------------------------------------------
/// Function Breakpoints:
/// #0%sfoo [disabled]
/// prompt> foo
/// end
/// [Script ended normally]
/// prompt> ------------------------------------------------
/// Function Breakpoints:
/// #0%sfoo
/// prompt> [Breakpoint #0 in foo() at %SCRATCH%/witness.php:2, hits: 1]
/// >00002: function foo() { echo "foo\n"; }
///  00003: foo();
///  00004: echo "end\n";
/// prompt>
/// ```
fn set_break(s: &mut Session, _: &mut Here<'_>, params: &[Param]) -> Ret {
    match params {
        [Param::Num(id), Param::Num(on), ..] => s.breaks.enable(*id, *on != 0),
        [Param::Num(id), ..] => match s.breaks.enabled(*id) {
            Some(on) => s
                .out
                .writeln(format!("Breakpoint #{id} {}", if on { "on" } else { "off" }).as_bytes()),
            None => s
                .out
                .error(format!("Failed to find breakpoint #{id}").as_bytes()),
        },
        _ => s
            .out
            .error(b"set break used incorrectly: set break [id] <on|off>"),
    }
    Ret::Success
}

/// `PHPDBG_SET(breaks)`: every breakpoint enabled or disabled at once,
/// or whether they are, `PHPDBG_IS_BP_ENABLED`: a flag of
/// `PHPDBG_G(flags)`, which each cycle sets from the line again.
///
/// ```php
/// function foo() { echo "foo\n"; }
/// foo();
/// echo "end\n";
/// ```
/// ```request phpdbg
/// set breaks
/// b foo
/// set breaks off
/// set breaks
/// r
/// set breaks
/// r
/// q
/// ```
/// ```expect
/// [Successful compilation of %SCRATCH%/witness.php]
/// prompt> Breakpoints on
/// prompt> [Breakpoint #0 added at foo]
/// prompt> prompt> Breakpoints off
/// prompt> foo
/// end
/// [Script ended normally]
/// prompt> Breakpoints on
/// prompt> [Breakpoint #0 in foo() at %SCRATCH%/witness.php:2, hits: 1]
/// >00002: function foo() { echo "foo\n"; }
///  00003: foo();
///  00004: echo "end\n";
/// prompt>
/// ```
fn set_breaks(s: &mut Session, _: &mut Here<'_>, params: &[Param]) -> Ret {
    match params.first() {
        None => s.out.writeln(
            format!(
                "Breakpoints {}",
                if s.breaks.disabled { "off" } else { "on" }
            )
            .as_bytes(),
        ),
        Some(Param::Num(on)) => s.breaks.disabled = *on == 0,
        Some(_) => s
            .out
            .error(b"set breaks used incorrectly: set breaks <on|off>"),
    }
    Ret::Success
}

/// `PHPDBG_SET(quiet)`: `PHPDBG_IS_QUIET`, which `-v` clears and each
/// cycle sets from the line again. Cleared, a step writes each opline
/// the loop reaches until it stops ([`print_opline_ex`]).
///
/// ```php
/// function f($x) {
///   $y = $x + 1;
///   return $y;
/// }
/// echo f(1), "\n";
/// echo "a\n";
/// echo "b\n";
/// ```
/// ```request phpdbg
/// set quiet
/// b f
/// r
/// set quiet off
/// set quiet
/// s
/// set quiet on
/// s
/// q
/// ```
/// ```expect
/// [Successful compilation of %SCRATCH%/witness.php]
/// prompt> Quietness on
/// prompt> [Breakpoint #0 added at f]
/// prompt> [Breakpoint #0 in f() at %SCRATCH%/witness.php:3, hits: 1]
/// >00003:   $y = $x + 1;
///  00004:   return $y;
///  00005: }
/// prompt> prompt> Quietness off
/// prompt> >00004:   return $y;
///  00005: }
///  00006: echo f(1), "\n";
/// prompt> prompt> >00006: echo f(1), "\n";
///  00007: echo "a\n";
///  00008: echo "b\n";
/// prompt>
/// ```
/// ```stderr
/// L0003 0002 ASSIGN CV1($y) T2
/// L0004 0003 RETURN CV1($y)
/// ```
fn set_quiet(s: &mut Session, _: &mut Here<'_>, params: &[Param]) -> Ret {
    match params.first() {
        None => {
            s.out
                .writeln(format!("Quietness {}", if s.quiet { "on" } else { "off" }).as_bytes());
        }
        Some(Param::Num(on)) => s.quiet = *on != 0,
        Some(param) => s
            .out
            .error(format!("Unsupported parameter type ({}) for command", param.kind()).as_bytes()),
    }
    Ret::Success
}

/// `PHPDBG_COMMAND(register)`: the name looked up in the function table
/// of the request standing, which with none standing is served a request
/// of its own, as `ev` is there, where php's `EG(function_table)` holds
/// the internal functions between its requests: here, a context the
/// compiler refused.
///
/// ```php
/// function (
/// ```
/// ```request phpdbg
/// register intdiv
/// intdiv 7 2
/// q
/// ```
/// ```expect exit=255
/// Parse error: Unclosed '(' in %SCRATCH%/witness.php on line 2
/// prompt> [Registered intdiv]
/// prompt>%s
/// prompt>
/// ```
fn register(s: &mut Session, here: &mut Here<'_>, params: &[Param]) -> Ret {
    let Some(Param::Str(name)) = params.first() else {
        return Ret::Failure;
    };
    let lowered = name.to_ascii_lowercase();
    if s.registered.contains(&lowered) {
        s.out.error(
            format!("The requested name ({}) is already in use", lossy(&lowered)).as_bytes(),
        );
        return Ret::Success;
    }
    let Some(req) = here.request() else {
        crate::host::idle(
            s,
            Command::Params(vec![
                Param::Str(b"register".to_vec()),
                Param::Str(name.clone()),
            ]),
        );
        return Ret::Success;
    };
    if php_ext::call::lookup_function(req, &lowered).is_some() {
        s.out
            .notice(format!("Registered {}", lossy(&lowered)).as_bytes());
        s.registered.push(lowered);
    } else {
        s.out.error(
            format!(
                "The requested function ({}) could not be found",
                lossy(name)
            )
            .as_bytes(),
        );
    }
    Ret::Success
}

/// `PHPDBG_COMMAND(quit)`.
fn quit(s: &mut Session, _: &mut Here<'_>, _: &[Param]) -> Ret {
    s.stopped = Some(Stop::Quit);
    Ret::Success
}
