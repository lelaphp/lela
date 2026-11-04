//! Thresholds and the word's state machine, pure (`jit-design.md` §4.7):
//! [`Policy`], php's `opcache.jit*` directives with the tier's two
//! engine-only ones read once at engine start; [`read`], [`bump`],
//! [`claim`] and [`settle`], the one state machine over a [`Word`]
//! whichever home holds it; and [`JitDebug::dump`], php's `TRACE` lines.
//! Nothing here touches a request, a frame, a `Mutex` or a table.
//!
//! The word's second half is the state and the first the heat
//! (`jit-design.md` §3.2):
//!
//! | bits of `halves[1]` | meaning |
//! |---|---|
//! | 0..2 | cold 0, recording 1, compiled 2, blacklisted 3 |
//! | 2..8 | failed attempts |
//! | 8..32 | the [`TraceId`] it enters, when compiled |
//!
//! so the sealed word, all ones, reads blacklisted: the one bit pattern
//! the engine knows, and a layout invariant held by a test below. A
//! landing word has one state more, [`HEADED`], read by [`armed`] and
//! [`sealed`] and blacklisted to every other reader.

use std::io::{self, Write};
use std::sync::atomic::Ordering;

use php_engine::api::request::{Ini, TierStatus};
use php_engine::tier::{Bound, Epoch, FuncRef, Laid, SafepointIdx, StartupIni, Word};
use php_unit::{Opcode, OplineIdx};

use crate::types::ExitId;

use php_unit::decl::IniDecl;

use crate::record::{self, Start, Stop};
use crate::table::{TraceId, code};

/// How a unit's words are laid where php hands it counters out of a
/// pool no request start filled ([`Policy::laid`]): cold, with the heat
/// at its top, so the first visit's bump reaches any threshold and
/// claims, and an abort or a release starts the count again from zero
/// as php's slot of the pool starts again from `ZEND_JIT_COUNTER_INIT`.
const PRIMED: Laid = Laid::Counting([PRIMED_HEAT, COLD]);

/// The heat of a word php's unfilled pool hands a slot for: the top
/// ([`PRIMED`]).
const PRIMED_HEAT: u32 = u32::MAX;

/// The state bits of a word's second half: [`decoded`] reads them, and
/// so does compiled code where a trace escapes to a landing whose word
/// reads [`COMPILED`] (`lower::Escapes`), the trace number above
/// [`ID_SHIFT`].
pub(crate) const STATE: u32 = 0b11;
const COLD: u32 = 0;
const RECORDING: u32 = 1;
pub(crate) const COMPILED: u32 = 2;
const BLACKLISTED: u32 = 3;
/// Where the failed attempts stand, and how many the six bits hold.
const ATTEMPTS_SHIFT: u32 = 2;
const ATTEMPTS_MAX: u8 = 0x3F;
/// Where the trace number stands.
pub(crate) const ID_SHIFT: u32 = 8;
/// The state half of a landing word a loop's trace was published at
/// ([`head`]): a word state of its own, neither sealed nor armed, whose
/// state bits read blacklisted, so no visit of the word itself acts and
/// compiled code reads no trace in it. An armed word that reads
/// blacklisted holds nothing above its attempts ([`Claim::store`]), so
/// none reads as this.
const HEADED: u32 = !(1 << ID_SHIFT);

/// php's `ZEND_JIT_ON_HOT_TRACE`, the one trigger this tier runs.
const HOT_TRACE: u32 = 5;

/// How the tier traces where it traces, decided once at engine start;
/// when it traces is [`Runs`]'s.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    /// A safepoint answers `Continue` before it reads a word.
    Off,
    /// php's tracing tier: roots grow at their thresholds.
    Tracing,
    /// Every eligible root compiled at its first execution (`jit.mode=force`).
    Force,
    /// Every guard fails once, so every landing is taken (`jit.mode=exit-fuzz`).
    ExitFuzz,
}

/// Where a trace compiles: on the engine's compile thread, or on the
/// request's at the claim (`jit.compile`). `Force` and `ExitFuzz` imply
/// `Inline`; so does, for one recording, the request's `opcache.jit_debug`
/// where it asks for a line the compile writes ([`JitDebug::on_request`]),
/// which php writes on the request in one order a worker could not keep
/// (`compiler::Submitter::submit`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Compile {
    Thread,
    Inline,
}

/// `opcache.jit_debug` by php's bit names (`Z/ext/opcache/jit/zend_jit.h:58-77`),
/// the ones the tier reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) struct JitDebug(u32);

impl JitDebug {
    /// php's analysis of each op array a compiled trace passes through,
    /// `ZEND_JIT_DEBUG_SSA`.
    pub(crate) const SSA: u32 = 1 << 1;
    /// A `/tmp/perf-<pid>.map` line per trace.
    pub(crate) const PERF: u32 = 1 << 4;
    /// A jitdump record per trace, where php writes one.
    pub(crate) const PERF_DUMP: u32 = 1 << 5;
    /// The engine's end writes how many bytes its traces took.
    pub(crate) const SIZE: u32 = 1 << 9;
    pub(crate) const TRACE_START: u32 = 1 << 12;
    pub(crate) const TRACE_STOP: u32 = 1 << 13;
    pub(crate) const TRACE_COMPILED: u32 = 1 << 14;
    pub(crate) const TRACE_EXIT: u32 = 1 << 15;
    pub(crate) const TRACE_ABORT: u32 = 1 << 16;
    pub(crate) const TRACE_BLACKLIST: u32 = 1 << 17;

    /// The bits a value of the directive sets, read as php's
    /// `zend_ini_parse_quantity` reads it into `JIT_G(debug)`, a
    /// `zend_long` whose low 32 bits are every bit php names, so `-1`
    /// asks for every line.
    pub(crate) fn of(value: &[u8]) -> JitDebug {
        JitDebug::bits(Ini::parse_quantity(value).0)
    }

    /// The low 32 bits of `JIT_G(debug)` as php stored it.
    pub(crate) fn bits(debug: i64) -> JitDebug {
        JitDebug(u32::try_from(debug & 0xFFFF_FFFF).unwrap_or(0))
    }

    /// The bits as `JIT_G(debug)` holds them.
    pub(crate) fn get(self) -> u32 {
        self.0
    }

    /// Whether any of `bits` is set.
    pub(crate) fn any(self, bits: u32) -> bool {
        self.0 & bits != 0
    }

    /// Whether the bits ask for any of a root's lines after its start,
    /// `stop`, `compiled`, `abort` or `blacklisted`, which php writes on
    /// the request in the order it compiles: where they do, the
    /// recording compiles on the request.
    pub(crate) fn ends(self) -> bool {
        self.any(
            JitDebug::TRACE_STOP
                | JitDebug::TRACE_COMPILED
                | JitDebug::TRACE_ABORT
                | JitDebug::TRACE_BLACKLIST,
        )
    }

    /// Whether the bits ask for a line a recording's compile writes: a
    /// root's end lines ([`JitDebug::ends`]), or php's analysis of the op
    /// arrays it passes through, which php's build of the trace writes on
    /// the request (`zend_jit_trace_build_ssa`): where they do, the
    /// recording compiles on the request.
    pub(crate) fn on_request(self) -> bool {
        self.ends() || self.any(JitDebug::SSA)
    }

    /// One of php's `TRACE` lines for `event`, where the bits ask for it
    /// (`Z/ext/opcache/jit/zend_jit_trace.c:8152-8250`).
    pub(crate) fn dump(self, sink: &mut dyn Write, event: &TraceEvent<'_>) -> io::Result<()> {
        let debug = self;
        match *event {
            TraceEvent::Start { trace, start, at } if debug.any(JitDebug::TRACE_START) => {
                let n = trace.get();
                match start {
                    Start::Side { parent, exit } => write!(
                        sink,
                        "---- TRACE {n} start (side trace {}/{}) ",
                        parent.get(),
                        exit.0
                    ),
                    _ => write!(sink, "---- TRACE {n} start ({}) ", start.describe()),
                }?;
                write_place(sink, at)
            }
            TraceEvent::Exit { trace, exit, at } if debug.any(JitDebug::TRACE_EXIT) => {
                write!(sink, "     TRACE {} exit {} ", trace.get(), exit.0)?;
                write_place(sink, at)
            }
            TraceEvent::Stop { trace, stop } if debug.any(JitDebug::TRACE_STOP) => match stop {
                Stop::Link(to) => writeln!(
                    sink,
                    "---- TRACE {} stop (link to {})",
                    trace.get(),
                    to.get()
                ),
                _ => writeln!(
                    sink,
                    "---- TRACE {} stop ({})",
                    trace.get(),
                    stop.describe()
                ),
            },
            TraceEvent::Compiled { trace } if debug.any(JitDebug::TRACE_COMPILED) => {
                writeln!(sink, "---- TRACE {} compiled", trace.get())
            }
            TraceEvent::Abort { trace, stop } if debug.any(JitDebug::TRACE_ABORT) => {
                writeln!(
                    sink,
                    "---- TRACE {} abort ({})",
                    trace.get(),
                    stop.describe()
                )
            }
            TraceEvent::Blacklisted { trace } if debug.any(JitDebug::TRACE_BLACKLIST) => {
                writeln!(sink, "---- TRACE {} blacklisted", trace.get())
            }
            TraceEvent::ExitBlacklisted { trace, exit } if debug.any(JitDebug::TRACE_BLACKLIST) => {
                writeln!(sink, "---- EXIT {}/{} blacklisted", trace.get(), exit.0)
            }
            TraceEvent::Done if debug.ends() => writeln!(sink),
            _ => Ok(()),
        }
    }
}

