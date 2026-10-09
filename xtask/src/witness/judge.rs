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
//! A fence naming a `locale=<name>` is asked first whether the host knows
//! it, as a `.phpt`'s `--SKIPIF--` asks `setlocale` (#1678): the oracle
//! and the linked engine each run [`probe`], and where neither can move
//! `LC_ALL` to one of the names the witness is a skip naming it, run by
//! neither and counted in the reading.

use std::fmt;

use super::anchor::{Entry, Record};
use super::fence::{Exit, Served, Witness};
use super::process::{self, Binary};
use super::{Accept, Ask, Captured, Judged, Judgement, fence};
use crate::divergence::Registry;
use crate::oracle::expect::Expectation;
use crate::oracle::{self, Observation, Oracle, Pin, Program, Regime, Verdict as Compared, linked};
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
    pub(super) entered: Record,
}

/// One witness, end to end: the oracle's run, the status `--accept` may
/// capture from it, the run of record, and the verdict. `regime` is the
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
    match lacking(oracle, witness) {
        Ok(None) => {}
        Ok(Some(name)) => {
            // Not run, so what it would have entered is not known: its
            // natives are no witness short for a locale this host lacks.
            kept.entered = Record::Unserved;
            return Ok(judged(
                witness,
                Judgement::Skipped,
                format!(
                    "neither the oracle nor the linked engine can setlocale(LC_ALL, \"{name}\") on this host, which the fence's `locale={name}` needs"
                ),
            ));
        }
        Err(trouble) => {
            kept.entered = Record::Unserved;
            return borked(witness, trouble);
        }
    }
    let oracle_out = match ran_by_oracle(oracle, witness) {
        Ok(out) => out,
        Err(trouble) => {
            kept.entered = Record::Unserved;
            return borked(witness, trouble.context("the oracle did not run it"));
        }
    };
    let Some(status) = oracle_out.exit.code() else {
        kept.entered = Record::Unserved;
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
    if ask.accept != Accept::Nothing && exit == Exit::Bare {
        head = fence::pinned(&head, status);
        exit = Exit::Pinned(status);
    }
    // A fence marked `capture` is written whole from the oracle's streams,
    // where a second run from a scratch tree apart prints them again
    // (#801); one it refuses is written nothing, and names the line.
    let mut captured = witness.clone();
    let (was, now) = if ask.accept == Accept::Streams && witness.capture {
        let streams = again(oracle, witness, &oracle_out).and_then(|(stdout, stderr)| {
            fence::streams(&head, &stdout, &stderr).map(|lines| (stdout, stderr, lines))
        });
        let (stdout, stderr, lines) = match streams {
            Ok(written) => written,
            Err(refused) => {
                kept.entered = Record::Unserved;
                return Ok(judged(
                    witness,
                    Judgement::Unpinned,
                    format!(
                        "`capture` wrote nothing: {refused}; write the fence by hand, a wildcard where the line moves, and drop `capture`"
                    ),
                ));
            }
        };
        captured.stdout = stdout;
        captured.stderr = (!stderr.is_empty()).then_some(stderr);
        (&witness.fences[..], lines)
    } else {
        (&witness.fences[..1], vec![head])
    };
    if now != was {
        kept.captured.push(Captured {
            file: witness.file.clone(),
            line: witness.fence,
            was: was.to_vec(),
            now,
        });
    }
    let witness = &captured;
    if exit == Exit::Bare {
        return Ok(judged(
            witness,
            Judgement::Unpinned,
            format!(
                "the fence's `exit` is bare and the oracle exits {status}; `xtask witness --accept` writes it, and nothing else does"
            ),
        ));
    }

    let ran = match serve(oracle, binary, witness, regime) {
        Ok((out, natives)) => {
            kept.entered = Record::Entered(natives);
            out
        }
        Err(trouble) => {
            kept.entered = Record::Unserved;
            return borked(witness, trouble);
        }
    };
    Ok(held(
        oracle.pin(),
        registry,
        witness,
        exit,
        [(Run::Oracle, &oracle_out), (Run::Engine, &ran)],
    ))
}

