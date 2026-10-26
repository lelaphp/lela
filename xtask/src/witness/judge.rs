//! How one witness is judged: the per-item judge `witness` contributes to
//! the tally. The snippet runs under the oracle, whose status is what
//! `--accept` captures onto a bare `exit`; then under the engine linked
//! into this process, the one adapter this lane has and never the engine's
//! own binary (harness §7, revision 48; #401). Both runs are held to what
//! the fences pin by one rule, [`held`]: the two streams and the status
//! through [`met`] and [`oracle::verdict`], so the registry decides what a
//! difference means (harness §4.3). The oracle's run is held as the
//! engine's is, since what a fence pins is the oracle's bytes (design §8),
//! and a fence typed by hand that the oracle does not print fails however
//! the engine ran (#783).
//!
//! A fence marked `process` is run of record by the `php` binary in a
//! process of its own instead ([`super::process`]; harness §8, revision
//! 61), held by the same rule; one with a `request` fence is served as a
//! request of the web, by the php-cgi `[sapi]` pins and by the engine's
//! `php-cgi` (#1197).
//!
//! Both runs stand under the recipe, and under the `[regime.<name>]` the
//! fence names where it names one, its ini after the recipe's in each
//! (harness §8, ledger 55). The name is resolved against the pin before
//! anything runs, so the two runs are handed one regime and never two.
//!
//! A fence an accepted `trace:` row covers pins the engine's side of the
//! row ([`alone`]; harness §4.3, §8; #1650): it is held to its pinned
//! `expect` against the engine alone, and the oracle still runs, its
//! signal, status and bytes recorded in the verdict's note and not judged.
//! A fence no such row covers whose oracle ends on a signal is a bork.
//!
//! A fence naming a `locale=<name>` is asked first whether the host knows
//! it, as a `.phpt`'s `--SKIPIF--` asks `setlocale` (#1678): the oracle
//! and the linked engine each run [`probe`], and where neither can move
//! `LC_ALL` to one of the names the witness is a skip naming it, run by
//! neither and counted in the reading.

use std::fmt;

use super::anchor::{Entry, Record};
use super::fence::{Exit, Served, Witness};
use super::makala;
use super::process::{self, Binary};
use super::{Accept, Ask, Captured, Judged, Judgement, body, fence};
use crate::divergence::{Divergence, Registry};
use crate::oracle::expect::Expectation;
use crate::oracle::hosts::FPM;
use crate::oracle::{
    self, Invocation, Observation, Oracle, Pin, Program, Regime, Verdict as Compared, linked,
};
use crate::phpt::Lane;
use crate::scratch::Full;
use anyhow::{Context, Result};

/// A verdict on one witness, placed where it stands and at the item it
/// documents, which is what the printed line says after the id.
fn judged(witness: &Witness, judgement: Judgement, note: impl Into<String>) -> Judged {
    let mut judged = Judged::new(&witness.id(), &witness.at(), judgement, note);
    judged.place = format!(" at {}", witness.at());
    if !witness.subject.is_empty() {
        judged.place.push_str(", ");
        judged.place.push_str(&witness.subject);
    }
    judged
}

/// What judging one witness leaves beside its verdict: the fence line
/// `--accept` captures, whose write is the caller's, and what the run of
/// record entered, which is what the fence covers (harness §4.5
/// condition 2, revision 58).
#[derive(Debug, Default)]
pub(super) struct Kept {
    pub(super) captured: Vec<Captured>,
    /// The fence an `--accept` run left as it stood because the oracle's
    /// run reproduced the verdict it pins, at its file and the line of its
    /// `expect` fence: a verdict `--accept` takes as the oracle's as surely
    /// as one it writes ([`super::record_accepted`]).
    pub(super) reproduced: Option<(std::path::PathBuf, usize)>,
    pub(super) entered: Record,
}