/// What `opcache.jit` says, as `zend_jit_config` reads it
/// (`Z/ext/opcache/jit/zend_jit.c:3646-3700`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Setting {
    /// `disable`: the tier is never set up.
    Disable,
    /// `off`, `0` and their kind: set up, not on.
    Off,
    /// On, as php's four `CRTO` digits: `on` and `tracing` are 1254,
    /// `function` 1205.
    On { crto: u32 },
    /// What php refuses with its warning, and runs with the tier off;
    /// `crto` the digits php wrote to `JIT_G` before the one it refused,
    /// the rest zero.
    Invalid { crto: u32 },
}

impl Setting {
    /// `zend_jit_config` over the setting's text.
    pub(crate) fn of(value: &[u8]) -> Setting {
        let lower = value.to_ascii_lowercase();
        match lower.as_slice() {
            b"disable" => Setting::Disable,
            b"" | b"0" | b"off" | b"no" | b"false" => Setting::Off,
            b"1" | b"on" | b"yes" | b"true" | b"tracing" => Setting::On { crto: 1254 },
            b"function" => Setting::On { crto: 1205 },
            _ => std::str::from_utf8(value)
                .ok()
                .map(|text| text.trim_start_matches(is_c_space))
                .and_then(|text| text.parse::<i64>().ok())
                .map_or(Setting::Invalid { crto: 0 }, Setting::of_number),
        }
    }

    /// `zend_jit_parse_config_num`: zero is off; otherwise each digit in
    /// its range from the right, `O` 1 to 5, `T` 0 to 5 but 4, `R` 0 to
    /// 2, `C` 0 or 1, and nothing past them. php writes each digit to
    /// `JIT_G` as it passes it, so a refused number keeps those it
    /// passed.
    fn of_number(jit: i64) -> Setting {
        if jit == 0 {
            return Setting::Off;
        }
        if jit < 0 {
            return Setting::Invalid { crto: 0 };
        }
        let ranges: [fn(i64) -> bool; 4] = [
            |o| (1..=5).contains(&o),
            |t| t <= 5 && t != 4,
            |r| r <= 2,
            |c| c <= 1,
        ];
        let (mut rest, mut crto, mut place) = (jit, 0, 1);
        for valid in ranges {
            let digit = rest % 10;
            if !valid(digit) {
                return Setting::Invalid { crto };
            }
            // A digit below ten, at a place below ten thousand.
            crto += u32::try_from(digit).unwrap_or(0) * place;
            (rest, place) = (rest / 10, place * 10);
        }
        if rest == 0 {
            Setting::On { crto }
        } else {
            Setting::Invalid { crto }
        }
    }

    /// `JIT_G` as `zend_jit_config` leaves it: `enabled` but under
    /// `disable` and a refused setting, `on` where on, and the `CRTO`
    /// digits php wrote, `ZEND_JIT_CPU_AVX` for a `C` of one. No buffer
    /// yet: php's start lays it down ([`SetUp`]).
    fn status(self) -> TierStatus {
        let digits = |crto: u32| {
            let digit = |place: u32| u8::try_from(crto / 10_u32.pow(place) % 10).unwrap_or(0);
            TierStatus {
                kind: digit(1),
                opt_level: digit(0),
                opt_flags: digit(2) | (digit(3) << 2),
                ..TierStatus::default()
            }
        };
        match self {
            Setting::Disable => TierStatus::default(),
            Setting::Off => TierStatus {
                enabled: true,
                ..TierStatus::default()
            },
            Setting::On { crto } => TierStatus {
                enabled: true,
                on: true,
                ..digits(crto)
            },
            Setting::Invalid { crto } => digits(crto),
        }
    }
}

/// C's `isspace` in the C locale, which `ZEND_STRTOL` skips before a
/// number: Rust's ASCII whitespace and the vertical tab.
pub(crate) fn is_c_space(c: char) -> bool {
    c.is_ascii_whitespace() || c == '\x0B'
}

/// When a tier that is not `Off` traces, decided once at engine start
/// ([`Policy::bound`], [`Policy::laid`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Runs {
    /// From the start: `opcache.jit` turns php's tier on at startup, so
    /// `JIT_G(on)` holds at every request start, which fills php's pool of
    /// counters (`zend_jit_activate`, `Z/ext/opcache/jit/zend_jit.c:3906-3912`).
    FromStart,
    /// Set up and off, `opcache.jit=off` with a buffer: only what a
    /// request publishes after it switches the tier on is traced, out of
    /// a pool of counters no request start fills.
    Switched,
    /// A `jit.mode` lane: every unit counts whatever `opcache.jit` says.
    Lane,
}

/// A line the start of a tier that runs logs (`jit-design.md` §4,
/// §4.7). A tier that runs [`Mode::Off`] logs none, as php writes none:
/// php never starts its compiled tier where opcache does not start, nor
/// under `opcache.jit=disable`, a refused setting or a zero buffer
/// ([`SetUp`]), so nothing reads `opcache.jit_debug`
/// and php writes nothing whatever the bits ask (`gh13232.phpt`); a
/// refused setting's warning is the opcache extension's. Under
/// `opcache.jit=off` php starts it and runs no trace, so no `TRACE` line
/// is written; the bits it reads then are for its stubs (`ASM_STUBS`,
/// `PERF`, `GDB`, `zend_jit_ir.c:2981-2986`), and this tier has none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Note {
    /// `function`, or a number whose trigger is not `HOT_TRACE`, runs as
    /// tracing: there is one tier and no optimizer.
    AsTracing { crto: u32 },
    /// `jit.mode` asked for a lane: `force` or `exit-fuzz`.
    Lane(Mode),
    /// An engine-only directive set to a word it does not know, and
    /// read as unset.
    Unknown {
        directive: &'static str,
        value: String,
    },
}

impl std::fmt::Display for Note {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Note::AsTracing { crto } => write!(
                f,
                "JIT: opcache.jit={crto} asks for a trigger other than tracing; it runs as tracing"
            ),
            Note::Lane(mode) => write!(f, "JIT: jit.mode={}", mode_name(*mode)),
            Note::Unknown { directive, value } => {
                write!(f, "JIT: {directive}={value} is not known; read as unset")
            }
        }
    }
}

/// How php's start leaves its compiled tier, decided once at engine
/// start from the directives and the host: `accel_startup`, which starts
/// opcache at all, and `accel_post_startup`, which sets the tier up only
/// where opcache started, `JIT_G(enabled)` holds and the buffer is not
/// empty, and then asks `zend_jit_check_support`
/// (`Z/ext/opcache/ZendAccelerator.c:3210-3220`, `:3253-3254`). Only a
/// tier set up is refused with php's warning; under any other, nothing
/// reads `opcache.jit_debug` and nothing is raised.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SetUp {
    /// opcache does not start for the host: `opcache.enable` is off, or
    /// the host is the command line or phpdbg (`accel_sapi_is_cli`) and
    /// `opcache.enable_cli` is off, php's `Opcode Caching is disabled
    /// for CLI`.
    Unstarted,
    /// opcache starts and its tier is not set up: `opcache.jit=disable`,
    /// a setting `zend_jit_config` refuses, a buffer of no bytes, or the
    /// file cache alone, `opcache.file_cache_only`, which lays no segment
    /// (`Z/ext/opcache/ZendAccelerator.c:3336-3360`).
    Disabled,
    /// Set up as the directives ask: `opcache.jit` on or off, with a
    /// buffer.
    Yes,
    /// Set up and refused by `zend_jit_check_support`.
    Refused(Refused),
}

impl SetUp {
    /// `accel_startup`'s verdict and `accel_post_startup`'s test over the
    /// startup ini, for the host `php_sapi_name()` names.
    fn of(ini: &StartupIni, host: &str, setting: Setting, buffer_size: i64) -> SetUp {
        let flag = |name: &str, default: bool| value(ini, name).map_or(default, Ini::parse_bool);
        let command_line = matches!(host, "cli" | "phpdbg");
        let started =
            flag("opcache.enable", true) && (!command_line || flag("opcache.enable_cli", false));
        match setting {
            _ if !started => SetUp::Unstarted,
            _ if flag("opcache.file_cache_only", false) => SetUp::Disabled,
            Setting::Off | Setting::On { .. } if buffer_size > 0 => SetUp::Yes,
            _ => SetUp::Disabled,
        }
    }
}

/// Why php refuses its compiled tier once it set it up, with a warning
/// raised once through the startup diagnostics, whatever
/// `opcache.jit_debug` asks, since it is php's `zend_error` and not a
/// `TRACE` line; the tier then runs `Off` ([`Policy::check_support`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refused {
    /// An extension observes an opcode other than the two silence
    /// opcodes (design §7.1).
    ObservedOpcode,
    /// `opcache.jit_buffer_size` is past what the target's code reaches,
    /// [`REACH`].
    BufferSize,
}

/// The largest `opcache.jit_buffer_size` `zend_jit_check_support` takes
/// on the target, with php's warning past it
/// (`Z/ext/opcache/jit/zend_jit.c:3759-3772`): 128 MiB on `aarch64`, 2 GiB
/// on `x86_64`, and no bound on a target php checks none on.
const REACH: (u64, &str) = if cfg!(target_arch = "aarch64") {
    (
        128 << 20,
        "JIT on AArch64 doesn't support opcache.jit_buffer_size above 128M.",
    )
} else if cfg!(target_arch = "x86_64") {
    (
        2 << 30,
        "JIT on x86_64 doesn't support opcache.jit_buffer_size above 2G.",
    )
} else {
    (u64::MAX, "")
};

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refused::ObservedOpcode => f.write_str(
                "JIT is incompatible with third party extensions that setup user opcode handlers. JIT disabled.",
            ),
            Refused::BufferSize => f.write_str(REACH.1),
        }
    }
}

/// `jit.mode`'s word for a `Mode`.
fn mode_name(mode: Mode) -> &'static str {
    match mode {
        Mode::Off => "off",
        Mode::Tracing => "tracing",
        Mode::Force => "force",
        Mode::ExitFuzz => "exit-fuzz",
    }
}

