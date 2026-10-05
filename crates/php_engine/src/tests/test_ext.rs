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
//! out whole, on purpose. A scope built once per process (#1800) is
//! held by a test whose `N` no other test builds, since the tests of
//! one binary share the process.

use std::sync::{Mutex, MutexGuard, PoisonError};

use super::api::abi::{Extension, ExtensionBuilder, Refusal, Start};
use super::api::raising::{Level, Thrown};
use super::api::request::Request;
use super::api::values::Resource;

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
        let cut = at(from).zip(at(to)).filter(|(start, end)| start < end);
        assert!(cut.is_some(), "no mark `{from}` before a mark `{to}`");
        let (start, end) = cut.unwrap_or_default();
        Events(self.0.get(start + 1..end).unwrap_or_default().to_vec())
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

/// Extension `N`'s request state: recorded where it is built and where
/// it is dropped, and nothing else.
pub(crate) struct RequestSlot<const N: u8>;

impl<const N: u8> Default for RequestSlot<N> {
    fn default() -> Self {
        record(N, Phase::RequestStateBuilt);
        RequestSlot
    }
}

impl<const N: u8> Drop for RequestSlot<N> {
    fn drop(&mut self) {
        record(N, Phase::RequestStateDropped);
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

    fn engine_state() -> EngineSlot<N> {
        let built = guard(&LOG)
            .iter()
            .filter(|e| matches!(e, Event::Ext(n, Phase::EngineStateBuilt(_)) if *n == N))
            .count();
        let engine = u32::try_from(built).unwrap_or(u32::MAX);
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
            .resources()
            .register_type(b"test_ext", Some(Self::resource_closed));
        Resource::new(req, kind, ())?;
        Ok(())
    }

    fn rshutdown(_: &mut Request) {
        record(N, Phase::Rshutdown);
    }

    fn deactivate(_: &mut Request) {
        record(N, Phase::Deactivate);
    }

    fn post_deactivate(_: &mut Request) {
        record(N, Phase::PostDeactivate);
    }

    fn engine_end(req: &mut Request) {
        let engine = req.engine::<EngineSlot<N>>().map(|state| state.engine);
        record(N, Phase::EngineEnd(engine));
    }

    fn resource_closed(_: &mut Request, _: Resource) {
        record(N, Phase::ResourceClosed);
    }
}
