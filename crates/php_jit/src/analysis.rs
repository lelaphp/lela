//! php's function-level analysis of the op arrays a trace passes through
//! (ADR-0015, `jit-design.md` §4.6): `php_ssa`'s CFG, DFG and SSA, asked
//! for where `zend_jit_trace_build_tssa` builds them, for the op arrays
//! it builds them for, built once per op array of the epoch and kept in
//! its table, and written under `ZEND_JIT_DEBUG_SSA` at each trace's
//! build, as php writes them each time it builds them anew.
//!
//! Nothing reads the analysis yet; inference over it is #3176's next
//! slice.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use php_engine::tier::{ClassId, FuncRef, UnitView};
use php_ssa::{Analysis, Function, LinkedTable};

use crate::Lines;
use crate::policy::JitDebug;
use crate::record::{Event, TraceRecord};

/// The analysis of one op array, none where php's fails.
type Kept = Option<Arc<Analysis<ClassId>>>;

/// An epoch's analyses by op array, each written once
/// (`jit-design.md` §4.6).
#[derive(Debug, Default)]
pub(crate) struct Analyses(Mutex<HashMap<FuncRef, Kept>>);

impl Analyses {
    /// The analysis of `f`, built by `build` where none was kept, and the
    /// first one kept where two builds race.
    fn get_or_build(&self, f: FuncRef, build: impl FnOnce() -> Kept) -> Kept {
        let kept = self
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&f)
            .cloned();
        if let Some(kept) = kept {
            return kept;
        }
        let made = build();
        let mut map = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        map.entry(f).or_insert(made).clone()
    }
}

/// The names an op array's code binds to classes, as the analysis asks
/// after them for an `INSTANCEOF`. php answers from its linked table
/// (`zend_optimizer_get_class_entry`); the view keeps no class by its
/// name, so none binds here, and no pi of an `INSTANCEOF` is placed.
struct Unbound;

impl LinkedTable for Unbound {
    type Class = ClassId;

    fn class(&self, _lcname: &[u8]) -> Option<ClassId> {
        None
    }

    fn scope(&self) -> Option<ClassId> {
        None
    }

    fn name(&self, _class: ClassId) -> &[u8] {
        b""
    }
}

/// The op array `f` names and what php reads beside it, from `view`.
fn function(view: &UnitView, f: FuncRef) -> Option<Function<'_>> {
    let body = view.op_array(f)?;
    let declared = view.declaration(f)?;
    let decl = declared.function;
    let num_args = decl.map_or(0, |decl| {
        decl.parameters.iter().filter(|p| !p.variadic).count()
    });
    Some(Function {
        body,
        name: decl.map(|decl| &*decl.name.0),
        scope: declared.scope.map(|name| &*name.0),
        num_args: u32::try_from(num_args).unwrap_or(u32::MAX),
        generator: decl.is_some_and(|decl| decl.is_generator),
    })
}

/// The op arrays `zend_jit_trace_build_tssa` builds an analysis of for
/// `record`, in its order: the one the trace starts in, each one a call
/// enters, and the one a return past the trace's first frame leaves,
/// each once.
fn passed(record: &TraceRecord) -> Vec<FuncRef> {
    let mut order = vec![record.root];
    let mut build = |f: FuncRef| {
        if !order.contains(&f) {
            order.push(f);
        }
    };
    let (mut at, mut level) = (record.root, 0u32);
    for event in &record.events {
        match *event {
            Event::Op { func, .. } => at = func,
            Event::Enter { func } => {
                at = func;
                level += 1;
                build(func);
            }
            Event::Back if level == 0 => build(at),
            Event::Back => level -= 1,
            Event::InitCall { .. } => {}
        }
    }
    order
}

/// The analyses of the op arrays `record` passes through, built where
/// `analyses` keeps none, each written to `lines` where they ask for
/// `ZEND_JIT_DEBUG_SSA`.
pub(crate) fn build(record: &TraceRecord, view: &UnitView, analyses: &Analyses, lines: &mut Lines) {
    let dump = lines.debug().any(JitDebug::SSA);
    for f in passed(record) {
        let Some(function) = function(view, f) else {
            continue;
        };
        let kept = analyses.get_or_build(f, || php_ssa::analyse(&function, &Unbound).map(Arc::new));
        if dump && let Some(analysis) = kept {
            let mut written = Vec::new();
            analysis.dump(&mut written, &function, &Unbound, view.frameless_names());
            lines.push(&written);
        }
    }
}

#[cfg(test)]
mod tests;
