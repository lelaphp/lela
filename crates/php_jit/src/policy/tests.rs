//! `policy` at its seam with no engine (`jit-design.md` §4.7, §7): php's
//! `zend_jit_parse_config_num` cases, the state machine as a table over a fixture epoch's
//! word through a `Holder::Root`, the sealed word, a claim settled on
//! another thread, and the dump's lines.

use std::sync::atomic::Ordering;

use php_engine::api::request::TierStatus;
use php_engine::tier::{Epoch, FuncRef, Laid, SafepointIdx, StartupIni, Word};
use php_unit::{OpArray, Opcode, OplineIdx, Unit};

use super::{
    Bound, Claim, Compile, Holder, JitDebug, Mode, Note, Outcome, Place, Policy, Refused, Setting,
    Spent, State, TraceEvent, bad_stop, bump, claim, read, settle, spend,
};
use crate::record::{Start, Stop};
use crate::table::TraceId;
use crate::types::ExitId;

fn ini(entries: &[(&str, &str)]) -> StartupIni {
    StartupIni {
        entries: entries
            .iter()
            .map(|(name, value)| (name.as_bytes().to_vec(), Some(value.as_bytes().to_vec())))
            .collect(),
    }
}

/// The `Policy` `entries` read to, with the lines it logged.
fn read_ini(entries: &[(&str, &str)]) -> (Policy, Vec<Note>) {
    read_on("", entries)
}

/// [`read_ini`] for the host `host` names.
fn read_on(host: &str, entries: &[(&str, &str)]) -> (Policy, Vec<Note>) {
    let mut notes = Vec::new();
    let policy = Policy::from_ini(&ini(entries), host, &mut notes);
    (policy, notes)
}

/// An epoch holding one unit whose main op array has two safepoints,
/// and the op array's name: its words are zero, `Word::default()`'s.
fn fixture() -> (Epoch, FuncRef) {
    Epoch::fixture(Unit {
        main: OpArray {
            safepoints: vec![OplineIdx(0), OplineIdx(1)].into(),
            ..OpArray::default()
        },
        ..Unit::default()
    })
}

fn root(epoch: &Epoch, func: FuncRef, at: u32) -> Holder {
    Holder::Root {
        epoch: epoch.clone(),
        func,
        at: SafepointIdx(at),
    }
}

fn trace(n: u32) -> TraceId {
    TraceId::new(n).unwrap_or_else(|| unreachable_trace(n))
}

#[track_caller]
fn unreachable_trace(n: u32) -> TraceId {
    assert!(
        n != 0 && n < TraceId::LIMIT,
        "trace {n} is a number a word holds"
    );
    TraceId::new(1).unwrap_or_else(|| unreachable_trace(1))
}

fn side() -> Start {
    Start::Side {
        parent: trace(1),
        exit: ExitId(3),
    }
}

fn attempts(word: &Word) -> u32 {
    (word.halves()[1].load(Ordering::Relaxed) >> 2) & 0x3F
}

fn heat(word: &Word) -> u32 {
    word.halves()[0].load(Ordering::Relaxed)
}

/// `zend_jit_config` and `zend_jit_parse_config_num`: `on` and
/// `tracing` are 1254, `function` 1205, `disable` and `off` are off, a
/// number is read digit by digit and one out of range is php's refusal.
#[test]
fn opcache_jit_is_read_as_php_reads_it() {
    assert_eq!(Setting::of(b"on"), Setting::On { crto: 1254 });
    assert_eq!(Setting::of(b"TRACING"), Setting::On { crto: 1254 });
    assert_eq!(Setting::of(b"function"), Setting::On { crto: 1205 });
    assert_eq!(Setting::of(b"disable"), Setting::Disable);
    assert_eq!(Setting::of(b"off"), Setting::Off);
    assert_eq!(Setting::of(b"0"), Setting::Off);
    assert_eq!(Setting::of(b"1235"), Setting::On { crto: 1235 });
    assert_eq!(
        Setting::of(b"1245"),
        Setting::Invalid { crto: 5 },
        "trigger 4 is php's refusal, after the opt level it wrote"
    );
    assert_eq!(
        Setting::of(b"1250"),
        Setting::Invalid { crto: 0 },
        "opt level 0"
    );
    assert_eq!(
        Setting::of(b"11255"),
        Setting::Invalid { crto: 1255 },
        "five digits"
    );
    assert_eq!(Setting::of(b"-1254"), Setting::Invalid { crto: 0 });
    assert_eq!(Setting::of(b"sideways"), Setting::Invalid { crto: 0 });
}