/// The directives the tier declares itself, each system ini with its
/// default: the two engine-only ones. php's `opcache.jit*` are the
/// opcache extension's, which registers them as php does with php's
/// modes and handlers, so `ini_get` answers as the oracle's and nothing
/// is declared twice; `Policy` reads them from the startup ini.
pub(crate) const INI: [(&str, &str); 2] = [("jit.mode", ""), ("jit.compile", "thread")];

/// [`INI`] as the entries an extension declares.
pub(crate) fn ini() -> Vec<IniDecl> {
    INI.iter()
        .map(|(name, default)| IniDecl {
            name: name.as_bytes(),
            default: Some(default.as_bytes()),
            user: false,
            per_dir: false,
            system: true,
        })
        .collect()
}

/// The tier's configuration, read once from the startup ini.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Policy {
    mode: Mode,
    compile: Compile,
    hot_loop: u8,
    hot_func: u8,
    hot_return: u8,
    hot_side_exit: u8,
    blacklist_root: u8,
    blacklist_side: u8,
    max_trace_length: u32,
    max_root_traces: u32,
    max_side_traces: u32,
    max_exit_counters: u32,
    max_loop_unrolls: u8,
    max_recursive_calls: u8,
    max_recursive_returns: u8,
    max_polymorphic_calls: u32,
    buffer_size: u64,
    folds: crate::record::Folds,
    debug: JitDebug,
    /// How php's start leaves its tier under these directives and this
    /// host, which is where `zend_jit_check_support` runs and so where its
    /// refusal warns ([`Policy::check_support`]).
    set_up: SetUp,
    /// When the tier traces what it may: from the start, once a request
    /// switches it on, or always, for a lane ([`Runs`]).
    runs: Runs,
    /// What php's `zend_jit_status` reports once its start is over, no
    /// code in the buffer ([`Policy::status`]).
    started: TierStatus,
}

/// What `zend_jit_status` reports once php's start is over: `JIT_G` as
/// `opcache.jit` set it ([`Setting::status`]), kept where php sets the
/// tier up and its buffer down, `dasm_end - dasm_buf` the buffer
/// ([`code::capacity`]) less the two pointers `zend_jit_startup` keeps at
/// its end (`Z/ext/opcache/jit/zend_jit.c:3790`), all free; `enabled`
/// and `on` dropped for a buffer of no bytes, as `accel_post_startup`
/// drops them (`Z/ext/opcache/ZendAccelerator.c:3322-3325`).
fn started(setting: Setting, set_up: SetUp, buffer_size: i64) -> TierStatus {
    let status = setting.status();
    match set_up {
        SetUp::Yes => {
            let buffer = code::capacity(u64::try_from(buffer_size).unwrap_or(0))
                .saturating_sub(2 * size_of::<usize>());
            let buffer = u64::try_from(buffer).unwrap_or(0);
            TierStatus {
                buffer_size: buffer,
                buffer_free: buffer,
                ..status
            }
        }
        SetUp::Disabled => TierStatus {
            enabled: false,
            on: false,
            ..status
        },
        SetUp::Unstarted | SetUp::Refused(_) => status,
    }
}

/// The value the host set for `name`, none where it set none.
fn value<'a>(ini: &'a StartupIni, name: &str) -> Option<&'a [u8]> {
    ini.entries
        .iter()
        .find(|(entry, _)| entry.as_slice() == name.as_bytes())
        .and_then(|(_, value)| value.as_deref())
}

/// `name` read as php's quantity, `default` where unset.
fn long(ini: &StartupIni, name: &str, default: i64) -> i64 {
    value(ini, name).map_or(default, |text| Ini::parse_quantity(text).0)
}

/// `name` held to php's bounds for it, `default` outside them, as php's
/// `OnUpdateCounter`, `OnUpdateUnroll*` and `OnUpdateMaxTraceLength`
/// refuse a value and keep the default.
fn within(
    ini: &StartupIni,
    name: &str,
    default: i64,
    bounds: std::ops::RangeInclusive<i64>,
) -> i64 {
    let read = long(ini, name, default);
    if bounds.contains(&read) {
        read
    } else {
        default
    }
}

/// [`within`] for a directive php holds below 256.
fn small(ini: &StartupIni, name: &str, default: u8, bounds: std::ops::RangeInclusive<i64>) -> u8 {
    u8::try_from(within(ini, name, i64::from(default), bounds)).unwrap_or(default)
}

/// `name` as a count that cannot be negative, `default` where it is.
fn count(ini: &StartupIni, name: &str, default: u32) -> u32 {
    u32::try_from(long(ini, name, i64::from(default))).unwrap_or(default)
}

impl Policy {
    /// The directives under php's names and defaults
    /// (`Z/ext/opcache/zend_accelerator_module.c:343-362`), plus
    /// `jit.mode` and `jit.compile`, for the host `host` names, with what
    /// the start logs pushed to `notes`. A value outside php's bounds
    /// reads as the default, as php's handlers keep it. The tier runs
    /// only where php sets it up ([`SetUp`]): not where opcache does not
    /// start, `opcache.enable` off or, on the command line and in phpdbg,
    /// `opcache.enable_cli` off. `jit.mode`, where set, names the `Mode`
    /// whatever `opcache.jit`, `opcache.enable` and `opcache.enable_cli`
    /// say, which is how the tier's lanes serve a corpus that sets none
    /// of them, but where the directives set say php's tier never starts,
    /// `opcache.jit=disable`, a setting php refuses or a zero buffer: a
    /// test that says so is a test about having no tier,
    /// served by the interpreter with no lane's line (#3136's ruling). A
    /// tier that runs `Off` reads no `opcache.jit_debug` bit
    /// ([`Policy::off`]), so it writes no line, as php writes none
    /// ([`Note`]).
    ///
    /// The directives as the oracle and the engine hold them under the
    /// recipe, which sets the hot counters to one and the trace bounds to
    /// 100000:
    ///
    /// ```php
    /// foreach (['opcache.jit', 'opcache.jit_buffer_size', 'opcache.jit_debug', 'opcache.jit_bisect_limit',
    ///     'opcache.jit_prof_threshold', 'opcache.jit_max_root_traces', 'opcache.jit_max_side_traces',
    ///     'opcache.jit_max_exit_counters', 'opcache.jit_hot_loop', 'opcache.jit_hot_func',
    ///     'opcache.jit_hot_return', 'opcache.jit_hot_side_exit', 'opcache.jit_blacklist_root_trace',
    ///     'opcache.jit_blacklist_side_trace', 'opcache.jit_max_loop_unrolls', 'opcache.jit_max_recursive_calls',
    ///     'opcache.jit_max_recursive_returns', 'opcache.jit_max_polymorphic_calls',
    ///     'opcache.jit_max_trace_length'] as $name) {
    ///     echo $name, ' ', var_export(ini_get($name), true), "\n";
    /// }
    /// ```
    /// ```expect
    /// opcache.jit 'disable'
    /// opcache.jit_buffer_size '64M'
    /// opcache.jit_debug '0'
    /// opcache.jit_bisect_limit '0'
    /// opcache.jit_prof_threshold '0.005'
    /// opcache.jit_max_root_traces '100000'
    /// opcache.jit_max_side_traces '100000'
    /// opcache.jit_max_exit_counters '100000'
    /// opcache.jit_hot_loop '1'
    /// opcache.jit_hot_func '1'
    /// opcache.jit_hot_return '1'
    /// opcache.jit_hot_side_exit '1'
    /// opcache.jit_blacklist_root_trace '16'
    /// opcache.jit_blacklist_side_trace '8'
    /// opcache.jit_max_loop_unrolls '8'
    /// opcache.jit_max_recursive_calls '2'
    /// opcache.jit_max_recursive_returns '2'
    /// opcache.jit_max_polymorphic_calls '2'
    /// opcache.jit_max_trace_length '1024'
    /// ```
    pub(crate) fn from_ini(ini: &StartupIni, host: &str, notes: &mut Vec<Note>) -> Policy {
        let setting = value(ini, "opcache.jit").map_or(Setting::Disable, Setting::of);
        let buffer_size = long(ini, "opcache.jit_buffer_size", 64 << 20);
        let set_up = SetUp::of(ini, host, setting, buffer_size);
        let lane = match value(ini, "jit.mode").unwrap_or_default() {
            b"" => None,
            b"tracing" => Some(Mode::Tracing),
            b"force" => Some(Mode::Force),
            b"exit-fuzz" => Some(Mode::ExitFuzz),
            other => {
                notes.push(Note::Unknown {
                    directive: "jit.mode",
                    value: String::from_utf8_lossy(other).into_owned(),
                });
                None
            }
        };
        // `opcache.jit=disable` or a setting php refuses, set and not
        // php's default `disable`, says php's tier never starts, which a
        // lane gives way to (#3136's ruling); a zero buffer is the line
        // below's. `off` is not one: php may turn it on from a request.
        let disabled = value(ini, "opcache.jit").is_some()
            && matches!(setting, Setting::Disable | Setting::Invalid { .. });
        // Set up and off, the tier traces what a request publishes after
        // it turns the setting on ([`Policy::counts`]).
        let lane = lane.filter(|_| !disabled);
        let asked = match (lane, setting, set_up) {
            (Some(mode), ..) => mode,
            (None, Setting::On { crto }, SetUp::Yes) => {
                if (crto / 10) % 10 != HOT_TRACE {
                    notes.push(Note::AsTracing { crto });
                }
                Mode::Tracing
            }
            (None, Setting::Off, SetUp::Yes) => Mode::Tracing,
            (None, ..) => Mode::Off,
        };
        let mode = if buffer_size > 0 { asked } else { Mode::Off };
        if matches!(mode, Mode::Force | Mode::ExitFuzz) {
            notes.push(Note::Lane(mode));
        }
        let debug = value(ini, "opcache.jit_debug").map_or(JitDebug::default(), JitDebug::of);
        let compile = match (mode, value(ini, "jit.compile").unwrap_or_default()) {
            (Mode::Force | Mode::ExitFuzz, _) | (_, b"inline") => Compile::Inline,
            (_, b"" | b"thread") => Compile::Thread,
            (_, other) => {
                notes.push(Note::Unknown {
                    directive: "jit.compile",
                    value: String::from_utf8_lossy(other).into_owned(),
                });
                Compile::Thread
            }
        };
        let mut policy = Policy {
            mode,
            compile,
            hot_loop: small(ini, "opcache.jit_hot_loop", 61, 0..=255),
            hot_func: small(ini, "opcache.jit_hot_func", 127, 0..=255),
            hot_return: small(ini, "opcache.jit_hot_return", 8, 0..=255),
            hot_side_exit: small(ini, "opcache.jit_hot_side_exit", 8, 0..=255),
            blacklist_root: small(ini, "opcache.jit_blacklist_root_trace", 16, 0..=255),
            blacklist_side: small(ini, "opcache.jit_blacklist_side_trace", 8, 0..=255),
            max_trace_length: u32::try_from(within(
                ini,
                "opcache.jit_max_trace_length",
                1024,
                4..=1024,
            ))
            .unwrap_or(1024),
            max_root_traces: count(ini, "opcache.jit_max_root_traces", 1024),
            max_side_traces: count(ini, "opcache.jit_max_side_traces", 128),
            max_exit_counters: count(ini, "opcache.jit_max_exit_counters", 8192),
            max_loop_unrolls: small(ini, "opcache.jit_max_loop_unrolls", 8, 1..=9),
            max_recursive_calls: small(ini, "opcache.jit_max_recursive_calls", 2, 1..=9),
            max_recursive_returns: small(ini, "opcache.jit_max_recursive_returns", 2, 0..=3),
            max_polymorphic_calls: count(ini, "opcache.jit_max_polymorphic_calls", 2),
            buffer_size: u64::try_from(buffer_size).unwrap_or(0),
            folds: crate::record::Folds::of(long(ini, "opcache.optimization_level", 0x7FFE_BFFF)),
            debug,
            set_up,
            runs: match lane {
                Some(_) => Runs::Lane,
                None if started(setting, set_up, buffer_size).on => Runs::FromStart,
                None => Runs::Switched,
            },
            started: TierStatus {
                debug: debug.get(),
                ..started(setting, set_up, buffer_size)
            },
        };
        if mode == Mode::Off {
            policy.off();
        }
        policy
    }

