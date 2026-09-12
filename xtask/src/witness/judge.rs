//! How one witness is judged: the per-item judge `witness` contributes to
//! the tally. The snippet runs under the oracle, whose status is what
//! `--accept` captures onto a bare `exit`; then under the engine, its own
//! binary where one is built and the linked engine in this process where
//! none is, the oracle's run standing in only where neither can; and the two streams and
//! the status are held to what the fences pin through [`oracle::verdict`],
//! so the registry decides what a difference means (harness §4.3).

use anyhow::{Context, Result};

use super::fence::{Exit, Witness};
use super::{Ask, Captured, Judged, Judgement, fence};
use crate::divergence::Registry;
use crate::oracle::expect::Expectation;
use crate::oracle::{
    self, Invocation, Observation, Oracle, Pin, Program, Runner, Verdict as Compared, linked,
};

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

/// One witness, end to end: the oracle's run, the status `--accept` may
/// capture from it, the run of record, and the verdict. What `--accept`
/// captures goes onto `captured`; the write is the caller's.
pub(super) fn one(
    oracle: &Oracle<'_>,
    registry: &Registry,
    runner: &Runner,
    ask: &Ask,
    witness: &Witness,
    captured: &mut Vec<Captured>,
) -> Result<Judged> {
    let oracle_out = match oracle.run(Program::Snippet(&witness.snippet), None) {
        Ok(out) => out,
        Err(trouble) => {
            return Ok(judged(
                witness,
                Judgement::Bork,
                format!("the oracle did not run it: {trouble:#}"),
            ));
        }
    };
    let Some(status) = oracle_out.exit else {
        return Ok(judged(
            witness,
            Judgement::Bork,
            "the oracle ended on a signal, so there is no status to pin",
        ));
    };

    // The bare `exit` is the opt-in, and the only thing `--accept` writes:
    // a status the fence already pins and the oracle no longer gives is a
    // difference for the registry or for a fix, never one to repin over
    // (design §8, the paragraph on witnesses in rustdoc).
    let mut exit = witness.exit;
    if ask.accept && exit == Exit::Bare {
        captured.push(Captured {
            file: witness.file.clone(),
            line: witness.fence,
            was: witness.fence_line.clone(),
            now: fence::pinned(&witness.fence_line, status),
        });
        exit = Exit::Pinned(status);
    }
    if exit == Exit::Bare {
        return Ok(judged(
            witness,
            Judgement::Unpinned,
            format!(
                "the fence's `exit` is bare and the oracle exits {status}; `xtask witness --accept` writes it, and nothing else does"
            ),
        ));
    }

    let ran = match runner {
        Runner::Engine(binary) => oracle
            .run_under(&Invocation::new(binary, Program::Snippet(&witness.snippet)))
            .with_context(|| format!("running {} through {}", witness.at(), binary.display()))?,
        // The engine linked into the harness, which needs no binary and is
        // the run of record all the same (design §8; #119). A panic inside
        // is trouble the witness borks on, never the lane's end.
        Runner::Linked => match linked::serve(&oracle.pin().recipe, &witness.snippet) {
            Ok(out) => out,
            Err(trouble) => {
                return Ok(judged(
                    witness,
                    Judgement::Bork,
                    format!("the linked engine did not serve it: {trouble:#}"),
                ));
            }
        },
        _ => oracle_out,
    };
    let met = match met(oracle.pin(), witness, exit, &ran) {
        Ok(met) => met,
        Err(trouble) => {
            return Ok(judged(
                witness,
                Judgement::Bork,
                format!("the pinned stream is not one this engine can match: {trouble:#}"),
            ));
        }
    };
    let id = witness.id();
    let expected = shown(
        &witness.stdout,
        witness.stderr.as_deref().unwrap_or_default(),
        exit.asserted(),
    );
    let actual = shown(
        &String::from_utf8_lossy(&ran.stdout),
        &String::from_utf8_lossy(&ran.stderr),
        ran.exit,
    );
    Ok(
        match oracle::verdict(registry, &id, met, &expected, &actual) {
            Compared::Same => judged(witness, Judgement::Pass, ""),
            Compared::Diverged(row) => judged(witness, Judgement::Diverged, row.reason),
            Compared::Differs(difference) => judged(
                witness,
                Judgement::Fail,
                format!("what the fences pin is not what ran:\n{difference}"),
            ),
        },
    )
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
