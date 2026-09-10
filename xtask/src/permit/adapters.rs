//! The two adapters over one judge and what each records: the command
//! hook, and the function hook above it (harness-design.md §4.4). `run` is
//! the command both are over: the call on stdin, the hook's JSON on stdout,
//! the findings as readings, and the refusal when this build cannot judge.

use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use super::call::Call;
use super::rules::{Judgement, Kind, deny_output, judge};
use super::{INSTRUMENT, STALE_RULE};
use crate::built;
use crate::manifest::{self, Manifest};
use crate::reading::{Reading, Status, Store};

/// Which adapter judges the call: the command hook records every
/// finding; the function hook above it records a denial, which ends the
/// call there, and leaves a note to the command hook beneath, which sees
/// the same call and records it once (harness-design.md §4.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Adapter {
    CommandHook,
    AboveTheCommandHook,
}

impl Adapter {
    fn records(self, kind: Kind) -> bool {
        match self {
            Adapter::CommandHook => true,
            Adapter::AboveTheCommandHook => kind == Kind::Deny,
        }
    }
}

/// Record one reading per finding the adapter records: scope and rule are
/// the rule id, a denial fails and a note passes, the stamp is over the
/// file the call touches. The outcome is dropped: the decision is in the
/// hook's JSON and the exit is 0 whatever it was (harness-design.md §4.4).
pub(super) fn record(
    root: &Path,
    call: &Call,
    judgement: &Judgement,
    adapter: Adapter,
) -> Result<()> {
    let touched = call.path(root).unwrap_or_else(|| PathBuf::from("."));
    let mut readings: Vec<Reading> = Vec::new();
    for finding in judgement
        .findings
        .iter()
        .filter(|f| adapter.records(f.kind))
    {
        // One call is one moment and one tree: the first finding's stamp
        // is every finding's, and git is spawned once per call.
        let mut reading = match readings.first() {
            Some(first) => {
                let mut reading = first.clone();
                reading.scope.clone_from(&finding.rule);
                reading
            }
            None => Reading::stamped_over(INSTRUMENT, &finding.rule, root, &touched)?,
        };
        reading.rule.clone_from(&finding.rule);
        reading.status = match finding.kind {
            Kind::Deny => Status::Fail,
            Kind::Note => Status::Pass,
        };
        reading.message = format!(
            "{} {}: {}",
            call.tool_name,
            call.subject(root),
            finding.text
        );
        reading.adr = finding.adr.first().cloned().unwrap_or_default();
        reading.session.clone_from(&call.session_id);
        readings.push(reading);
    }
    Store::of_workspace(root).record(&readings)?;
    Ok(())
}

/// The command: judge the call on stdin (or in `--json`), print the hook's
/// JSON, record the findings the adapter records. A call this cannot read
/// is a deny that says so; a manifest it cannot judge by is the refusal,
/// the one error returned, since that is the signal each adapter walks its
/// next candidate on (§4.4, #16). The manifest is read here and not by the
/// caller for that reason: a build that cannot read it must record the
/// refusal before it exits.
pub fn run(root: &Path, json: Option<&str>, adapter: Adapter) -> Result<()> {
    let text = json.map_or_else(
        || {
            let mut bytes = Vec::new();
            std::io::stdin()
                .read_to_end(&mut bytes)
                .context("reading the tool call from stdin")
                .map(|_| String::from_utf8_lossy(&bytes).into_owned())
        },
        |json| Ok(json.to_owned()),
    );
    let (root, call) = match text.and_then(|text| Call::parse(&text)) {
        Ok(call) => (call.workspace(root), call),
        Err(err) => {
            println!(
                "{}",
                deny_output(&format!("permit could not judge this call: {err:#}"))
            );
            return Ok(());
        }
    };
    // What this build judges by: the manifest in front of it, the sources
    // it was built from, and the manifest readable as rules. Any of the
    // three failing is a refusal to decide, and a refusal decides nothing
    // whichever way the decision would have fallen.
    let manifest = built::judges(&root, &crate::built_root()).and_then(|()| Manifest::load(&root));
    let manifest = match manifest {
        Ok(manifest) => manifest,
        Err(err) => {
            record_the_refusal(&root, &call, &err);
            return Err(err);
        }
    };
    match judge(&manifest, &root, &call) {
        Ok(judgement) => {
            if let Some(output) = judgement.hook_output() {
                println!("{output}");
            }
            if let Err(err) = record(&root, &call, &judgement, adapter) {
                eprintln!("xtask permit: the decision stands but was not recorded: {err:#}");
            }
        }
        Err(err) => {
            println!(
                "{}",
                deny_output(&format!("permit could not judge this call: {err:#}"))
            );
        }
    }
    Ok(())
}

/// The refusal as a reading, so the store says the judge was down for this
/// call and this session: through the outage of #16 a build that could not
/// start recorded nothing, and `status --section permits` said the judge
/// was up. Best effort, since the refusal stands whether or not the store
/// takes it.
fn record_the_refusal(root: &Path, call: &Call, err: &anyhow::Error) {
    let recorded = Reading::stamped_over(INSTRUMENT, STALE_RULE, root, Path::new(manifest::FILE))
        .and_then(|mut reading| {
            STALE_RULE.clone_into(&mut reading.rule);
            reading.status = Status::Fail;
            reading.message = format!(
                "{} {}: this call was not judged. {err:#}",
                call.tool_name,
                call.subject(root)
            );
            reading.session.clone_from(&call.session_id);
            Store::of_workspace(root).record(&[reading])
        });
    // The refusal is the exit already; the outcome of recording it is not.
    if let Err(err) = recorded {
        eprintln!("xtask permit: the refusal to judge was not recorded: {err:#}");
    }
}