    /// The tier off, as the directives ask or as a host with no
    /// `Backend` runs it (`jit-design.md` §4.3): the one place that holds
    /// an `Off` tier to reading no `opcache.jit_debug` bit ([`Note`]).
    pub(crate) fn off(&mut self) {
        self.mode = Mode::Off;
        self.debug = JitDebug::default();
    }

    /// `zend_jit_check_support` past the directives, once every other
    /// extension's start has declared what it observes
    /// (`Z/ext/opcache/jit/zend_jit.c:3728-3756`): where an extension
    /// observes an opcode other than `ZEND_BEGIN_SILENCE` and
    /// `ZEND_END_SILENCE`, by `observes`, php's
    /// `zend_get_user_opcode_handler(i) != NULL` over every opcode, the
    /// tier runs `Off` (`jit-design.md` §4.3, *Observed opcodes*), and
    /// the refusal is kept where php sets its tier up ([`SetUp::Yes`]),
    /// which is where it warns ([`Policy::refused`]); none where the tier
    /// runs as the directives say. At the
    /// two silence opcodes nothing is refused: compiled code serves them
    /// through the interpreter's handler, which tells their observers.
    /// Past them, a tier php set up whose `opcache.jit_buffer_size` the
    /// target's code cannot reach is refused too ([`Refused::BufferSize`],
    /// `:3759-3772`). A refused tier reports neither enabled nor on, and
    /// no buffer ([`Policy::status`]).
    pub(crate) fn check_support(&mut self, observes: impl Fn(Opcode) -> bool) {
        let beyond = (0..=u8::MAX)
            .filter_map(Opcode::from_u8)
            .filter(|opcode| !matches!(opcode, Opcode::BeginSilence | Opcode::EndSilence))
            .any(observes);
        let refused = if beyond {
            Refused::ObservedOpcode
        } else if self.set_up == SetUp::Yes && self.buffer_size > REACH.0 {
            Refused::BufferSize
        } else {
            return;
        };
        self.off();
        if self.set_up == SetUp::Yes {
            self.set_up = SetUp::Refused(refused);
            self.started = TierStatus {
                enabled: false,
                on: false,
                buffer_size: 0,
                buffer_free: 0,
                ..self.started
            };
        }
    }

    /// `zend_jit_status` (`Z/ext/opcache/jit/zend_jit.c:825-842`): `JIT_G`
    /// as `jit_g`, the request's, holds it, and the buffer it holds as the
    /// start laid it down, of which the code of `used` bytes is taken. php's stubs take some of it before
    /// any trace; this tier has none, so a buffer no trace was published
    /// into is free whole. The request's `JIT_G` starts as the start left
    /// it ([`Policy::start_setting`]) and is rewritten by each of its
    /// `ini_set('opcache.jit', ...)` as `zend_jit_config` rewrites it, in
    /// ext/opcache's `on_jit` alone: on, and its digits, under a setting
    /// that turns it on; `on` dropped under `off`, the digits the last
    /// such setting wrote kept; and `enabled` with it under `disable` and
    /// a setting php refuses, after which php refuses every change.
    ///
    /// Under `opcache.jit=off` with a buffer, `ini_set('opcache.jit',
    /// 'function')` turns the tier on with php's digits for it, and a
    /// later `off` keeps them:
    ///
    /// ```php
    /// $code = '$s = fn () => array_slice(opcache_get_status(false)["jit"], 0, 5); echo json_encode($s()), "\n"; ini_set("opcache.jit", "function"); echo json_encode($s()), "\n"; ini_set("opcache.jit", "off"); echo json_encode($s()), "\n";';
    /// $p = proc_open([PHP_BINARY, '-n', '-d', 'opcache.enable_cli=1', '-d', 'opcache.jit=off', '-r', $code], [1 => ['pipe', 'w']], $pipes);
    /// echo stream_get_contents($pipes[1]);
    /// proc_close($p);
    /// ```
    /// ```expect process
    /// {"enabled":true,"on":false,"kind":0,"opt_level":0,"opt_flags":0}
    /// {"enabled":true,"on":true,"kind":0,"opt_level":5,"opt_flags":6}
    /// {"enabled":true,"on":false,"kind":0,"opt_level":5,"opt_flags":6}
    /// ```
    pub(crate) fn status(used: usize, jit_g: &TierStatus) -> TierStatus {
        let used = u64::try_from(used).unwrap_or(u64::MAX);
        TierStatus {
            buffer_free: jit_g.buffer_free.saturating_sub(used),
            ..*jit_g
        }
    }

    /// php's `JIT_G` as its start leaves it, which each request starts
    /// from: `opcache.jit` as `zend_jit_config` folds it at startup,
    /// with `enabled` and `on` dropped where php's start does not keep
    /// the tier, a zero buffer, the file cache alone or a refusal of
    /// `zend_jit_check_support` ([`SetUp`]), the buffer it lays down, and
    /// `opcache.jit_debug`'s bits as the start read them, whatever the
    /// tier runs.
    pub(crate) fn start_setting(&self) -> TierStatus {
        self.started
    }

