//! The prompt (`Z/sapi/phpdbg/phpdbg_prompt.c`, `phpdbg_cmd.c`): a line
//! read, cut into commands, each resolved against php's table by its
//! name, an abbreviation of it or its one-letter alias, its parameters
//! held to the command's specification, and run. A command that goes
//! on with the program ends the prompt; one that fails drops the rest of
//! its line, the error it failed with written unless the line named a
//! registered function, which is called instead.

use std::collections::BTreeSet;

use php_sapi::tier::{self, Argument, FrameRef, Opcode, Request};

use crate::bp::Kind;
use crate::cmd::{self, Command, Param};
use crate::input::{self, Line};
use crate::out;
use crate::session::{Context, Origin, Seek, SeekKind, Session, Stop};

/// Where the prompt stands.
pub enum Here<'a> {
    /// A run stopped between two handlers, at `frame`.
    Paused {
        req: &'a mut Request,
        frame: FrameRef,
    },
    /// The execution context compiled before `run`, with nothing
    /// executing: php's `PHPDBG_G(ops)` with `EG(current_execute_data)`
    /// `NULL`.
    Held { req: &'a mut Request },
    /// A request with no program of the context's, standing for one
    /// command outside a run, with nothing executing.
    Idle { req: &'a mut Request },
    /// No request standing.
    Outside,
}

impl Here<'_> {
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
            Here::Paused { req, frame } => Some((&mut **req, *frame)),
            Here::Held { .. } | Here::Idle { .. } | Here::Outside => None,
        }
    }

    /// The request the prompt has serve code of its own, an `ev` or a
    /// registered function, and what that code stands on: the frame a
    /// run stopped in, or nothing, as php's `EG(current_execute_data)` is
    /// `NULL` at a prompt no run stands under.
    fn serving(&mut self) -> Option<(&mut Request, tier::Under)> {
        match self {
            Here::Paused { req, frame } => Some((&mut **req, tier::Under::Frame(*frame))),
            Here::Held { req } | Here::Idle { req } => Some((&mut **req, tier::Under::Nothing)),
            Here::Outside => None,
        }
    }

    /// `PHPDBG_G(in_execution)`.
    pub fn executing(&self) -> bool {
        matches!(self, Here::Paused { .. })
    }

    /// `PHPDBG_G(ops)`: the context is compiled and standing.
    fn compiled(&self) -> bool {
        matches!(self, Here::Paused { .. } | Here::Held { .. })
    }
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
    loop {
        if s.stopped.is_some() {
            return Outcome::Stopped;
        }
        let commands = if s.queued.is_empty() {
            let prompt = [s.prompt.as_slice(), b" "].concat();
            match input::read_input(&prompt) {
                Line::End => {
                    s.stopped = Some(Stop::Quit);
                    return Outcome::Stopped;
                }
                Line::Read(line) => match cmd::parse(&line) {
                    Ok(commands) => commands,
                    Err(cmd::Malformed(message)) => {
                        out::error(format!("Parse Error: {message}").as_bytes());
                        continue;
                    }
                },
            }
        } else {
            std::mem::take(&mut s.queued)
        };
        if commands.is_empty() {
            out::error(b"The stack contains nothing !");
            continue;
        }
        let mut rest = commands.into_iter();
        while let Some(command) = rest.next() {
            let asked = execute(s, here, &command);
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
                    out::error(b"Not running");
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
                out::error(b"Not running");
                return;
            }
        }
    }
}

/// One command, the one the idle request stands for.
pub fn idle_job(s: &mut Session, req: &mut Request) -> Outcome {
    let Some(command) = s.job.take() else {
        return Outcome::Resume;
    };
    let mut here = Here::Idle { req };
    let _ = execute(s, &mut here, &command);
    Outcome::Resume
}

