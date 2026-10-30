//! The debugging session: what phpdbg keeps across the requests it
//! serves (`zend_phpdbg_globals`, `Z/sapi/phpdbg/phpdbg.h`), in one
//! value the host owns and lends each request it serves in the request's
//! sink ([`Sink`]), which it takes back at the request's end, so the
//! session is in no static (design §5.14 *Threads*). The tier's
//! `safepoint` and the natives reach it through the request; the tier
//! takes it out of the sink while it runs a prompt and puts it back
//! after. What the prompt itself serves, an `ev` or a registered
//! function, runs with the session lent back to the sink ([`lend`]), so a
//! native reads and writes it as php's read `PHPDBG_G` at any time, and
//! the debugger drives it as it drives the program, by
//! [`Session::origin`]'s rules where `ev` serves.

use std::collections::BTreeSet;
use std::path::PathBuf;

use php_ext::request::Opcode;
use php_sapi::tier::{self, FrameRef, Request};

use crate::bp::Breakpoints;
use crate::cmd::Command;
use crate::out::{Colours, Out, Sink};
use crate::watch::Watches;

/// Why the request being served was started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plan {
    /// The `exec` file compiled and held at its first opline, for
    /// the commands before `run`: php's compile after each restart.
    Hold,
    /// `run`: the program served under the debugger.
    Serve,
    /// A request with no program of the `exec` file's: where
    /// there is none to hold, or the compiler refused it, the prompt's,
    /// standing across its commands as php's one request does; or the
    /// one command a `run` left that needs a request standing.
    Idle,
    /// A request with no program, where the cycle starts with an `exec` file
    /// that does not compile: php runs the init files before its compile
    /// fails, so what they write comes first, the compiler's log after
    /// them, and the prompt stands in the request they ran in.
    Init,
}

/// Why the request being served was ended before its program did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stop {
    /// `quit`, or the end of stdin.
    Quit,
    /// `run` again, `y` to its question: the `run` starts over with the
    /// arguments it names.
    Restart(Option<Vec<u8>>),
    /// `run` with arguments before the first `run`: the held request ends
    /// and the `run` starts with them.
    ServeWith(Option<Vec<u8>>),
    /// `clean`: the environment is let go and the breakpoints kept.
    Clean,
    /// `exec`: another `exec` file.
    Exec,
    /// A command a `run` left that needs a request standing: served by a
    /// request of its own.
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

/// `PHPDBG_G(in_execution)` where no frame stands to say it, and whether
/// a `run` stands under it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Execution {
    /// Nothing: `in_execution` is clear.
    Not,
    /// `run` has begun the program, which stands until its request ends.
    Serving,
    /// No `run` stands, but `in_execution` is set: an `exit` served at a
    /// prompt where nothing was executing bailed out of
    /// `phpdbg_execute_ex`, which set it on entry, past the return that
    /// puts it back (`Z/sapi/phpdbg/phpdbg_prompt.c:1626-1660, 1814`), so
    /// php asks before `run` restarts, `clean` cleans or `exec` replaces
    /// what it takes to be executing, and a command that goes on with the
    /// program is not refused as not running.
    BailedOut,
}

/// What follows a `run`'s end: the prompt, or, under `-rr`, the end of
/// the session (`quit_immediately`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AfterServe {
    Prompt,
    Quit,
}

/// The `exec` file and the compiler: not yet announced, its
/// compile notice written, or refused when it was last held.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExecCompile {
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

/// A file the session saw compiled, `phpdbg_file_source`: its code's
/// oplines, and the bytes it was compiled from, which a listing reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    pub file: Vec<u8>,
    pub ops: Vec<(Opcode, u32)>,
    pub bytes: Vec<u8>,
}

/// Whose code the loop reaches: the program's, or what `ev` evaluates
/// for the prompt, `PHPDBG_IN_EVAL` (`Z/sapi/phpdbg/phpdbg_prompt.c:951-975`),
/// where a breakpoint, a watchpoint and a step still stop, at a prompt
/// with no listing, and the seek and the notice of an uncaught throwable
/// are not made. A function `register` named is called with no mark of
/// its own, and is the program's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    Program,
    Eval,
}

