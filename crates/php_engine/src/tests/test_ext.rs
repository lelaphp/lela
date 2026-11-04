//! `TestExt`, the engine tests' adapter for seam 3 (design §4.3), as
//! `Probe` is seam 8's: an extension that records every phase the
//! engine hands it, in order, so a test asserts the Extension seam's
//! lifecycle as a sequence and not as the engine's arrangement inside.
//!
//! This file is compiled twice: by the engine's tests, where `super::api`
//! is `crate::api`, and by `php_ext`'s lifecycle test, where it is the
//! facade. It names the face only through `super::api`, so a phase that
//! an extension cannot reach through `php_ext` does not build there.
//!
//! The extensions of one test are told apart by a number, `N`, which
//! also names their state types, so each has a request state and an
//! engine state of its own. Every phase is a function generic over `N`
//! that records `Event::Ext(N, phase)` into one log the whole process
//! shares, since a request state is built by `Default` with nothing to
//! hand it; [`Recording`] holds that log for one test at a time. A
//! phase the engine gains later is one more [`Phase`] variant, one more
//! function here and one more line in [`TestExt::builder`], and the
//! tests already written read the sequence through [`Events`]. A test
//! that wants one extension to act and not only record, a request-end
//! function that raises a fatal or an engine start that refuses, takes
//! [`TestExt::builder`] and replaces that one declaration before it
//! builds, with a function that calls [`record`] as these do. A phase
//! added to [`TestExt::builder`] shows in every sequence a test spells
//! out whole, on purpose. The process scope stands apart: built
//! once per process (ADR-0010), and the tests of one binary share the
//! process, so [`TestExt::builder`] declares no process state, and a
//! test that wants one takes [`TestExt::with_process`] or
//! [`TestExt::process_refusing`] under an `N` no other test builds.

use std::sync::{Mutex, MutexGuard, PoisonError};

use super::api::abi::{
    Beneath, Compiled, ErrorNotice, Extension, ExtensionBuilder, FiberStatus, Observed,
    ObservedFiber, ObserverHandlers, Refusal, Start, Statement, ZendExtension, chain,
};
use super::api::call::Function;
use super::api::decl::{IniDecl, Name};
use super::api::raising::{Level, Thrown};
use super::api::request::{Allocation, AllocationObserverId, ObserveAllocations, Request};
use super::api::values::{ClassId, Owner, PhpObject, Resource, Value};

/// One thing the log was handed, in the order it was handed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Event {
    /// What the test wrote between two of its own acts, so a sequence
    /// can be cut at a build, a request or a drop.
    Mark(&'static str),
    /// Extension `N` was handed this phase.
    Ext(u8, Phase),
}