/// `zend_jit_status` once php's start is over, as the oracle reports it
/// on this target (`opcache.enable_cli=1`; 16368 and 67108848 under 16K
/// and 64M on its 16 KiB page): `JIT_G` as `opcache.jit` set it,
/// the digits a refused number wrote kept, a zero buffer and a buffer
/// past the target's reach neither enabled nor on, and the buffer
/// rounded to the page less two pointers, free but for the code of the
/// traces published.
#[test]
fn the_status_is_zend_jit_status() {
    let status = |entries: &[(&str, &str)]| {
        let (mut policy, _) = read_ini(entries);
        policy.check_support(|_| false);
        let s = Policy::status(0, &policy.start_setting());
        (
            (s.enabled, s.on),
            (s.kind, s.opt_level, s.opt_flags),
            s.buffer_size,
            s.buffer_free,
        )
    };
    let buffer = |size: u64| u64::try_from(crate::table::code::capacity(size) - 16).unwrap_or(0);
    let big = buffer(64 << 20);
    for (asked, expected) in [
        (&[][..], ((false, false), (0, 0, 0), 0, 0)),
        (
            &[("opcache.jit", "disable")],
            ((false, false), (0, 0, 0), 0, 0),
        ),
        (
            &[("opcache.jit", "off")],
            ((true, false), (0, 0, 0), big, big),
        ),
        (
            &[("opcache.jit", "tracing")],
            ((true, true), (5, 4, 6), big, big),
        ),
        (
            &[("opcache.jit", "function")],
            ((true, true), (0, 5, 6), big, big),
        ),
        (
            &[("opcache.jit", "1235")],
            ((true, true), (3, 5, 6), big, big),
        ),
        (
            &[("opcache.jit", "0205")],
            ((true, true), (0, 5, 2), big, big),
        ),
        (
            &[("opcache.jit", "1294")],
            ((false, false), (0, 4, 0), 0, 0),
        ),
        (&[("opcache.jit", "abc")], ((false, false), (0, 0, 0), 0, 0)),
        (
            &[("opcache.jit", "tracing"), ("opcache.jit_buffer_size", "0")],
            ((false, false), (5, 4, 6), 0, 0),
        ),
        (
            &[
                ("opcache.jit", "tracing"),
                ("opcache.jit_buffer_size", "16K"),
            ],
            ((true, true), (5, 4, 6), buffer(16 << 10), buffer(16 << 10)),
        ),
        (
            &[("opcache.jit", "tracing"), ("opcache.jit_buffer_size", "1")],
            ((true, true), (5, 4, 6), buffer(1), buffer(1)),
        ),
        (
            &[
                ("opcache.jit", "tracing"),
                ("opcache.jit_buffer_size", "4G"),
            ],
            ((false, false), (5, 4, 6), 0, 0),
        ),
    ] {
        assert_eq!(status(asked), expected, "{asked:?}");
    }
    let (policy, _) = read_ini(&[("opcache.jit", "tracing")]);
    let started = policy.start_setting();
    let used = Policy::status(1000, &started);
    assert_eq!((used.buffer_size, used.buffer_free), (big, big - 1000));
    assert_eq!(Policy::status(usize::MAX, &started).buffer_free, 0);
}

/// `zend_jit_status` after a request's `ini_set('opcache.jit', ...)`:
/// `JIT_G` as the request holds it, which the opcache extension's
/// handler rewrote, and the buffer it holds as the start laid it down,
/// less what the traces take; the start's `JIT_G` keeps
/// `opcache.jit_debug`'s bits whatever the tier runs.
#[test]
fn the_status_is_the_requests_jit_g_and_its_buffer() {
    let (policy, _) = read_ini(&[("opcache.jit", "off"), ("opcache.jit_debug", "16")]);
    let started = policy.start_setting();
    let big = u64::try_from(crate::table::code::capacity(64 << 20) - 16).unwrap_or(0);
    assert_eq!((started.buffer_size, started.debug), (big, 16));
    let jit_g = TierStatus {
        on: true,
        opt_level: 5,
        opt_flags: 6,
        ..started
    };
    let s = Policy::status(100, &jit_g);
    assert_eq!(
        s,
        TierStatus {
            buffer_free: big - 100,
            ..jit_g
        }
    );
    let (off, _) = read_ini(&[("opcache.jit", "disable"), ("opcache.jit_debug", "0x3000")]);
    assert_eq!(off.start_setting().debug, 0x3000);
    assert_eq!(JitDebug::of(b"-1").get(), u32::MAX);
}