/// One witness, end to end: the oracle's run, then the fence held to it
/// and to the engine's ([`both`]), or, where an accepted `trace:` row
/// covers the fence, to the engine's alone ([`alone`]). `regime` is the
/// block the fence's own `regime=<name>` names, resolved against the pin
/// by the caller once for both runs, so the oracle is passed the name and
/// the linked engine the ini it carries and neither is passed another's.
/// What the run leaves beside its verdict goes onto `kept`. Every way
/// this can go wrong is a judgement the witness carries, the oracle's
/// trouble and the linked engine's included, but one: a scratch tree the
/// volume had no room for ([`Full`]), which is no verdict on the witness
/// but the run not measured, and the `Err` the crate's reading says as a
/// skip naming the volume (#644).
pub(super) fn one(
    oracle: &Oracle<'_>,
    registry: &Registry,
    ask: &Ask,
    witness: &Witness,
    regime: Option<&Regime>,
    binary: &Binary,
    kept: &mut Kept,
) -> Result<Judged> {
    // Unserved until the run of record answers ([`of_record`]): what a
    // fence that stops before then would have entered is not known.
    kept.entered = Record::Unserved;
    match lacking(oracle, witness) {
        Ok(None) => {}
        Ok(Some(name)) => {
            // Not run, so what it would have entered is not known: its
            // natives are no witness short for a locale this host lacks.
            return Ok(judged(
                witness,
                Judgement::Skipped,
                format!(
                    "neither the oracle nor the linked engine can setlocale(LC_ALL, \"{name}\") on this host, which the fence's `locale={name}` needs"
                ),
            ));
        }
        Err(trouble) => return borked(witness, trouble),
    }
    let oracle_out = match ran_by_oracle(oracle, witness) {
        Ok(out) => out,
        Err(trouble) => return borked(witness, trouble.context("the oracle did not run it")),
    };
    let on = On {
        oracle,
        registry,
        ask,
        regime,
        binary,
    };
    match registry.pinning(&witness.id()) {
        Some(row) => alone(&on, witness, row, &oracle_out, kept),
        None => both(&on, witness, &oracle_out, kept),
    }
}