/// What an extension is handed, each named for the declaration on
/// [`ExtensionBuilder`] that asked for it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Phase {
    /// Its request state was built, by `Default`.
    RequestStateBuilt,
    /// Its request state was dropped.
    RequestStateDropped,
    /// Its engine state was built: the number of engines this
    /// extension had built a state for before, in this recording.
    EngineStateBuilt(u32),
    /// The engine state of that number was dropped.
    EngineStateDropped(u32),
    /// Its engine start refused, as [`TestExt::refusing`] declares it.
    EngineStartRefused,
    /// Its process state was built, as [`TestExt::with_process`]
    /// declares it.
    ProcessStateBuilt,
    /// Its process state was dropped, which no process scope ever is.
    ProcessStateDropped,
    /// Its process start refused, as [`TestExt::process_refusing`]
    /// declares it.
    ProcessStartRefused,
    /// Its engine start, as [`TestExt::with_process`] declares it, was
    /// handed its process state: the number of process states this
    /// extension had built before that one, `None` where it was handed
    /// none.
    ProcessStateReached(Option<u32>),
    /// Its `rinit` ran, and the number of the engine state the request
    /// reached, `None` where it reached none.
    Rinit(Option<u32>),
    /// Its `rshutdown` ran.
    Rshutdown,
    /// Its `deactivate` ran.
    Deactivate,
    /// Its `post_deactivate` ran.
    PostDeactivate,
    /// Its engine-end function ran, on the shutdown request, and the
    /// number of the engine state that request reached, `None` where it
    /// reached none.
    EngineEnd(Option<u32>),
    /// The resource its `rinit` opened and never released was closed:
    /// php's `zend_close_rsrc_list` at request end.
    ResourceClosed,
    /// Its `post_startup` ran, on the request the engine lays to ask
    /// it. [`TestExt::builder`] declares none, since one declared lays
    /// that request before every engine's first; a test that wants one
    /// declares its own and records this.
    PostStartup,
    /// Its `zend_error_cb` link was handed a diagnostic, the message, and
    /// served the one beneath after.
    Error(String),
    /// Its `gc_collect_cycles` link was served, before it served the one
    /// beneath.
    Collect,
    /// The one beneath its `gc_collect_cycles` link answered, the count.
    Collected(i64),
    /// Its `zend_compile_string` link was handed a string, before it
    /// served the one beneath with it.
    CompileString(String),
    /// Its `zend_throw_exception_hook` link was told of a throwable
    /// thrown, its message.
    Thrown(String),
    /// Its observer was asked of a function on the function's first
    /// call, or of an included or evaluated file's frame, as php's
    /// `zend_observer_fcall_init` is ([`TestExt::observing`]): what the
    /// frame serves, as [`observed_name`] renders it.
    ObserverInit(String),
    /// Its begin handler was handed a frame, what the frame serves.
    Begin(String),
    /// Its end handler was handed a frame, what the frame serves, and
    /// what it returned as [`returned_name`] renders it: `None` where
    /// it returned nothing, on an unwind or at a fatal.
    End(String, Option<String>),
    /// Its error observer was told of a diagnostic, its level, message
    /// and whether it was an uncaught throwable's ([`TestExt::observing`]).
    ErrorObserved(Level, String, bool),
    /// Its observer of declared functions was told of one, the lowered
    /// name it was declared under.
    Declared(String),
    /// Its observer of linked classes was told of one, the lowered name
    /// it stands under.
    Linked(String),
    /// Its fiber observer was told of a fiber's creation, the fiber as
    /// [`fiber_name`] renders it.
    FiberInit(String),
    /// Its fiber observer was told of a switch, the fiber left and the
    /// fiber entered.
    FiberSwitch(String, String),
    /// Its fiber observer was told of a fiber given back.
    FiberDestroy(String),
    /// Its allocation observer was told of what the request's heap
    /// served, while its `test_ext_N.observe_allocations` entry was on
    /// ([`TestExt::observing_allocations`]).
    Allocation(Allocation),
    /// Its Zend extension's statement handler was told of a statement
    /// ([`TestExt::stepping`]): where the frame stands, as
    /// [`statement_name`] renders it.
    Statement(String),
}

/// The process's one log, and the mutex that gives it to one test at a
/// time. An extension holds no mutex (ADR-0005, `ext.no-locks`); this
/// one is the test's, which a request state built by `Default` cannot
/// be handed, and no extension under `ext/` copies it.
static LOG: Mutex<Vec<Event>> = Mutex::new(Vec::new());
static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

fn guard<T>(held: &Mutex<T>) -> MutexGuard<'_, T> {
    held.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Write `phase` as handed to extension `n`: what each phase below
/// does, and what a declaration a test puts in place of one does too.
pub(crate) fn record(n: u8, phase: Phase) {
    guard(&LOG).push(Event::Ext(n, phase));
}

/// The log, held by one test from [`Recording::start`] until it is
/// dropped; the next test waits for it. A test declares it before any
/// engine, so that it is dropped after them and what they drop is in
/// this test's log and not the next one's.
pub(crate) struct Recording {
    log: &'static Mutex<Vec<Event>>,
    _held: MutexGuard<'static, ()>,
}

impl Recording {
    /// The log, emptied, for this test alone.
    pub(crate) fn start() -> Recording {
        let held = guard(&ONE_AT_A_TIME);
        guard(&LOG).clear();
        Recording {
            log: &LOG,
            _held: held,
        }
    }

    /// Write `label` into the sequence.
    pub(crate) fn mark(&self, label: &'static str) {
        guard(self.log).push(Event::Mark(label));
    }

