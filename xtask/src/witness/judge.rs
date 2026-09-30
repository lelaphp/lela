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

use std::fmt;

use super::anchor::Entry;
use super::fence::{Exit, Served, Witness};
use super::process::{self, Binary};
use super::{Ask, Captured, Judged, Judgement, fence};
use crate::divergence::Registry;
use crate::oracle::expect::Expectation;
use crate::oracle::{self, Observation, Oracle, Pin, Program, Regime, Verdict as Compared, linked};
use anyhow::Result;

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
    pub(super) entered: Vec<Entry>,
}

/// One witness, end to end: the oracle's run, the status `--accept` may
/// capture from it, the run of record, and the verdict. `regime` is the
/// block the fence's own `regime=<name>` names, resolved against the pin
/// by the caller once for both runs, so the oracle is passed the name and
/// the linked engine the ini it carries and neither is passed another's.
/// What the run leaves beside its verdict goes onto `kept`. Every way
/// this can go wrong is a judgement the witness carries, the oracle's
/// trouble and the linked engine's included, so there is nothing here for
/// an `Err` to say.
pub(super) fn one(
    oracle: &Oracle<'_>,
    registry: &Registry,
    ask: &Ask,
    witness: &Witness,
    regime: Option<&Regime>,
    binary: &Binary,
    kept: &mut Kept,
) -> Judged {
    let oracle_run = match &witness.request {
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
            process::served_as_cgi(oracle, &cgi.path, &witness.snippet, request, &[])
        }),
        None => oracle.run(
            Program::Snippet(&witness.snippet),
            witness.regime.as_deref(),
        ),
    };
    let oracle_out = match oracle_run {
        Ok(out) => out,
        Err(trouble) => {
            return judged(
                witness,
                Judgement::Bork,
                format!("the oracle did not run it: {trouble:#}"),
            );
        }
    };
    let Some(status) = oracle_out.exit else {
        return judged(
            witness,
            Judgement::Bork,
            "the oracle ended on a signal, so there is no status to pin",
        );
    };

    // The bare `exit` is the opt-in, and the only thing `--accept` writes:
    // a status the fence already pins and the oracle no longer gives is a
    // difference for the registry or for a fix, never one to repin over
    // (design §8, the paragraph on witnesses in rustdoc).
    let mut exit = witness.exit;
    if ask.accept && exit == Exit::Bare {
        kept.captured.push(Captured {
            file: witness.file.clone(),
            line: witness.fence,
            was: witness.fence_line.clone(),
            now: fence::pinned(&witness.fence_line, status),
        });
        exit = Exit::Pinned(status);
    }
    if exit == Exit::Bare {
        return judged(
            witness,
            Judgement::Unpinned,
            format!(
                "the fence's `exit` is bare and the oracle exits {status}; `xtask witness --accept` writes it, and nothing else does"
            ),
        );
    }

    let ran = match serve(oracle, binary, witness, regime) {
        Ok((out, natives)) => {
            kept.entered = natives;
            out
        }
        Err(trouble) => return judged(witness, Judgement::Bork, trouble),
    };
    held(
        oracle.pin(),
        registry,
        witness,
        exit,
        [(Run::Oracle, &oracle_out), (Run::Engine, &ran)],
    )
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
) -> Result<(Observation, Vec<Entry>), String> {
    match witness.served {
        Served::Linked => linked::serve_traced(&oracle.pin().recipe, regime, &witness.snippet)
            .map(|(out, natives)| (out, natives.into_iter().map(Entry::from).collect()))
            .map_err(|trouble| format!("the linked engine did not serve it: {trouble:#}")),
        Served::Binary => binary
            .serve(oracle, &witness.snippet, witness.regime.as_deref())
            .map_err(|trouble| format!("the php binary did not serve it: {trouble:#}")),
        Served::Cgi => binary
            .serve_cgi(
                oracle,
                &witness.snippet,
                witness.request.as_deref().unwrap_or_default(),
            )
            .map_err(|trouble| format!("the php-cgi binary did not serve it: {trouble:#}")),
        Served::Fpm => binary
            .serve_fpm(
                oracle,
                &witness.snippet,
                witness.request.as_deref().unwrap_or_default(),
            )
            .map_err(|trouble| format!("the php-fpm binary did not serve it: {trouble:#}")),
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
            observed.exit,
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
    if ran.exit != exit.asserted() {
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
fn normalised(pin: &Pin, text: &str) -> String {
    let bytes = oracle::normalise(&pin.recipe.normalise, text.as_bytes().to_vec());
    String::from_utf8_lossy(&bytes).into_owned()
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