/// A fence no accepted `trace:` row covers, held to the oracle's run and
/// the engine's by one rule ([`held`]): the status `--accept` may capture
/// from the oracle, the streams a `capture` writes from it, the run of
/// record, and the verdict. An oracle that ends on a signal leaves no
/// status to pin, and the fence borks.
fn both(
    on: &On<'_>,
    witness: &Witness,
    oracle_out: &Observation,
    kept: &mut Kept,
) -> Result<Judged> {
    let Some(status) = oracle_out.exit.code() else {
        return Ok(judged(
            witness,
            Judgement::Bork,
            "the oracle ended on a signal, so there is no status to pin",
        ));
    };

    // The bare `exit` is the opt-in, and the only status `--accept` writes:
    // a status the fence already pins and the oracle no longer gives is a
    // difference for the registry or for a fix, never one to repin over
    // (design §8, the paragraph on witnesses in rustdoc).
    let mut exit = witness.exit;
    let mut head = witness.fences[0].clone();
    if on.ask.accept != Accept::Nothing && exit == Exit::Bare {
        head = fence::pinned(&head, status);
        exit = Exit::Pinned(status);
    }
    // A fence marked `capture` is written whole from the oracle's streams,
    // where a second run from a scratch tree apart prints them again
    // (#801); one it refuses is written nothing, and names the line. A
    // plain fence the oracle does not meet is written so under `--bodies`,
    // its authored wildcard lines kept (#3455); one it refuses is written
    // nothing, judged as it stands, and its note names why.
    let mut captured = witness.clone();
    let mut refused = None;
    let bodied = on.body_asked(witness, exit, oracle_out);
    let streams = if bodied {
        Some(bodies(on.oracle, witness, exit, oracle_out))
    } else {
        (on.ask.accept == Accept::Streams && witness.capture)
            .then(|| again(on.oracle, witness, oracle_out))
    };
    let written = streams.map(|streams| {
        streams.and_then(|(stdout, stderr)| {
            fence::streams(&head, &stdout, &stderr).map(|lines| (stdout, stderr, lines))
        })
    });
    let (was, now) = match written {
        Some(Ok((stdout, stderr, lines))) => {
            captured.stdout = stdout;
            captured.stderr = (!stderr.is_empty()).then_some(stderr);
            (&witness.fences[..], lines)
        }
        Some(Err(why)) if bodied => {
            refused = Some(why);
            (&witness.fences[..1], vec![head])
        }
        Some(Err(refused)) => {
            return Ok(judged(
                witness,
                Judgement::Unpinned,
                format!(
                    "`capture` wrote nothing: {refused}; write the fence by hand, a wildcard where the line moves, and drop `capture`"
                ),
            ));
        }
        None => (&witness.fences[..1], vec![head]),
    };
    if now != was {
        kept.captured.push(Captured {
            file: witness.file.clone(),
            line: witness.fence,
            was: was.to_vec(),
            now,
        });
    } else if on.ask.accept != Accept::Nothing
        && (matches!(exit, Exit::Pinned(_)) || witness.capture)
        && met(on.oracle.pin(), witness, exit, oracle_out).unwrap_or(false)
    {
        kept.reproduced = Some((witness.file.clone(), witness.fence));
    }
    let witness = &captured;
    if exit == Exit::Bare {
        kept.entered = Record::default();
        return Ok(judged(
            witness,
            Judgement::Unpinned,
            format!(
                "the fence's `exit` is bare and the oracle exits {status}; `xtask witness --accept` writes it, and nothing else does"
            ),
        ));
    }

    let ran = match of_record(on, witness, kept) {
        Ok(ran) => ran,
        Err(verdict) => return verdict,
    };
    let mut verdict = held(
        on.oracle.pin(),
        on.registry,
        witness,
        exit,
        &[(Run::Oracle, oracle_out), (Run::Engine, &ran)],
    );
    if let Some(why) = refused {
        verdict.note = format!("`--bodies` wrote nothing: {why}\n{}", verdict.note);
    }
    match on.registry.accepted(&witness.id()) {
        Some(row) if verdict.judgement == Judgement::Diverged => {
            Ok(proven(on, witness, row, (&ran, None), verdict))
        }
        _ => Ok(verdict),
    }
}

/// The run of record, with what it entered kept, or the bork where it did
/// not run.
fn of_record(
    on: &On<'_>,
    witness: &Witness,
    kept: &mut Kept,
) -> Result<Observation, Result<Judged>> {
    match serve(on.oracle, on.binary, witness, on.regime) {
        Ok((out, natives)) => {
            kept.entered = Record::Entered(natives);
            Ok(out)
        }
        Err(trouble) => Err(borked(witness, trouble)),
    }
}

/// What a witness is judged on beside the fence itself: the oracle, the
/// registry, what the run was asked for, the regime resolved for both runs
/// and the binary a `process` fence is served by.
struct On<'a> {
    oracle: &'a Oracle<'a>,
    registry: &'a Registry,
    ask: &'a Ask,
    regime: Option<&'a Regime>,
    binary: &'a Binary,
}

impl On<'_> {
    /// Whether the run asks `--bodies` of `witness`: a plain fence, not
    /// marked `capture`, whose pins under `exit` the oracle's run does not
    /// meet (#3455).
    fn body_asked(&self, witness: &Witness, exit: Exit, oracle_out: &Observation) -> bool {
        self.ask.accept == Accept::Bodies
            && !witness.capture
            && !met(self.oracle.pin(), witness, exit, oracle_out).unwrap_or(true)
    }
}