    /// How the safepoint words of a unit published now are laid
    /// ([`Laid`]), php's `zend_persist` giving an op array its counters
    /// only while `JIT_G(on)` (`Z/ext/opcache/zend_persist.c:721-726`,
    /// `:1471-1473`), which none of its handlers reads again: sealed
    /// where the tier runs `Off`, and where `on`, `JIT_G(on)` of the
    /// request publishing it ([`Policy::status`]), the start's for the
    /// engine's own, is off; counting where it is on, or where a lane
    /// counts every unit. A request whose `opcache.jit` php refused has
    /// `JIT_G(on)` off from then on, so what it includes after is never
    /// traced. A unit whose words do not count
    /// is never traced, whatever a later request sets, as php's op array
    /// keeps the handlers it was persisted with.
    ///
    /// Counting words start cold where the tier was on at the start,
    /// since php's `zend_jit_activate` fills its pool of counters at
    /// every request start where `JIT_G(on)` holds then
    /// (`Z/ext/opcache/jit/zend_jit.c:3906-3912`). Where the tier is set
    /// up and off, nothing ever fills that pool, which starts at zero
    /// (`zend_jit.c:78`), so each slot php hands a unit persisted after
    /// `ini_set('opcache.jit', ...)` fires at its first arrival, as
    /// `0 - cost <= 0` (`Z/ext/opcache/jit/zend_jit_vm_helpers.c:470`), and
    /// starts again at `ZEND_JIT_COUNTER_INIT` as it fires: such a unit's
    /// words are laid [`PRIMED`], so each roots at its first arrival and
    /// counts php's thresholds after (#3363). The pool's counters are
    /// php's per process and these words the engine's (ADR-0006), as
    /// the `trace:hot-counter-pool` row of `docs/divergence.toml` says.
    ///
    /// Under `opcache.jit=off` with a buffer and php's thresholds, a file
    /// included after `ini_set('opcache.jit', 'tracing')` roots its entry
    /// and its loop of three turns at their first arrivals:
    ///
    /// ```php
    /// $dir = realpath(sys_get_temp_dir()) . '/jit-first-' . getmypid(); mkdir($dir);
    /// file_put_contents("$dir/main.php", '<?php ini_set("opcache.jit", "tracing"); include __DIR__ . "/loop.php";');
    /// file_put_contents("$dir/loop.php", '<?php function g() {} $i = 0; while ($i < 3) { $i++; g(); } echo "loop done\n";');
    /// touch("$dir/main.php", time() - 60); touch("$dir/loop.php", time() - 60);
    /// $cmd = [PHP_BINARY, '-n', '-d', 'opcache.enable_cli=1', '-d', 'opcache.jit=off', '-d', 'opcache.jit_debug=12288', "$dir/main.php"];
    /// $p = proc_open($cmd, [1 => ['pipe', 'w'], 2 => ['pipe', 'w']], $pipes);
    /// echo stream_get_contents($pipes[1]), str_replace($dir, 'DIR', stream_get_contents($pipes[2]));
    /// proc_close($p); unlink("$dir/main.php"); unlink("$dir/loop.php"); rmdir($dir);
    /// ```
    /// ```expect process
    /// loop done
    /// ---- TRACE 1 start (enter) $main() DIR/loop.php:1
    /// ---- TRACE 1 start (loop) $main() DIR/loop.php:1
    /// ---- TRACE 1 stop (loop)
    /// ```
    ///
    /// and the same program under a startup `opcache.jit=tracing`, whose
    /// request starts with the tier on, keeps php's thresholds, so the
    /// loop of three turns roots nothing:
    ///
    /// ```php
    /// $dir = realpath(sys_get_temp_dir()) . '/jit-first-' . getmypid(); mkdir($dir);
    /// file_put_contents("$dir/main.php", '<?php ini_set("opcache.jit", "tracing"); include __DIR__ . "/loop.php";');
    /// file_put_contents("$dir/loop.php", '<?php function g() {} $i = 0; while ($i < 3) { $i++; g(); } echo "loop done\n";');
    /// touch("$dir/main.php", time() - 60); touch("$dir/loop.php", time() - 60);
    /// $cmd = [PHP_BINARY, '-n', '-d', 'opcache.enable_cli=1', '-d', 'opcache.jit=tracing', '-d', 'opcache.jit_debug=12288', "$dir/main.php"];
    /// $p = proc_open($cmd, [1 => ['pipe', 'w'], 2 => ['pipe', 'w']], $pipes);
    /// echo stream_get_contents($pipes[1]), str_replace($dir, 'DIR', stream_get_contents($pipes[2]));
    /// proc_close($p); unlink("$dir/main.php"); unlink("$dir/loop.php"); rmdir($dir);
    /// ```
    /// ```expect process
    /// loop done
    /// ```
    ///
    /// and under a startup `tracing`, after a setting php refuses, the
    /// request's tier is neither enabled nor on, a later setting is
    /// refused too, and a file it includes after is not traced:
    ///
    /// ```php
    /// $dir = realpath(sys_get_temp_dir()) . '/jit-refused-' . getmypid(); mkdir($dir);
    /// file_put_contents("$dir/main.php", '<?php ini_set("opcache.jit", "sideways"); echo json_encode(array_slice(opcache_get_status(false)["jit"], 0, 2)), "\n"; var_dump(ini_set("opcache.jit", "tracing")); include __DIR__ . "/loop.php";');
    /// file_put_contents("$dir/loop.php", '<?php $s = 0; for ($i = 0; $i < 4; $i++) { $s += $i; } echo $s, "\n";');
    /// touch("$dir/main.php", time() - 60); touch("$dir/loop.php", time() - 60);
    /// $cmd = [PHP_BINARY, '-n', '-d', 'opcache.enable_cli=1', '-d', 'opcache.jit=tracing', '-d', 'opcache.jit_debug=12288', '-d', 'opcache.jit_hot_loop=1', "$dir/main.php"];
    /// $p = proc_open($cmd, [1 => ['pipe', 'w'], 2 => ['pipe', 'w']], $pipes);
    /// echo str_replace($dir, 'DIR', stream_get_contents($pipes[1])), str_replace($dir, 'DIR', stream_get_contents($pipes[2]));
    /// proc_close($p); unlink("$dir/main.php"); unlink("$dir/loop.php"); rmdir($dir);
    /// ```
    /// ```expect process
    /// Warning: Invalid "opcache.jit" setting. Should be "disable", "on", "off", "tracing", "function" or 4-digit number in DIR/main.php on line 1
    /// {"enabled":false,"on":false}
    ///
    /// Warning: Cannot change opcache.jit setting at run-time (JIT is disabled) in DIR/main.php on line 1
    /// bool(false)
    /// 6
    /// ```
    pub(crate) fn laid(&self, on: bool) -> Laid {
        match self.runs {
            _ if self.mode == Mode::Off => Laid::Sealed,
            Runs::Lane => Laid::COLD,
            Runs::FromStart if on => Laid::COLD,
            Runs::Switched if on => PRIMED,
            Runs::FromStart | Runs::Switched => Laid::Sealed,
        }
    }

    /// How the engine binds the tier, for a host whose `Backend` was
    /// `backed`, made, where the directives asked for one ([`Bound`]):
    /// `Nop`'s loop alone where the tier never runs in this process,
    /// opcache not starting for the host, `opcache.jit=disable`, a
    /// refused setting, a buffer of no bytes ([`SetUp`]), a refusal of
    /// `zend_jit_check_support` or no `Backend`, so php's VM keeps its own
    /// handlers; both loops where it is set up and off, `opcache.jit=off`
    /// with a buffer, which php turns on from a request with
    /// `ini_set('opcache.jit', 'tracing')` (`zend_jit_config`,
    /// `Z/ext/opcache/jit/zend_jit.c:3646-3700`); and the tier's alone
    /// where it runs from the start, tracing or a lane `jit.mode` names.
    ///
    /// Under `opcache.jit=off` with a buffer, a request starts in `Nop`'s
    /// loop, and a program cached before it switches the tier on with
    /// `ini_set('opcache.jit', 'tracing')` writes no `TRACE` line from its
    /// loop ([`Policy::laid`]);
    /// a file it includes after the switch is published with counting
    /// words, which raises the request's interrupt, and at the check of
    /// the file's activation `Nop`'s loop hands the frame to the tier's,
    /// which roots its entry and its loop as php does (#3362):
    ///
    /// ```php
    /// $dir = realpath(sys_get_temp_dir()) . '/jit-switch-' . getmypid(); mkdir($dir);
    /// $loop = 'function %s() { $s = 0; for ($i = 0; $i < 4; $i++) { $s += $i; } return $s; }';
    /// file_put_contents("$dir/main.php", '<?php ' . sprintf($loop, 'before') . ' var_dump(opcache_is_script_cached(__FILE__)); ini_set("opcache.jit", "tracing"); echo include __DIR__ . "/after.php", " ", before(), "\n";');
    /// file_put_contents("$dir/after.php", '<?php ' . sprintf($loop, 'after') . ' return after();');
    /// touch("$dir/main.php", time() - 60); touch("$dir/after.php", time() - 60);
    /// $cmd = [PHP_BINARY, '-n', '-d', 'opcache.enable_cli=1', '-d', 'opcache.jit=off', '-d', 'opcache.jit_debug=12288', '-d', 'opcache.jit_hot_loop=1', '-d', 'opcache.jit_hot_func=1', "$dir/main.php"];
    /// $p = proc_open($cmd, [1 => ['pipe', 'w'], 2 => ['pipe', 'w']], $pipes);
    /// echo stream_get_contents($pipes[1]), str_replace($dir, 'DIR', stream_get_contents($pipes[2]));
    /// proc_close($p); unlink("$dir/main.php"); unlink("$dir/after.php"); rmdir($dir);
    /// ```
    /// ```expect regime=jit-off process
    /// bool(true)
    /// 6 6
    /// ---- TRACE 1 start (enter) $main() DIR/after.php:1
    /// ---- TRACE 1 start (loop) after() DIR/after.php:1
    /// ---- TRACE 1 stop (loop)
    /// ```
    pub(crate) fn bound(&self, backed: bool) -> Bound {
        match self.mode {
            Mode::Off => Bound::Never,
            _ if !backed => Bound::Never,
            _ => match self.runs {
                Runs::Lane | Runs::FromStart => Bound::On,
                Runs::Switched => Bound::Runtime,
            },
        }
    }

    /// Why php refuses the tier it set up, which the engine's first
    /// request raises as php's warning; none where it does not.
    pub(crate) fn refused(&self) -> Option<Refused> {
        match self.set_up {
            SetUp::Refused(refused) => Some(refused),
            SetUp::Unstarted | SetUp::Disabled | SetUp::Yes => None,
        }
    }

    /// How the tier runs.
    #[inline(always)] // jit-design.md §4.9: one field load before any word is read
    #[allow(clippy::inline_always)]
    pub(crate) fn mode(&self) -> Mode {
        self.mode
    }

    /// Where a trace compiles.
    pub(crate) fn compile(&self) -> Compile {
        self.compile
    }

    /// The heat at which a word of `start`'s kind is claimed; none where
    /// php's `opcache.jit_hot_*` for it is zero, which installs no handler.
    /// Under `force` and `exit-fuzz` every eligible root is claimed at its
    /// first visit (`jit-design.md` §7), so the heat is one.
    #[inline(always)] // jit-design.md §4.9: one field load at a threshold test
    #[allow(clippy::inline_always)]
    pub(crate) fn threshold(&self, start: Start) -> Option<u32> {
        let hot = match start {
            Start::Loop => self.hot_loop,
            Start::Enter => self.hot_func,
            Start::Return => self.hot_return,
            Start::Side { .. } => self.hot_side_exit,
        };
        let lane = matches!(self.mode, Mode::Force | Mode::ExitFuzz);
        (hot != 0).then_some(if lane { 1 } else { u32::from(hot) })
    }

    /// `opcache.jit_blacklist_root_trace`: how many failed attempts
    /// blacklist a root, at most the 63 the word keeps.
    pub(crate) fn blacklist_root(&self) -> u8 {
        self.blacklist_root.min(ATTEMPTS_MAX)
    }

