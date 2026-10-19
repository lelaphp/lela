//! Thresholds and the word's state machine, pure (`jit-design.md` §4.7):
//! [`Policy`], php's `opcache.jit*` directives with the tier's two
//! engine-only ones read once at engine start; [`read`], [`bump`],
//! [`claim`] and [`settle`], the one state machine over a [`Word`]
//! whichever home holds it; and [`Policy::dump`], php's `TRACE` lines.
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
//! the engine knows, and a layout invariant held by a test below.
#![cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "dispatch (#2952) is the first caller of the state machine and the recorder (#2951) of the dump"
    )
)]

use std::io::{self, Write};
use std::sync::atomic::Ordering;

use php_engine::tier::{Epoch, FuncRef, Request, SafepointIdx, StartupIni, Word};
use php_unit::decl::{IniDecl, Name};

use crate::record::{Start, Stop};
use crate::table::TraceId;

/// The state bits of a word's second half.
const STATE: u32 = 0b11;
const COLD: u32 = 0;
const RECORDING: u32 = 1;
const COMPILED: u32 = 2;
const BLACKLISTED: u32 = 3;
/// Where the failed attempts stand, and how many the six bits hold.
const ATTEMPTS_SHIFT: u32 = 2;
const ATTEMPTS_MAX: u8 = 0x3F;
/// Where the trace number stands.
const ID_SHIFT: u32 = 8;

/// php's `ZEND_JIT_ON_HOT_TRACE`, the one trigger this tier runs.
const HOT_TRACE: u32 = 5;

/// How the tier runs, decided once at engine start.
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
/// `Inline`.
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
    pub(crate) const TRACE_START: u32 = 1 << 12;
    pub(crate) const TRACE_STOP: u32 = 1 << 13;
    pub(crate) const TRACE_COMPILED: u32 = 1 << 14;
    pub(crate) const TRACE_ABORT: u32 = 1 << 16;
    pub(crate) const TRACE_BLACKLIST: u32 = 1 << 17;

    /// Whether any of `bits` is set.
    pub(crate) fn any(self, bits: u32) -> bool {
        self.0 & bits != 0
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
    /// What php refuses with its warning, and runs with the tier off.
    Invalid,
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
                .map(|text| text.trim_start_matches(|c: char| c.is_ascii_whitespace()))
                .and_then(|text| text.parse::<i64>().ok())
                .map_or(Setting::Invalid, Setting::of_number),
        }
    }

    /// `zend_jit_parse_config_num`: zero is off; otherwise each digit in
    /// its range, `O` 1 to 5, `T` 0 to 5 but 4, `R` 0 to 2, `C` 0 or 1.
    fn of_number(jit: i64) -> Setting {
        if jit == 0 {
            return Setting::Off;
        }
        let digit = |place: u32| (jit / 10_i64.pow(place)) % 10;
        let valid = jit > 0
            && (1..=5).contains(&digit(0))
            && (0..=5).contains(&digit(1))
            && digit(1) != 4
            && (0..=2).contains(&digit(2))
            && (0..=1).contains(&digit(3))
            && jit / 10_000 == 0;
        match u32::try_from(jit) {
            Ok(crto) if valid => Setting::On { crto },
            _ => Setting::Invalid,
        }
    }
}

/// Why the tier runs with [`Mode::Off`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OffBecause {
    /// `opcache.jit=disable`, php's default.
    Disabled,
    /// `opcache.jit` set up and not on.
    NotOn,
    /// An `opcache.jit` php refuses.
    Invalid,
    /// `opcache.jit_buffer_size` is zero.
    NoBuffer,
}

