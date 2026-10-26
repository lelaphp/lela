//! `zend_dump_op_array` under `ZEND_DUMP_HIDE_UNREACHABLE`,
//! `ZEND_DUMP_RC_INFERENCE` and `ZEND_DUMP_SSA`, as
//! `zend_jit_trace_build_ssa` prints an analysis with the message `JIT`:
//! the header, each reachable block with its phis, its oplines as the SSA
//! names their variables, and the `try`/`catch` table by block. What only
//! type inference knows prints as php prints it before inference: the
//! return's types empty and its range `0..0`, and no line per CV.

use std::io::Write as _;

use php_unit::decl::Name;
use php_unit::{InBlock, OpArray, OperandKind, OplineIdx, SsaOperands, SsaVars, dump_type_info};

use crate::cfg::Block;
use crate::ssa::{Constraint, Negative, PhiKind, RangeConstraint, Ssa};
use crate::{Analysis, Function, LinkedTable, SsaVar, Var};

/// `zend_dump_ssa_var`'s printing over one SSA, for the opline dump.
struct Names<'a, C> {
    body: &'a OpArray,
    ssa: &'a Ssa<C>,
}

impl<C> Names<'_, C> {
    /// `zend_dump_ssa_var`: `#3.CV0($n)`, `#7.T2`, ` NOVAL` after a
    /// variable whose value nothing reads.
    fn write(&self, out: &mut Vec<u8>, ssa_var: Option<SsaVar>, kind: OperandKind, var: u32) {
        match ssa_var {
            Some(n) => write!(out, "#{}.", n.0).ok(),
            None => write!(out, "#?.").ok(),
        };
        let kind = if var < self.body.num_cvs {
            OperandKind::Cv
        } else {
            kind
        };
        self.body.dump_var(out, kind, var);
        if let Some(info) = ssa_var.and_then(|n| self.ssa.var(n))
            && info.no_val
        {
            out.extend_from_slice(b" NOVAL");
        }
    }
}

impl<C> SsaVars for Names<'_, C> {
    fn ssa_var(&self, out: &mut Vec<u8>, ssa_var: u32, kind: OperandKind, var: u32) {
        self.write(out, Some(SsaVar(ssa_var)), kind, var);
    }
}

/// `zend_dump_block_info`, the header of reachable block `n`.
fn block_info(out: &mut Vec<u8>, n: usize, block: &Block) {
    if n > 0 {
        out.push(b'\n');
    }
    write!(out, "BB{n}:\n     ;").ok();
    let m = &block.marks;
    let words = [
        (m.start, " start"),
        (m.recv_entry, " recv"),
        (m.follow, " follow"),
        (m.target, " target"),
        (m.exit, " exit"),
        (m.entry || m.recv_entry, " entry"),
        (m.try_, " try"),
        (m.catch, " catch"),
        (m.finally, " finally"),
        (m.finally_end, " finally_end"),
        (m.unreachable_free, " unreachable_free"),
        (m.loop_header, " loop_header"),
        (m.irreducible, " irreducible"),
    ];
    for (on, word) in words {
        if on {
            out.extend_from_slice(word.as_bytes());
        }
    }
    if block.len == 0 {
        out.extend_from_slice(b" empty\n");
    } else {
        writeln!(
            out,
            " lines=[{}-{}]",
            block.start,
            block.start + block.len - 1
        )
        .ok();
    }
    let list = |out: &mut Vec<u8>, label: &str, ids: &[crate::BlockId]| {
        if ids.is_empty() {
            return;
        }
        write!(out, "     ; {label}=(").ok();
        for (i, id) in ids.iter().enumerate() {
            let sep = if i == 0 { "" } else { ", " };
            write!(out, "{sep}BB{}", id.0).ok();
        }
        out.extend_from_slice(b")\n");
    };
    list(out, "from", &block.predecessors);
    list(out, "to", &block.successors);
    if let Some(idom) = block.idom {
        writeln!(out, "     ; idom=BB{}", idom.0).ok();
    }
    if let Some(level) = block.level {
        writeln!(out, "     ; level={level}").ok();
    }
    if let Some(header) = block.loop_header {
        writeln!(out, "     ; loop_header={}", header.0).ok();
    }
    list(out, "children", &block.children);
}

/// `zend_dump_range_constraint`.
fn range<C>(out: &mut Vec<u8>, names: &Names<'_, C>, r: &RangeConstraint) {
    if r.underflow && r.overflow {
        return;
    }
    out.extend_from_slice(b" RANGE");
    if r.negative != Negative::None {
        out.push(b'~');
    }
    out.push(b'[');
    let bound = |out: &mut Vec<u8>, ssa_var: Option<SsaVar>, var: Option<Var>, n: i64| {
        let Some(ssa_var) = ssa_var else {
            write!(out, "{n}").ok();
            return;
        };
        let var = var.map_or(u32::MAX, |v| v.0);
        names.write(out, Some(ssa_var), OperandKind::Unused, var);
        if n > 0 {
            write!(out, " + {n}").ok();
        } else if n < 0 {
            write!(out, " - {}", n.unsigned_abs()).ok();
        }
    };
    if r.underflow {
        out.extend_from_slice(b"-- .. ");
    } else {
        bound(out, r.min_ssa_var, r.min_var, r.min);
        out.extend_from_slice(b" .. ");
    }
    if r.overflow {
        out.extend_from_slice(b"++]");
    } else {
        bound(out, r.max_ssa_var, r.max_var, r.max);
        out.push(b']');
    }
}