/// php gives an op array its counters as it persists it while
/// `JIT_G(on)`: a unit's words count where the request publishing it has
/// the tier on, whatever the start's was, and a lane counts every unit;
/// a tier that runs `Off` counts none. With no request, the start's
/// `JIT_G` says. Counting words start cold where the tier was on
/// at the start, which fills php's pool of counters at each request
/// start, and primed where it was set up and off, whose pool nothing
/// fills, so each roots at its first arrival (#3363).
#[test]
fn a_units_words_are_laid_as_its_request_has_the_tier() {
    let laid = |entries: &[(&str, &str)], on: Option<bool>| {
        let (policy, _) = read_ini(entries);
        policy.laid(on.unwrap_or(policy.start_setting().on))
    };
    let off = [("opcache.jit", "off")];
    let tracing = [("opcache.jit", "tracing")];
    assert_eq!(laid(&off, Some(false)), Laid::Sealed);
    assert_eq!(laid(&off, None), Laid::Sealed);
    assert_eq!(laid(&off, Some(true)), super::PRIMED);
    assert_eq!(laid(&tracing, Some(true)), Laid::COLD);
    assert_eq!(laid(&tracing, Some(false)), Laid::Sealed);
    assert_eq!(laid(&tracing, None), Laid::COLD);
    let lane = [("opcache.jit", "off"), ("jit.mode", "force")];
    assert_eq!(laid(&lane, Some(false)), Laid::COLD);
    assert_eq!(
        laid(&[("opcache.jit", "disable")], Some(true)),
        Laid::Sealed
    );
    let zero = [("opcache.jit", "tracing"), ("opcache.jit_buffer_size", "0")];
    assert_eq!(laid(&zero, Some(true)), Laid::Sealed);
    let file_cache_only = [
        ("opcache.enable_cli", "1"),
        ("opcache.jit", "tracing"),
        ("opcache.file_cache_only", "1"),
    ];
    assert_eq!(laid(&file_cache_only, None), Laid::Sealed);
}

/// A word laid primed reads cold and reaches every threshold php's
/// directives allow at its first visit's bump, the heat saturating there.
#[test]
fn a_primed_word_reaches_any_threshold_at_its_first_visit() {
    let Laid::Counting([heat, state]) = super::PRIMED else {
        panic!("primed words count");
    };
    let word = Word::default();
    word.halves()[0].store(heat, Ordering::Relaxed);
    word.halves()[1].store(state, Ordering::Relaxed);
    assert_eq!(read(&word), State::Cold);
    let most = read_ini(&[
        ("opcache.jit_hot_loop", "255"),
        ("opcache.jit_hot_func", "255"),
    ])
    .0;
    let reached = bump(&word);
    for start in [Start::Loop, Start::Enter] {
        assert!(
            most.threshold(start).is_some_and(|hot| reached >= hot),
            "{start:?}"
        );
    }
    assert_eq!(bump(&word), u32::MAX, "the heat saturates");
}

/// A landing is armed as php's `zend_jit_trace_setup_ret_counter` hands
/// it a slot of the pool of counters: where the tier is set up and off,
/// a slot no request start filled, primed, so the first return claims;
/// where it was on from the start, from zero, so php's
/// `opcache.jit_hot_return` returns claim. An armed landing is left.
#[test]
fn a_landing_is_armed_from_the_pool_php_hands_it_a_slot_of() {
    for (setting, heat) in [("off", u32::MAX), ("tracing", 0)] {
        let (policy, _) = read_ini(&[("opcache.jit", setting)]);
        let word = Word::sealed();
        super::arm(&word, &policy);
        assert_eq!(read(&word), State::Cold, "{setting}");
        assert_eq!(word.halves()[0].load(Ordering::Relaxed), heat, "{setting}");
        word.halves()[0].store(7, Ordering::Relaxed);
        super::arm(&word, &policy);
        assert_eq!(word.halves()[0].load(Ordering::Relaxed), 7, "{setting}");
    }
}

/// `zend_jit_check_support` refuses a buffer the target's code cannot
/// reach once it set the tier up, with php's warning, and refuses no
/// lane over a tier php never set up.
#[test]
fn a_buffer_past_the_targets_reach_is_refused() {
    let past = if cfg!(target_arch = "aarch64") {
        "129M"
    } else {
        "3G"
    };
    let (mut policy, _) = read_ini(&[
        ("opcache.jit", "tracing"),
        ("opcache.jit_buffer_size", past),
    ]);
    policy.check_support(|_| false);
    assert_eq!(policy.refused(), Some(Refused::BufferSize));
    assert_eq!(policy.mode(), Mode::Off);
    let (mut policy, _) = read_ini(&[("jit.mode", "force"), ("opcache.jit_buffer_size", past)]);
    policy.check_support(|_| false);
    assert_eq!((policy.refused(), policy.mode()), (None, Mode::Force));
    let (mut policy, _) = read_ini(&[
        ("opcache.jit", "tracing"),
        ("opcache.jit_buffer_size", "128M"),
    ]);
    policy.check_support(|_| false);
    assert_eq!(policy.refused(), None);
    let expected = if cfg!(target_arch = "aarch64") {
        "JIT on AArch64 doesn't support opcache.jit_buffer_size above 128M."
    } else {
        "JIT on x86_64 doesn't support opcache.jit_buffer_size above 2G."
    };
    assert_eq!(Refused::BufferSize.to_string(), expected);
}

