//! The recording tier the tests of `record` and `types` serve programs
//! through: it records once, from the safepoint its plan in the engine's
//! ini names, and keeps what it recorded, the view beside the record, for
//! the test to read back.

use php_engine::tier::{
    self, Extension, ExtensionBuilder, Flow, FrameRef, FuncRef, Refusal, Request, SafepointIdx,
    Site, Start as EngineStart, Stepped, Tier, UnitView,
};
use php_unit::{MapPtrOffset, OplineIdx};

use crate::policy::{self, Holder, Outcome, Policy};
use crate::record::{
    self, Bailed, Folds, Recorded, Start, Stop, TraceRecord, header_at, returns, run,
};
use crate::table::TraceId;
use crate::testing::{self, function_of};
use php_engine::api::abi::EngineState;

/// What php's default `opcache.optimization_level` folds, which a test's
/// engine runs under where it sets none ([`Folds::of`]).
pub(crate) const DEFAULT_FOLDS: Folds = Folds(true);

/// Where the recording tier starts, as the engine's ini says: the
/// activation of the function `record.function` names (empty for the
/// included file's own code), or with `record.every` set, of every
/// function once, then, for `record.start=loop`, the first
/// loop header it reaches past the `record.headers` it passes, stepping
/// only within that frame, and, for `return`, the landing the first
/// return lands on, following the calls on the way. `record.compiled` and `record.blacklisted`
/// name functions met before whose entry words are settled so first;
/// `record.compiled_inner` names one whose loop headers after its first
/// are settled compiled.
pub(crate) const PLAN: [&str; 7] = [
    "record.function",
    "record.every",
    "record.start",
    "record.headers",
    "record.compiled",
    "record.blacklisted",
    "record.compiled_inner",
];

/// What the recording tier keeps, the request's: the op arrays of the
/// functions it met, the records it took, the holder of the word each
/// grew from, in an epoch whose trace table it made, and whether the
/// request recorded when each site was asked.
#[derive(Debug, Default)]
pub(crate) struct Kept {
    pub(crate) funcs: Vec<(Vec<u8>, FuncRef)>,
    pub(crate) taken: Vec<Recorded>,
    pub(crate) roots: Vec<Holder>,
    pub(crate) asked: Vec<bool>,
    pub(crate) done: bool,
}

/// The `Policy` the recording tier records under, the engine's.
#[derive(Debug, EngineState)]
pub(crate) struct Recorder(Policy);

/// The tier that records once, where its plan says.
pub(crate) struct Records;

impl Tier for Records {
    type State = crate::JitRequest;

    /// The recording exclusion, over the recorder's `Policy`.
    fn lay(started: &tier::Started<'_>) -> Option<crate::JitRequest> {
        let recorder = started.engine::<Recorder>()?;
        Some(crate::JitRequest::recorder(&recorder.0))
    }

    fn safepoint(req: &mut Request, frame: FrameRef, site: Site, _: tier::Held<'_>) -> Flow {
        let asked = record::recording(req);
        req.state::<Kept>().asked.push(asked);
        if site != Site::Entry {
            return Flow::Continue;
        }
        let name = function_of(req, frame);
        let held = tier::func(req, frame).filter(|f| matches!(f.offset(), MapPtrOffset::Engine(_)));
        if let (Some(name), Some(func)) = (&name, held) {
            let kept = req.state::<Kept>();
            if !kept.funcs.iter().any(|(n, _)| n == name) {
                kept.funcs.push((name.clone(), func));
            }
        }
        let every = plan(req, "record.every").is_some_and(|set| !set.is_empty()) && name.is_some();
        let wanted = every
            || plan(req, "record.function").as_deref() == Some(name.as_deref().unwrap_or_default());
        let again = every && held.is_some_and(|func| !recorded_from(req, func));
        if !wanted || held.is_none() || (req.state::<Kept>().done && !again) {
            return Flow::Continue;
        }
        req.state::<Kept>().done = true;
        settle_words(req);
        let start = match plan(req, "record.start").as_deref() {
            Some(b"loop") => Start::Loop,
            Some(b"return") => Start::Return,
            _ => Start::Enter,
        };
        let skip = plan(req, "record.headers")
            .and_then(|n| std::str::from_utf8(&n).ok()?.parse().ok())
            .unwrap_or(0);
        let (at, root) = match walk_to(req, frame, start, skip) {
            Ok(walked) => walked,
            Err(flow) => return flow,
        };
        let Some(policy) = req.engine::<Recorder>().map(|recorder| recorder.0.clone()) else {
            return Flow::Continue;
        };
        crate::table::of(req, &policy, &crate::table::Outputs::none());
        match run(req, at, start, &root, &policy, record::Opening::default()) {
            Ok((recorded, ended)) => {
                let kept = req.state::<Kept>();
                kept.taken.push(recorded);
                kept.roots.push(root);
                if ended.stepped || at != frame {
                    Flow::Resume(ended.at)
                } else {
                    Flow::Continue
                }
            }
            Err(Bailed::Fatal(fatal)) => Flow::Bail(fatal),
            Err(Bailed::Unwinding(at)) => Flow::Resume(at),
        }
    }