/// The oracle's run of a witness: the snippet under the recipe and the
/// fence's regime, or, for a `request` fence, the request served by the
/// host the pin's `[sapi]` names.
fn ran_by_oracle(oracle: &Oracle<'_>, witness: &Witness) -> Result<Observation> {
    match &witness.request {
        Some(commands) if witness.served == Served::Phpdbg => oracle.asserted().and_then(|()| {
            let Some(phpdbg) = &oracle.pin().sapi.phpdbg else {
                anyhow::bail!("oracle.toml's [sapi] names no phpdbg to serve the program");
            };
            process::served_as_phpdbg(oracle, &phpdbg.path, &witness.snippet, commands, &[])
        }),
        Some(request) if witness.served == Served::Fpm => oracle.asserted().and_then(|()| {
            let Some(fpm) = &oracle.pin().sapi.fpm else {
                anyhow::bail!("oracle.toml's [sapi] names no php-fpm to serve the request");
            };
            super::fpm::served_as_fpm(oracle, &fpm.path, &witness.snippet, request, &[])
        }),
        Some(request) => oracle.asserted().and_then(|()| {
            let Some(cgi) = &oracle.pin().sapi.cgi else {
                anyhow::bail!("oracle.toml's [sapi] names no php-cgi to serve the request");
            };
            process::served_as_cgi(
                oracle,
                &cgi.path,
                &witness.snippet,
                request,
                witness.regime.as_deref(),
                &[],
            )
        }),
        None => oracle.run(
            Program::Snippet(&witness.snippet),
            witness.regime.as_deref(),
        ),
    }
}

/// The two streams a capture writes: the oracle's `first` run, held to a
/// second from a scratch tree apart ([`oracle::Place::Apart`]), each
/// already under the recipe's normalisation. `Err` names what the two runs
/// did not agree on, the status or each line that differs, since a line
/// that moves with the directory, the process or the clock is the
/// author's to write with a wildcard (harness §8, #801).
fn again(
    oracle: &Oracle<'_>,
    witness: &Witness,
    first: &Observation,
) -> Result<(String, String), String> {
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
    let mut moved = Vec::new();
    let mut texts = [String::new(), String::new()];
    let pairs = [
        ("stdout", &first.stdout, &second.stdout),
        ("stderr", &first.stderr, &second.stderr),
    ];
    for (text, (name, one, two)) in texts.iter_mut().zip(pairs) {
        let (Ok(one), Ok(two)) = (std::str::from_utf8(one), std::str::from_utf8(two)) else {
            moved.push(format!("{name} is not UTF-8, which a fence cannot hold"));
            continue;
        };
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
        one.clone_into(text);
    }
    if !moved.is_empty() {
        return Err(moved.join("; "));
    }
    let [stdout, stderr] = texts;
    Ok((stdout, stderr))
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
        Served::Linked => linked::serve_traced(&oracle.pin().recipe, regime, &witness.snippet)
            .map(|(out, entered)| (out, entered.lines().filter_map(Entry::parse).collect()))
            .context("the linked engine did not serve it"),
        Served::Binary => binary
            .serve(oracle, &witness.snippet, witness.regime.as_deref())
            .context("the php binary did not serve it"),
        Served::Cgi => binary
            .serve_cgi(
                oracle,
                &witness.snippet,
                witness.request.as_deref().unwrap_or_default(),
                witness.regime.as_deref(),
            )
            .context("the php-cgi binary did not serve it"),
        Served::Fpm => binary
            .serve_fpm(
                oracle,
                &witness.snippet,
                witness.request.as_deref().unwrap_or_default(),
            )
            .context("the php-fpm binary did not serve it"),
        Served::Phpdbg => binary
            .serve_phpdbg(
                oracle,
                &witness.snippet,
                witness.request.as_deref().unwrap_or_default(),
            )
            .context("the phpdbg binary did not serve it"),
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
    runs: [(Run, &Observation); 2],
) -> Judged {
    let id = witness.id();
    let expected = shown(
        &witness.stdout,
        witness.stderr.as_deref().unwrap_or_default(),
        exit.asserted(),
    );
    let mut missed = Vec::new();
    for (run, observed) in runs {
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
        let actual = shown(
            &String::from_utf8_lossy(&observed.stdout),
            &String::from_utf8_lossy(&observed.stderr),
            observed.exit.code(),
        );
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
                .map(|(run, _, actual)| {
                    format!(
                        "what the fences pin is not what {run} ran:\n{}",
                        oracle::diff(&expected, actual)
                    )
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