/// `function` and any trigger but `HOT_TRACE` trace as tracing does and say so
/// once; `HOT_TRACE` says nothing; `disable`, a refused setting and a
/// zero buffer are `Off`, say nothing and read no `opcache.jit_debug`
/// bit, as php's compiled tier that never started reads none (`gh13232.phpt`);
/// `off` with a buffer traces what a request publishes after turning it
/// on, says nothing, and keeps its bits for those traces.
#[test]
fn a_setting_maps_to_its_mode_with_one_line_where_it_is_not_tracing_as_asked() {
    assert_eq!(read_ini(&[("opcache.jit", "tracing")]), {
        let (policy, _) = read_ini(&[("opcache.jit", "1254")]);
        (policy, Vec::new())
    });
    let (policy, notes) = read_ini(&[("opcache.jit", "function")]);
    assert_eq!(policy.mode(), Mode::Tracing);
    assert_eq!(notes, [Note::AsTracing { crto: 1205 }]);
    let (policy, notes) = read_ini(&[("opcache.jit", "1235")]);
    assert_eq!(policy.mode(), Mode::Tracing);
    assert_eq!(notes, [Note::AsTracing { crto: 1235 }]);
    for asked in [
        [
            ("opcache.jit", "disable"),
            ("opcache.jit_buffer_size", "64M"),
        ],
        [("opcache.jit", "1245"), ("opcache.jit_buffer_size", "64M")],
        [("opcache.jit", "on"), ("opcache.jit_buffer_size", "0")],
    ] {
        let mut ini = asked.to_vec();
        ini.push(("opcache.jit_debug", "0xfffff"));
        let (policy, notes) = read_ini(&ini);
        assert_eq!(
            (policy.mode(), notes, policy.debugs()),
            (Mode::Off, vec![], false),
            "{asked:?}"
        );
    }
    let off = [
        ("opcache.jit", "off"),
        ("opcache.jit_buffer_size", "64M"),
        ("opcache.jit_debug", "0x3f000"),
    ];
    let (policy, notes) = read_ini(&off);
    assert_eq!(
        (policy.mode(), notes, policy.debugs()),
        (Mode::Tracing, vec![], true)
    );
    assert_eq!(policy.bound(true), Bound::Runtime);
}

/// php's defaults with nothing set; its bounds keep the default for a
/// value outside them; `jit.mode` names the `Mode` whatever `opcache.jit`
/// says, and its lanes compile inline.
#[test]
fn the_directives_take_phps_defaults_and_bounds_and_jit_mode_names_the_lane() {
    let (policy, _) = read_ini(&[]);
    assert_eq!(policy.mode(), Mode::Off);
    assert_eq!(policy.compile(), Compile::Thread);
    assert_eq!(policy.threshold(Start::Loop), Some(61));
    assert_eq!(policy.threshold(Start::Enter), Some(127));
    assert_eq!(policy.threshold(Start::Return), Some(8));
    assert_eq!(policy.threshold(side()), Some(8));
    assert_eq!(
        (policy.spent(Start::Loop), policy.spent(side())),
        (Spent::After(16), Spent::At(16))
    );
    assert_eq!(policy.buffer_size, 64 << 20);
    assert_eq!(
        (
            policy.max_root_traces,
            policy.max_side_traces,
            policy.max_exit_counters
        ),
        (1024, 128, 8192)
    );
    assert_eq!(policy.max_trace_length, 1024);
    let (policy, _) = read_ini(&[
        ("opcache.jit_hot_loop", "256"),
        ("opcache.jit_hot_func", "0"),
        ("opcache.jit_max_loop_unrolls", "0"),
        ("opcache.jit_max_recursive_returns", "4"),
        ("opcache.jit_max_trace_length", "3"),
        ("opcache.jit_debug", "0x3F000"),
        ("opcache.jit", "tracing"),
    ]);
    assert_eq!(
        policy.threshold(Start::Loop),
        Some(61),
        "past 255: the default"
    );
    assert_eq!(
        policy.threshold(Start::Enter),
        None,
        "zero installs no counter"
    );
    assert_eq!(
        (policy.max_loop_unrolls, policy.max_recursive_returns),
        (8, 2)
    );
    assert_eq!(policy.max_trace_length, 1024);
    assert!(policy.debug.any(JitDebug::TRACE_START) && policy.debug.any(JitDebug::TRACE_BLACKLIST));
    let (policy, notes) = read_ini(&[("jit.mode", "force")]);
    assert_eq!(
        (policy.mode(), policy.compile()),
        (Mode::Force, Compile::Inline)
    );
    assert_eq!(notes, [Note::Lane(Mode::Force)]);
    let (policy, _) = read_ini(&[("jit.mode", "tracing"), ("jit.compile", "inline")]);
    assert_eq!(
        (policy.mode(), policy.compile()),
        (Mode::Tracing, Compile::Inline)
    );
    // A root's end lines are written on its request, so bits that ask
    // for one compile that request's recordings there
    // (`compiler::Submitter::submit`); the start line or a `PERF` map
    // alone does not, and the engine's worker stands whichever.
    for (debug, ends) in [
        ("8192", true),
        ("65536", true),
        ("4096", false),
        ("16", false),
    ] {
        let (policy, _) = read_ini(&[("opcache.jit", "tracing"), ("opcache.jit_debug", debug)]);
        assert_eq!(policy.compile(), Compile::Thread, "jit_debug={debug}");
        assert_eq!(
            JitDebug::of(debug.as_bytes()).ends(),
            ends,
            "jit_debug={debug}"
        );
    }
}