/// Everything the session keeps.
#[derive(Debug)]
#[allow(clippy::struct_excessive_bools)] // bits of `PHPDBG_G(flags)` and `PHPDBG_G`, one field each
pub struct Session {
    /// `PHPDBG_G(exec)`: the `exec` file, its resolved path, or
    /// `Standard input code` with the source `stdin` read.
    pub exec: Option<Vec<u8>>,
    /// The source `stdin` read, where that is the `exec` file.
    pub stdin_code: Option<Vec<u8>>,
    /// The arguments after the program on the command line.
    pub line_args: Vec<Vec<u8>>,
    /// The arguments of the request being served: the command line's, or
    /// the ones `run` named for that `run` alone.
    pub args: Vec<Vec<u8>>,
    /// `run <file`: the file the program reads as its stdin.
    pub stdin_file: Option<PathBuf>,
    /// `exec` set the file at a prompt inside the host's log, where
    /// php's `phpdbg_compile` compiles it then and there.
    pub exec_in_log: bool,
    pub breaks: Breakpoints,
    /// `bp_tmp`: the breakpoints a cycle that ended exported as `break`
    /// lines, which the next cycle runs once its init files have
    /// (`Z/sapi/phpdbg/phpdbg.c:1604-1615, 1556-1562`).
    pub carried: Option<Vec<u8>>,
    pub watches: Watches,
    /// `register`'s functions, lowered.
    pub registered: Vec<Vec<u8>>,
    /// The request being served, and why.
    pub plan: Option<Plan>,
    /// Why the request was ended early, when it was.
    pub stopped: Option<Stop>,
    /// What php's phpdbg takes to be executing.
    pub execution: Execution,
    /// `PHPDBG_IS_STEPPING`.
    pub stepping: bool,
    /// `PHPDBG_G(last_line)`.
    pub last_line: u32,
    /// `PHPDBG_G(vmret)`: what the last handler the loop ran answered,
    /// which a cycle starts at zero.
    pub vmret: i32,
    /// `PHPDBG_IS_QUIET`, which `-v` clears.
    pub quiet: bool,
    /// Whether the line leaves quietness on, `-v` not: what each cycle
    /// of php's main loop starts with.
    pub line_quiet: bool,
    pub seek: Option<Seek>,
    /// `PHPDBG_G(handled_exception)`: the object `handle` of the throwable the
    /// session last stopped on as uncaught.
    pub handled: Option<u32>,
    /// The commands of the line being read that are still to come, which a
    /// `run` that ends hands back to the prompt outside it.
    pub queued: Vec<Command>,
    /// `-rr`: quit once the `run` is over.
    pub after_serve: AfterServe,
    /// Where the `exec` file stands with the compiler.
    pub exec_compile: ExecCompile,
    /// The command a `run` left that an idle request is served for.
    pub job: Option<Command>,
    /// `PHPDBG_G(prompt)[0]`: the prompt's text, shown with a space after.
    pub prompt: Vec<u8>,
    /// `PHPDBG_G(oplog_list)`: each oplog `phpdbg_start_oplog` opened and
    /// no `phpdbg_end_oplog` closed yet, the last the one written to.
    pub oplogs: Vec<Vec<Logged>>,
    /// `PHPDBG_G(file_sources)`: each file compiled, in the order of its
    /// last compile, with its oplines, as `phpdbg_get_executable` walks
    /// them, and its source, as a listing reads it ([`crate::list`]).
    pub sources: Vec<Source>,
    /// The frame the loop is stepping, the nearest user frame to a native
    /// it calls, which `phpdbg_break_next` reads.
    pub standing: Option<FrameRef>,
    /// `PHPDBG_G(watchpoint_hit)`: a watch reported since the check that
    /// reads it last cleared it.
    pub watch_hit: bool,
    /// `phpdbg_fully_started`: a serving has been entered, so the
    /// `exec` file's first compile is behind the session, and a
    /// fatal's log stands at a prompt.
    pub started: bool,
    /// The frame a registered call at a paused prompt left its throwable
    /// in: php's `EX(opline)` at `EG(exception_op)`, so the `run` goes on
    /// there by unwinding it, whether or not the throwable is still
    /// pending by then.
    pub thrown_into: Option<FrameRef>,
    /// Whose code the loop reaches.
    pub origin: Origin,
    /// php's `EG(exit_status)`, which the last write decides and the
    /// process exits with. The oracle's build is not thread-safe, so its
    /// `executor_globals` are built once (`executor_globals_ctor` runs
    /// again at each `php_module_startup` under ZTS alone, `Z/Zend/zend.c:1146`)
    /// and the status outlives the cycles of phpdbg's main loop
    /// (`Z/sapi/phpdbg/phpdbg.c:1707-1709`); this host serves those
    /// cycles as requests of the engine's, each started with it.
    pub exit_status: i32,
    /// A status phpdbg wrote itself during the request being served,
    /// `EG(exit_status) = FAILURE` where `run` could not compile, which
    /// the request's own ending does not know of.
    pub status: Option<i32>,
    /// What the line asked of `phpdbg_init`.
    pub init_files: crate::init::Files,
    /// Where a `run` in the init files left their reading, for once the
    /// program it ran inline is over.
    pub init_rest: Option<crate::init::Rest>,
    /// `PHPDBG_IS_CLEANING` while the init files' lines after a `run`
    /// are read: what would serve code bails out of it at once.
    pub init_cleaning: bool,
    /// A `run` in the init files asked for the program inline.
    pub inline_serve: bool,
    /// `PHPDBG_G(cur_command)`: the command line the cycle parsed last,
    /// an init file's or the prompt's, since the last `phpdbg_clean` that
    /// let it go, at a `run`'s end, `clean` and `exec`; a restart keeps it,
    /// and the next cycle resubmits it ([`Session::first_command`]).
    pub command_line: Option<Vec<u8>>,
    /// A yes to a `run` the init files read after their `run`, php's
    /// `phpdbg_startup_run++`: the cycle's startup `run` asks after them.
    pub startup_serve: bool,
    /// `first_command`: the line the last cycle parsed last, which this
    /// cycle's startup `run` resubmits at a prompt in place of `run`
    /// (`Z/sapi/phpdbg/phpdbg.c:1588-1592`, `:1694`).
    pub first_command: Option<Vec<u8>>,
    /// A refusal inside the log's prompt bailed out of it.
    pub left_log: bool,
    /// The request the init files are read in holds the `exec` file's
    /// program at its first opline, which a `run` among them runs.
    pub program_held: bool,
    /// The program a `run` in the init files ran, kept for a `print exec`
    /// after it, which compiles it again where no request stands.
    pub ran_code: Option<crate::read::Code>,
    /// A cycle of php's main loop has started and its first request has
    /// not yet read the init files.
    pub init_due: bool,
    /// `PHPDBG_IS_INITIALIZING`: the init files are being read, and the
    /// loop checks nothing between handlers but for a throwable nothing
    /// catches.
    pub initializing: bool,
    /// The init files have been read once: `phpdbg_fully_started` is set
    /// after the first cycle's, and stays set.
    pub init_ran: bool,
    /// The request for the init files stands where a `run` was asked for.
    pub init_runs: bool,
    /// The init files being read have compiled the `exec` file, which php's
    /// `print exec` and `run` do there before its cycle does.
    pub init_compiled: bool,
    /// Whether the line has notices and errors coloured, `-b` not: what
    /// each cycle of php's main loop starts with.
    pub line_colours: Colours,
    /// What phpdbg keeps of its own stream.
    pub out: Out,
    /// `PHPDBG_G(buffer)`, the last line read that held something
    /// ([`crate::input::remembered`]).
    pub buffer: Option<Vec<u8>>,
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
            exec_in_log: false,
            breaks: Breakpoints::default(),
            carried: None,
            watches: Watches::default(),
            registered: Vec::new(),
            plan: None,
            stopped: None,
            execution: Execution::Not,
            stepping: false,
            last_line: 0,
            vmret: 0,
            quiet: true,
            line_quiet: true,
            seek: None,
            handled: None,
            queued: Vec::new(),
            after_serve: AfterServe::Prompt,
            exec_compile: ExecCompile::Unannounced,
            job: None,
            prompt: b"prompt>".to_vec(),
            oplogs: Vec::new(),
            sources: Vec::new(),
            standing: None,
            watch_hit: false,
            started: false,
            thrown_into: None,
            origin: Origin::Program,
            exit_status: 0,
            status: None,
            init_files: crate::init::Files::default(),
            init_rest: None,
            init_cleaning: false,
            inline_serve: false,
            command_line: None,
            startup_serve: false,
            first_command: None,
            left_log: false,
            program_held: false,
            ran_code: None,
            init_due: true,
            initializing: false,
            init_ran: false,
            init_compiled: false,
            init_runs: false,
            line_colours: Colours::On,
            out: Out::default(),
            buffer: None,
        }
    }
}