    /// Everything recorded so far.
    pub(crate) fn events(&self) -> Events {
        Events(guard(self.log).clone())
    }
}

impl Drop for Recording {
    fn drop(&mut self) {
        guard(self.log).clear();
    }
}

/// A recorded sequence, and the cuts a test reads it through.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Events(pub(crate) Vec<Event>);

impl Events {
    /// What was recorded after the mark `from` and before the mark
    /// `to`. A test that names a mark it did not write, or two out of
    /// order, fails here rather than reading an empty cut as a pass.
    #[must_use]
    pub(crate) fn between(&self, from: &str, to: &str) -> Events {
        let at = |label: &str| {
            self.0
                .iter()
                .position(|e| matches!(e, Event::Mark(m) if *m == label))
        };
        let (start, end) = at(from)
            .zip(at(to))
            .filter(|(start, end)| start < end)
            .unwrap_or_else(|| panic!("no mark `{from}` before a mark `{to}`"));
        Events(self.0[start + 1..end].to_vec())
    }

    /// Every event but those of `phase`'s kind: what it carries is not
    /// compared, so `without(&EngineStateBuilt(0))` drops every engine
    /// state built, where [`Events::handed`] compares it.
    #[must_use]
    pub(crate) fn without(&self, phase: &Phase) -> Events {
        let kind = std::mem::discriminant(phase);
        Events(
            self.0
                .iter()
                .filter(|e| !matches!(e, Event::Ext(_, p) if std::mem::discriminant(p) == kind))
                .cloned()
                .collect(),
        )
    }

    /// The events of the kinds of `phases` alone, what each carries
    /// kept, as [`Events::without`] drops one kind.
    #[must_use]
    pub(crate) fn only(&self, phases: &[Phase]) -> Events {
        let kinds: Vec<_> = phases.iter().map(std::mem::discriminant).collect();
        Events(
            self.0
                .iter()
                .filter(
                    |e| matches!(e, Event::Ext(_, p) if kinds.contains(&std::mem::discriminant(p))),
                )
                .cloned()
                .collect(),
        )
    }

    /// Which extensions were handed `phase`, exactly as it is, in order.
    #[must_use]
    pub(crate) fn handed(&self, phase: &Phase) -> Vec<u8> {
        self.0
            .iter()
            .filter_map(|e| match e {
                Event::Ext(n, p) if p == phase => Some(*n),
                _ => None,
            })
            .collect()
    }
}

/// A throwable's `message`, as its link records it.
pub(crate) fn message_of(req: &Request, throwable: PhpObject) -> String {
    let message = throwable.prop(req, b"message");
    let bytes = message
        .as_ref()
        .and_then(|m| m.as_str_bytes())
        .unwrap_or_default();
    String::from_utf8_lossy(bytes).into_owned()
}

/// What an observed frame serves, as php's `zend_test` observer prints
/// it: `Class::method` or `function` for a function's frame, and
/// `file 'name'` for an included, evaluated or main file's, the name
/// after its last `/`.
pub(crate) fn observed_name(observed: &Observed<'_>) -> String {
    if let Some(function) = observed.function() {
        let name = observed
            .symbols()
            .function_decl(function)
            .map(|decl| String::from_utf8_lossy(&decl.name.0).into_owned())
            .unwrap_or_default();
        return match observed.function_scope() {
            Some(class) => format!(
                "{}::{name}",
                String::from_utf8_lossy(observed.symbols().class_name(class))
            ),
            None => name,
        };
    }
    let file = observed.filename();
    let base = file.rsplit(|&b| b == b'/').next().unwrap_or(file);
    format!("file '{}'", String::from_utf8_lossy(base))
}

/// Where a statement handler's frame stands, as a debugger reads a line
/// breakpoint off it: the file, after its last `/`, and the line,
/// `EX(opline)->lineno`.
pub(crate) fn statement_name(statement: &Statement<'_>) -> String {
    let frame = statement.frame();
    let file = frame.filename();
    let base = file.rsplit(|&b| b == b'/').next().unwrap_or(file);
    format!("{}:{}", String::from_utf8_lossy(base), frame.line())
}