/// One row of the state machine: a word of `start`'s kind, cold, heated
/// three times, claimed and settled with `outcome`, and what it reads.
fn row(start: Start, outcome: Outcome) -> (State, u32, u32) {
    let (policy, _) = read_ini(&[]);
    let (epoch, func) = fixture();
    let holder = root(&epoch, func, 1);
    let word = holder.word();
    for _ in 0..3 {
        bump(word);
    }
    let claimed = claim(holder.clone(), &policy, start);
    assert_eq!(read(word), State::Recording);
    if let Some(claimed) = claimed {
        assert_eq!(claimed.start(), start);
        settle(claimed, &outcome);
    }
    (read(word), attempts(word), heat(word))
}

/// The machine as a table, one row per `(State, Start, Outcome)` from
/// cold: a publish compiles, an abort is cold again with one more
/// attempt, a root's with no heat and a side's with its heat kept, a
/// release counts no attempt, a root's with no heat and a side's with
/// the firing that claimed it uncounted, a blacklist is at once; claim is `None` on a
/// word that is not cold.
#[test]
fn the_state_machine_as_a_table() {
    let rows = [
        (
            Start::Loop,
            Outcome::Published(trace(7)),
            (State::Compiled(trace(7)), 0, 3),
        ),
        (
            Start::Enter,
            Outcome::Published(trace(9)),
            (State::Compiled(trace(9)), 0, 3),
        ),
        (
            Start::Return,
            Outcome::Abort(Stop::NotSupported),
            (State::Cold, 1, 0),
        ),
        (side(), Outcome::Abort(Stop::TooLong), (State::Cold, 1, 3)),
        (Start::Loop, Outcome::Released, (State::Cold, 0, 0)),
        (side(), Outcome::Released, (State::Cold, 0, 2)),
        (
            Start::Loop,
            Outcome::Blacklist(Stop::TooManyTraces),
            (State::Blacklisted, 0, 3),
        ),
        (
            side(),
            Outcome::Blacklist(Stop::TooManyExits),
            (State::Blacklisted, 0, 3),
        ),
    ];
    for (start, outcome, expected) in rows {
        assert_eq!(row(start, outcome), expected, "{start:?} {outcome:?}");
    }
    let (policy, _) = read_ini(&[]);
    let (epoch, func) = fixture();
    let first = claim(root(&epoch, func, 0), &policy, Start::Loop);
    assert!(first.is_some());
    assert!(
        claim(root(&epoch, func, 0), &policy, Start::Loop).is_none(),
        "recording"
    );
    if let Some(first) = first {
        settle(first, &Outcome::Published(trace(2)));
    }
    assert!(
        claim(root(&epoch, func, 0), &policy, Start::Loop).is_none(),
        "compiled"
    );
}

/// Sixteen aborts blacklist a root, each abort a claim of its own after
/// the word is cold again with no heat, as php's bad root cache counts
/// them; the exit a side grows from, whose heat is kept, is blacklisted
/// by the abort that finds its heat at `opcache.jit_hot_side_exit` plus
/// `opcache.jit_blacklist_side_trace`, its ninth at php's defaults,
/// claimed at the exit's eighth firing and at each firing after, as
/// php's `zend_jit_trace_exit_is_hot` and `_is_bad` keep one count.
#[test]
fn sixteen_aborts_blacklist_a_root_and_a_sides_exit_its_ninth() {
    let (policy, _) = read_ini(&[("opcache.jit", "tracing")]);
    let (epoch, func) = fixture();
    let holder = root(&epoch, func, 0);
    for attempt in 1..=16 {
        assert_eq!(read(holder.word()), State::Cold, "before attempt {attempt}");
        let claimed = claim(holder.clone(), &policy, Start::Loop);
        assert!(claimed.is_some());
        if let Some(claimed) = claimed {
            settle(claimed, &Outcome::Abort(Stop::LoopExit));
        }
        assert_eq!(heat(holder.word()), 0);
    }
    assert_eq!(read(holder.word()), State::Blacklisted);
    assert!(claim(holder, &policy, Start::Loop).is_none());

    let (epoch, func) = fixture();
    let holder = root(&epoch, func, 1);
    let word = holder.word();
    let threshold = policy.threshold(side()).unwrap_or(u32::MAX);
    let (mut firings, mut aborts) = (0, 0);
    while read(word) == State::Cold && firings < 100 {
        firings += 1;
        if bump(word) < threshold {
            continue;
        }
        let claimed = claim(holder.clone(), &policy, side());
        assert!(claimed.is_some(), "firing {firings}");
        if let Some(claimed) = claimed {
            aborts += 1;
            settle(claimed, &Outcome::Abort(Stop::Exception));
        }
    }
    assert_eq!((read(word), firings, aborts), (State::Blacklisted, 16, 9));
}