    fn extension() -> Option<Extension> {
        Some(
            testing::switched(ExtensionBuilder::default())
                .ini(
                    testing::declared(&PLAN)
                        .into_iter()
                        .chain(testing::declared(&testing::OPCACHE))
                        .collect(),
                )
                .engine_state(recorder)
                .request_state::<Kept>()
                .build("records"),
        )
    }
}

/// The recording tier's engine state.
#[expect(
    clippy::unnecessary_wraps,
    reason = "an engine start answers a Refusal where it fails; this one never does"
)]
pub(crate) fn recorder(start: &EngineStart) -> Result<Recorder, Refusal> {
    Ok(Recorder(Policy::from_ini(
        start.ini(),
        start.host(),
        &mut Vec::new(),
    )))
}

/// The plan's entry `name`, set.
pub(crate) fn plan(req: &Request, name: &str) -> Option<Vec<u8>> {
    req.ini().get(name.as_bytes()).map(<[u8]>::to_vec)
}

/// The entry words the plan names settled, through claims of their own.
pub(crate) fn settle_words(req: &mut Request) {
    let policy = testing::policy(&[]);
    let compiled = plan(req, "record.compiled").unwrap_or_default();
    let blacklisted = plan(req, "record.blacklisted").unwrap_or_default();
    let settled = (compiled
        .split(|b| *b == b',')
        .map(|name| (name, Outcome::Published(trace(7)))))
    .chain(
        blacklisted
            .split(|b| *b == b',')
            .map(|name| (name, Outcome::Blacklist(Stop::NotSupported))),
    );
    for (name, outcome) in settled {
        let Some(func) = met(req, name) else { continue };
        let holder = Holder::Root {
            epoch: tier::epoch(req),
            func,
            at: SafepointIdx::ENTRY,
        };
        if let Some(claim) = policy::claim(holder, &policy, Start::Enter) {
            policy::settle(claim, &outcome);
        }
    }
    let inner = plan(req, "record.compiled_inner").unwrap_or_default();
    let Some(func) = met(req, &inner) else { return };
    let mut view = UnitView::new(req);
    let headers: Vec<SafepointIdx> = view
        .hold(req, func)
        .then(|| view.op_array(func))
        .flatten()
        .map(|code| {
            let mut headers: Vec<SafepointIdx> = (0..code.oplines.len())
                .filter_map(|at| header_at(code, OplineIdx(u32::try_from(at).ok()?), DEFAULT_FOLDS))
                .collect();
            // The first is the outermost, whose body its back edge lands
            // on first, wherever php's trace roots it.
            headers.sort_by_key(|header| code.safepoints.get(header.0 as usize).copied());
            headers.into_iter().skip(1).collect()
        })
        .unwrap_or_default();
    for at in headers {
        let holder = Holder::Root {
            epoch: tier::epoch(req),
            func,
            at,
        };
        if let Some(claim) = policy::claim(holder, &policy, Start::Loop) {
            policy::settle(claim, &Outcome::Published(trace(7)));
        }
    }
}

/// Whether the recording tier recorded from `func`'s entry already.
fn recorded_from(req: &mut Request, func: FuncRef) -> bool {
    (req.state::<Kept>().roots.iter()).any(|root| {
        matches!(root, Holder::Root { func: f, .. } | Holder::Landing { func: f, .. } if *f == func)
    })
}

/// The name of an op array no epoch holds.
const REQUEST: FuncRef = FuncRef::at(MapPtrOffset::Request(0));

/// The op array of the function `name` the tier met.
pub(crate) fn met(req: &mut Request, name: &[u8]) -> Option<FuncRef> {
    req.state::<Kept>()
        .funcs
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, f)| *f)
}