/// A line the tier's start logs (`jit-design.md` §4, §4.7).
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
    /// The tier runs off.
    Off(OffBecause),
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
            Note::Off(why) => f.write_str(match why {
                OffBecause::Disabled => "JIT: off: opcache.jit=disable",
                OffBecause::NotOn => "JIT: off: opcache.jit is not on",
                OffBecause::Invalid => "JIT: off: opcache.jit is invalid",
                OffBecause::NoBuffer => "JIT: off: opcache.jit_buffer_size is 0",
            }),
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
            name: Name::from(*name),
            default: Some(default.as_bytes().into()),
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
    debug: JitDebug,
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
    value(ini, name).map_or(default, |text| Request::ini_parse_quantity(text).0)
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
    /// `jit.mode` and `jit.compile`, with what the start logs pushed to
    /// `notes`. A value outside php's bounds reads as the default, as
    /// php's handlers keep it; `jit.mode`, where set, names the `Mode`
    /// whatever `opcache.jit` says, which is how the tier's lanes serve a
    /// corpus.
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
    pub(crate) fn from_ini(ini: &StartupIni, notes: &mut Vec<Note>) -> Policy {
        let setting = value(ini, "opcache.jit").map_or(Setting::Disable, Setting::of);
        let buffer_size = long(ini, "opcache.jit_buffer_size", 64 << 20);
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
        let asked = match (lane, setting) {
            (Some(mode), _) => Ok(mode),
            (None, Setting::On { crto }) => {
                if (crto / 10) % 10 != HOT_TRACE {
                    notes.push(Note::AsTracing { crto });
                }
                Ok(Mode::Tracing)
            }
            (None, Setting::Disable) => Err(OffBecause::Disabled),
            (None, Setting::Off) => Err(OffBecause::NotOn),
            (None, Setting::Invalid) => Err(OffBecause::Invalid),
        };
        let mode = match asked.and_then(|mode| {
            if buffer_size > 0 {
                Ok(mode)
            } else {
                Err(OffBecause::NoBuffer)
            }
        }) {
            Ok(mode) => {
                if matches!(mode, Mode::Force | Mode::ExitFuzz) {
                    notes.push(Note::Lane(mode));
                }
                mode
            }
            Err(why) => {
                notes.push(Note::Off(why));
                Mode::Off
            }
        };
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
        Policy {
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
            debug: JitDebug(u32::try_from(long(ini, "opcache.jit_debug", 0)).unwrap_or(0)),
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

    /// How many failed attempts blacklist a word of `start`'s kind:
    /// `opcache.jit_blacklist_side_trace` for a side, the root's
    /// otherwise, at most the 63 the word keeps.
    pub(crate) fn attempts(&self, start: Start) -> u8 {
        let limit = match start {
            Start::Side { .. } => self.blacklist_side,
            Start::Loop | Start::Enter | Start::Return => self.blacklist_root,
        };
        limit.min(ATTEMPTS_MAX)
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

    /// `opcache.jit_max_loop_unrolls`.
    pub(crate) fn max_loop_unrolls(&self) -> u8 {
        self.max_loop_unrolls
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

    /// One of php's `TRACE` lines for `event`, where `opcache.jit_debug`
    /// asks for it (`Z/ext/opcache/jit/zend_jit_trace.c:8152-8250`).
    pub(crate) fn dump(&self, sink: &mut dyn Write, event: &TraceEvent<'_>) -> io::Result<()> {
        let debug = self.debug;
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
                if let Some(scope) = at.scope {
                    sink.write_all(scope)?;
                    sink.write_all(b"::")?;
                }
                sink.write_all(at.function.unwrap_or(b"$main"))?;
                sink.write_all(b"() ")?;
                sink.write_all(at.file)?;
                writeln!(sink, ":{}", at.line)
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
            TraceEvent::Done
                if debug.any(
                    JitDebug::TRACE_STOP
                        | JitDebug::TRACE_ABORT
                        | JitDebug::TRACE_COMPILED
                        | JitDebug::TRACE_BLACKLIST,
                ) =>
            {
                writeln!(sink)
            }
            _ => Ok(()),
        }
    }
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
    /// Cold again with no attempt counted: the recording became the trace
    /// of another word, a loop php unrolled and rooted the trace at
    /// (`zend_jit_trace_subtrace`), and this word's heat starts again as
    /// php's `zend_jit_trace_hot_root` starts a root's count again.
    Released,
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
/// safepoint under the epoch it holds, which keeps the word. An exit's
/// word joins as `Exit { epoch, trace, exit }` with the trace table it
/// stands in.
#[derive(Debug, Clone)]
pub(crate) enum Holder {
    Root {
        epoch: Epoch,
        func: FuncRef,
        at: SafepointIdx,
    },
}

impl Holder {
    /// The word it holds.
    pub(crate) fn word(&self) -> &Word {
        match self {
            Holder::Root { epoch, func, at } => epoch.word(*func, *at),
        }
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
    attempts: u8,
    settled: bool,
}

impl Claim {
    /// What the recording starts as.
    pub(crate) fn start(&self) -> Start {
        self.start
    }

    /// The one release store that ends the claim; the state it left.
    fn store(&self, outcome: &Outcome) -> State {
        let [heat, state] = self.holder.word().halves();
        let held = state.load(Ordering::Relaxed);
        let mut attempts = u8::try_from((held >> ATTEMPTS_SHIFT) & u32::from(ATTEMPTS_MAX))
            .unwrap_or(ATTEMPTS_MAX);
        let next = match outcome {
            Outcome::Published(trace) => (trace.get() << ID_SHIFT) | COMPILED,
            Outcome::Abort(stop) => {
                attempts = attempts.saturating_add(1).min(ATTEMPTS_MAX);
                // php's counter starts again when it fires (`zend_jit_trace_hot_root`).
                heat.store(0, Ordering::Relaxed);
                if attempts >= self.attempts {
                    BLACKLISTED
                } else {
                    // The stop kept where the trace number would stand,
                    // php's bad root cache entry for the word
                    // (`zend_jit_trace_is_bad_root`), read by [`bad_stop`].
                    (u32::from(stop.code()) << ID_SHIFT) | COLD
                }
            }
            Outcome::Blacklist(_) => BLACKLISTED,
            Outcome::Released => {
                heat.store(0, Ordering::Relaxed);
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
        attempts: policy.attempts(start),
        settled: false,
    })
}

/// End `claim` with `outcome`, one release store; an abort also stores
/// zero to the heat, as php's hot root starts its count again. The
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