/// An exit that grows no side is spent at the firing that brings its
/// heat to `opcache.jit_hot_side_exit` plus
/// `opcache.jit_blacklist_side_trace`, sixteen at php's defaults and
/// nine under the recipe's threshold of one, once; a root's word is never
/// spent.
#[test]
fn an_exit_that_grows_no_side_is_blacklisted_at_its_bound() {
    for (ini, bound) in [
        (vec![("opcache.jit", "tracing")], 16),
        (
            vec![
                ("opcache.jit", "tracing"),
                ("opcache.jit_hot_side_exit", "1"),
            ],
            9,
        ),
        (
            vec![
                ("opcache.jit", "tracing"),
                ("opcache.jit_hot_side_exit", "0"),
            ],
            8,
        ),
    ] {
        let (policy, _) = read_ini(&ini);
        let word = Word::default();
        let spent: Vec<u32> = (1..=40).filter(|_| spend(&word, &policy, side())).collect();
        assert_eq!(read(&word), State::Blacklisted, "{ini:?}");
        assert_eq!(spent, [bound], "{ini:?}: the firing that spent it");
        assert_eq!(heat(&word), bound, "{ini:?}: bumped until it was spent");
        assert!(!spend(&Word::default(), &policy, Start::Loop));
    }
}

/// A claim moved to another thread and dropped there unsettled settles
/// itself as `Abort(CompilerError)`: cold, one more attempt, no heat.
#[test]
fn a_claim_dropped_on_another_thread_leaves_the_word_cold_with_one_more_attempt() {
    let (policy, _) = read_ini(&[]);
    let (epoch, func) = fixture();
    let holder = root(&epoch, func, 1);
    bump(holder.word());
    let claimed: Option<Claim> = claim(holder.clone(), &policy, Start::Loop);
    assert!(claimed.is_some());
    std::thread::spawn(move || drop(claimed)).join().ok();
    let word = holder.word();
    assert_eq!(
        (read(word), attempts(word), heat(word)),
        (State::Cold, 1, 0)
    );
}

/// The layout invariant: the sealed word, all ones, reads blacklisted and
/// is never claimed, as is any number past an op array's safepoints, which
/// reads the sealed word.
#[test]
fn the_sealed_word_reads_blacklisted_and_is_never_claimed() {
    assert_eq!(read(&Word::sealed()), State::Blacklisted);
    assert_eq!(read(&Word::default()), State::Cold);
    let (policy, _) = read_ini(&[]);
    let (epoch, func) = fixture();
    let past = root(&epoch, func, 2);
    assert_eq!(read(past.word()), State::Blacklisted);
    assert!(claim(past, &policy, Start::Loop).is_none());
}

/// php's `TRACE` lines, each under its `jit_debug` bit, the blank line
/// after a root's lines under any of the four that end them.
#[test]
fn the_dump_writes_phps_trace_lines_under_their_bits() {
    let (all, _) = read_ini(&[("opcache.jit", "tracing"), ("opcache.jit_debug", "258048")]);
    let (none, _) = read_ini(&[]);
    let at = Place {
        scope: Some(b"Foo"),
        function: Some(b"bar"),
        file: b"/t.php",
        line: 4,
    };
    let main = Place {
        scope: None,
        function: None,
        file: b"/t.php",
        line: 2,
    };
    let events = [
        TraceEvent::Start {
            trace: trace(1),
            start: Start::Loop,
            at,
        },
        TraceEvent::Start {
            trace: trace(2),
            start: Start::Enter,
            at: main,
        },
        TraceEvent::Start {
            trace: trace(3),
            start: side(),
            at: main,
        },
        TraceEvent::Stop {
            trace: trace(1),
            stop: Stop::Loop,
        },
        TraceEvent::Stop {
            trace: trace(2),
            stop: Stop::Link(trace(1)),
        },
        TraceEvent::Compiled { trace: trace(1) },
        TraceEvent::Abort {
            trace: trace(3),
            stop: Stop::CompilerError,
        },
        TraceEvent::Blacklisted { trace: trace(3) },
        TraceEvent::Done,
    ];
    let mut written = Vec::new();
    let mut quiet = Vec::new();
    for event in &events {
        all.debug().dump(&mut written, event).ok();
        none.debug().dump(&mut quiet, event).ok();
    }
    assert_eq!(
        String::from_utf8_lossy(&written),
        "---- TRACE 1 start (loop) Foo::bar() /t.php:4\n\
         ---- TRACE 2 start (enter) $main() /t.php:2\n\
         ---- TRACE 3 start (side trace 1/3) $main() /t.php:2\n\
         ---- TRACE 1 stop (loop)\n\
         ---- TRACE 2 stop (link to 1)\n\
         ---- TRACE 1 compiled\n\
         ---- TRACE 3 abort (JIT compilation error)\n\
         ---- TRACE 3 blacklisted\n\
         \n"
    );
    assert!(quiet.is_empty());
}