impl<C: Copy> Analysis<C> {
    /// The analysis as `zend_dump_op_array` prints it for
    /// `ZEND_JIT_DEBUG_SSA`, appended to `out`: `frameless` names the
    /// frameless functions an opline's `extended_value` indexes.
    pub fn dump<T: LinkedTable<Class = C>>(
        &self,
        out: &mut Vec<u8>,
        function: &Function<'_>,
        table: &T,
        frameless: &[Name],
    ) {
        let body = function.body;
        out.push(b'\n');
        match (function.name, function.scope) {
            (Some(name), Some(scope)) => {
                out.extend_from_slice(scope);
                out.extend_from_slice(b"::");
                out.extend_from_slice(name);
            }
            (Some(name), None) => out.extend_from_slice(name),
            (None, _) => out.extend_from_slice(b"$_main"),
        }
        let ssa_vars = self.ssa.as_ref().map_or(0, |ssa| ssa.vars.len());
        write!(
            out,
            ":\n     ; (lines={}, args={}, vars={}, tmps={}, ssa_vars={ssa_vars})\n     ; (JIT)\n     ; ",
            body.oplines.len(),
            function.num_args,
            body.num_cvs,
            body.num_tmps,
        )
        .ok();
        out.extend_from_slice(&body.filename.0);
        writeln!(out, ":{}-{}", body.line_start, body.line_end).ok();
        out.extend_from_slice(b"     ; return ");
        dump_type_info(out, 0, None, true);
        out.extend_from_slice(b" RANGE[0..0]\n");

        for (n, block) in self.cfg.blocks.iter().enumerate() {
            if !block.marks.reachable {
                continue;
            }
            block_info(out, n, block);
            if let Some(ssa) = &self.ssa {
                let names = Names { body, ssa };
                Self::phis(out, &names, n, table);
            }
            let successors: Vec<u32> = block.successors.iter().map(|b| b.0).collect();
            for at in block.start..block.start + block.len {
                let mut line = Vec::new();
                write!(line, "{at:04}").ok();
                let pad = 5usize.saturating_sub(line.len()).max(1);
                line.resize(line.len() + pad, b' ');
                out.extend_from_slice(&line);
                let names = self.ssa.as_ref().map(|ssa| Names { body, ssa });
                let ssa = names.as_ref().and_then(|names| {
                    let op = names.ssa.ops.get(at as usize)?;
                    let n = |v: Option<SsaVar>| v.map(|v| v.0);
                    let operands = SsaOperands {
                        op1_use: n(op.op1_use),
                        op2_use: n(op.op2_use),
                        out_use: n(op.out_use),
                        op1_def: n(op.op1_def),
                        op2_def: n(op.op2_def),
                        out_def: n(op.out_def),
                    };
                    Some((operands, names as &dyn SsaVars))
                });
                let block = InBlock {
                    successors: &successors,
                    ssa,
                };
                body.dump_op(out, OplineIdx(at), frameless, Some(block));
                out.push(b'\n');
            }
        }

        if !self.cfg.try_catch.is_empty() {
            out.extend_from_slice(b"EXCEPTION TABLE:\n");
            for tc in &self.cfg.try_catch {
                write!(out, "        BB{}", self.cfg.block_of(tc.try_op.0).0).ok();
                let ends = [tc.catch_op.0, tc.finally_op.0, tc.finally_end.0];
                for (i, at) in ends.into_iter().enumerate() {
                    let end = if i == 2 { "\n" } else { "" };
                    if at == 0 {
                        write!(out, ", -{end}").ok();
                    } else {
                        write!(out, ", BB{}{end}", self.cfg.block_of(at).0).ok();
                    }
                }
            }
        }
    }

    /// `zend_dump_block_header`'s phis of block `n`.
    fn phis<T: LinkedTable<Class = C>>(
        out: &mut Vec<u8>,
        names: &Names<'_, C>,
        n: usize,
        table: &T,
    ) {
        let ssa = names.ssa;
        for &id in ssa.block_phis.get(n).map(Vec::as_slice).unwrap_or_default() {
            let Some(p) = ssa.phi(id) else {
                continue;
            };
            out.extend_from_slice(b"     ");
            names.write(out, Some(p.ssa_var), OperandKind::Unused, p.var.0);
            match &p.kind {
                PhiKind::Phi => {
                    out.extend_from_slice(b" = Phi(");
                    for (j, source) in p.sources.iter().enumerate() {
                        if j > 0 {
                            out.extend_from_slice(b", ");
                        }
                        names.write(out, Some(*source), OperandKind::Unused, p.var.0);
                    }
                    out.extend_from_slice(b")\n");
                }
                PhiKind::Pi { from, constraint } => {
                    write!(out, " = Pi<BB{}>(", from.0).ok();
                    names.write(
                        out,
                        p.sources.first().copied(),
                        OperandKind::Unused,
                        p.var.0,
                    );
                    out.extend_from_slice(b" &");
                    match constraint {
                        Constraint::Range(r) => range(out, names, r),
                        Constraint::Type { mask, class } => {
                            out.extend_from_slice(b" TYPE");
                            let class = class.map(|class| (table.name(class), true));
                            dump_type_info(out, *mask, class, true);
                        }
                    }
                    out.extend_from_slice(b")\n");
                }
            }
        }
    }
}