/// A fence `row`, an accepted `trace:` row, covers: held to its pinned
/// `expect` against the engine alone (harness §4.3, §8; #1650). The oracle
/// has run, and how it ended and what it printed is recorded in the note,
/// not judged. The engine's run is the one the fence is held to, so it is
/// the one `--accept` captures a bare `exit` from; a fence marked
/// `capture` is written nothing, since what a capture writes is the
/// oracle's streams.
fn alone(
    on: &On<'_>,
    witness: &Witness,
    row: &Divergence,
    oracle_out: &Observation,
    kept: &mut Kept,
) -> Result<Judged> {
    let mut recorded = format!(
        "held to the engine alone under {}; the oracle's run, not judged: {}",
        row.id,
        ended(oracle_out)
    );
    // The fence pins the engine's side of the row, so the oracle's streams
    // are no body for it (#3455).
    if on.body_asked(witness, witness.exit, oracle_out) {
        recorded = format!(
            "`--bodies` wrote nothing: the fence pins the engine's side of {}, not the oracle's; {recorded}",
            row.id
        );
    }
    if on.ask.accept == Accept::Streams && witness.capture {
        return Ok(judged(
            witness,
            Judgement::Unpinned,
            format!(
                "`capture` writes the oracle's streams, and this fence is {recorded}; write the fence by hand and drop `capture`"
            ),
        ));
    }
    let ran = match of_record(on, witness, kept) {
        Ok(ran) => ran,
        Err(verdict) => return verdict,
    };
    let mut exit = witness.exit;
    if on.ask.accept != Accept::Nothing && exit == Exit::Bare {
        let Some(status) = ran.exit.code() else {
            return Ok(judged(
                witness,
                Judgement::Bork,
                format!(
                    "{} ended on a signal, so there is no status to pin; {recorded}",
                    Run::Engine
                ),
            ));
        };
        let head = witness.fences[0].clone();
        kept.captured.push(Captured {
            file: witness.file.clone(),
            line: witness.fence,
            now: vec![fence::pinned(&head, status)],
            was: vec![head],
        });
        exit = Exit::Pinned(status);
    }
    if exit == Exit::Bare {
        return Ok(judged(
            witness,
            Judgement::Unpinned,
            format!(
                "the fence's `exit` is bare and {} exits {}; `xtask witness --accept` writes it, and nothing else does; {recorded}",
                Run::Engine,
                ran.exit
            ),
        ));
    }
    let mut verdict = held(
        on.oracle.pin(),
        on.registry,
        witness,
        exit,
        &[(Run::Engine, &ran)],
    );
    if verdict.judgement == Judgement::Pass {
        let covered = judged(witness, Judgement::Covered, recorded);
        return Ok(proven(on, witness, row, (&ran, Some(exit)), covered));
    }
    verdict.note = format!("{recorded}\n{}", verdict.note);
    Ok(verdict)
}

/// `verdict` on a fence `row` holds, where the row names the regime its
/// cause is proven at (#3365): the oracle's run of the snippet again under
/// that regime beside the fence's own, held to the engine's run `ran`.
/// The verdict stands where the two are one run's bytes, or, for a fence
/// pinning the engine's side under `exit`, where the run meets the fence
/// as the engine's did, its wildcards over what moves; and the fence
/// fails, naming its id and the regime, where they still differ (#3369);
/// a request fence, whose host takes the fence's regime alone, fails so.
fn proven(
    on: &On<'_>,
    witness: &Witness,
    row: &Divergence,
    (ran, pinned): (&Observation, Option<Exit>),
    verdict: Judged,
) -> Judged {
    let Some(regime) = &row.proven_at else {
        return verdict;
    };
    let regimes: Vec<&str> = (witness.regime.as_deref().into_iter())
        .chain([regime.as_str()])
        .collect();
    let snippet = Program::Snippet(&witness.snippet);
    let again = match &witness.request {
        Some(_) => Err(anyhow::anyhow!(
            "a request fence's host runs under the fence's regime alone"
        )),
        None => on.oracle.run_under(&Invocation {
            regimes: &regimes,
            ..Invocation::new(&on.oracle.pin().oracle.path, snippet)
        }),
    };
    let pin = on.oracle.pin();
    let meets =
        |again: &Observation| pinned.is_some_and(|e| met(pin, witness, e, again).unwrap_or(false));
    let differs = match again.map(|again| (meets(&again), shown_of(&again), shown_of(ran))) {
        Ok((meets, theirs, ours)) if meets || theirs == ours => return verdict,
        Ok((_, theirs, ours)) => oracle::diff(&theirs, &ours),
        Err(trouble) => format!("{trouble:#}"),
    };
    let why = format!(
        "{} proves its cause at {regime}, and the oracle under it is not the linked engine on {}:\n{differs}",
        row.id,
        witness.id()
    );
    judged(witness, Judgement::Fail, why)
}