/// The tier declares its two directives, system ini with their defaults.
#[test]
fn the_tier_declares_jit_mode_and_jit_compile_as_system_ini() {
    let declared = super::ini();
    let names: Vec<_> = declared
        .iter()
        .map(|decl| decl.name.0.as_ref().to_vec())
        .collect();
    assert_eq!(names, [b"jit.mode".to_vec(), b"jit.compile".to_vec()]);
    assert!(
        declared
            .iter()
            .all(|decl| decl.system && !decl.user && !decl.per_dir)
    );
}

/// Under `force` and `exit-fuzz` every eligible root is claimed at its
/// first visit, and one whose `opcache.jit_hot_*` is zero still at none.
#[test]
fn the_lanes_claim_at_the_first_visit() {
    let (force, _) = read_ini(&[("jit.mode", "force")]);
    let (fuzz, _) = read_ini(&[("jit.mode", "exit-fuzz"), ("opcache.jit_hot_return", "0")]);
    for start in [Start::Loop, Start::Enter, Start::Return, side()] {
        assert_eq!(force.threshold(start), Some(1));
    }
    assert_eq!(fuzz.threshold(Start::Loop), Some(1));
    assert_eq!(fuzz.threshold(Start::Return), None);
}

/// An abort keeps its stop in the word, php's `bad_root_cache` slot: read
/// back once the word failed at least as often as asked, and gone once the
/// word compiles; a release leaves the word cold with its attempts and its
/// stop as they were and no heat.
#[test]
fn a_failed_attempt_keeps_its_stop_and_a_release_counts_none() {
    let (policy, _) = read_ini(&[]);
    let (epoch, func) = fixture();
    let holder = root(&epoch, func, 1);
    let word = holder.word();
    for stop in [Stop::InnerLoop, Stop::LoopExit] {
        if let Some(claimed) = claim(holder.clone(), &policy, Start::Loop) {
            assert_eq!(settle(claimed, &Outcome::Abort(stop)), State::Cold);
        }
    }
    assert_eq!(bad_stop(word, 2), Some(Stop::LoopExit));
    assert_eq!(bad_stop(word, 3), None);
    bump(word);
    if let Some(claimed) = claim(holder.clone(), &policy, Start::Loop) {
        assert_eq!(
            bad_stop(word, 2),
            Some(Stop::LoopExit),
            "kept while recording"
        );
        assert_eq!(settle(claimed, &Outcome::Released), State::Cold);
    }
    assert_eq!((attempts(word), heat(word)), (2, 0));
    assert_eq!(bad_stop(word, 2), Some(Stop::LoopExit));
    if let Some(claimed) = claim(holder.clone(), &policy, Start::Loop) {
        assert_eq!(
            settle(claimed, &Outcome::Published(trace(4))),
            State::Compiled(trace(4))
        );
    }
    assert_eq!(bad_stop(word, 0), None);
    assert_eq!(bad_stop(&Word::sealed(), 0), None);
}

/// A lane's `jit.mode` leaves to the interpreter a program whose
/// directives say php's tier never starts, `opcache.jit=disable`, a
/// refused setting or a zero buffer, with no note, and still
/// names its lane over `off`, `tracing` and nothing set (#3136's ruling).
#[test]
fn a_lane_leaves_a_tier_that_never_starts_to_the_interpreter() {
    for lane in ["force", "exit-fuzz"] {
        for (set, bound) in [
            (&[("opcache.jit", "disable")][..], Bound::Never),
            (&[("opcache.jit", "bogus")], Bound::Never),
            (&[("opcache.jit_buffer_size", "0")], Bound::Never),
            (
                &[("opcache.jit", "tracing"), ("opcache.jit_buffer_size", "0")],
                Bound::Never,
            ),
            (&[("opcache.jit", "off")], Bound::On),
            (&[("opcache.jit", "tracing")], Bound::On),
            (&[], Bound::On),
        ] {
            let mut asked = set.to_vec();
            asked.push(("jit.mode", lane));
            let (policy, notes) = read_on("cli", &asked);
            assert_eq!(policy.bound(true), bound, "{asked:?}");
            let lined = [Note::Lane(policy.mode())];
            let expected: &[Note] = if bound == Bound::On { &lined } else { &[] };
            assert_eq!(notes, expected, "{asked:?}");
        }
    }
}

