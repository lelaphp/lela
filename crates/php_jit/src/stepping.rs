//! The stepping arm of `xtask bench`'s `step` bar (`jit-design.md`
//! §4.1 *What recording costs*, §10): what `step` costs per opline under
//! recording, measured as the recorder's loop without the record. Reached
//! only under `cfg(php_jit_stepping)` ([`crate::Jit`]'s `safepoint`), which
//! the bench's build of the engine alone sets, added to the workspace's
//! rustflags by a `--config` line, into a target directory of its own; no
//! ini and no `Policy` value reaches it.
//!
//! At the first safepoint the engine asks, [`driven`] takes the serving
//! over and runs it to its end one `tier::step` at a time, as
//! `record::run` steps a recording: before each opline it reads the
//! frame's op array through the view, samples the opline's three operands
//! as the recorder samples them ([`record::sampled`]) and keeps them in a
//! buffer it throws away, then steps and follows the frame `Stepped`
//! names. What it leaves out is the record and the recorder's stop
//! rules: it writes no event, counts no length and stops at nothing short
//! of the serving's end, and a step that throws hands the frame back to
//! the interpreter, which asks again at its next safepoint. Where the
//! serving ends it writes `stepped <n> oplines` to the
//! second stream, the count the bar divides each process's time and
//! instructions by; the first stream is the program's, which the bar holds
//! equal to the same program's under `execute::<Nop>` before it measures.

use std::hint::black_box;

use php_engine::tier::{self, EngineFuncRef, Flow, FrameRef, Request, Sample, Stepped, UnitView};

use crate::record;

/// How many oplines' samples the buffer keeps before it is thrown away.
const KEPT: usize = 4096;

/// The serving from `frame` stepped to its end, its oplines' operands
/// sampled and thrown away: [`Flow::Returned`] where it ended, the frame
/// the interpreter takes back where a step threw, and the fatal a step
/// returned.
pub(crate) fn driven(req: &mut Request, mut frame: FrameRef) -> Flow {
    let mut view = UnitView::new(req);
    let mut func = held(req, &mut view, frame);
    let mut kept: Vec<[Option<Sample>; 3]> = Vec::with_capacity(KEPT);
    let mut stepped: u64 = 0;
    let flow = loop {
        if let Some(code) = func.and_then(|f| view.op_array(f)) {
            let at = record::position(req, frame).0 as usize;
            if let Some(op) = code.oplines.get(at) {
                let (op1, op2, data) = record::sampled(req, frame, op, code.oplines.get(at + 1));
                if kept.len() == KEPT {
                    black_box(&kept);
                    kept.clear();
                }
                kept.push([op1, op2, data]);
            }
        }
        stepped += 1;
        match tier::step(req, frame) {
            Ok(Stepped::Same(next)) => frame = next,
            Ok(Stepped::Pushed(next) | Stepped::Popped(next) | Stepped::Moved(next)) => {
                frame = next;
                func = held(req, &mut view, frame);
            }
            Ok(Stepped::Threw(next)) => break Flow::Resume(next),
            Ok(Stepped::Done) => break Flow::Returned,
            Err(fatal) => break Flow::Bail(fatal),
        }
    };
    black_box(kept);
    req.response_mut()
        .write_stderr(format!("stepped {stepped} oplines\n").as_bytes());
    flow
}

/// The function `frame` runs, where the view holds it, as the recorder
/// holds a callee's op array as it steps into it.
fn held(req: &Request, view: &mut UnitView, frame: FrameRef) -> Option<EngineFuncRef> {
    tier::func(req, frame).and_then(|f| view.hold(req, f))
}

#[cfg(test)]
mod tests {
    use php_engine::tier::{Flow, FrameRef, Held, Request, Site, Tier};

    use crate::testing::serve;

    /// [`super::driven`] as the stepping build's `Jit` calls it, bound without
    /// the cfg: every safepoint it is asked at steps the serving to its
    /// end.
    struct Stepping;

    impl Tier for Stepping {
        type State = ();

        fn safepoint(req: &mut Request, frame: FrameRef, _: Site, _: Held<'_>) -> Flow {
            super::driven(req, frame)
        }
    }

    /// A program stepped to its end from its first safepoint, a call's
    /// entry, writes what it writes under `execute::<Nop>`, the loop `Jit`
    /// binds under its default `Mode::Off`, and the
    /// loop counts the oplines it stepped on the second stream: more
    /// than the loop's 200 iterations, each several oplines.
    #[test]
    fn a_stepped_serving_writes_what_the_interpreter_writes_and_counts_its_oplines() {
        let program = "function f($n) { $s = 0; for ($i = 0; $i < $n; $i++) { $s += $i * 2; } return $s; }\necho f(200), \"\\n\";";
        let stepped = serve::<Stepping>(&[], program);
        let nop = serve::<crate::Jit>(&[], program);
        assert_eq!(stepped.stdout, nop.stdout);
        assert_eq!(String::from_utf8_lossy(&stepped.stdout), "39800\n");
        let said = String::from_utf8_lossy(&stepped.stderr);
        let count = said
            .trim()
            .strip_prefix("stepped ")
            .and_then(|s| s.strip_suffix(" oplines"));
        let count: u64 = count.and_then(|n| n.parse().ok()).unwrap_or(0);
        assert!(count > 200, "{said:?}");
    }
}