/// `frame` stepped to where `start` records from: within itself for an
/// entry or a loop, and for a return into the calls it makes until one
/// returns; the frame it records in there, `frame` or, for a return, the
/// caller the first return landed in, and the holder of the word that
/// roots there.
pub(crate) fn walk_to(
    req: &mut Request,
    mut frame: FrameRef,
    start: Start,
    mut skip: u32,
) -> Result<(FrameRef, Holder), Flow> {
    let mut view = UnitView::new(req);
    let Some(mut func) = tier::func(req, frame).filter(|f| view.hold(req, *f)) else {
        return Err(Flow::Continue);
    };
    let epoch = tier::epoch(req);
    let root = |func, at| Holder::Root {
        epoch: epoch.clone(),
        func,
        at,
    };
    loop {
        let at = tier::frame(req, frame).map_or(OplineIdx(0), |view| view.opline());
        let Some(op) = view
            .op_array(func)
            .and_then(|code| code.oplines.get(at.0 as usize))
        else {
            return Err(Flow::Continue);
        };
        let header = view
            .op_array(func)
            .and_then(|code| header_at(code, at, DEFAULT_FOLDS));
        match (start, header) {
            (Start::Enter, _) => return Ok((frame, root(func, SafepointIdx::ENTRY))),
            (Start::Loop, Some(header)) if skip == 0 => return Ok((frame, root(func, header))),
            (Start::Loop, Some(_)) => skip -= 1,
            (Start::Return, _) if returns(op.opcode) => return landed(req, frame),
            _ => {}
        }
        match tier::step(req, frame) {
            Ok(Stepped::Same(next)) if next == frame => {}
            Ok(Stepped::Pushed(callee)) if start == Start::Return => {
                let Some(entered) = tier::func(req, callee).filter(|f| view.hold(req, *f)) else {
                    return Err(Flow::Resume(callee));
                };
                (frame, func) = (callee, entered);
            }
            Ok(
                Stepped::Pushed(at) | Stepped::Popped(at) | Stepped::Moved(at) | Stepped::Threw(at),
            ) => {
                return Err(Flow::Resume(at));
            }
            Ok(Stepped::Same(at)) => return Err(Flow::Resume(at)),
            Ok(Stepped::Done) => return Err(Flow::Returned),
            Err(fatal) => return Err(Flow::Bail(fatal)),
        }
    }
}

/// The return `frame` stands at stepped: the caller it popped to and the
/// holder of its landing's word (`jit-design.md` §2.2).
fn landed(req: &mut Request, frame: FrameRef) -> Result<(FrameRef, Holder), Flow> {
    let caller = match tier::step(req, frame) {
        Ok(Stepped::Popped(caller)) => caller,
        Ok(_) => return Err(Flow::Continue),
        Err(fatal) => return Err(Flow::Bail(fatal)),
    };
    let holder = Holder::Landing {
        epoch: tier::epoch(req),
        func: tier::func(req, caller).unwrap_or(REQUEST),
        at: tier::frame(req, caller).map_or(OplineIdx(0), |view| view.opline()),
    };
    Ok((caller, holder))
}

/// A plan: the function to record at and the start.
pub(crate) fn at(function: &'static str, start: &'static str) -> Vec<(&'static str, &'static str)> {
    vec![("record.function", function), ("record.start", start)]
}

/// `program` served under the recording tier with `ini`: what the tier
/// kept and what was served.
pub(crate) fn kept(ini: &[(&str, &str)], program: &str) -> (Kept, php_engine::test::Served) {
    let scratch = testing::Scratch::new();
    let engine = testing::engine::<Records>(ini);
    let (served, kept) = scratch.serve_kept::<Kept>(&engine, program);
    (kept.unwrap_or_default(), served)
}

/// `program` served under the recording tier with `ini`: the one record
/// it took with the view it read through, the record held to `dump` then
/// `parse`, and what the program wrote.
pub(crate) fn recording(ini: &[(&str, &str)], program: &str) -> Result<(Recorded, String), String> {
    let (mut kept, served) = kept(ini, program);
    let recorded = kept.taken.pop().ok_or_else(|| {
        format!(
            "nothing was recorded: {:?} {:?}",
            testing::stdout(&served),
            served.fatal
        )
    })?;
    let record = &recorded.record;
    assert_eq!(TraceRecord::parse(&record.dump()).as_ref(), Ok(record));
    if let Some(fatal) = served.fatal {
        return Err(format!("the program ended in a fatal: {fatal}"));
    }
    Ok((recorded, testing::stdout(&served)))
}

/// [`recording`], the record alone.
pub(crate) fn recorded(
    ini: &[(&str, &str)],
    program: &str,
) -> Result<(TraceRecord, String), String> {
    recording(ini, program).map(|(recorded, out)| (recorded.record, out))
}

/// Trace number `n`.
pub(crate) fn trace(n: u32) -> TraceId {
    TraceId::new(n).unwrap_or_else(|| {
        assert!(
            n != 0 && n < TraceId::LIMIT,
            "trace {n} is a number a word holds"
        );
        trace(1)
    })
}