    /// When failed attempts blacklist a word of `start`'s kind
    /// ([`Spent`]): a root's after `opcache.jit_blacklist_root_trace`
    /// of them, an exit's once its heat reaches the heat it is claimed
    /// at plus `opcache.jit_blacklist_side_trace`.
    pub(crate) fn spent(&self, start: Start) -> Spent {
        match start {
            Start::Side { .. } => Spent::At(
                self.threshold(start)
                    .unwrap_or(0)
                    .saturating_add(u32::from(self.blacklist_side)),
            ),
            Start::Loop | Start::Enter | Start::Return => Spent::After(self.blacklist_root()),
        }
    }

    /// `opcache.jit_max_trace_length`: how many records a recording holds
    /// before it stops with `TooLong`.
    pub(crate) fn max_trace_length(&self) -> u32 {
        self.max_trace_length
    }

    /// `opcache.jit_max_root_traces`: how many traces the tier numbers
    /// before a root stops with `TooManyTraces`.
    pub(crate) fn max_root_traces(&self) -> u32 {
        self.max_root_traces
    }

    /// `opcache.jit_max_side_traces`: how many side traces one root
    /// grows before a side stops with `TooManyChildren`.
    pub(crate) fn max_side_traces(&self) -> u32 {
        self.max_side_traces
    }

    /// `opcache.jit_max_exit_counters`: how many exits the traces of an
    /// epoch hold before a trace stops with `TooManyExits`.
    pub(crate) fn max_exit_counters(&self) -> u32 {
        self.max_exit_counters
    }

    /// `opcache.jit_buffer_size`: how many bytes of code an epoch holds.
    pub(crate) fn buffer_size(&self) -> u64 {
        self.buffer_size
    }

    /// `opcache.jit_debug`'s bits as the engine started with them, none
    /// for a tier that runs `Off`: what the start's lines and the `PERF`
    /// outputs read, whose bits a request cannot change. A request's
    /// `TRACE` lines read its own ([`crate::Lines`]).
    pub(crate) fn debug(&self) -> JitDebug {
        self.debug
    }

    /// `opcache.jit_max_loop_unrolls`.
    pub(crate) fn max_loop_unrolls(&self) -> u8 {
        self.max_loop_unrolls
    }

    /// Whether `opcache.optimization_level` runs the passes that fold a
    /// loop's constant condition ([`crate::record::Folds`]).
    pub(crate) fn folds(&self) -> crate::record::Folds {
        self.folds
    }

    /// `opcache.jit_max_recursive_calls`.
    pub(crate) fn max_recursive_calls(&self) -> u8 {
        self.max_recursive_calls
    }

    /// `opcache.jit_max_recursive_returns`.
    pub(crate) fn max_recursive_returns(&self) -> u8 {
        self.max_recursive_returns
    }

    /// `opcache.jit_max_polymorphic_calls`.
    pub(crate) fn max_polymorphic_calls(&self) -> u32 {
        self.max_polymorphic_calls
    }

    /// Whether `opcache.jit_debug` asks for any line at all, which is
    /// when the start's lines are written (`jit-design.md` §4).
    pub(crate) fn debugs(&self) -> bool {
        self.debug.0 != 0
    }
}

/// `at` as a `TRACE` line ends with it: `Class::function() file:line`,
/// `$main` for a file's own code.
fn write_place(sink: &mut dyn Write, at: Place<'_>) -> io::Result<()> {
    if let Some(scope) = at.scope {
        sink.write_all(scope)?;
        sink.write_all(b"::")?;
    }
    sink.write_all(at.function.unwrap_or(b"$main"))?;
    sink.write_all(b"() ")?;
    sink.write_all(at.file)?;
    writeln!(sink, ":{}", at.line)
}

/// Where a root starts, as its `TRACE start` line names it: the class,
/// the function (`$main` for a file's code), the file and the line.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Place<'a> {
    pub(crate) scope: Option<&'a [u8]>,
    pub(crate) function: Option<&'a [u8]>,
    pub(crate) file: &'a [u8],
    pub(crate) line: u32,
}

/// One event of a trace's life that php's `jit_debug` reports, in the
/// order `zend_jit_trace_hot_root` reports them.
#[derive(Debug, Clone, Copy)]
pub(crate) enum TraceEvent<'a> {
    Start {
        trace: TraceId,
        start: Start,
        at: Place<'a>,
    },
    Stop {
        trace: TraceId,
        stop: Stop,
    },
    Compiled {
        trace: TraceId,
    },
    Abort {
        trace: TraceId,
        stop: Stop,
    },
    Blacklisted {
        trace: TraceId,
    },
    /// An exit of `trace` that grows no side trace from now on, php's
    /// `zend_jit_blacklist_trace_exit` line.
    ExitBlacklisted {
        trace: TraceId,
        exit: ExitId,
    },
    /// A trace left through `exit` for the interpreter, at the place it
    /// landed: php's `zend_jit_trace_exit` line.
    Exit {
        trace: TraceId,
        exit: ExitId,
        at: Place<'a>,
    },
    /// The blank line that ends a root's lines.
    Done,
}

/// A word's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum State {
    Cold,
    Recording,
    Compiled(TraceId),
    Blacklisted,
}

/// How a claim ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// The trace was published: the word enters it from now on.
    Published(TraceId),
    /// One more failed attempt; the word is cold again, or blacklisted
    /// once its attempts reach the `Policy`'s count.
    Abort(Stop),
    /// Blacklisted now, whatever the count: php's stops past
    /// `COMPILER_ERROR`, which no attempt recovers.
    Blacklist(Stop),
    /// Blacklisted now, with no attempt and no stop: a loop php counts
    /// no heat of (`record::counts`), whose header's handler php never
    /// swaps, so the word is visited for nothing from now on.
    Uncounted,
    /// Cold again with no attempt counted: the recording became the trace
    /// of another word, a loop php unrolled and rooted the trace at
    /// (`zend_jit_trace_subtrace`), and a root's heat starts again as
    /// php's `zend_jit_trace_hot_root` starts a root's count again, where
    /// an exit's firing is not counted ([`Spent::At`]).
    Released,
}

/// When a claimed word's failed attempts blacklist it, as php counts
/// them for a root and for an exit, which it counts apart.
///
/// A loop whose recording sees a throw aborts at each turn and is
/// blacklisted at its sixteenth abort:
///
/// ```php
/// function g($i) {
///     try { throw new Exception("x"); } catch (Exception $e) { return $i; }
/// }
/// $s = 0;
/// for ($i = 0; $i < 16; $i++) {
///     $s += g($i);
/// }
/// echo $s, "\n";
/// ```
/// ```expect regime=jit-inline process
/// 120
/// ```
/// ```stderr
/// ---- TRACE 1 start (enter) $main() %s:5
/// ---- TRACE 1 abort (inner loop)
/// ---- TRACE 1 start (loop) $main() %s:6
/// ---- TRACE 1 abort (exception)
///
/// ---- TRACE 1 start (loop) $main() %s:6
/// ---- TRACE 1 abort (exception)
///
/// ---- TRACE 1 start (loop) $main() %s:6
/// ---- TRACE 1 abort (exception)
///
/// ---- TRACE 1 start (loop) $main() %s:6
/// ---- TRACE 1 abort (exception)
///
/// ---- TRACE 1 start (loop) $main() %s:6
/// ---- TRACE 1 abort (exception)
///
/// ---- TRACE 1 start (loop) $main() %s:6
/// ---- TRACE 1 abort (exception)
///
/// ---- TRACE 1 start (loop) $main() %s:6
/// ---- TRACE 1 abort (exception)
///
/// ---- TRACE 1 start (loop) $main() %s:6
/// ---- TRACE 1 abort (exception)
///
/// ---- TRACE 1 start (loop) $main() %s:6
/// ---- TRACE 1 abort (exception)
///
/// ---- TRACE 1 start (loop) $main() %s:6
/// ---- TRACE 1 abort (exception)
///
/// ---- TRACE 1 start (loop) $main() %s:6
/// ---- TRACE 1 abort (exception)
///
/// ---- TRACE 1 start (loop) $main() %s:6
/// ---- TRACE 1 abort (exception)
///
/// ---- TRACE 1 start (loop) $main() %s:6
/// ---- TRACE 1 abort (exception)
///
/// ---- TRACE 1 start (loop) $main() %s:6
/// ---- TRACE 1 abort (exception)
///
/// ---- TRACE 1 start (loop) $main() %s:6
/// ---- TRACE 1 abort (exception)
///
/// ---- TRACE 1 start (loop) $main() %s:6
/// ---- TRACE 1 abort (exception)
/// ---- TRACE 1 blacklisted
///
/// ```
///
/// A side that aborts on every attempt records again at its exit's next
/// firing and blacklists the exit at its ninth abort, the exit's count
/// having reached `opcache.jit_hot_side_exit` plus
/// `opcache.jit_blacklist_side_trace`, one past the side's eight; the
/// rest of the throws leave through the exit with no line:
///
/// ```php
/// function f($i) { if ($i % 2) throw new Exception("x"); return $i; }
/// $s = 0;
/// for ($i = 0; $i < 41; $i++) {
///     try { $s += f($i); } catch (Exception $e) { $s++; }
/// }
/// echo $s, "\n";
/// ```
/// ```expect regime=jit-inline process
/// 440
/// ```
/// ```stderr
/// ---- TRACE 1 start (enter) $main() %s:3
/// ---- TRACE 1 abort (inner loop)
/// ---- TRACE 1 start (loop) $main() %s:4
/// ---- TRACE 1 stop (loop)
/// ---- TRACE 1 compiled
///
///      TRACE 1 exit %d f() %s:2
/// ---- TRACE 2 start (side trace 1/%d) f() %s:2
/// ---- TRACE 2 abort (exception)
///
///      TRACE 1 exit %d f() %s:2
/// ---- TRACE 2 start (side trace 1/%d) f() %s:2
/// ---- TRACE 2 abort (exception)
///
///      TRACE 1 exit %d f() %s:2
/// ---- TRACE 2 start (side trace 1/%d) f() %s:2
/// ---- TRACE 2 abort (exception)
///
///      TRACE 1 exit %d f() %s:2
/// ---- TRACE 2 start (side trace 1/%d) f() %s:2
/// ---- TRACE 2 abort (exception)
///
///      TRACE 1 exit %d f() %s:2
/// ---- TRACE 2 start (side trace 1/%d) f() %s:2
/// ---- TRACE 2 abort (exception)
///
///      TRACE 1 exit %d f() %s:2
/// ---- TRACE 2 start (side trace 1/%d) f() %s:2
/// ---- TRACE 2 abort (exception)
///
///      TRACE 1 exit %d f() %s:2
/// ---- TRACE 2 start (side trace 1/%d) f() %s:2
/// ---- TRACE 2 abort (exception)
///
///      TRACE 1 exit %d f() %s:2
/// ---- TRACE 2 start (side trace 1/%d) f() %s:2
/// ---- TRACE 2 abort (exception)
///
///      TRACE 1 exit %d f() %s:2
/// ---- TRACE 2 start (side trace 1/%d) f() %s:2
/// ---- TRACE 2 abort (exception)
/// ---- EXIT 1/%d blacklisted
///
///      TRACE 1 exit %d $main() %s:7
/// ---- TRACE 2 start (side trace 1/%d) $main() %s:7
/// ---- TRACE 2 stop (return)
/// ---- TRACE 2 compiled
///
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Spent {
    /// A root's word, at this many failed attempts, which its state
    /// keeps, php's bad root cache (`zend_jit_trace_is_bad_root`,
    /// `Z/ext/opcache/jit/zend_jit_trace.c:7765-7798`); its heat starts
    /// again at each, as php starts a root's count again when it fires.
    After(u8),
    /// An exit's word, once its heat reaches this, the one count php
    /// keeps for an exit, held to `opcache.jit_hot_side_exit` plus
    /// `opcache.jit_blacklist_side_trace` (`zend_jit_trace_exit_is_bad`,
    /// `:8299-8309`): its heat runs on through its attempts, so a side
    /// that aborted records again at the exit's next firing, and the
    /// exit is blacklisted at the attempt past the side's count, or, for
    /// an exit that grows none, at the firing that reaches it
    /// ([`spend`]).
    At(u32),
}