/// What a frame returned, as the end handler was handed it: an int or a
/// string as `var_export` writes it, and the type's name otherwise.
pub(crate) fn returned_name(returned: &Value) -> String {
    if let Some(int) = returned.as_int() {
        return int.to_string();
    }
    if let Some(bytes) = returned.as_str_bytes() {
        return format!("'{}'", String::from_utf8_lossy(bytes));
    }
    String::from(returned.type_name())
}

/// A fiber as a fiber observer is handed it: `main` for the request's
/// own stack, and `#n` for a `Fiber`, `n` its object's number as
/// `spl_object_id()` reads it; then its status, and ` threw` or
/// ` destroyed` where the fiber is marked so.
pub(crate) fn fiber_name(fiber: ObservedFiber) -> String {
    let status = match fiber.status() {
        FiberStatus::Init => "init",
        FiberStatus::Running => "running",
        FiberStatus::Suspended => "suspended",
        FiberStatus::Dead => "dead",
    };
    let flags = match (fiber.threw(), fiber.destroyed()) {
        (true, true) => " threw destroyed",
        (true, false) => " threw",
        (false, true) => " destroyed",
        (false, false) => "",
    };
    match fiber.fiber() {
        Some(object) => format!("#{} {status}{flags}", object.index()),
        None => format!("main {status}{flags}"),
    }
}

/// Extension `N`'s request state: recorded where it is built and where
/// it is dropped, and the owner of the resource its `rinit` opened.
pub(crate) struct RequestSlot<const N: u8> {
    listed: Option<Owner<Resource>>,
}

impl<const N: u8> Default for RequestSlot<N> {
    fn default() -> Self {
        record(N, Phase::RequestStateBuilt);
        RequestSlot { listed: None }
    }
}

impl<const N: u8> Drop for RequestSlot<N> {
    fn drop(&mut self) {
        record(N, Phase::RequestStateDropped);
    }
}

/// Extension `N`'s request state under
/// [`TestExt::observing_allocations`]: where its allocation observer
/// stands in the heap's list, while its entry is on, as `zend_test`
/// keeps its observed heap in `ZT_G(observed_heap)`.
#[derive(Default)]
pub(crate) struct AllocationSlot<const N: u8>(Option<AllocationObserverId>);

/// What [`TestExt::observing_allocations`] puts in the heap's list: each
/// allocation recorded as handed to extension `N`, as `zend_test`'s
/// custom handlers print each.
struct AllocationRecorder<const N: u8>;

impl<const N: u8> ObserveAllocations for AllocationRecorder<N> {
    fn observe(&mut self, what: Allocation) {
        record(N, Phase::Allocation(what));
    }
}

/// Extension `N`'s process state, numbered as it is built.
pub(crate) struct ProcessSlot<const N: u8> {
    built: u32,
}

impl<const N: u8> Drop for ProcessSlot<N> {
    fn drop(&mut self) {
        record(N, Phase::ProcessStateDropped);
    }
}

/// Extension `N`'s engine state, numbered as it is built.
pub(crate) struct EngineSlot<const N: u8> {
    engine: u32,
}

impl<const N: u8> Drop for EngineSlot<N> {
    fn drop(&mut self) {
        record(N, Phase::EngineStateDropped(self.engine));
    }
}

/// The recording extension numbered `N`, named `test_ext_N`.
pub(crate) struct TestExt<const N: u8>;

impl<const N: u8> TestExt<N> {
    /// The extension with every phase it records declared.
    pub(crate) fn extension() -> Extension {
        Self::builder().build(&format!("test_ext_{N}"))
    }

    /// Those declarations, for a test that adds or replaces one before
    /// it builds; the name it builds under is `test_ext_N`.
    pub(crate) fn builder() -> ExtensionBuilder {
        ExtensionBuilder::default()
            .request_state::<RequestSlot<N>>()
            .engine_state(|_| Ok(Self::engine_state()))
            .rinit(Self::rinit)
            .rshutdown(Self::rshutdown)
            .deactivate(Self::deactivate)
            .post_deactivate(Self::post_deactivate)
            .engine_end(Self::engine_end)
            .error(Self::error)
            .gc_collect(Self::gc_collect)
            .compile_string(Self::compile_string)
            .thrown(Self::thrown)
    }

