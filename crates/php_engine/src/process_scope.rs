//! The process scope (ADR-0010): each extension's process state, built
//! at most once per process and held here, never in the extension. php
//! runs one engine per process, so its `MINIT` is both the engine start
//! and the once per process; a process here may build several engines,
//! so the once is held here. What a process start built, or the
//! refusal it answered, is kept for the process: php meets a failed
//! `MINIT` once and ends the process (`Z/main/main.c:1505-1507`), so a
//! later engine is refused with the same refusal and the start is not
//! called again. Nothing here is ever released; process exit releases it,
//! as `xmlCleanupParser` and `curl_global_cleanup` are documented unsafe
//! while another thread may use the library.

use std::any::{Any, TypeId};
use std::sync::{Mutex, OnceLock, PoisonError};

use crate::api::abi::{Extension, Refusal};

/// One extension's process state, as an engine start reaches it.
pub(crate) type ProcessState = &'static (dyn Any + Send + Sync);

/// What one process start answered, kept for the process.
type Outcome = Result<ProcessState, Refusal>;

/// Each process state's once, by the state's type. The mutex is held
/// only to find or add a once, never across a start, so two engines
/// built at once on two threads wait on one start each and not on each
/// other's.
static HELD: Mutex<Vec<(TypeId, &'static OnceLock<Outcome>)>> = Mutex::new(Vec::new());

/// `extension`'s process state, started for the process the first time
/// it is asked for and the same one afterwards, from any engine on any
/// thread; `None` for an extension that declared none, and the kept
/// refusal where its start refused.
pub(crate) fn reach(extension: &Extension) -> Result<Option<ProcessState>, &'static Refusal> {
    let Some((key, build)) = &extension.process_state else {
        return Ok(None);
    };
    let once = once(*key);
    let outcome = once.get_or_init(|| build().map(|state| &*Box::leak(state)));
    outcome.as_ref().map(|&state| Some(state))
}

/// The once for the process state of type `key`, laid down the first
/// time it is asked for.
fn once(key: TypeId) -> &'static OnceLock<Outcome> {
    let mut held = HELD.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(&(_, once)) = held.iter().find(|(k, _)| *k == key) {
        return once;
    }
    let once: &'static OnceLock<Outcome> = Box::leak(Box::new(OnceLock::new()));
    held.push((key, once));
    once
}
