//! The debugging session: what phpdbg keeps across the requests it
//! serves (`zend_phpdbg_globals`, `Z/sapi/phpdbg/phpdbg.h`), in one
//! value the host owns. The tier's `safepoint` is a function with no
//! receiver, so the value stands in a thread-local cell the tier takes it
//! out of while it runs a prompt and puts back after; a safepoint that
//! finds the cell empty is inside what the prompt itself served, an `ev`
//! or a registered function, where php's phpdbg does not stop either
//! (`PHPDBG_IN_EVAL`).

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::path::PathBuf;

use php_sapi::tier::{FrameRef, Opcode};

use crate::bp::Breakpoints;
use crate::cmd::Command;
use crate::watch::Watches;

/// Why the request being served was started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plan {
    /// The execution context compiled and held at its first opline, for
    /// the commands before `run`: php's compile after each restart.
    Hold,
    /// `run`: the program served under the debugger.
    Run,
    /// A request with no program of the execution context, for one
    /// command outside a run that needs one standing.
    Idle,
}

/// Why the request being served was ended before its program did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stop {
    /// `quit`, or the end of stdin.
    Quit,
    /// `run` again, `y` to its question: the run starts over with the
    /// arguments it names.
    Restart(Option<Vec<u8>>),
    /// `run` with arguments before the first run: the held request ends
    /// and the run starts with them.
    RunWith(Option<Vec<u8>>),
    /// `clean`: the environment is let go and the breakpoints kept.
    Clean,
    /// `exec`: another execution context.
    Exec,
    /// A command outside any request that needs one: served by a request
    /// of its own.
    Idle,
}

/// A seek the loop runs to: `until`, `next`, `finish` or `leave`, over
/// the oplines of one frame (`PHPDBG_G(seek)`, `PHPDBG_G(seek_ex)`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seek {
    pub kind: SeekKind,
    pub frame: u32,
    pub oplines: BTreeSet<u32>,
}

/// `PHPDBG_IN_UNTIL`, `PHPDBG_IN_FINISH`, `PHPDBG_IN_LEAVE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SeekKind {
    Until,
    Finish,
    Leave,
}

/// What follows a run's end: the prompt, or, under `-rr`, the end of
/// the session (`quit_immediately`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AfterRun {
    Prompt,
    Quit,
}

/// The execution context and the compiler: not yet announced, its
/// compile notice written, or refused when it was last held.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Context {
    Unannounced,
    Announced,
    Refused,
}

/// One opline the loop reached while an oplog was open
/// (`phpdbg_oplog_entry`): the op array's file, function and scope, and
/// the opline's number, opcode and line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Logged {
    pub file: Vec<u8>,
    pub function: Option<Vec<u8>>,
    pub class: Option<Vec<u8>>,
    pub opline: u32,
    pub opcode: Opcode,
    pub line: u32,
}

/// A file the session saw compiled, and its code's oplines.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    pub file: Vec<u8>,
    pub ops: Vec<(Opcode, u32)>,
}

/// Everything the session keeps.
#[derive(Debug)]
pub struct Session {
    /// `PHPDBG_G(exec)`: the execution context, its resolved path, or
    /// `Standard input code` with the source `stdin` read.
    pub exec: Option<Vec<u8>>,
    /// The source `stdin` read, where that is the execution context.
    pub stdin_code: Option<Vec<u8>>,
    /// The arguments after the program on the command line.
    pub line_args: Vec<Vec<u8>>,
    /// The arguments of the request being served: the command line's, or
    /// the ones `run` named for its run alone.
    pub args: Vec<Vec<u8>>,
    /// `run <file`: the file the program reads as its stdin.
    pub stdin_file: Option<PathBuf>,
    pub breaks: Breakpoints,
    pub watches: Watches,
    /// `register`'s functions, lowered.
    pub registered: Vec<Vec<u8>>,
    /// The request being served, and why.
    pub plan: Option<Plan>,
    /// Why the request was ended early, when it was.
    pub stopped: Option<Stop>,
    /// `PHPDBG_G(in_execution)`: a run is standing.
    pub executing: bool,
    /// `PHPDBG_IS_STEPPING`.
    pub stepping: bool,
    /// `PHPDBG_G(last_line)`.
    pub last_line: u32,
    pub seek: Option<Seek>,
    /// `PHPDBG_G(handled_exception)`: the handle of the throwable the
    /// session last stopped on as uncaught.
    pub handled: Option<u32>,
    /// The commands of the line being run that are still to run, which a
    /// run that ends hands back to the prompt outside it.
    pub queued: Vec<Command>,
    /// `-rr`: quit once the run is over.
    pub after_run: AfterRun,
    /// Where the execution context stands with the compiler.
    pub context: Context,
    /// The command an idle request stands for.
    pub job: Option<Command>,
    /// `PHPDBG_G(prompt)[0]`: the prompt's text, shown with a space after.
    pub prompt: Vec<u8>,
    /// `PHPDBG_G(oplog_list)`: each oplog `phpdbg_start_oplog` opened and
    /// no `phpdbg_end_oplog` closed yet, the last the one written to.
    pub oplogs: Vec<Vec<Logged>>,
    /// `PHPDBG_G(file_sources)`: each file whose code the loop reached, in
    /// that order, with its oplines, as `phpdbg_get_executable` walks them.
    pub sources: Vec<Source>,
    /// The frame the loop is stepping, the nearest user frame to a native
    /// it calls, which `phpdbg_break_next` reads.
    pub standing: Option<FrameRef>,
}

impl Session {
    #[must_use]
    pub fn new() -> Session {
        Session {
            exec: None,
            stdin_code: None,
            line_args: Vec::new(),
            args: Vec::new(),
            stdin_file: None,
            breaks: Breakpoints::default(),
            watches: Watches::default(),
            registered: Vec::new(),
            plan: None,
            stopped: None,
            executing: false,
            stepping: false,
            last_line: 0,
            seek: None,
            handled: None,
            queued: Vec::new(),
            after_run: AfterRun::Prompt,
            context: Context::Unannounced,
            job: None,
            prompt: b"prompt>".to_vec(),
            oplogs: Vec::new(),
            sources: Vec::new(),
            standing: None,
        }
    }
}

impl Default for Session {
    fn default() -> Session {
        Session::new()
    }
}

thread_local! {
    static SESSION: RefCell<Option<Session>> = const { RefCell::new(None) };
}

/// The session put where the tier finds it.
pub fn install(session: Session) {
    SESSION.set(Some(session));
}

/// The session taken out of its cell, none while a prompt holds it.
pub fn take() -> Option<Session> {
    SESSION.with(|cell| cell.try_borrow_mut().ok()?.take())
}

/// The session put back.
pub fn put(session: Session) {
    SESSION.set(Some(session));
}

/// `f` over the session where it stands in its cell; none where it is
/// out of it or already being read, which is where a safepoint inside
/// what the session itself is doing finds it.
pub fn with<R>(f: impl FnOnce(&mut Session) -> R) -> Option<R> {
    SESSION.with(|cell| {
        let mut cell = cell.try_borrow_mut().ok()?;
        cell.as_mut().map(f)
    })
}