    /// [`TestExt::builder`] with an observer declared (design §4.3 seam
    /// 9): asked of each function on its first call and of each file's
    /// frame, it declines a function named `declined`, and observes the
    /// rest with a begin and an end handler that record what they are
    /// handed; and the three fiber notices, each recording the fibers
    /// it is handed. The observer is not in [`TestExt::builder`], since every
    /// frame a request serves would then show in each sequence a test of
    /// the lifecycle spells out whole, and a bundle with no observer
    /// declared is the one the plain test is measured on.
    ///
    /// It observes php's notices too, as `zend_test`'s observer does under
    /// `zend_test.observer.observe_declaring`: diagnostics, declared
    /// functions and linked classes, each recorded as told.
    pub(crate) fn observing() -> Extension {
        Self::observing_builder().build(&format!("test_ext_{N}"))
    }

    /// [`TestExt::observing`]'s declarations, for a test that adds or
    /// replaces one before it builds.
    pub(crate) fn observing_builder() -> ExtensionBuilder {
        Self::builder()
            .observer(Self::observer_init)
            .error_observer(Self::error_observed)
            .function_declared_observer(Self::declared)
            .class_linked_observer(Self::linked)
            .fiber_init(Self::fiber_init)
            .fiber_switch(Self::fiber_switch)
            .fiber_destroy(Self::fiber_destroy)
    }

    /// [`TestExt::builder`] as a Zend extension with a statement handler,
    /// as a debugger is one, that records where each statement it is
    /// told of stands. Not in [`TestExt::builder`], since only a Zend
    /// extension has one, and being one is listed under `[Zend Modules]`.
    pub(crate) fn stepping() -> Extension {
        Self::builder()
            .zend_extension(Self::zend_row(Self::statement))
            .build(&format!("test_ext_{N}"))
    }

    /// The Zend extension row of `test_ext_N`, its statement handler
    /// `statement_handler`, for a test that declares a handler of its own.
    pub(crate) fn zend_row(statement_handler: super::api::abi::StatementHandler) -> ZendExtension {
        ZendExtension {
            name: format!("test_ext_{N}"),
            version: String::from("0"),
            author: String::new(),
            url: String::new(),
            copyright: String::new(),
            statement_handler: Some(statement_handler),
        }
    }

    #[allow(clippy::unnecessary_wraps)]
    fn statement(statement: &mut Statement<'_>) -> Result<(), Thrown> {
        record(N, Phase::Statement(statement_name(statement)));
        Ok(())
    }

    fn error_observed(_: &mut Request, notice: &ErrorNotice<'_>) {
        let message = String::from_utf8_lossy(notice.message()).into_owned();
        let level = notice.level();
        record(N, Phase::ErrorObserved(level, message, notice.uncaught()));
    }

    fn declared(_: &mut Request, _: Function, lowered: &[u8]) {
        record(
            N,
            Phase::Declared(String::from_utf8_lossy(lowered).into_owned()),
        );
    }

    fn linked(_: &mut Request, _: ClassId, lowered: &[u8]) {
        record(
            N,
            Phase::Linked(String::from_utf8_lossy(lowered).into_owned()),
        );
    }

    /// [`TestExt::builder`] with an ini entry,
    /// `test_ext_N.observe_allocations`, whose `on_modify` puts an
    /// allocation observer in the request's heap where it is turned on
    /// and takes it out where it is turned off, as `zend_test`'s
    /// `zend_test.zend_mm_custom_handlers.enabled` installs and removes
    /// its handlers (`Z/ext/zend_test/zend_mm_custom_handlers.c`); its
    /// request state is where the observer stands.
    pub(crate) fn observing_allocations() -> Extension {
        // The name an `on_modify` is given lasts the process, as an
        // extension spells it in its source; a test's is made once per
        // engine and kept.
        let name: &'static str = format!("test_ext_{N}.observe_allocations").leak();
        Self::builder()
            .request_state::<AllocationSlot<N>>()
            .ini(vec![IniDecl {
                name: Name(name.as_bytes().into()),
                default: Some(b"0".as_slice().into()),
                user: true,
                per_dir: true,
                system: true,
            }])
            .on_modify(name.as_bytes(), Self::observe_allocations)
            .build(&format!("test_ext_{N}"))
    }