/// How a run ended and what it printed, as a covered fence's note records
/// the oracle's: the status or the signal, and each stream's length and
/// blob id, so a reading over another oracle run says whether the bytes
/// moved.
fn ended(ran: &Observation) -> String {
    let stream = |bytes: &[u8]| {
        let id = crate::git::blob_id(bytes);
        format!(
            "{} bytes, blob {}",
            bytes.len(),
            id.get(..12).unwrap_or(&id)
        )
    };
    format!(
        "{}, stdout {}, stderr {}",
        ran.exit,
        stream(&ran.stdout),
        stream(&ran.stderr)
    )
}

/// The oracle's run of a witness: the snippet under the recipe and the
/// fence's regime, or, for a `request` fence, the request served by the
/// host the pin's `[sapi]` names.
fn ran_by_oracle(oracle: &Oracle<'_>, witness: &Witness) -> Result<Observation> {
    let sapi = &oracle.pin().sapi;
    let (snippet, regime) = (&witness.snippet, witness.regime.as_deref());
    let Some(request) = &witness.request else {
        return oracle.run(Program::Snippet(snippet), regime);
    };
    oracle.asserted()?;
    match witness.served {
        Served::Makala => {
            makala::served_by_mapping(oracle, &sapi.named(FPM)?.path, snippet, request)
        }
        served => {
            let host = &sapi.named(process::host_of(served))?.path;
            process::served_as(oracle, served, host, snippet, request, regime, &[])
        }
    }
}

/// The oracle's run of `witness` again from a scratch tree apart
/// ([`oracle::Place::Apart`]), held to `first`'s status: the second run a
/// write of the oracle's streams stands on, a capture's (#801) and a
/// body's (#3455), since a line that moves with the directory, the
/// process or the clock is the author's to write with a wildcard
/// (harness §8). `Err` names a status the two runs did not agree on.
fn apart(
    oracle: &Oracle<'_>,
    witness: &Witness,
    first: &Observation,
) -> Result<Observation, String> {
    let second = ran_by_oracle(&oracle.apart(), witness)
        .map_err(|trouble| format!("the oracle's second run did not run: {trouble:#}"))?;
    if first.exit != second.exit {
        let status = |ran: &Observation| {
            ran.exit
                .code()
                .map_or_else(|| "on a signal".to_owned(), |code| format!("{code}"))
        };
        return Err(format!(
            "the oracle exits {} from one scratch directory and {} from another",
            status(first),
            status(&second)
        ));
    }
    Ok(second)
}

/// A run's two streams as the text a fence holds, each already under the
/// recipe's normalisation; `Err` names each that is not UTF-8.
fn texts(ran: &Observation) -> Result<[&str; 2], String> {
    let (stdout, stderr) = (
        std::str::from_utf8(&ran.stdout),
        std::str::from_utf8(&ran.stderr),
    );
    if let (Ok(stdout), Ok(stderr)) = (stdout, stderr) {
        return Ok([stdout, stderr]);
    }
    let not = [("stdout", stdout.is_err()), ("stderr", stderr.is_err())];
    let named: Vec<String> = (not.iter().filter(|(_, not)| *not))
        .map(|(name, _)| format!("{name} is not UTF-8, which a fence cannot hold"))
        .collect();
    Err(named.join("; "))
}