/// `phpdbg_internal_stack_execute`.
fn execute(s: &mut Session, here: &mut Here<'_>, command: &Command) -> Ret {
    match command {
        Command::Eval(code) => ev(s, here, code),
        Command::Run(args) => run(s, here, args.as_deref()),
        Command::Shell(_) => Ret::Success,
        Command::Params(params) => {
            let resolved = resolve(ROOT, None, params).and_then(|(def, rest)| {
                verify(def, rest)?;
                Ok((def, rest))
            });
            match resolved {
                Ok((def, rest)) => (def.handler)(s, here, rest),
                Err(error) => {
                    if !call_registered(s, here, params)
                        && let Some(error) = error
                    {
                        out::error(error.as_bytes());
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
    parent: Option<&'static str>,
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
    parent: &'static str,
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
static ROOT: &[Def] = &[
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
    def("frame", Some(b'f'), "|n", unsupported),
    with_subs("list", Some(b'l'), "*", LIST, list),
    with_subs("info", Some(b'i'), "|s", INFO, info_overview),
    def("clean", Some(b'X'), "", clean),
    def("clear", Some(b'C'), "", clear),
    def("help", Some(b'h'), "|s", help),
    def("set", Some(b'S'), "s", unsupported),
    def("register", Some(b'R'), "s", register),
    def("source", Some(b'<'), "s", unsupported),
    def("export", Some(b'>'), "s", unsupported),
    def("sh", None, "i", unsupported),
    def("quit", Some(b'q'), "", quit),
    with_subs("watch", Some(b'w'), "|ss", WATCH, crate::watch::watch),
    def("next", Some(b'n'), "", next),
];

/// `phpdbg_print_commands`.
static PRINT: &[Def] = &[
    sub("print", "exec", Some(b'e'), "", print_exec),
    sub("print", "opline", Some(b'o'), "", print_opline),
    sub("print", "class", Some(b'c'), "s", print_class),
    sub("print", "method", Some(b'm'), "m", print_method),
    sub("print", "func", Some(b'f'), "s", print_func),
    sub("print", "stack", Some(b's'), "", print_stack),
];

/// `phpdbg_break_commands`.
static BREAK: &[Def] = &[
    sub("break", "at", Some(b'@'), "*c", unsupported),
    sub("break", "del", Some(b'~'), "n", break_del),
];

/// `phpdbg_list_commands`.
static LIST: &[Def] = &[
    sub("list", "lines", Some(b'l'), "l", list),
    sub("list", "class", Some(b'c'), "s", unsupported),
    sub("list", "method", Some(b'm'), "m", unsupported),
    sub("list", "func", Some(b'f'), "s", unsupported),
];

/// `phpdbg_info_commands`.
static INFO: &[Def] = &[
    sub("info", "break", Some(b'b'), "", crate::info::breaks),
    sub("info", "files", Some(b'F'), "", crate::info::files),
    sub("info", "classes", Some(b'c'), "", crate::info::classes),
    sub("info", "funcs", Some(b'f'), "", crate::info::funcs),
    sub("info", "error", Some(b'e'), "", crate::info::error),
    sub("info", "constants", Some(b'd'), "", crate::info::constants),
    sub("info", "vars", Some(b'v'), "", crate::info::vars),
    sub("info", "globals", Some(b'g'), "", crate::info::globals),
    sub("info", "literal", Some(b'l'), "", crate::info::literal),
    sub("info", "memory", Some(b'm'), "", unsupported),
];

/// `phpdbg_watch_commands`.
static WATCH: &[Def] = &[
    sub("watch", "array", Some(b'a'), "s", crate::watch::array),
    sub("watch", "delete", Some(b'd'), "n", crate::watch::delete),
    sub(
        "watch",
        "recursive",
        Some(b'r'),
        "s",
        crate::watch::recursive,
    ),
    sub("watch", "list", Some(b'l'), "", crate::watch::list),
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

/// `phpdbg_command_name`.
fn command_name(def: &Def) -> String {
    match def.parent {
        Some(parent) => format!("{parent} {}", def.name),
        None => def.name.to_owned(),
    }
}

/// `phpdbg_stack_verify`.
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
        let (expected, fits): (&str, fn(&Param) -> bool) = match spec {
            b'i' => ("raw input", |p| matches!(p, Param::Str(_))),
            b's' => ("string", |p| matches!(p, Param::Str(_))),
            b'n' => ("number", |p| matches!(p, Param::Num(_))),
            b'm' => ("method", |p| matches!(p, Param::Method { .. })),
            b'a' => ("address", |p| matches!(p, Param::Addr(_))),
            b'f' => ("file:line", |p| matches!(p, Param::File { .. })),
            b'c' => ("condition", |p| matches!(p, Param::Cond(_))),
            b'o' => ("opcode", |p| matches!(p, Param::Op(_))),
            b'b' => ("boolean", |p| matches!(p, Param::Num(_))),
            _ => ("anything", |_| true),
        };
        match top {
            None if !optional => {
                return Err(Some(format!(
                    "The command \"{name}\" expected {expected} and got nothing at parameter {current}"
                )));
            }
            None => break,
            Some(param) if spec != b'*' && !fits(param) => {
                return Err(Some(format!(
                    "The command \"{name}\" expected {expected} and got {} at parameter {current}",
                    param.kind()
                )));
            }
            Some(_) => {}
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
    match crate::session::lend(s, || tier::call(req, under, &lowered, &arguments)) {
        tier::Called::Returned => out::out(b"\n"),
        tier::Called::Threw => {
            if let tier::Under::Frame(frame) = under {
                s.thrown_into = Some(frame);
            }
            out::out(b"\n");
        }
        tier::Called::Rendered | tier::Called::Exited => {}
    }
    true
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
        out::error(format!("Cannot stat {shown}, ensure the file exists").as_bytes());
        return Ret::Success;
    };
    if !meta.is_file() {
        out::error(
            format!("Cannot use {shown} as execution context, not a valid file or symlink")
                .as_bytes(),
        );
        return Ret::Success;
    }
    let resolved = crate::host::real_path(path);
    if s.exec.as_deref() == Some(resolved.as_slice()) {
        out::notice(b"Execution context not changed");
        return Ret::Success;
    }
    if here.executing()
        && !input::ask(b"Do you really want to stop execution to set a new execution context?")
    {
        return Ret::Failure;
    }
    if let Some(old) = &s.exec {
        out::notice(
            [&b"Unsetting old execution context: "[..], old]
                .concat()
                .as_slice(),
        );
    }
    if here.compiled() {
        out::notice(b"Destroying compiled opcodes");
    }
    if let Some(dir) = std::path::Path::new(std::ffi::OsStr::from_bytes(&resolved)).parent() {
        let _ = std::env::set_current_dir(dir);
    }
    out::notice(
        [&b"Set execution context: "[..], &resolved]
            .concat()
            .as_slice(),
    );
    s.exec = Some(resolved);
    s.stdin_code = None;
    s.context = if here.executing() {
        Context::Announced
    } else {
        Context::Unannounced
    };
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
        match input::consume_line() {
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
        && !input::ask(b"Do you really want to stop execution to set a new execution context?")
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
        out::error(b"Nothing to execute!");
        return Ret::Success;
    }
    let args = args.filter(|args| !args.is_empty()).map(<[u8]>::to_vec);
    match here {
        Here::Paused { .. } => {
            if input::ask(b"Do you really want to restart execution?") {
                s.stopped = Some(Stop::Restart(args));
            }
            Ret::Success
        }
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
    s.executing = true;
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
fn ev(s: &mut Session, here: &mut Here<'_>, code: &[u8]) -> Ret {
    let Some((req, under)) = here.serving() else {
        crate::host::idle(s, Command::Eval(code.to_vec()));
        return Ret::Success;
    };
    let stepping = std::mem::take(&mut s.stepping);
    crate::watch::freeze(s, req);
    s.origin = Origin::Eval;
    let evaluated = crate::session::lend(s, || tier::evaluate(req, under, code));
    s.origin = Origin::Program;
    crate::watch::thaw(s);
    s.stepping |= stepping;
    match evaluated {
        Ok(tier::Evaluated::Printed(printed)) => {
            out::write(&printed);
            out::out(b"\n");
        }
        Ok(tier::Evaluated::Rendered) | Err(_) => {}
    }
    Ret::Success
}

/// The oplines of the frame the prompt stands in, and the frame.
fn seek_over(here: &mut Here<'_>, keep: impl Fn(Opcode, u32, u32) -> bool) -> Option<Seek> {
    let (req, frame) = here.standing()?;
    let position = tier::position(req, frame)?;
    let oplines: BTreeSet<u32> = tier::oplines(req, frame)
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

/// `ZEND_RETURN`, `ZEND_FAST_RET`, `ZEND_GENERATOR_RETURN`, `ZEND_YIELD`
/// and `ZEND_YIELD_FROM`, where a seek to a function's end stops.
fn ends(opcode: Opcode) -> bool {
    matches!(
        opcode,
        Opcode::Return
            | Opcode::FastRet
            | Opcode::GeneratorReturn
            | Opcode::Yield
            | Opcode::YieldFrom
    )
}

/// `phpdbg_skip_line_helper`.
fn skip_line(s: &mut Session, here: &mut Here<'_>) -> Ret {
    s.seek = seek_over(here, |opcode, line, current| {
        line != current || ends(opcode)
    });
    Ret::Resume
}

/// `PHPDBG_COMMAND(until)`.
fn until(s: &mut Session, here: &mut Here<'_>, _: &[Param]) -> Ret {
    if !here.executing() {
        out::error(b"Not executing");
        return Ret::Success;
    }
    skip_line(s, here)
}

/// `PHPDBG_COMMAND(next)`.
fn next(s: &mut Session, here: &mut Here<'_>, _: &[Param]) -> Ret {
    if !here.executing() {
        out::error(b"Not executing");
        return Ret::Success;
    }
    s.stepping = true;
    skip_line(s, here)
}

/// Whether the frame stands at an opline where its function ends, the
/// seek to it set where it does not.
fn seek_to_end(s: &mut Session, here: &mut Here<'_>, kind: SeekKind) -> bool {
    let Some(mut seek) = seek_over(here, |opcode, _, _| ends(opcode)) else {
        return true;
    };
    let at = here
        .standing()
        .and_then(|(req, frame)| tier::position(req, frame))
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
        out::error(b"Not executing");
        return Ret::Success;
    }
    seek_to_end(s, here, SeekKind::Finish);
    Ret::Resume
}

/// `PHPDBG_COMMAND(leave)`.
fn leave(s: &mut Session, here: &mut Here<'_>, _: &[Param]) -> Ret {
    if !here.executing() {
        out::error(b"Not executing");
        return Ret::Success;
    }
    if seek_to_end(s, here, SeekKind::Leave) {
        out::notice(b"Already at the end of the function");
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
            out::error(
                b"Invalid arguments to print, expected nothing, function name or method name",
            );
            Ret::Success
        }
    }
}

fn lossy(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

/// `PHPDBG_PRINT(exec)`.
fn print_exec(s: &mut Session, here: &mut Here<'_>, _: &[Param]) -> Ret {
    let Some(exec) = s.exec.clone() else {
        out::error(b"No execution context set");
        return Ret::Success;
    };
    let Some(req) = here.request() else {
        return Ret::Success;
    };
    let entry = tier::entry(req);
    let main = main_frame(req, entry);
    if let Some(code) = tier::frame_code(req, main) {
        out::notice(format!("Context {} ({} ops)", lossy(&exec), code.ops.len()).as_bytes());
        out::dump(&code.dump);
    }
    Ret::Success
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

/// `PHPDBG_PRINT(opline)`.
fn print_opline(_: &mut Session, here: &mut Here<'_>, _: &[Param]) -> Ret {
    let executing = here.executing();
    match here.standing() {
        Some((req, frame)) if executing => {
            if let (Some(code), Some(position)) =
                (tier::frame_code(req, frame), tier::position(req, frame))
                && let Some(line) = opline_line(&code.dump, position.opline)
            {
                out::writeln(line);
            }
        }
        _ => out::error(b"Not Executing!"),
    }
    Ret::Success
}

/// The dump's line for the opline numbered `at`, `L0015 0001 ...`.
fn opline_line(dump: &[u8], at: u32) -> Option<&[u8]> {
    let wanted = format!(" {at:04} ");
    dump.split(|b| *b == b'\n')
        .find(|line| line.first() == Some(&b'L') && line.get(5..11) == Some(wanted.as_bytes()))
}

/// `PHPDBG_PRINT(class)`.
fn print_class(_: &mut Session, here: &mut Here<'_>, params: &[Param]) -> Ret {
    let Some(Param::Str(name)) = params.first() else {
        return Ret::Failure;
    };
    let class = here.request().and_then(|req| tier::class_named(req, name));
    let Some(class) = class else {
        out::error(format!("The class {} could not be found", lossy(name)).as_bytes());
        return Ret::Success;
    };
    let shape = match class.shape {
        tier::Shape::Interface => "Interface",
        tier::Shape::AbstractClass => "Abstract Class",
        tier::Shape::Class => "Class",
    };
    out::notice(
        format!(
            "{} {shape}: {} ({} methods)",
            if class.user { "User" } else { "Internal" },
            lossy(&class.name),
            class.methods.len()
        )
        .as_bytes(),
    );
    for method in &class.methods {
        print_function(method);
    }
    Ret::Success
}

/// `phpdbg_print_function_helper`.
fn print_function(code: &tier::Code) {
    if code.user {
        out::dump(&code.dump);
        return;
    }
    let line = match &code.class {
        Some(class) => format!("\tInternal {}::{}()", lossy(class), lossy(&code.name)),
        None => format!("\tInternal {}()", lossy(&code.name)),
    };
    out::writeln(line.as_bytes());
}

/// `PHPDBG_PRINT(method)`.
fn print_method(_: &mut Session, here: &mut Here<'_>, params: &[Param]) -> Ret {
    let Some(Param::Method { class, name }) = params.first() else {
        return Ret::Failure;
    };
    let found = here.request().map_or(tier::Named::NoClass, |req| {
        tier::method_named(req, class, name)
    });
    match found {
        tier::Named::Code(code) => {
            out::notice(
                format!(
                    "{} Method {} ({} ops)",
                    if code.user { "User" } else { "Internal" },
                    lossy(&code.name),
                    code.ops.len()
                )
                .as_bytes(),
            );
            print_function(&code);
        }
        tier::Named::NoFunction => out::error(
            format!(
                "The method {}::{} could not be found",
                lossy(class),
                lossy(name)
            )
            .as_bytes(),
        ),
        tier::Named::NoClass => {
            out::error(format!("The class {} could not be found", lossy(class)).as_bytes());
        }
    }
    Ret::Success
}

/// `PHPDBG_PRINT(func)`.
fn print_func(_: &mut Session, here: &mut Here<'_>, params: &[Param]) -> Ret {
    let Some(Param::Str(name)) = params.first() else {
        return Ret::Failure;
    };
    let found = here.request().map_or(tier::Named::NoFunction, |req| {
        tier::function_named(req, name)
    });
    match found {
        tier::Named::Code(code) => {
            out::notice(
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
            print_function(&code);
        }
        tier::Named::NoFunction | tier::Named::NoClass => {
            out::error(format!("The function {} could not be found", lossy(name)).as_bytes());
        }
    }
    Ret::Success
}

/// `PHPDBG_PRINT(stack)`.
fn print_stack(_: &mut Session, here: &mut Here<'_>, _: &[Param]) -> Ret {
    let executing = here.executing();
    let Some((req, frame)) = here.standing().filter(|_| executing) else {
        out::error(b"Not Executing!");
        return Ret::Success;
    };
    let Some(code) = tier::frame_code(req, frame) else {
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
    out::notice(title.as_bytes());
    print_function(&code);
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
        .and_then(|(req, frame)| tier::position(req, frame))
        .map(|position| (position.file, position.line));
    match params.first() {
        None => match (&s.exec, current) {
            (Some(_), Some((file, line))) => s.breaks.set_file(Some(file), &[], i64::from(line)),
            (Some(_), None) => crate::host::file_break(s, NO_ACTIVE_FILE, 0),
            (None, _) => out::error(b"Execution context not set!"),
        },
        Some(Param::Num(line)) => match &s.exec {
            Some(exec) => {
                let file = current.map_or_else(|| exec.clone(), |(file, _)| file);
                s.breaks.set_file(Some(file), &[], *line);
            }
            None => out::error(b"Execution context not set!"),
        },
        Some(Param::File { name, line }) => crate::host::file_break(s, name, *line),
        Some(Param::Method { class, name }) => s.breaks.set_method(class, name),
        Some(Param::NumericMethod { class, name, num }) => {
            opline_break(s, here, Some(class), name, *num);
        }
        Some(Param::NumericFunction { name, num }) => opline_break(s, here, None, name, *num),
        Some(Param::NumericFile { name, line }) => {
            let path = crate::host::real_path(name);
            s.breaks
                .set_file_opline(&path, u32::try_from(*line).unwrap_or(u32::MAX));
        }
        Some(Param::Str(name)) => s.breaks.set_symbol(name),
        Some(Param::Op(name)) => s.breaks.set_opcode(name),
        Some(_) => {}
    }
    Ret::Success
}

/// `phpdbg_set_breakpoint_method_opline` and
/// `phpdbg_set_breakpoint_function_opline`: resolved against the tables
/// where the function exists, pending where the class or the function
/// does not.
fn opline_break(
    s: &mut Session,
    here: &mut Here<'_>,
    class: Option<&Vec<u8>>,
    name: &[u8],
    num: i64,
) {
    let id = s.breaks.next_id();
    let opline = u32::try_from(num).unwrap_or(u32::MAX);
    let found = match here.request() {
        Some(req) => match class {
            Some(class) => tier::method_named(req, class, name),
            None => tier::function_named(req, name),
        },
        None => tier::Named::NoClass,
    };
    let shown = match class {
        Some(class) => format!("{}::{}#{num}", lossy(class), lossy(name)),
        None => format!("{}#{num}", lossy(name)),
    };
    match found {
        tier::Named::Code(code) if !code.user => {
            let message = match class {
                Some(class) => format!(
                    "{}::{} is not a user defined method, no oplines exist",
                    lossy(class),
                    lossy(name)
                ),
                None => format!(
                    "{} is not a user defined function, no oplines exist",
                    lossy(name)
                ),
            };
            out::error(message.as_bytes());
            return;
        }
        tier::Named::Code(code) if code.ops.len() <= opline as usize => {
            let message = match class {
                Some(class) => format!(
                    "There are only {} oplines in method {}::{} (breaking at opline {num} impossible)",
                    code.ops.len(),
                    lossy(class),
                    lossy(name)
                ),
                None => format!(
                    "There are only {} oplines in function {} (breaking at opline {num} impossible)",
                    code.ops.len(),
                    lossy(name)
                ),
            };
            out::error(message.as_bytes());
            return;
        }
        tier::Named::Code(_) => {
            out::notice(format!("Breakpoint #{id} added at {shown}").as_bytes());
        }
        tier::Named::NoFunction if class.is_some() => {
            out::error(
                format!(
                    "Method {} doesn't exist in class {}",
                    lossy(name),
                    class.map(|c| lossy(c)).unwrap_or_default()
                )
                .as_bytes(),
            );
            return;
        }
        tier::Named::NoFunction | tier::Named::NoClass => {
            out::notice(format!("Pending breakpoint #{id} at {shown}").as_bytes());
        }
    }
    let kind = match class {
        Some(class) => Kind::MethodOpline {
            class: class.clone(),
            name: name.to_vec(),
            opline,
        },
        None => Kind::FunctionOpline {
            name: name.to_vec(),
            opline,
        },
    };
    s.breaks.push(id, kind);
}

/// `PHPDBG_BREAK(del)`.
fn break_del(s: &mut Session, _: &mut Here<'_>, params: &[Param]) -> Ret {
    if let Some(Param::Num(id)) = params.first() {
        s.breaks.delete(*id);
    }
    Ret::Success
}

/// `PHPDBG_COMMAND(back)`.
fn back(_: &mut Session, here: &mut Here<'_>, params: &[Param]) -> Ret {
    let executing = here.executing();
    let Some((req, frame)) = here.standing().filter(|_| executing) else {
        out::error(b"Not executing!");
        return Ret::Success;
    };
    let limit = match params.first() {
        Some(Param::Num(n)) => usize::try_from(*n).unwrap_or(0),
        _ => 0,
    };
    crate::info::backtrace(req, frame, limit);
    Ret::Success
}

/// `PHPDBG_COMMAND(list)` and `PHPDBG_LIST(lines)`.
fn list(s: &mut Session, here: &mut Here<'_>, params: &[Param]) -> Ret {
    let executing = here.executing();
    let current = here
        .standing()
        .and_then(|(req, frame)| tier::position(req, frame));
    let (file, line) = if let (Some(position), true) = (&current, executing) {
        (position.file.clone(), position.line)
    } else {
        let Some(exec) = &s.exec else {
            out::error(b"Not executing, and execution context not set");
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

/// `PHPDBG_COMMAND(info)` with no sub-command.
fn info_overview(_: &mut Session, _: &mut Here<'_>, _: &[Param]) -> Ret {
    Ret::Success
}

/// `PHPDBG_COMMAND(clean)`.
fn clean(s: &mut Session, here: &mut Here<'_>, _: &[Param]) -> Ret {
    if here.executing() && !input::ask(b"Do you really want to clean your current environment?") {
        return Ret::Success;
    }
    out::out(b"Cleaning Execution Environment\n");
    let [classes, functions, constants, includes] =
        here.request().map_or([0; 4], |req| tier::table_sizes(req));
    out::writeln(format!("Classes    {classes}").as_bytes());
    out::writeln(format!("Functions  {functions}").as_bytes());
    out::writeln(format!("Constants  {constants}").as_bytes());
    out::writeln(format!("Includes   {includes}").as_bytes());
    s.stopped = Some(Stop::Clean);
    Ret::Success
}

/// `PHPDBG_COMMAND(clear)`.
fn clear(s: &mut Session, _: &mut Here<'_>, _: &[Param]) -> Ret {
    s.breaks.clear();
    Ret::Success
}

/// `PHPDBG_COMMAND(help)`, as far as a topic nobody has.
fn help(_: &mut Session, _: &mut Here<'_>, params: &[Param]) -> Ret {
    if let Some(Param::Str(topic)) = params.first()
        && !ROOT.iter().any(|def| {
            def.name.as_bytes().starts_with(topic.as_slice())
                || (topic.len() == 1 && def.alias == Some(topic[0]))
        })
    {
        out::error(format!("No help topic found for {}", lossy(topic)).as_bytes());
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
        out::error(
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
    if tier::function_exists(req, &lowered) {
        out::notice(format!("Registered {}", lossy(&lowered)).as_bytes());
        s.registered.push(lowered);
    } else {
        out::error(
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