    /// `OnUpdateZendTestMMCustomHandlersEnabled`: no value is refused,
    /// on puts the observer in where none stands, and off takes the one
    /// standing out.
    #[allow(clippy::unnecessary_wraps)]
    fn observe_allocations(req: &mut Request, value: Option<&[u8]>) -> Result<bool, Thrown> {
        let Some(value) = value else {
            return Ok(false);
        };
        let on = crate::api::request::Ini::parse_bool(value);
        let standing = req.state::<AllocationSlot<N>>().0.take();
        let standing = match (on, standing) {
            (true, None) => Some(
                req.memory_mut()
                    .observe_allocations(Box::new(AllocationRecorder::<N>)),
            ),
            (false, Some(at)) => {
                req.memory_mut().unobserve_allocations(at);
                None
            }
            (_, standing) => standing,
        };
        req.state::<AllocationSlot<N>>().0 = standing;
        Ok(true)
    }

    fn fiber_init(_: &mut Request, fiber: ObservedFiber) {
        record(N, Phase::FiberInit(fiber_name(fiber)));
    }

    fn fiber_switch(_: &mut Request, from: ObservedFiber, to: ObservedFiber) {
        record(N, Phase::FiberSwitch(fiber_name(from), fiber_name(to)));
    }

    fn fiber_destroy(_: &mut Request, fiber: ObservedFiber) {
        record(N, Phase::FiberDestroy(fiber_name(fiber)));
    }

    fn observer_init(observed: &mut Observed<'_>) -> ObserverHandlers {
        let name = observed_name(observed);
        let declined = name == "declined";
        record(N, Phase::ObserverInit(name));
        if declined {
            return ObserverHandlers::default();
        }
        ObserverHandlers {
            begin: Some(Self::observer_begin),
            end: Some(Self::observer_end),
        }
    }

    fn observer_begin(observed: &mut Observed<'_>) {
        record(N, Phase::Begin(observed_name(observed)));
    }

    fn observer_end(observed: &mut Observed<'_>, returned: Option<&Value>) {
        record(
            N,
            Phase::End(observed_name(observed), returned.map(returned_name)),
        );
    }

    /// [`TestExt::builder`] with its engine start replaced by one that
    /// refuses, a warning raised first that names the host it was
    /// handed, as ffi's `MINIT` warns of the header it failed on before
    /// it returns `FAILURE`.
    pub(crate) fn refusing() -> Extension {
        Self::builder()
            .engine_state(Self::refuse)
            .build(&format!("test_ext_{N}"))
    }

    fn refuse(start: &Start) -> Result<EngineSlot<N>, Refusal> {
        record(N, Phase::EngineStartRefused);
        let message = format!("test_ext_{N} refuses to start under '{}'", start.host());
        Err(Refusal::new().raise(Level::Warning, message))
    }

    /// [`TestExt::builder`] with a process state declared, and its
    /// engine start replaced by one that records the process state it
    /// was handed before it builds its engine state. Built once per
    /// process, so a test that takes it takes an `N` no other test
    /// builds.
    pub(crate) fn with_process() -> Extension {
        Self::with_process_named(&format!("test_ext_{N}"))
    }

    /// [`TestExt::with_process`] built under `name`, for a test of two
    /// extensions whose process states are of one type.
    pub(crate) fn with_process_named(name: &str) -> Extension {
        Self::builder()
            .process_state(|| Ok(Self::process_state()))
            .engine_state(|start| Ok(Self::engine_state_with_process(start)))
            .build(name)
    }

    /// [`TestExt::builder`] with a process start that refuses, a warning
    /// raised first, as sodium's `MINIT` raises `sodium_init()` where
    /// the library fails to start. Called once per process, so a test that
    /// takes it takes an `N` no other test builds.
    pub(crate) fn process_refusing() -> Extension {
        Self::builder()
            .process_state(Self::refuse_process)
            .build(&format!("test_ext_{N}"))
    }