/// The two streams a capture writes: the oracle's `first` run, held to a
/// second apart ([`apart`]) line by line, each as text that matches it
/// alone ([`body::literal`]). `Err` names what the two runs
/// did not agree on, the status or each line that differs.
fn again(
    oracle: &Oracle<'_>,
    witness: &Witness,
    first: &Observation,
) -> Result<(String, String), String> {
    let second = apart(oracle, witness, first)?;
    let [one, two] = [texts(first)?, texts(&second)?];
    let mut moved = Vec::new();
    for (name, one, two) in [("stdout", one[0], two[0]), ("stderr", one[1], two[1])] {
        let (mut a, mut b) = (one.split('\n'), two.split('\n'));
        for at in 1.. {
            match (a.next(), b.next()) {
                (None, None) => break,
                (x, y) if x == y => {}
                (x, y) => moved.push(format!(
                    "{name} line {at} is `{}` from one scratch directory and `{}` from another",
                    x.unwrap_or_default(),
                    y.unwrap_or_default()
                )),
            }
        }
    }
    if !moved.is_empty() {
        return Err(moved.join("; "));
    }
    let rules = &oracle.pin().recipe.normalise;
    Ok((body::literal(rules, one[0]), body::literal(rules, one[1])))
}

/// The two streams `--bodies` writes into a plain fence the oracle's
/// `first` run does not meet: the oracle's, from it and a second run
/// apart ([`apart`]), each authored line whose wildcards the oracle's
/// still meets in both kept as written ([`body::kept`]). The fence's
/// status is not a body's to move: one the fence pins and the oracle does
/// not give is refused, since a difference in a status is the registry's
/// or a fix's (design §8). What it would write is held whole to both runs
/// by [`met`], the judge's own rule, so it writes nothing the judge would
/// fail. `Err` names why nothing is written.
fn bodies(
    oracle: &Oracle<'_>,
    witness: &Witness,
    exit: Exit,
    first: &Observation,
) -> Result<(String, String), String> {
    if first.exit.code() != exit.asserted() {
        let pinned = (exit.asserted()).map_or_else(|| "none".to_owned(), |c| format!("status {c}"));
        return Err(format!(
            "the oracle ends on {} and the fence pins {pinned}, and a body's write moves no status",
            first.exit
        ));
    }
    let second = apart(oracle, witness, first)?;
    let [one, two] = [texts(first)?, texts(&second)?];
    let pin = oracle.pin();
    let rules = &pin.recipe.normalise;
    let stderr = witness.stderr.as_deref().unwrap_or_default();
    let mut written = witness.clone();
    written.stdout = body::kept(rules, "stdout", &witness.stdout, [one[0], two[0]])?;
    let stderr = body::kept(rules, "stderr", stderr, [one[1], two[1]])?;
    written.stderr = Some(stderr);
    for ran in [first, &second] {
        if !met(pin, &written, exit, ran).unwrap_or(false) {
            return Err(
                "the authored lines kept among the oracle's do not meet the oracle's run as a whole"
                    .to_owned(),
            );
        }
    }
    Ok((written.stdout, written.stderr.unwrap_or_default()))
}

/// A witness borked on `trouble`, or the `Err` [`one`] returns where the
/// trouble is a full volume ([`Full`]).
pub(super) fn borked(witness: &Witness, trouble: anyhow::Error) -> Result<Judged> {
    if Full::of(&trouble).is_some() {
        return Err(trouble);
    }
    Ok(judged(witness, Judgement::Bork, format!("{trouble:#}")))
}

/// The snippet that asks a host whether it knows a locale: `bool(true)`
/// where `setlocale(LC_ALL, <name>)` takes. The fence grammar admits only
/// names that stand in a single-quoted string as written.
fn probe(name: &str) -> String {
    format!("var_dump(setlocale(LC_ALL, '{name}') !== false);")
}

/// Whether a probe's run says the host knows the locale.
fn knows(ran: &Observation) -> bool {
    String::from_utf8_lossy(&ran.stdout).contains("bool(true)")
}