impl Default for Session {
    fn default() -> Session {
        Session::new()
    }
}

/// The session taken out of the request's sink, none while a prompt
/// holds it or where the request writes to another sink.
pub fn take(req: &mut Request) -> Option<Session> {
    tier::sink::<Sink>(req)?.session.take()
}

/// The session put back in the request's sink.
pub fn put(req: &mut Request, session: Session) {
    if let Some(sink) = tier::sink::<Sink>(req) {
        sink.session = Some(session);
    }
}

/// `f` called with `s` lent back to the request's sink, for what the prompt
/// serves; `s` is what the sink holds after, what `f` left of it.
pub fn lend<R>(req: &mut Request, s: &mut Session, f: impl FnOnce(&mut Request) -> R) -> R {
    put(req, std::mem::take(s));
    let result = f(req);
    if let Some(back) = take(req) {
        *s = back;
    }
    result
}

/// `f` over the session where it stands in the request's sink; none
/// where it is out of it, which is where a safepoint inside what the
/// session itself is doing finds it.
pub fn with<R>(req: &mut Request, f: impl FnOnce(&mut Session) -> R) -> Option<R> {
    tier::sink::<Sink>(req)?.session.as_mut().map(f)
}

/// `f` over phpdbg's stream where the session stands in the request's
/// sink: what the debugger writes while the program runs.
pub fn say(req: &mut Request, f: impl FnOnce(&mut Out)) {
    with(req, |s| f(&mut s.out));
}