    fn process_state() -> ProcessSlot<N> {
        let built = Self::counted(&Phase::ProcessStateBuilt);
        record(N, Phase::ProcessStateBuilt);
        ProcessSlot { built }
    }

    fn refuse_process() -> Result<ProcessSlot<N>, Refusal> {
        record(N, Phase::ProcessStartRefused);
        let message = format!("test_ext_{N} refuses to start its process");
        Err(Refusal::new().raise(Level::Warning, message))
    }

    fn engine_state_with_process(start: &Start) -> EngineSlot<N> {
        let reached = start.process::<ProcessSlot<N>>().map(|slot| slot.built);
        record(N, Phase::ProcessStateReached(reached));
        Self::engine_state()
    }

    /// How many times extension `N` was handed `phase`'s kind so far.
    fn counted(phase: &Phase) -> u32 {
        let kind = std::mem::discriminant(phase);
        let count = guard(&LOG)
            .iter()
            .filter(
                |e| matches!(e, Event::Ext(n, p) if *n == N && std::mem::discriminant(p) == kind),
            )
            .count();
        u32::try_from(count).unwrap_or(u32::MAX)
    }

    fn engine_state() -> EngineSlot<N> {
        let engine = Self::counted(&Phase::EngineStateBuilt(0));
        record(N, Phase::EngineStateBuilt(engine));
        EngineSlot { engine }
    }

    /// Records which engine state the request reached, and opens a
    /// resource it never releases, as mysqli's unkeyed links stand
    /// until the request's resources are closed.
    fn rinit(req: &mut Request) -> Result<(), Thrown> {
        let engine = req.engine::<EngineSlot<N>>().map(|state| state.engine);
        record(N, Phase::Rinit(engine));
        let kind = req
            .memory_mut()
            .resources()
            .register_type(b"test_ext", Some(Self::resource_closed));
        // The owner stands with the request until `post_deactivate`, after
        // the list's close at the request's end has closed the resource.
        let listed = Resource::new(req, kind, ())?;
        req.state::<RequestSlot<N>>().listed = Some(listed);
        Ok(())
    }

    fn rshutdown(_: &mut Request) {
        record(N, Phase::Rshutdown);
    }

    fn deactivate(_: &mut Request) {
        record(N, Phase::Deactivate);
    }

    fn post_deactivate(req: &mut Request) {
        if let Some(listed) = req.state::<RequestSlot<N>>().listed.take() {
            listed.give_up(req);
        }
        record(N, Phase::PostDeactivate);
    }

    fn engine_end(req: &mut Request) {
        let engine = req.engine::<EngineSlot<N>>().map(|state| state.engine);
        record(N, Phase::EngineEnd(engine));
    }

    #[expect(
        clippy::unnecessary_wraps,
        reason = "the resource destructor's shape, which a destructor that throws needs"
    )]
    fn resource_closed(_: &mut Request, _: Resource) -> Result<(), Thrown> {
        record(N, Phase::ResourceClosed);
        Ok(())
    }

    fn error(req: &mut Request, beneath: Beneath<'_, chain::Error>) {
        let message = String::from_utf8_lossy(beneath.message()).into_owned();
        record(N, Phase::Error(message));
        beneath.serve(req);
    }

    fn gc_collect(req: &mut Request, beneath: Beneath<'_, chain::GcCollect>) -> i64 {
        record(N, Phase::Collect);
        let collected = beneath.serve(req);
        record(N, Phase::Collected(collected));
        collected
    }

    fn thrown(req: &mut Request, beneath: Beneath<'_, chain::Throw>) {
        record(N, Phase::Thrown(message_of(req, beneath.throwable())));
        beneath.serve(req);
    }

    fn compile_string(
        req: &mut Request,
        beneath: Beneath<'_, chain::CompileString>,
    ) -> Result<Compiled, Thrown> {
        let source = String::from_utf8_lossy(beneath.source()).into_owned();
        record(N, Phase::CompileString(source));
        beneath.serve(req)
    }
}