/// The first locale the fence names that neither the oracle nor the linked
/// engine can move `LC_ALL` to; nothing where each name is known to one of
/// them, and then the witness runs and the verdict says whether the two
/// agree. The engine is asked only where the oracle does not know it.
fn lacking<'w>(oracle: &Oracle<'_>, witness: &'w Witness) -> Result<Option<&'w str>> {
    for name in &witness.locales {
        let snippet = probe(name);
        let oracle_ran = oracle
            .run(Program::Snippet(&snippet), None)
            .with_context(|| format!("the oracle did not run the probe for `locale={name}`"))?;
        if knows(&oracle_ran) {
            continue;
        }
        let engine_ran =
            linked::serve(&oracle.pin().recipe, None, &snippet).with_context(|| {
                format!("the linked engine did not serve the probe for `locale={name}`")
            })?;
        if !knows(&engine_ran) {
            return Ok(Some(name));
        }
    }
    Ok(None)
}

/// The bundle features the `php` serving a `process` fence is built
/// with: its crate's where the binary under test does not turn it on,
/// and the tier's where its regime sets the lane's `jit.mode`, so the
/// lane runs on a binary that carries the tier, as the phpt lane's
/// `--jit` builds it (#2962), or turns php's compiled tier on, so what a
/// program sees of the tier is the tier's (#3193).
fn features<'w>(witness: &'w Witness, regime: Option<&Regime>) -> Vec<&'w str> {
    let mut features: Vec<&str> = witness.feature.as_deref().into_iter().collect();
    if regime.is_some_and(tiered) && !features.contains(&Lane::FEATURE) {
        features.push(Lane::FEATURE);
    }
    features
}

/// Whether `regime` asks for the compiled tier: it names a lane, or sets
/// `opcache.jit` to a setting `zend_jit_config` turns on, anything but
/// `disable` and the forms of off (`Z/ext/opcache/jit/zend_jit.c:3655-3667`).
fn tiered(regime: &Regime) -> bool {
    let on = |setting: &str| {
        let off = ["disable", "", "0", "off", "no", "false"];
        !off.iter().any(|word| setting.eq_ignore_ascii_case(word))
    };
    regime.sets(Lane::DIRECTIVE) || regime.value("opcache.jit").is_some_and(on)
}

/// The run of record, and what it entered: the engine linked into the
/// harness, which needs no binary (design §8; #119, #401), or for a fence
/// marked `process` the `php` binary in a process of its own (harness
/// §8, revision 61). A panic in the linked engine is trouble the witness
/// borks on, never the lane's end; the trouble is the sentence it borks
/// with.
pub(super) fn serve(
    oracle: &Oracle<'_>,
    binary: &Binary,
    witness: &Witness,
    regime: Option<&Regime>,
) -> Result<(Observation, Vec<Entry>)> {
    match witness.served {
        Served::Linked => linked::serve_by(None, &oracle.pin().recipe, regime, &witness.snippet)
            .map(|(out, entered)| (out, entered.lines().filter_map(Entry::parse).collect()))
            .context("the linked engine did not serve it"),
        Served::Binary => binary
            .serve(
                oracle,
                &witness.snippet,
                witness.regime.as_deref(),
                &features(witness, regime),
            )
            .context("the php binary did not serve it"),
        served @ (Served::Cgi | Served::Fpm | Served::Makala | Served::Phpdbg) => {
            let request = witness.request.as_deref().unwrap_or_default();
            let regime = witness.regime.as_deref();
            (binary.serve_request(oracle, served, &witness.snippet, request, regime)).with_context(
                || {
                    let host = process::host_of(served).binary;
                    format!("the {host} binary did not serve it")
                },
            )
        }
    }
}

/// The two runs a witness stands on, which is what a miss names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Run {
    Oracle,
    Engine,
}

impl fmt::Display for Run {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Run::Oracle => "the oracle",
            Run::Engine => "the linked engine",
        })
    }
}