/// php's `zend_jit_check_support` over the opcodes the extensions
/// observe: one past the two silence opcodes runs the tier `Off` with
/// its `opcache.jit_debug` bits cleared, refused with php's warning
/// where php sets its tier up, `opcache.jit=off` among them, and with
/// none where php never does; the silence opcodes alone refuse nothing.
#[test]
fn an_observed_opcode_past_the_silence_opcodes_runs_the_tier_off() {
    let echo = |opcode| opcode == Opcode::Echo;
    let silence = |opcode| matches!(opcode, Opcode::BeginSilence | Opcode::EndSilence);
    let debug = ("opcache.jit_debug", "0x3f000");
    for (asked, warned) in [
        (&[("opcache.jit", "tracing"), debug][..], true),
        (&[("opcache.jit", "off"), debug], true),
        (&[("opcache.jit", "1205")], true),
        (&[("opcache.jit", "tracing"), ("jit.mode", "force")], true),
        (&[("opcache.jit", "disable"), ("jit.mode", "force")], false),
        (&[("opcache.jit", "disable")], false),
        (&[("opcache.jit", "1245")], false),
        (
            &[("opcache.jit", "tracing"), ("opcache.jit_buffer_size", "0")],
            false,
        ),
    ] {
        let (mut policy, _) = read_ini(asked);
        policy.check_support(echo);
        let refused = policy.refused();
        assert_eq!(
            refused,
            warned.then_some(Refused::ObservedOpcode),
            "{asked:?}"
        );
        assert_eq!(policy.mode(), Mode::Off, "{asked:?}");
        assert!(!policy.debugs(), "{asked:?}");
    }
    let (mut policy, _) = read_ini(&[("opcache.jit", "tracing"), debug]);
    policy.check_support(silence);
    assert_eq!(policy.refused(), None);
    assert_eq!(policy.mode(), Mode::Tracing);
    assert!(policy.debugs());
    assert_eq!(
        Refused::ObservedOpcode.to_string(),
        "JIT is incompatible with third party extensions that setup user opcode handlers. JIT disabled."
    );
}

/// php sets its tier up only where opcache starts (`accel_startup`,
/// `Z/ext/opcache/ZendAccelerator.c:3210-3220`): with `opcache.enable`
/// on, and on the command line and in phpdbg with `opcache.enable_cli`
/// on too, which no other host reads. Elsewhere the tier runs `Off` with
/// no `opcache.jit_debug` bit, and an observed opcode refuses nothing,
/// so no warning is raised; `jit.mode` names its lane whatever the two
/// say, and refuses with php's warning only where php set its tier up.
#[test]
fn the_tier_is_set_up_only_where_opcache_starts() {
    let echo = |opcode| opcode == Opcode::Echo;
    let tracing = ("opcache.jit", "tracing");
    let debug = ("opcache.jit_debug", "0x3000");
    let cli_on = ("opcache.enable_cli", "1");
    let disabled = ("opcache.enable", "0");
    let force = ("jit.mode", "force");
    for (host, asked, mode, warned) in [
        ("cli", &[tracing, debug][..], Mode::Off, false),
        ("phpdbg", &[tracing, debug], Mode::Off, false),
        (
            "cli",
            &[tracing, debug, ("opcache.enable_cli", "0")],
            Mode::Off,
            false,
        ),
        ("cli", &[tracing, debug, cli_on], Mode::Tracing, true),
        (
            "phpdbg",
            &[tracing, debug, ("opcache.enable_cli", "On")],
            Mode::Tracing,
            true,
        ),
        ("cli", &[tracing, debug, cli_on, disabled], Mode::Off, false),
        ("cgi-fcgi", &[tracing, debug], Mode::Tracing, true),
        (
            "fpm-fcgi",
            &[tracing, debug, ("opcache.enable_cli", "0")],
            Mode::Tracing,
            true,
        ),
        ("fpm-fcgi", &[tracing, debug, disabled], Mode::Off, false),
        ("cli-server", &[tracing, debug], Mode::Tracing, true),
        ("cli", &[force], Mode::Force, false),
        ("cli", &[force, tracing], Mode::Force, false),
        ("cli", &[force, tracing, cli_on], Mode::Force, true),
        (
            "cli",
            &[("jit.mode", "exit-fuzz"), disabled],
            Mode::ExitFuzz,
            false,
        ),
    ] {
        let (mut policy, _) = read_on(host, asked);
        assert_eq!(policy.mode(), mode, "{host} {asked:?}");
        assert_eq!(
            policy.debugs(),
            asked.contains(&debug) && mode != Mode::Off,
            "{host} {asked:?}"
        );
        policy.check_support(echo);
        assert_eq!(
            policy.refused(),
            warned.then_some(Refused::ObservedOpcode),
            "{host} {asked:?}"
        );
        assert_eq!(policy.mode(), Mode::Off, "{host} {asked:?}");
    }
}