/// The word's state, by one relaxed load: all ones reads blacklisted.
#[inline(always)] // jit-design.md §4.9: the load at every visit of a word
#[allow(clippy::inline_always)]
pub(crate) fn read(word: &Word) -> State {
    decoded(word.halves()[1].load(Ordering::Relaxed))
}

/// The state a word's second half says.
#[inline(always)] // one mask and a compare, inside `read`
#[allow(clippy::inline_always)]
fn decoded(held: u32) -> State {
    match held & STATE {
        COLD => State::Cold,
        RECORDING => State::Recording,
        COMPILED => TraceId::new(held >> ID_SHIFT).map_or(State::Blacklisted, State::Compiled),
        _ => State::Blacklisted,
    }
}

/// One more visit of a cold word: a racy load, saturating add and store
/// on the heat half, a lost update accepted as php's counters accept it.
/// The new heat.
#[inline(always)] // jit-design.md §4.9: the bump at every cold visit
#[allow(clippy::inline_always)]
pub(crate) fn bump(word: &Word) -> u32 {
    let heat = &word.halves()[0];
    let hotter = heat.load(Ordering::Relaxed).saturating_add(1);
    heat.store(hotter, Ordering::Relaxed);
    hotter
}

/// Where a claimed word lives (`jit-design.md` §4.7), owned, so a claim
/// can leave the thread that made it: a root's word by its op array and
/// safepoint, a return root's by its op array and the landing opline
/// past the call (§2.2, #3036), an exit's by its trace and exit, each
/// under the epoch it holds, which keeps the word, the op array's row or
/// the trace table's.
#[derive(Debug, Clone)]
pub(crate) enum Holder {
    Root {
        epoch: Epoch,
        func: FuncRef,
        at: SafepointIdx,
    },
    Landing {
        epoch: Epoch,
        func: FuncRef,
        at: OplineIdx,
    },
    Exit {
        epoch: Epoch,
        trace: TraceId,
        exit: ExitId,
    },
}

impl Holder {
    /// The word it holds: the sealed one for an exit the epoch's table
    /// holds no trace for, and for a landing no trace armed, which
    /// nothing claims.
    pub(crate) fn word(&self) -> &Word {
        match self {
            Holder::Root { epoch, func, at } => epoch.word(*func, *at),
            Holder::Landing { epoch, func, at } => epoch.landing(*func, *at),
            Holder::Exit { epoch, trace, exit } => crate::table::exit_word(epoch, *trace, *exit),
        }
    }

    /// The epoch it holds.
    pub(crate) fn epoch(&self) -> &Epoch {
        match self {
            Holder::Root { epoch, .. }
            | Holder::Landing { epoch, .. }
            | Holder::Exit { epoch, .. } => epoch,
        }
    }
}

/// A landing word's state half as a return read it (`jit-design.md`
/// §2.2, #4105): compared with the sealed word's in the `RETURN*` arms,
/// and read for what it holds in the visit it asks, so a word is loaded
/// once a return and decoded only where it asks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Landing(u32);

impl Landing {
    /// The word a `RETURN*` handler's test reads.
    #[inline(always)] // jit-design.md §2.2: one load in the `RETURN*` arms
    #[allow(clippy::inline_always)]
    pub(crate) fn of(word: &Word) -> Landing {
        Landing(word.halves()[1].load(Ordering::Relaxed))
    }

    /// Whether it is sealed, as its op array's landings are laid down, and
    /// neither armed nor [`head`]ed: it asks nothing of a return.
    #[inline(always)] // jit-design.md §2.2: one compare in the `RETURN*` arms
    #[allow(clippy::inline_always)]
    pub(crate) fn sealed(self) -> bool {
        self.0 == u32::MAX
    }

    /// Whether a loop's trace [`head`]ed it, sealed but for that trace.
    pub(crate) fn headed(self) -> bool {
        self.0 == HEADED
    }

    /// What an armed word holds; never read of a word that is [`sealed`]
    /// or [`headed`], which reads blacklisted.
    ///
    /// [`sealed`]: Landing::sealed
    /// [`headed`]: Landing::headed
    pub(crate) fn state(self) -> State {
        decoded(self.0)
    }
}

/// Whether a landing word was armed (`jit-design.md` §2.2, #3036): any
/// word whose state half is neither the sealed word's all ones, which its
/// op array's landings are laid down as, nor [`HEADED`], php's
/// `trace_flags` of an opline `zend_jit_trace_setup_ret_counter` set. An
/// armed landing that is blacklisted is armed: its state half keeps its
/// attempts and the stop below the trace number's bits, so it is never
/// all ones and never [`HEADED`].
#[inline(always)] // jit-design.md §4.9: one load where a return lands
#[allow(clippy::inline_always)]
pub(crate) fn armed(word: &Word) -> bool {
    !matches!(word.halves()[1].load(Ordering::Relaxed), u32::MAX | HEADED)
}

/// Lay a landing word [`HEADED`], a loop's trace published where a return
/// lands (`jit-design.md` §2.2, #4105): sealed made headed, so a return
/// that lands there visits and enters the loop's trace, and an armed word,
/// which a return visits already, left. Never armed: php makes no return
/// root of a loop's header, whose `trace_flags` its arming finds set
/// (`zend_jit_trace_setup_ret_counter`,
/// `Z/ext/opcache/jit/zend_jit_trace.c:3417-3430`), so a headed word
/// counts nothing and is read for the loop's word beside it.
pub(crate) fn head(word: &Word) {
    let state = &word.halves()[1];
    state
        .compare_exchange(u32::MAX, HEADED, Ordering::Release, Ordering::Relaxed)
        .ok();
}

/// Arm a landing word, php's `zend_jit_trace_setup_ret_counter`
/// (`Z/ext/opcache/jit/zend_jit_trace.c:3417-3430`): a sealed word made
/// cold, so the returns that land on it count toward
/// `opcache.jit_hot_return`; one armed already, whatever its state, is
/// left, as php arms an opline whose `trace_flags` are clear. php hands
/// the landing a slot of the same pool of counters as a unit's entries
/// and loops ([`Policy::laid`]), so where the tier is set up and off and
/// nothing fills that pool, the slot fires at the first return, and the
/// landing is armed primed as such a unit's words are laid (#3363);
/// otherwise its heat starts at zero.
///
/// Under `opcache.jit=off` with a buffer and php's thresholds, a file
/// included after `ini_set('opcache.jit', 'tracing')` roots its entry,
/// whose trace stops at the `eval` and arms the landing past it, and the
/// `eval`'d code's first return roots php's return trace there:
///
/// ```php
/// $dir = realpath(sys_get_temp_dir()) . '/jit-return-' . getmypid(); mkdir($dir);
/// file_put_contents("$dir/main.php", '<?php ini_set("opcache.jit", "tracing"); include __DIR__ . "/inc.php";');
/// file_put_contents("$dir/inc.php", '<?php eval("\$x = 1;"); $s = 0; for ($i = 0; $i < 3; $i++) { $s += $i; } echo $s, "\n";');
/// touch("$dir/main.php", time() - 60); touch("$dir/inc.php", time() - 60);
/// $cmd = [PHP_BINARY, '-n', '-d', 'opcache.enable_cli=1', '-d', 'opcache.jit=off', '-d', 'opcache.jit_debug=28672', "$dir/main.php"];
/// $p = proc_open($cmd, [1 => ['pipe', 'w'], 2 => ['pipe', 'w']], $pipes);
/// echo stream_get_contents($pipes[1]), str_replace($dir, 'DIR', stream_get_contents($pipes[2]));
/// proc_close($p); unlink("$dir/main.php"); unlink("$dir/inc.php"); rmdir($dir);
/// ```
/// ```expect process
/// 3
/// ---- TRACE 1 start (enter) $main() DIR/inc.php:1
/// ---- TRACE 1 stop (exit to VM interpreter)
/// ---- TRACE 1 compiled
///
/// ---- TRACE 2 start (return) $main() DIR/inc.php:1
/// ---- TRACE 2 start (loop) $main() DIR/inc.php:1
/// ---- TRACE 2 stop (loop)
/// ---- TRACE 2 compiled
/// ```
pub(crate) fn arm(word: &Word, policy: &Policy) {
    if armed(word) {
        return;
    }
    // The heat first: a sealed word is never bumped, and a visit that
    // finds the state cold finds no heat left over from the seal.
    let [heat, state] = word.halves();
    let armed_heat = match policy.runs {
        Runs::Switched => PRIMED_HEAT,
        Runs::FromStart | Runs::Lane => 0,
    };
    heat.store(armed_heat, Ordering::Relaxed);
    // Another arming of the same landing may win; either leaves it cold.
    // A headed word is sealed but for its loop's trace, and an arming
    // keeps its landing armed over it.
    if state
        .compare_exchange(u32::MAX, COLD, Ordering::Release, Ordering::Relaxed)
        .is_err()
    {
        state
            .compare_exchange(HEADED, COLD, Ordering::Release, Ordering::Relaxed)
            .ok();
    }
}