/// The verdict on one witness from its two runs, each held to what the
/// fences pin by [`met`] and [`oracle::verdict`], one rule for both. A run
/// that misses is the registry's to accept; an accepted row names one side
/// of a difference between the engine and the oracle, so a fence neither
/// run meets fails whatever the registry says. The note names each run
/// that missed, with its difference.
pub(super) fn held(
    pin: &Pin,
    registry: &Registry,
    witness: &Witness,
    exit: Exit,
    runs: &[(Run, &Observation)],
) -> Judged {
    let id = witness.id();
    let expected = shown(
        &witness.stdout,
        witness.stderr.as_deref().unwrap_or_default(),
        exit.asserted(),
    );
    let mut missed = Vec::new();
    for &(run, observed) in runs {
        let met = match met(pin, witness, exit, observed) {
            Ok(met) => met,
            Err(trouble) => {
                return judged(
                    witness,
                    Judgement::Bork,
                    format!("the pinned stream is not one this engine can match: {trouble:#}"),
                );
            }
        };
        let actual = shown_of(observed);
        match oracle::verdict(registry, &id, met, &expected, &actual) {
            Compared::Same => {}
            verdict => missed.push((run, verdict, actual)),
        }
    }
    match missed.as_slice() {
        [] => judged(witness, Judgement::Pass, ""),
        [(_, Compared::Diverged(row), _)] => {
            judged(witness, Judgement::Diverged, row.reason.clone())
        }
        _ => judged(
            witness,
            Judgement::Fail,
            missed
                .iter()
                .map(|(run, verdict, actual)| match verdict {
                    Compared::Moved(moved) => format!("what {run} ran is not what {moved}\n"),
                    _ => format!(
                        "what the fences pin is not what {run} ran:\n{}",
                        oracle::diff(&expected, actual)
                    ),
                })
                .collect::<Vec<_>>()
                .concat(),
        ),
    }
}

/// Whether a run meets what the fences pin: the two streams as the phpt
/// runner matches an `--EXPECTF--` section, since the lines a witness pins
/// carry the same wildcards (design §8), and the status exactly.
fn met(pin: &Pin, witness: &Witness, exit: Exit, ran: &Observation) -> Result<bool> {
    if ran.exit.code() != exit.asserted() {
        return Ok(false);
    }
    let stdout = Expectation::Format(normalised(pin, &witness.stdout));
    if !stdout.met(&ran.stdout)? {
        return Ok(false);
    }
    let stderr = Expectation::Format(normalised(
        pin,
        witness.stderr.as_deref().unwrap_or_default(),
    ));
    stderr.met(&ran.stderr)
}

/// A pinned stream under the recipe's normalisation, which is what the run's
/// own streams have been through; the oracle module owns the rule.
fn normalised(pin: &Pin, text: &str) -> Vec<u8> {
    oracle::normalise(&pin.recipe.normalise, text.as_bytes().to_vec())
}

/// What `ran` printed and how it ended as [`shown`] renders it, a byte
/// that is no UTF-8 shown lossily.
fn shown_of(ran: &Observation) -> Vec<u8> {
    let lossy = |bytes: &[u8]| String::from_utf8_lossy(bytes).into_owned();
    shown(&lossy(&ran.stdout), &lossy(&ran.stderr), ran.exit.code())
}

/// The three streams of one run as one document: what a difference is shown
/// as, and never a format anything reads back.
fn shown(stdout: &str, stderr: &str, exit: Option<i32>) -> Vec<u8> {
    let status = exit.map_or_else(|| "a signal".to_owned(), |code| code.to_string());
    format!("status {status}\nstdout:\n{stdout}\nstderr:\n{stderr}\n").into_bytes()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shown_document_carries_the_status_and_both_streams() {
        let text = String::from_utf8(shown("out", "err", Some(3))).unwrap();
        assert_eq!(text, "status 3\nstdout:\nout\nstderr:\nerr\n");
        let signal = String::from_utf8(shown("", "", None)).unwrap();
        assert!(signal.starts_with("status a signal"), "{signal}");
    }
}
