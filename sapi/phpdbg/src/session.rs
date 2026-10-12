//! The debugging session: what phpdbg keeps across the requests it
//! serves (`zend_phpdbg_globals`, `Z/sapi/phpdbg/phpdbg.h`), in one
//! value the host owns. The tier's `safepoint` is a function with no
//! receiver, so the value stands in a thread-local cell the tier takes it
//! out of while it runs a prompt and puts back after. What the prompt
//! itself serves, an `ev` or a registered function, runs with the session
//! lent back to the cell ([`lend`]), so a native reads and writes it as
//! php's read `PHPDBG_G` at any time, and the debugger drives it as it
//! drives the program, by [`Session::origin`]'s rules where `ev` serves.

use std::cell::RefCell;
use std::collections::BTreeSet;
use std::path::PathBuf;

use php_ext::request::Opcode;
use php_sapi::tier::FrameRef;

use crate::bp::Breakpoints;
use crate::cmd::Command;
use crate::out::Colours;
use crate::watch::Watches;

/// Why the request being served was started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Plan {
    /// The execution context compiled and held at its first opline, for
    /// the commands before `run`: php's compile after each restart.
    Hold,
    /// `run`: the program served under the debugger.
    Run,
    /// A request with no program of the execution context's: where
    /// there is none to hold, or the compiler refused it, the prompt's,
    /// standing across its commands as php's one request does; or the
    /// one command a run left that needs a request standing.
    Idle,
    /// A request with no program, where the cycle starts with a context
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
    /// A command a run left that needs a request standing: served by a
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
/// a run stands under it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Execution {
    /// Nothing: `in_execution` is clear.
    Not,
    /// `run` has begun the program, which stands until its request ends.
    Run,
    /// No run stands, but `in_execution` is set: an `exit` served at a
    /// prompt where nothing was executing bailed out of
    /// `phpdbg_execute_ex`, which set it on entry, past the return that
    /// puts it back (`Z/sapi/phpdbg/phpdbg_prompt.c:1626-1660, 1814`), so
    /// php asks before `run` restarts, `clean` cleans or `exec` replaces
    /// what it takes to be executing, and a command that goes on with the
    /// program is not refused as not running.
    BailedOut,
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
/// with no listing, and the seek and the report of an uncaught throwable
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
    /// `exec` set the context at a prompt inside the host's log, where
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
    /// The command a run left that an idle request is served for.
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
    /// execution context's first compile is behind the session, and a
    /// fatal's log stands at a prompt.
    pub started: bool,
    /// The frame a registered call at a paused prompt left its throwable
    /// in: php's `EX(opline)` at `EG(exception_op)`, so the run goes on
    /// there by unwinding it, whether or not the throwable is still
    /// pending by then.
    pub thrown_into: Option<FrameRef>,
    /// Whose code the loop reaches.
    pub origin: Origin,
    /// php's `EG(exit_status)`, which the last write decides and the
    /// process exits with. The oracle's build is not thread-safe, so its
    /// executor's globals are built once (`executor_globals_ctor` runs
    /// again at a module's start under ZTS alone, `Z/Zend/zend.c:1146`)
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
    pub inline_run: bool,
    /// A command line of the init files was read after a `run` among
    /// them, which a yes to the question after it has php's next cycle
    /// resubmit at its startup run (`first_command`) instead of `run`.
    pub read_after_run: bool,
    /// A yes to a `run` the init files read after their run, php's
    /// `phpdbg_startup_run++`: the cycle's startup run asks after them.
    pub startup_run: bool,
    /// The last cycle's init files read a line after their run, which
    /// php keeps as `first_command` for this cycle's startup run.
    pub resubmit: bool,
    /// A refusal inside the log's prompt bailed out of it.
    pub left_log: bool,
    /// The request the init files run in holds the execution context's
    /// program at its first opline, which a `run` among them runs.
    pub program_held: bool,
    /// The program a `run` in the init files ran, kept for a `print exec`
    /// after it, which compiles it again where no request stands.
    pub ran_code: Option<crate::read::Code>,
    /// A cycle of php's main loop has started and its first request has
    /// not yet run the init files.
    pub init_due: bool,
    /// `PHPDBG_IS_INITIALIZING`: the init files are being run, and the
    /// loop checks nothing between handlers but for a throwable nothing
    /// catches.
    pub initializing: bool,
    /// The init files have been run once: `phpdbg_fully_started` is set
    /// after the first cycle's, and stays set.
    pub init_ran: bool,
    /// What the compiler wrote of a context it refused, which a cycle's
    /// request for its init files writes after them.
    pub refused: Vec<crate::out::Written>,
    /// The request for the init files stands where a run was asked for.
    pub init_runs: bool,
    /// The init files being run have compiled the context, which php's
    /// `print exec` and `run` do there before its cycle does.
    pub init_compiled: bool,
    /// Whether the line has notices and errors coloured, `-b` not: what
    /// each cycle of php's main loop starts with.
    pub line_colours: Colours,
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
            after_run: AfterRun::Prompt,
            context: Context::Unannounced,
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
            inline_run: false,
            read_after_run: false,
            startup_run: false,
            resubmit: false,
            left_log: false,
            program_held: false,
            ran_code: None,
            init_due: true,
            initializing: false,
            init_ran: false,
            init_compiled: false,
            refused: Vec::new(),
            init_runs: false,
            line_colours: Colours::On,
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

/// `f` run with `s` lent back to its cell, for what the prompt serves;
/// `s` is what the cell holds after, what `f` left of it.
pub fn lend<R>(s: &mut Session, f: impl FnOnce() -> R) -> R {
    put(std::mem::take(s));
    let result = f();
    if let Some(back) = take() {
        *s = back;
    }
    result
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