/// A word in the recording state, owned until it is settled once
/// (`jit-design.md` §4.7; the glossary's *Claim*). Dropped without
/// [`settle`], by a panic under `step` or a fatal out of a recording, it
/// settles itself as `Abort(Stop::CompilerError)`, php's
/// `COMPILER_ERROR`, so no word stays recording. `Send`: it holds its
/// word's holder, not a borrow of the word.
#[must_use]
#[derive(Debug)]
pub(crate) struct Claim {
    holder: Holder,
    start: Start,
    spent: Spent,
    folds: record::Folds,
    settled: bool,
}

impl Claim {
    /// What the recording starts as.
    pub(crate) fn start(&self) -> Start {
        self.start
    }

    /// The epoch its word stands in, whose trace table the trace is
    /// published into.
    pub(crate) fn epoch(&self) -> &Epoch {
        self.holder.epoch()
    }

    /// What holds its word.
    pub(crate) fn holder(&self) -> &Holder {
        &self.holder
    }

    /// A loop's trace published: the landing word of its root [`head`]ed
    /// where a return lands there ([`record::root_landing`]), so a return
    /// that lands on the compiled loop visits, and no other claim's.
    fn head_root(&self) {
        if let (Start::Loop, Holder::Root { epoch, func, at }) = (self.start, &self.holder)
            && let Some(root) = (epoch.held(*func))
                .and_then(|held| record::root_landing(held.code(), *at, self.folds))
            && let Some(word) = epoch.lay_landing(*func, root)
        {
            head(word);
        }
    }

    /// The one release store that ends the claim; the state it left.
    fn store(&self, outcome: &Outcome) -> State {
        let [heat, state] = self.holder.word().halves();
        let held = state.load(Ordering::Relaxed);
        let mut attempts = u8::try_from((held >> ATTEMPTS_SHIFT) & u32::from(ATTEMPTS_MAX))
            .unwrap_or(ATTEMPTS_MAX);
        let next = match outcome {
            Outcome::Published(trace) => {
                self.head_root();
                (trace.get() << ID_SHIFT) | COMPILED
            }
            Outcome::Abort(stop) => {
                attempts = attempts.saturating_add(1).min(ATTEMPTS_MAX);
                let spent = match self.spent {
                    Spent::After(limit) => {
                        // A root's count starts again when it fires.
                        heat.store(0, Ordering::Relaxed);
                        attempts >= limit
                    }
                    Spent::At(bound) => heat.load(Ordering::Relaxed) >= bound,
                };
                if spent {
                    BLACKLISTED
                } else {
                    // The stop kept where the trace number would stand,
                    // php's bad root cache entry for the word
                    // (`zend_jit_trace_is_bad_root`), read by [`bad_stop`].
                    (u32::from(stop.code()) << ID_SHIFT) | COLD
                }
            }
            Outcome::Blacklist(_) | Outcome::Uncounted => BLACKLISTED,
            Outcome::Released => {
                // A root's count starts again; an exit's firing counts
                // nothing, as php's exit count does not move for it.
                let left = match self.spent {
                    Spent::After(_) => 0,
                    Spent::At(_) => heat.load(Ordering::Relaxed).saturating_sub(1),
                };
                heat.store(left, Ordering::Relaxed);
                held & !STATE & !(u32::from(ATTEMPTS_MAX) << ATTEMPTS_SHIFT)
            }
        };
        let settled = next | (u32::from(attempts) << ATTEMPTS_SHIFT);
        state.store(settled, Ordering::Release);
        decoded(settled)
    }
}

impl Drop for Claim {
    fn drop(&mut self) {
        if !self.settled {
            self.store(&Outcome::Abort(Stop::CompilerError));
        }
    }
}

/// Claim `holder`'s word for a recording of `start`: one compare-and-swap
/// from cold to recording, its attempts kept. `None` where the word is
/// not cold, another thread having won it or it being compiled,
/// blacklisted or sealed.
pub(crate) fn claim(holder: Holder, policy: &Policy, start: Start) -> Option<Claim> {
    let state = &holder.word().halves()[1];
    let held = state.load(Ordering::Relaxed);
    if held & STATE != COLD {
        return None;
    }
    state
        .compare_exchange(held, held | RECORDING, Ordering::Acquire, Ordering::Relaxed)
        .ok()?;
    Some(Claim {
        holder,
        start,
        spent: policy.spent(start),
        folds: policy.folds(),
        settled: false,
    })
}

/// One firing of an exit that grows no side, php's
/// `ZEND_JIT_EXIT_TO_VM`, counted as php's `zend_jit_trace_exit` counts
/// it: its word's heat bumped, and the word blacklisted at the firing
/// whose heat reaches its bound ([`Spent::At`]), after which it is left
/// with no line. Whether this firing blacklisted it, which writes php's
/// `---- EXIT m/k blacklisted`; a word not cold is neither bumped nor
/// stored.
///
/// An element a fetch does not find leaves its loop's trace through
/// such an exit, for the interpreter to warn of, and the ninth firing
/// blacklists it, `opcache.jit_hot_side_exit` plus
/// `opcache.jit_blacklist_side_trace` under the recipe's thresholds; the
/// thirteen after it write nothing (the exit numbers are #3176's: php's
/// inference types `$i` and holds no guard on it at entry):
///
/// ```php
/// function h($a) {
///     $s = 0;
///     for ($i = 0; $i < 58; $i++) {
///         $s += $a[$i % 5];
///     }
///     return $s;
/// }
/// error_reporting(E_ALL & ~E_WARNING);
/// echo h([1, 2, 3]), "\n";
/// ```
/// ```expect regime=jit-inline process
/// 72
/// ```
/// ```stderr
/// ---- TRACE 1 start (enter) $main() %s:9
/// ---- TRACE 1 abort (inner loop)
/// ---- TRACE 1 start (loop) h() %s:4
/// ---- TRACE 1 stop (loop)
/// ---- TRACE 1 compiled
///
///      TRACE 1 exit %d h() %s:5
///      TRACE 1 exit %d h() %s:5
///      TRACE 1 exit %d h() %s:5
///      TRACE 1 exit %d h() %s:5
///      TRACE 1 exit %d h() %s:5
///      TRACE 1 exit %d h() %s:5
///      TRACE 1 exit %d h() %s:5
///      TRACE 1 exit %d h() %s:5
///      TRACE 1 exit %d h() %s:5
/// ---- EXIT 1/%d blacklisted
///      TRACE 1 exit %d h() %s:7
/// ---- TRACE 2 start (side trace 1/%d) h() %s:7
/// ---- TRACE 2 stop (return)
/// ---- TRACE 2 compiled
///
/// ```
pub(crate) fn spend(word: &Word, policy: &Policy, start: Start) -> bool {
    let Spent::At(bound) = policy.spent(start) else {
        return false;
    };
    let state = &word.halves()[1];
    let held = state.load(Ordering::Relaxed);
    if held & STATE != COLD || bump(word) < bound {
        return false;
    }
    state
        .compare_exchange(
            held,
            (held & !STATE) | BLACKLISTED,
            Ordering::Release,
            Ordering::Relaxed,
        )
        .is_ok()
}

/// End `claim` with `outcome`, one release store; a root's abort also
/// stores zero to the heat, as php's hot root starts its count again,
/// where an exit's keeps it ([`Spent`]). The
/// state the word was left in, which says whether an abort blacklisted
/// it.
pub(crate) fn settle(mut claim: Claim, outcome: &Outcome) -> State {
    claim.settled = true;
    claim.store(outcome)
}

/// The stop the word's last failed attempt ended on, where it has failed
/// at least `times` times and stands cold or recording: php's
/// `zend_jit_trace_bad_stop_event` over the bad root cache, whose entry
/// for an opline is the word's own attempts and stop here.
pub(crate) fn bad_stop(word: &Word, times: u8) -> Option<Stop> {
    let held = word.halves()[1].load(Ordering::Relaxed);
    let attempts = (held >> ATTEMPTS_SHIFT) & u32::from(ATTEMPTS_MAX);
    if held & STATE > RECORDING || attempts < u32::from(times) {
        return None;
    }
    u8::try_from(held >> ID_SHIFT).ok().and_then(Stop::of_code)
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "a test starts its own thread (seams.toml rule fork.threads)"
)]
mod tests;
