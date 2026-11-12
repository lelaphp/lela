//! `zend_ssa.c` and the two passes of `zend_inference.c` the compiled tier
//! takes before inference: e-SSA's pis on a conditional jump's edges, the
//! phis where definitions merge, the renaming down the dominator tree,
//! the use-def chains, the variables whose value nothing reads, and the
//! strongly connected components of the def-use graph, by Pearce.

use php_heap::ZvalType;
use php_macros::EngineState;
use php_unit::{OpArray, Opcode, OperandKind, Opline};

use crate::bits::Bits;
use crate::cfg::Cfg;
use crate::dfg::{self, Dfg, is_var};
use crate::types::{Range, Types, VarInfo};
use crate::{BlockId, Function, LinkedTable, PhiId, SsaVar, Var};

/// `zend_build_ssa` declines a function whose blocks times variables pass
/// this.
const MOST_SETS: usize = 4 * 1024 * 1024;

/// What a pi's range constraint knows of whether the bound holds,
/// `zend_ssa_negative_lat`; e-SSA construction sets the first two.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, EngineState)]
pub enum Negative {
    #[default]
    None,
    /// The edge is taken where the variable is not in the range.
    Init,
}

/// `zend_ssa_range_constraint`: the variable lies in `range`, each
/// bound a constant or, where a variable names it, that variable's value
/// plus the constant; a bound passed has none.
#[derive(Debug, Clone, Copy, PartialEq, Eq, EngineState)]
pub struct RangeConstraint {
    pub range: Range,
    pub min_var: Option<Var>,
    pub max_var: Option<Var>,
    /// The SSA variable `min_var` was on the edge, set by renaming.
    pub min_ssa_var: Option<SsaVar>,
    pub max_ssa_var: Option<SsaVar>,
    pub negative: Negative,
}

/// A pi's constraint, `zend_ssa_pi_constraint`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, EngineState)]
pub enum Constraint<C> {
    Range(RangeConstraint),
    /// `zend_ssa_type_constraint`: the value is of `mask`, an instance of
    /// `class` where one is named.
    Type {
        mask: Types,
        class: Option<C>,
    },
}

/// A phi or an e-SSA pi, which `zend_ssa_phi` tells apart by `pi`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, EngineState)]
pub enum PhiKind<C> {
    Phi,
    /// The variable on the edge from `from`, constrained by the jump.
    Pi {
        from: BlockId,
        constraint: Constraint<C>,
    },
}

/// `zend_ssa_phi`.
#[derive(Debug, Clone, PartialEq, EngineState)]
pub struct Phi<C> {
    pub kind: PhiKind<C>,
    /// The variable of the frame it merges.
    pub var: Var,
    pub ssa_var: SsaVar,
    pub block: BlockId,
    /// One per predecessor of the block, in their order; a pi's first is
    /// the one it reads.
    pub sources: Vec<SsaVar>,
    /// The next phi using each source, `use_chains`.
    pub use_chains: Vec<Option<PhiId>>,
    /// The next pi whose range bound reads the same SSA variable.
    pub sym_use_chain: Option<PhiId>,
}

/// `zend_ssa_op`: the SSA variables one opline reads and writes, and the
/// next opline reading each one it reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, EngineState)]
#[allow(clippy::struct_field_names)]
pub struct SsaOp {
    pub op1_use: Option<SsaVar>,
    pub op2_use: Option<SsaVar>,
    pub out_use: Option<SsaVar>,
    pub op1_def: Option<SsaVar>,
    pub op2_def: Option<SsaVar>,
    pub out_def: Option<SsaVar>,
    pub op1_use_chain: Option<u32>,
    pub op2_use_chain: Option<u32>,
    pub out_use_chain: Option<u32>,
}

/// How a CV's value may change behind the code's back, `zend_ssa_alias_kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, EngineState)]
pub enum Alias {
    #[default]
    None,
    /// Reachable by name through the symbol table.
    Symtable,
    /// `$http_response_header`, which an HTTP stream writes.
    HttpResponseHeader,
}

/// `zend_ssa_var`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, EngineState)]
pub struct SsaVarInfo {
    pub var: Var,
    /// Its strongly connected component, numbered in topological order;
    /// none for a variable whose value nothing reads.
    pub scc: Option<u32>,
    /// The opline that defines it; none for a CV's value on entry or a phi's.
    pub definition: Option<u32>,
    pub definition_phi: Option<PhiId>,
    /// The first opline reading it.
    pub use_chain: Option<u32>,
    pub phi_use_chain: Option<PhiId>,
    pub sym_use_chain: Option<PhiId>,
    /// Nothing reads its value: it is only overwritten, `no_val`.
    pub no_val: bool,
    pub scc_entry: bool,
    pub alias: Alias,
}

/// `zend_ssa` as the compiled tier builds it, with what inference learned.
#[derive(Debug, Clone, PartialEq, EngineState)]
pub struct Ssa<C> {
    pub phis: Vec<Phi<C>>,
    /// Each block's phis in php's order: its pis, the latest placed first,
    /// then its phis by variable.
    pub block_phis: Vec<Vec<PhiId>>,
    /// One per opline.
    pub ops: Vec<SsaOp>,
    pub vars: Vec<SsaVarInfo>,
    /// How many strongly connected components there are.
    pub sccs: u32,
    /// What inference learned of each SSA variable, `ssa->var_info`.
    pub info: Vec<VarInfo<C>>,
}

impl<C> Ssa<C> {
    pub fn phi(&self, id: PhiId) -> Option<&Phi<C>> {
        self.phis.get(id.index())
    }

    pub fn var(&self, id: SsaVar) -> Option<&SsaVarInfo> {
        self.vars.get(id.index())
    }

    /// `zend_ssa_next_use`: the opline after `at` that reads `var`.
    pub fn next_use(&self, var: SsaVar, at: u32) -> Option<u32> {
        let op = self.ops.get(at as usize)?;
        if op.op1_use == Some(var) {
            op.op1_use_chain
        } else if op.op2_use == Some(var) {
            op.op2_use_chain
        } else {
            op.out_use_chain
        }
    }

    /// `zend_ssa_next_use_phi`: the phi after `phi` that reads `var`.
    pub fn next_use_phi(&self, var: SsaVar, phi: PhiId) -> Option<PhiId> {
        let p = self.phi(phi)?;
        match p.kind {
            PhiKind::Pi { .. } => p.use_chains.first().copied().flatten(),
            PhiKind::Phi => {
                let j = p.sources.iter().position(|s| *s == var)?;
                p.use_chains.get(j).copied().flatten()
            }
        }
    }

    /// The oplines reading `var`, first to last.
    pub fn uses(&self, var: SsaVar) -> impl Iterator<Item = u32> + '_ {
        let first = self.var(var).and_then(|v| v.use_chain);
        std::iter::successors(first, move |at| self.next_use(var, *at))
    }

    /// The phis reading `var`.
    pub fn phi_uses(&self, var: SsaVar) -> impl Iterator<Item = PhiId> + '_ {
        let first = self.var(var).and_then(|v| v.phi_use_chain);
        std::iter::successors(first, move |phi| self.next_use_phi(var, *phi))
    }
}

/// A phi while the SSA is built: its SSA variable and sources are set as
/// renaming meets them.
struct Draft<C> {
    kind: PhiKind<C>,
    var: Var,
    ssa_var: Option<SsaVar>,
    sources: Vec<Option<SsaVar>>,
}

/// `zend_build_ssa`, `zend_ssa_compute_use_def_chains`,
/// `zend_ssa_find_false_dependencies` and `zend_ssa_find_sccs`, under
/// `ZEND_SSA_RC_INFERENCE | ZEND_SSA_USE_CV_RESULTS`.
pub(crate) fn build<T: LinkedTable>(
    function: &Function<'_>,
    cfg: &Cfg,
    table: &T,
) -> Option<Ssa<T::Class>> {
    let body = function.body;
    let vars = (body.num_cvs + body.num_tmps) as usize;
    if cfg.blocks.len().saturating_mul(vars) > MOST_SETS {
        return None;
    }
    let mut dfg = dfg::build(body, cfg);
    // The use sets are taken over as each block's phi set.
    for set in &mut dfg.uses {
        set.clear();
    }
    let mut builder = Builder {
        body,
        cfg,
        phis: Vec::new(),
        block_phis: vec![Vec::new(); cfg.blocks.len()],
        ops: vec![SsaOp::default(); body.oplines.len()],
        count: body.num_cvs,
    };
    builder.place_pis(&mut dfg, function, table);
    propagate(cfg, &mut dfg);
    builder.place_phis(&dfg);
    builder.rename(vars);
    let mut ssa = builder.finish()?;
    chains(&mut ssa, body);
    false_dependencies(&mut ssa, body);
    sccs(&mut ssa, body);
    Some(ssa)
}

/// SSA construction, step 1: merge points take the definitions that reach
/// them as phis; into an irreducible loop every live variable does.
fn propagate(cfg: &Cfg, dfg: &mut Dfg) {
    loop {
        let mut changed = false;
        for (j, block) in cfg.blocks.iter().enumerate() {
            if !block.marks.reachable || block.predecessors.len() <= 1 {
                continue;
            }
            let Some(mut phi) = dfg.uses.get(j).cloned() else {
                continue;
            };
            let Some(live) = dfg.live_in.get(j) else {
                continue;
            };
            if block.marks.irreducible {
                phi.union(live);
            } else {
                for &pred in &block.predecessors {
                    let mut at = Some(pred);
                    while let Some(i) = at
                        && Some(i) != block.idom
                    {
                        if let Some(def) = dfg.def.get(i.index()) {
                            phi.union_with_intersection(def, live);
                        }
                        at = cfg.block(i).and_then(|b| b.idom);
                    }
                }
            }
            if let Some(def) = dfg.def.get_mut(j)
                && !phi.is_subset(def)
            {
                def.union(&phi);
                changed = true;
            }
            if let Some(slot) = dfg.uses.get_mut(j) {
                *slot = phi;
            }
        }
        if !changed {
            break;
        }
    }
}

struct Builder<'a, C> {
    body: &'a OpArray,
    cfg: &'a Cfg,
    phis: Vec<Draft<C>>,
    block_phis: Vec<Vec<PhiId>>,
    ops: Vec<SsaOp>,
    /// The next SSA variable's number, `ssa_vars_count`.
    count: u32,
}

/// `pi_range`.
fn range(
    min_var: Option<Var>,
    max_var: Option<Var>,
    min: i64,
    max: i64,
    under: bool,
    over: bool,
    negative: Negative,
) -> RangeConstraint {
    RangeConstraint {
        range: Range {
            min,
            max,
            underflow: under,
            overflow: over,
        },
        min_var,
        max_var,
        min_ssa_var: None,
        max_ssa_var: None,
        negative,
    }
}

fn equals(var: Option<Var>, val: i64) -> RangeConstraint {
    range(var, var, val, val, false, false, Negative::None)
}

fn not_equals(var: Option<Var>, val: i64) -> RangeConstraint {
    range(var, var, val, val, false, false, Negative::Init)
}

fn at_least(var: Option<Var>, val: i64) -> RangeConstraint {
    range(var, None, val, i64::MAX, false, true, Negative::None)
}

fn at_most(var: Option<Var>, val: i64) -> RangeConstraint {
    range(None, var, i64::MIN, val, true, false, Negative::None)
}

/// `pi_type_mask`.
fn type_mask<C>(mask: Types) -> Constraint<C> {
    let mut mask = Types::REF | Types::RC | mask;
    if mask.has(Types::NULL) {
        mask |= Types::UNDEF;
    }
    Constraint::Type { mask, class: None }
}

/// `pi_not_type_mask`.
fn not_type_mask<C>(mask: Types) -> Constraint<C> {
    type_mask(!mask & (Types::ANY | Types::ARRAY_ANY))
}

/// `mask_for_type_check`.
fn mask_for_type_check(mask: Types) -> Types {
    if mask.has(Types::ARRAY) {
        mask | Types::ARRAY_ANY
    } else {
        mask
    }
}

/// `find_adjusted_tmp_var`: the CV the temporary `var` was computed from
/// by the last opline before `at` that wrote it, with the constant that
/// moves the comparison onto the CV, as `$a + 5 == 0` is `$a == 0 - 5`.
fn adjusted_tmp_var(body: &OpArray, at: usize, var: u32) -> Option<(Var, i64)> {
    let ops = body.oplines.get(..at)?;
    let op = ops
        .iter()
        .rev()
        .find(|op| op.out_type == OperandKind::Tmp && op.out == var)?;
    let long = |n: u32| body.literals.get(n as usize).and_then(|v| v.as_long());
    let cv = OperandKind::Cv;
    let konst = OperandKind::Const;
    match op.opcode {
        Opcode::PostDec if op.op1_type == cv => Some((Var(op.op1), -1)),
        Opcode::PostInc if op.op1_type == cv => Some((Var(op.op1), 1)),
        Opcode::Add if op.op1_type == cv && op.op2_type == konst => long(op.op2)
            .filter(|n| *n != i64::MIN)
            .map(|n| (Var(op.op1), -n)),
        Opcode::Add if op.op2_type == cv && op.op1_type == konst => long(op.op1)
            .filter(|n| *n != i64::MIN)
            .map(|n| (Var(op.op2), -n)),
        Opcode::Sub if op.op1_type == cv && op.op2_type == konst => {
            long(op.op2).map(|n| (Var(op.op1), n))
        }
        _ => None,
    }
}

impl<C: Copy + PartialEq> Builder<'_, C> {
    fn live_in(dfg: &Dfg, block: BlockId, var: Var) -> bool {
        dfg.live_in
            .get(block.index())
            .is_some_and(|set| set.has(var.index()))
    }

    /// `needs_pi`.
    fn needs_pi(&self, dfg: &Dfg, from: BlockId, to: BlockId, var: Var) -> bool {
        if !Self::live_in(dfg, to, var) {
            return false;
        }
        let Some(from_block) = self.cfg.block(from) else {
            return false;
        };
        let (Some(&s0), Some(&s1)) = (from_block.successors.first(), from_block.successors.get(1))
        else {
            return false;
        };
        if s0 == s1 {
            return false;
        }
        let Some(to_block) = self.cfg.block(to) else {
            return false;
        };
        if to_block.predecessors.len() == 1 {
            return true;
        }
        let other = if s0 == to { s1 } else { s0 };
        // `will_rejoin`: the original value reaches `to` again from the
        // other successor, so a pi would change nothing.
        let rejoins = to_block.predecessors.iter().any(|&pred| {
            pred != from
                && !dfg
                    .def
                    .get(pred.index())
                    .is_some_and(|def| def.has(var.index()))
                && self.cfg.dominates(other, pred)
        });
        !rejoins
    }

    /// `add_pi`: a pi of `var` on the edge `from` to `to`, placed first
    /// among `to`'s, where it is needed.
    fn add_pi(
        &mut self,
        dfg: &mut Dfg,
        from: BlockId,
        to: BlockId,
        var: Var,
        constraint: Constraint<C>,
    ) {
        if !self.needs_pi(dfg, from, to, var) {
            return;
        }
        let preds = self.cfg.block(to).map_or(0, |b| b.predecessors.len());
        let id = PhiId::of(self.phis.len());
        self.phis.push(Draft {
            kind: PhiKind::Pi { from, constraint },
            var,
            ssa_var: None,
            sources: vec![None; preds],
        });
        if let Some(list) = self.block_phis.get_mut(to.index()) {
            list.insert(0, id);
        }
        if let Some(def) = dfg.def.get_mut(to.index()) {
            def.incl(var.index());
        }
        if preds > 1
            && let Some(phi) = dfg.uses.get_mut(to.index())
        {
            phi.incl(var.index());
        }
    }

    /// `place_essa_pis`, under php's names for the two sides of a
    /// comparison.
    #[allow(clippy::similar_names)]
    fn place_pis<T: LinkedTable<Class = C>>(
        &mut self,
        dfg: &mut Dfg,
        function: &Function<'_>,
        table: &T,
    ) {
        let body = self.body;
        let cfg = self.cfg;
        let cv = OperandKind::Cv;
        let tmp = OperandKind::Tmp;
        for (j, block) in cfg.blocks.iter().enumerate() {
            if !block.marks.reachable || block.len == 0 {
                continue;
            }
            let j = BlockId::of(j);
            let at = block.start as usize + block.len as usize - 1;
            let Some(op) = body.oplines.get(at) else {
                continue;
            };
            let succ = |n: usize| block.successors.get(n).copied().unwrap_or(BlockId(0));
            let (bt, bf) = match op.opcode {
                Opcode::Jmpz => (succ(1), succ(0)),
                Opcode::Jmpnz => (succ(0), succ(1)),
                Opcode::Coalesce | Opcode::JmpNull => {
                    if op.op1_type == cv {
                        let to = succ(usize::from(op.opcode == Opcode::JmpNull));
                        self.add_pi(dfg, j, to, Var(op.op1), not_type_mask(Types::NULL));
                    }
                    continue;
                }
                _ => continue,
            };
            if block.len == 1 {
                continue;
            }
            let Some(prev) = body.oplines.get(at - 1) else {
                continue;
            };
            if op.op1_type != tmp || op.op1 != prev.out {
                continue;
            }
            let long = |n: u32| body.literals.get(n as usize).and_then(|v| v.as_long());
            match prev.opcode {
                Opcode::IsEqual
                | Opcode::IsNotEqual
                | Opcode::IsSmaller
                | Opcode::IsSmallerOrEqual => {
                    let (mut val1, mut val2) = (0i64, 0i64);
                    let mut var1 = match prev.op1_type {
                        OperandKind::Cv => Some(Var(prev.op1)),
                        OperandKind::Tmp => adjusted_tmp_var(body, at, prev.op1).map(|(v, n)| {
                            val2 = n;
                            v
                        }),
                        _ => None,
                    };
                    let mut var2 = match prev.op2_type {
                        OperandKind::Cv => Some(Var(prev.op2)),
                        OperandKind::Tmp => adjusted_tmp_var(body, at, prev.op2).map(|(v, n)| {
                            val1 = n;
                            v
                        }),
                        _ => None,
                    };
                    match (var1, var2) {
                        (Some(_), Some(_)) => {
                            if let (Some(a), Some(b)) =
                                (val1.checked_sub(val2), val2.checked_sub(val1))
                            {
                                val1 = a;
                                val2 = b;
                            } else {
                                var1 = None;
                                var2 = None;
                            }
                        }
                        (Some(_), None) => {
                            let add = (prev.op2_type == OperandKind::Const)
                                .then(|| long(prev.op2))
                                .flatten();
                            match add.and_then(|add| val2.checked_add(add)) {
                                Some(sum) => val2 = sum,
                                None => var1 = None,
                            }
                        }
                        (None, Some(_)) => {
                            let add = (prev.op1_type == OperandKind::Const)
                                .then(|| long(prev.op1))
                                .flatten();
                            match add.and_then(|add| val1.checked_add(add)) {
                                Some(sum) => val1 = sum,
                                None => var2 = None,
                            }
                        }
                        (None, None) => {}
                    }
                    if let Some(var1) = var1 {
                        match prev.opcode {
                            Opcode::IsEqual => {
                                self.add_pi(
                                    dfg,
                                    j,
                                    bt,
                                    var1,
                                    Constraint::Range(equals(var2, val2)),
                                );
                                self.add_pi(
                                    dfg,
                                    j,
                                    bf,
                                    var1,
                                    Constraint::Range(not_equals(var2, val2)),
                                );
                            }
                            Opcode::IsNotEqual => {
                                self.add_pi(
                                    dfg,
                                    j,
                                    bf,
                                    var1,
                                    Constraint::Range(equals(var2, val2)),
                                );
                                self.add_pi(
                                    dfg,
                                    j,
                                    bt,
                                    var1,
                                    Constraint::Range(not_equals(var2, val2)),
                                );
                            }
                            Opcode::IsSmaller => {
                                if val2 > i64::MIN {
                                    self.add_pi(
                                        dfg,
                                        j,
                                        bt,
                                        var1,
                                        Constraint::Range(at_most(var2, val2 - 1)),
                                    );
                                }
                                self.add_pi(
                                    dfg,
                                    j,
                                    bf,
                                    var1,
                                    Constraint::Range(at_least(var2, val2)),
                                );
                            }
                            _ => {
                                self.add_pi(
                                    dfg,
                                    j,
                                    bt,
                                    var1,
                                    Constraint::Range(at_most(var2, val2)),
                                );
                                if val2 < i64::MAX {
                                    self.add_pi(
                                        dfg,
                                        j,
                                        bf,
                                        var1,
                                        Constraint::Range(at_least(var2, val2 + 1)),
                                    );
                                }
                            }
                        }
                    }
                    if let Some(var2) = var2 {
                        match prev.opcode {
                            Opcode::IsEqual => {
                                self.add_pi(
                                    dfg,
                                    j,
                                    bt,
                                    var2,
                                    Constraint::Range(equals(var1, val1)),
                                );
                                self.add_pi(
                                    dfg,
                                    j,
                                    bf,
                                    var2,
                                    Constraint::Range(not_equals(var1, val1)),
                                );
                            }
                            Opcode::IsNotEqual => {
                                self.add_pi(
                                    dfg,
                                    j,
                                    bf,
                                    var2,
                                    Constraint::Range(equals(var1, val1)),
                                );
                                self.add_pi(
                                    dfg,
                                    j,
                                    bt,
                                    var2,
                                    Constraint::Range(not_equals(var1, val1)),
                                );
                            }
                            Opcode::IsSmaller => {
                                if val1 < i64::MAX {
                                    self.add_pi(
                                        dfg,
                                        j,
                                        bt,
                                        var2,
                                        Constraint::Range(at_least(var1, val1 + 1)),
                                    );
                                }
                                self.add_pi(
                                    dfg,
                                    j,
                                    bf,
                                    var2,
                                    Constraint::Range(at_most(var1, val1)),
                                );
                            }
                            _ => {
                                self.add_pi(
                                    dfg,
                                    j,
                                    bt,
                                    var2,
                                    Constraint::Range(at_least(var1, val1)),
                                );
                                if val1 > i64::MIN {
                                    self.add_pi(
                                        dfg,
                                        j,
                                        bf,
                                        var2,
                                        Constraint::Range(at_most(var1, val1 - 1)),
                                    );
                                }
                            }
                        }
                    }
                }
                Opcode::PostInc | Opcode::PostDec if prev.op1_type == cv => {
                    let var = Var(prev.op1);
                    let by = if prev.opcode == Opcode::PostDec {
                        -1
                    } else {
                        1
                    };
                    self.add_pi(dfg, j, bf, var, Constraint::Range(equals(None, by)));
                    self.add_pi(dfg, j, bt, var, Constraint::Range(not_equals(None, by)));
                }
                Opcode::PreInc | Opcode::PreDec if prev.op1_type == cv => {
                    let var = Var(prev.op1);
                    self.add_pi(dfg, j, bf, var, Constraint::Range(equals(None, 0)));
                    // Speculative.
                    self.add_pi(dfg, j, bt, var, Constraint::Range(not_equals(None, 0)));
                }
                Opcode::TypeCheck if prev.op1_type == cv => {
                    let var = Var(prev.op1);
                    let mask = Types(prev.extended_value);
                    self.add_pi(dfg, j, bt, var, type_mask(mask_for_type_check(mask)));
                    // `is_resource()` is false for a closed resource.
                    if mask != Types::RESOURCE {
                        self.add_pi(dfg, j, bf, var, not_type_mask(mask_for_type_check(mask)));
                    }
                }
                Opcode::IsIdentical | Opcode::IsNotIdentical => {
                    let konst = OperandKind::Const;
                    let (var, value) = if prev.op1_type == cv && prev.op2_type == konst {
                        (Var(prev.op1), body.literals.get(prev.op2 as usize))
                    } else if prev.op1_type == konst && prev.op2_type == cv {
                        (Var(prev.op2), body.literals.get(prev.op1 as usize))
                    } else {
                        continue;
                    };
                    // `=== null`, `true` or `false` rules a type out on the
                    // other edge; another constant is seldom of use.
                    let mask = match value.map(|v| v.kind()) {
                        Some(ZvalType::Null) => Types::NULL,
                        Some(ZvalType::True) => Types::TRUE,
                        Some(ZvalType::False) => Types::FALSE,
                        _ => continue,
                    };
                    let (is, not) = if prev.opcode == Opcode::IsIdentical {
                        (bt, bf)
                    } else {
                        (bf, bt)
                    };
                    self.add_pi(dfg, j, is, var, type_mask(mask));
                    self.add_pi(dfg, j, not, var, not_type_mask(mask));
                }
                Opcode::Instanceof
                    if prev.op1_type == cv && prev.op2_type == OperandKind::Const =>
                {
                    // The constant after the class name is its lower case.
                    let lcname = body
                        .literals
                        .get(prev.op2 as usize + 1)
                        .and_then(|v| v.as_str());
                    let Some(lcname) = lcname else {
                        continue;
                    };
                    let Some(class) = crate::class_entry(function, table, lcname.bytes()) else {
                        continue;
                    };
                    let constraint = Constraint::Type {
                        mask: Types::REF | Types::RC | Types::OBJECT,
                        class: Some(class),
                    };
                    self.add_pi(dfg, j, bt, Var(prev.op1), constraint);
                }
                _ => {}
            }
        }
    }

    /// SSA construction, step 2: a phi per variable of each block's phi
    /// set, placed after its pis, so they stand by variable.
    fn place_phis(&mut self, dfg: &Dfg) {
        for (j, block) in self.cfg.blocks.iter().enumerate() {
            if !block.marks.reachable {
                continue;
            }
            let Some(set) = dfg.uses.get(j) else {
                continue;
            };
            for var in set.rev_members() {
                let id = PhiId::of(self.phis.len());
                self.phis.push(Draft {
                    kind: PhiKind::Phi,
                    var: Var::of(var),
                    ssa_var: None,
                    sources: vec![None; block.predecessors.len()],
                });
                let pis = self.pis_before(j);
                if let Some(list) = self.block_phis.get_mut(j) {
                    list.insert(pis, id);
                }
            }
        }
    }

    /// How many pis lead block `j`'s phi list.
    fn pis_before(&self, j: usize) -> usize {
        self.block_phis.get(j).map_or(0, |list| {
            list.iter()
                .take_while(|id| {
                    self.phis
                        .get(id.index())
                        .is_some_and(|p| matches!(p.kind, PhiKind::Pi { .. }))
                })
                .count()
        })
    }

    fn fresh(&mut self) -> SsaVar {
        let var = SsaVar(self.count);
        self.count += 1;
        var
    }

    /// SSA construction, step 3: `zend_ssa_rename`, down the dominator
    /// tree from the entry, each child from its own copy of its parent's
    /// names, the children in ascending order.
    fn rename(&mut self, vars: usize) {
        let mut names: Vec<Option<SsaVar>> = vec![None; vars];
        for (j, name) in names
            .iter_mut()
            .enumerate()
            .take(self.body.num_cvs as usize)
        {
            *name = Some(SsaVar::of(j));
        }
        let mut stack = vec![(BlockId(0), names)];
        while let Some((n, mut names)) = stack.pop() {
            self.rename_in_block(n, &mut names);
            let children = self
                .cfg
                .block(n)
                .map(|b| b.children.as_slice())
                .unwrap_or_default();
            for &child in children.iter().rev() {
                stack.push((child, names.clone()));
            }
        }
    }

    /// `zend_ssa_rename_in_block`.
    fn rename_in_block(&mut self, n: BlockId, names: &mut [Option<SsaVar>]) {
        let cfg = self.cfg;
        let Some(block) = cfg.block(n) else {
            return;
        };
        let phis = self.block_phis.get(n.index()).cloned().unwrap_or_default();
        for id in phis {
            let ssa_var = if let Some(var) = self.phis.get(id.index()).and_then(|p| p.ssa_var) {
                var
            } else {
                let var = self.fresh();
                if let Some(p) = self.phis.get_mut(id.index()) {
                    p.ssa_var = Some(var);
                }
                var
            };
            if let Some(var) = self.phis.get(id.index()).map(|p| p.var)
                && let Some(name) = names.get_mut(var.index())
            {
                *name = Some(ssa_var);
            }
        }
        let start = block.start as usize;
        let end = start + block.len as usize;
        for k in start..end {
            if self
                .body
                .oplines
                .get(k)
                .is_some_and(|op| op.opcode != Opcode::OpData)
            {
                self.rename_op(k, names);
            }
        }
        // On the exit edge of an `FE_FETCH`, a phi takes the value from
        // before the fetch.
        let fe_fetch = (block.len != 0)
            .then(|| self.body.oplines.get(end - 1))
            .flatten()
            .filter(|op| {
                matches!(op.opcode, Opcode::FeFetchR | Opcode::FeFetchRw)
                    && op.op2_type == OperandKind::Cv
            })
            .and_then(|_| self.ops.get(end - 1).copied());
        let name_of =
            |names: &[Option<SsaVar>], var: Var| names.get(var.index()).copied().flatten();
        for (i, &succ) in block.successors.iter().enumerate() {
            let Some(to) = cfg.block(succ) else {
                continue;
            };
            let edge = to.predecessors.iter().position(|p| *p == n);
            let list = self
                .block_phis
                .get(succ.index())
                .cloned()
                .unwrap_or_default();
            for &id in &list {
                let fresh = self.phis.get(id.index()).is_some_and(|p| {
                    matches!(p.kind, PhiKind::Pi { from, .. } if from == n) && p.ssa_var.is_none()
                });
                let fresh = fresh.then(|| self.fresh());
                let Some(p) = self.phis.get_mut(id.index()) else {
                    continue;
                };
                let value = name_of(names, p.var);
                match &mut p.kind {
                    PhiKind::Pi { from, constraint } if *from == n => {
                        if let Constraint::Range(range) = constraint {
                            if let Some(min) = range.min_var {
                                range.min_ssa_var = name_of(names, min);
                            }
                            if let Some(max) = range.max_var {
                                range.max_ssa_var = name_of(names, max);
                            }
                        }
                        p.sources.fill(value);
                        if let Some(fresh) = fresh {
                            p.ssa_var = Some(fresh);
                        }
                    }
                    PhiKind::Phi => {
                        let Some(source) = edge.and_then(|j| p.sources.get_mut(j)) else {
                            continue;
                        };
                        *source = value;
                        if let Some(fe) = fe_fetch
                            && i == 0
                            && value.is_some()
                            && value == fe.op2_def
                        {
                            *source = fe.op2_use;
                        }
                    }
                    PhiKind::Pi { .. } => {}
                }
            }
            // A phi after a pi of this edge takes the pi's variable.
            for (at, &id) in list.iter().enumerate() {
                let Some(p) = self.phis.get(id.index()) else {
                    break;
                };
                let PhiKind::Pi { from, .. } = p.kind else {
                    break;
                };
                if from != n {
                    continue;
                }
                let (var, ssa_var) = (p.var, p.ssa_var);
                for &later in list.get(at + 1..).unwrap_or_default() {
                    if let Some(q) = self.phis.get_mut(later.index())
                        && matches!(q.kind, PhiKind::Phi)
                        && q.var == var
                        && let Some(source) = edge.and_then(|j| q.sources.get_mut(j))
                    {
                        *source = ssa_var;
                    }
                }
            }
        }
    }

    /// `_zend_ssa_rename_op` for the opline at `k`.
    fn rename_op(&mut self, k: usize, names: &mut [Option<SsaVar>]) {
        let Some(&op) = self.body.oplines.get(k) else {
            return;
        };
        let next = self.body.oplines.get(k + 1).copied();
        let name = |names: &[Option<SsaVar>], var: u32| names.get(var as usize).copied().flatten();
        let mut ssa_op = self.ops.get(k).copied().unwrap_or_default();
        let mut data = self.ops.get(k + 1).copied().unwrap_or_default();
        let mut data_written = false;
        if is_var(op.op1_type) {
            ssa_op.op1_use = name(names, op.op1);
        }
        if is_var(op.op2_type) {
            ssa_op.op2_use = name(names, op.op2);
        }
        if op.out_type == OperandKind::Cv && op.opcode != Opcode::Recv {
            ssa_op.out_use = name(names, op.out);
        }
        let cv = |kind: OperandKind| kind == OperandKind::Cv;
        let def = |this: &mut Self, names: &mut [Option<SsaVar>], var: u32| {
            let fresh = this.fresh();
            if let Some(name) = names.get_mut(var as usize) {
                *name = Some(fresh);
            }
            Some(fresh)
        };
        // The `OP_DATA` after an assignment: its value is read, and
        // written where it is a CV.
        let data_op =
            |this: &mut Self, names: &mut [Option<SsaVar>], data: &mut SsaOp, written: bool| {
                if let Some(next) = next.filter(|next| is_var(next.op1_type)) {
                    data.op1_use = name(names, next.op1);
                    if written && cv(next.op1_type) {
                        data.op1_def = def(this, names, next.op1);
                    }
                    true
                } else {
                    false
                }
            };
        let op1_cv = cv(op.op1_type);
        match op.opcode {
            Opcode::Assign | Opcode::AssignRef => {
                if cv(op.op2_type) {
                    ssa_op.op2_def = def(self, names, op.op2);
                }
                if op1_cv {
                    ssa_op.op1_def = def(self, names, op.op1);
                }
            }
            Opcode::AssignDim | Opcode::AssignObj => {
                data_written = data_op(self, names, &mut data, true);
                if op1_cv {
                    ssa_op.op1_def = def(self, names, op.op1);
                }
            }
            Opcode::AssignObjRef => {
                if op1_cv {
                    ssa_op.op1_def = def(self, names, op.op1);
                }
                data_written = data_op(self, names, &mut data, true);
            }
            Opcode::AssignStaticProp | Opcode::AssignStaticPropRef => {
                data_written = data_op(self, names, &mut data, true);
            }
            Opcode::AssignStaticPropOp => {
                data_written = data_op(self, names, &mut data, false);
            }
            Opcode::AssignDimOp | Opcode::AssignObjOp => {
                if op1_cv {
                    ssa_op.op1_def = def(self, names, op.op1);
                }
                data_written = data_op(self, names, &mut data, false);
            }
            Opcode::AssignOp
            | Opcode::PreInc
            | Opcode::PreDec
            | Opcode::PostInc
            | Opcode::PostDec
            | Opcode::BindGlobal
            | Opcode::BindStatic
            | Opcode::BindInitStaticOrJmp
            | Opcode::SendVarNoRef
            | Opcode::SendVarNoRefEx
            | Opcode::SendVarEx
            | Opcode::SendFuncArg
            | Opcode::SendRef
            | Opcode::SendUnpack
            | Opcode::FeResetRw
            | Opcode::MakeRef
            | Opcode::PreIncObj
            | Opcode::PreDecObj
            | Opcode::PostIncObj
            | Opcode::PostDecObj
            | Opcode::UnsetDim
            | Opcode::UnsetObj
            | Opcode::FetchDimW
            | Opcode::FetchDimRw
            | Opcode::FetchDimFuncArg
            | Opcode::FetchDimUnset
            | Opcode::FetchListW
            | Opcode::SendVar
            | Opcode::Cast
            | Opcode::QmAssign
            | Opcode::JmpSet
            | Opcode::Coalesce
            | Opcode::FeResetR
            | Opcode::InitArray
            | Opcode::Yield
                if op1_cv =>
            {
                ssa_op.op1_def = def(self, names, op.op1);
            }
            Opcode::AddArrayUnpack => ssa_op.out_use = name(names, op.out),
            Opcode::AddArrayElement => {
                ssa_op.out_use = name(names, op.out);
                if op1_cv {
                    ssa_op.op1_def = def(self, names, op.op1);
                }
            }
            Opcode::UnsetCv | Opcode::CopyTmp => ssa_op.op1_def = def(self, names, op.op1),
            Opcode::VerifyReturnType if is_var(op.op1_type) => {
                ssa_op.op1_def = def(self, names, op.op1);
            }
            Opcode::FeFetchR | Opcode::FeFetchRw => {
                if !cv(op.op2_type) {
                    ssa_op.op2_use = None;
                }
                ssa_op.op2_def = def(self, names, op.op2);
            }
            Opcode::BindLexical => ssa_op.op2_def = def(self, names, op.op2),
            Opcode::FramelessIcall1 | Opcode::FramelessIcall2 | Opcode::FramelessIcall3 => {
                if op1_cv {
                    ssa_op.op1_def = def(self, names, op.op1);
                }
                if cv(op.op2_type) {
                    ssa_op.op2_def = def(self, names, op.op2);
                }
                if op.opcode == Opcode::FramelessIcall3 {
                    data_written = data_op(self, names, &mut data, true);
                }
            }
            _ => {}
        }
        if is_var(op.out_type) {
            ssa_op.out_def = def(self, names, op.out);
        }
        if let Some(slot) = self.ops.get_mut(k) {
            *slot = ssa_op;
        }
        if data_written && let Some(slot) = self.ops.get_mut(k + 1) {
            *slot = data;
        }
    }

    /// The phis with every variable and source set, or none where renaming
    /// left one unset, which php asserts never happens.
    fn finish(self) -> Option<Ssa<C>> {
        let mut phis = Vec::with_capacity(self.phis.len());
        let mut block_of = vec![BlockId(0); self.phis.len()];
        for (j, list) in self.block_phis.iter().enumerate() {
            for id in list {
                if let Some(slot) = block_of.get_mut(id.index()) {
                    *slot = BlockId::of(j);
                }
            }
        }
        for (draft, block) in self.phis.into_iter().zip(block_of) {
            let sources: Option<Vec<SsaVar>> = draft.sources.into_iter().collect();
            let sources = sources?;
            phis.push(Phi {
                kind: draft.kind,
                var: draft.var,
                ssa_var: draft.ssa_var?,
                block,
                use_chains: vec![None; sources.len()],
                sources,
                sym_use_chain: None,
            });
        }
        let blank = SsaVarInfo {
            var: Var(0),
            scc: None,
            definition: None,
            definition_phi: None,
            use_chain: None,
            phi_use_chain: None,
            sym_use_chain: None,
            no_val: false,
            scc_entry: false,
            alias: Alias::None,
        };
        Some(Ssa {
            phis,
            block_phis: self.block_phis,
            ops: self.ops,
            vars: vec![blank; self.count as usize],
            sccs: 0,
            info: Vec::new(),
        })
    }
}

/// `zend_ssa_compute_use_def_chains`.
fn chains<C: Copy>(ssa: &mut Ssa<C>, body: &OpArray) {
    let cvs = body.num_cvs as usize;
    for (i, info) in ssa.vars.iter_mut().enumerate().take(cvs) {
        info.var = Var::of(i);
    }
    for i in (0..ssa.ops.len()).rev() {
        let Some(mut op) = ssa.ops.get(i).copied() else {
            continue;
        };
        let at = u32::try_from(i).unwrap_or(u32::MAX);
        let link = |vars: &mut [SsaVarInfo], var: SsaVar| {
            vars.get_mut(var.index())
                .and_then(|v| v.use_chain.replace(at))
        };
        if let Some(u) = op.op1_use {
            op.op1_use_chain = link(&mut ssa.vars, u);
        }
        if let Some(u) = op.op2_use
            && op.op2_use != op.op1_use
        {
            op.op2_use_chain = link(&mut ssa.vars, u);
        }
        if let Some(u) = op.out_use
            && op.out_use != op.op1_use
            && op.out_use != op.op2_use
        {
            op.out_use_chain = link(&mut ssa.vars, u);
        }
        if let Some(opline) = body.oplines.get(i) {
            for (def, var) in [
                (op.op1_def, opline.op1),
                (op.op2_def, opline.op2),
                (op.out_def, opline.out),
            ] {
                if let Some(def) = def
                    && let Some(info) = ssa.vars.get_mut(def.index())
                {
                    info.var = Var(var);
                    info.definition = Some(at);
                }
            }
        }
        if let Some(slot) = ssa.ops.get_mut(i) {
            *slot = op;
        }
    }
    for list in ssa.block_phis.clone() {
        for id in list {
            let Some(phi) = ssa.phis.get(id.index()) else {
                continue;
            };
            let (ssa_var, var, kind) = (phi.ssa_var, phi.var, phi.kind);
            if let Some(info) = ssa.vars.get_mut(ssa_var.index()) {
                info.var = var;
                info.definition_phi = Some(id);
            }
            let sources: Vec<SsaVar> = match kind {
                PhiKind::Pi { .. } => phi.sources.first().copied().into_iter().collect(),
                PhiKind::Phi => phi.sources.clone(),
            };
            for (j, source) in sources.into_iter().enumerate() {
                let already = ssa.phi_uses(source).any(|p| p == id);
                if !already {
                    let head = ssa.vars.get(source.index()).and_then(|v| v.phi_use_chain);
                    if let Some(phi) = ssa.phis.get_mut(id.index())
                        && let Some(slot) = phi.use_chains.get_mut(j)
                    {
                        *slot = head;
                    }
                    if let Some(info) = ssa.vars.get_mut(source.index()) {
                        info.phi_use_chain = Some(id);
                    }
                }
            }
            if let PhiKind::Pi {
                constraint: Constraint::Range(range),
                ..
            } = kind
                && let Some(bound) = range.min_ssa_var.or(range.max_ssa_var)
            {
                // The two bounds are never both a variable.
                let head = ssa.vars.get(bound.index()).and_then(|v| v.sym_use_chain);
                if let Some(phi) = ssa.phis.get_mut(id.index()) {
                    phi.sym_use_chain = head;
                }
                if let Some(info) = ssa.vars.get_mut(bound.index()) {
                    info.sym_use_chain = Some(id);
                }
            }
        }
    }
    for (i, info) in ssa.vars.iter_mut().enumerate().take(cvs) {
        if body
            .vars
            .get(i)
            .is_some_and(|name| &*name.0 == b"http_response_header")
        {
            info.alias = Alias::HttpResponseHeader;
        }
    }
    for i in cvs..ssa.vars.len() {
        let var = ssa.vars.get(i).map_or(Var(u32::MAX), |v| v.var);
        if var.index() < cvs {
            let alias = ssa.vars.get(var.index()).map_or(Alias::None, |v| v.alias);
            if let Some(info) = ssa.vars.get_mut(i) {
                info.alias = alias;
            }
        }
    }
}

/// `zend_ssa_is_no_val_use`: `op` reads `var` only to overwrite it.
#[must_use]
pub fn is_no_val_use(op: &Opline, ssa_op: &SsaOp, var: SsaVar) -> bool {
    let var = Some(var);
    match op.opcode {
        Opcode::Assign | Opcode::UnsetCv | Opcode::BindGlobal | Opcode::BindStatic => {
            ssa_op.op1_use == var && ssa_op.op2_use != var
        }
        Opcode::FeFetchR | Opcode::FeFetchRw => ssa_op.op2_use == var && ssa_op.op1_use != var,
        Opcode::AddArrayElement | Opcode::AddArrayUnpack => false,
        _ if ssa_op.out_use == var => ssa_op.op1_use != var && ssa_op.op2_use != var,
        _ => false,
    }
}

/// `zend_ssa_find_false_dependencies`: a variable is `no_val` until an
/// opline reads its value or a phi passes it to one that is read.
fn false_dependencies<C>(ssa: &mut Ssa<C>, body: &OpArray) {
    let count = ssa.vars.len();
    let mut worklist = Bits::new(count);
    for i in 0..count {
        let var = SsaVar::of(i);
        let read = ssa.uses(var).any(|at| {
            match (body.oplines.get(at as usize), ssa.ops.get(at as usize)) {
                (Some(op), Some(ssa_op)) => !is_no_val_use(op, ssa_op, var),
                _ => false,
            }
        });
        if let Some(info) = ssa.vars.get_mut(i) {
            info.no_val = !read;
        }
        if read {
            worklist.incl(i);
        }
    }
    while let Some(i) = worklist.first() {
        worklist.excl(i);
        let Some(phi) = ssa.vars.get(i).and_then(|v| v.definition_phi) else {
            continue;
        };
        let Some(phi) = ssa.phi(phi) else {
            continue;
        };
        let sources: Vec<SsaVar> = match phi.kind {
            PhiKind::Pi { .. } => phi.sources.first().copied().into_iter().collect(),
            PhiKind::Phi => phi.sources.clone(),
        };
        for source in sources {
            if let Some(info) = ssa.vars.get_mut(source.index())
                && info.no_val
            {
                info.no_val = false;
                worklist.incl(source.index());
            }
        }
    }
}

/// The SSA variables the oplines at and beside `at` define, as
/// `FOR_EACH_DEFINED_VAR` takes them: its own, then an `OP_DATA`'s
/// assignment's or an assignment's `OP_DATA`'s.
pub(crate) fn defined_at<C>(ssa: &Ssa<C>, body: &OpArray, at: usize) -> Vec<SsaVar> {
    let defs = |at: usize| {
        ssa.ops
            .get(at)
            .map(|op| [op.op1_def, op.op2_def, op.out_def])
            .unwrap_or_default()
    };
    let is_data = |at: usize| {
        body.oplines
            .get(at)
            .is_some_and(|op| op.opcode == Opcode::OpData)
    };
    let mut all: Vec<SsaVar> = defs(at).into_iter().flatten().collect();
    if is_data(at) {
        all.extend(
            at.checked_sub(1)
                .map(defs)
                .unwrap_or_default()
                .into_iter()
                .flatten(),
        );
    } else if at + 1 < body.oplines.len() && is_data(at + 1) {
        all.extend(defs(at + 1).into_iter().flatten());
    }
    all
}

/// The edges out of `var` in the def-use graph, in `zend_scc_next`'s
/// order: what each opline reading it defines, the phis reading it, and
/// the pis whose range bound reads it, those whose value is read alone.
fn edges<C>(ssa: &Ssa<C>, body: &OpArray, var: SsaVar) -> Vec<SsaVar> {
    let read = |v: &SsaVar| ssa.var(*v).is_some_and(|info| !info.no_val);
    let mut all: Vec<SsaVar> = ssa
        .uses(var)
        .flat_map(|at| defined_at(ssa, body, at as usize))
        .filter(read)
        .collect();
    all.extend(
        ssa.phi_uses(var)
            .filter_map(|id| ssa.phi(id).map(|p| p.ssa_var))
            .filter(read),
    );
    let first = ssa.var(var).and_then(|v| v.sym_use_chain);
    let sym = std::iter::successors(first, |id| ssa.phi(*id).and_then(|p| p.sym_use_chain));
    all.extend(
        sym.filter_map(|id| ssa.phi(id).map(|p| p.ssa_var))
            .filter(read),
    );
    all
}

/// One variable's place in Pearce's walk: its edges and the next to take,
/// and the last one taken.
#[derive(Default, Clone)]
struct Walk {
    edges: Vec<SsaVar>,
    next: usize,
    last: Option<SsaVar>,
}

/// `zend_ssa_find_sccs`, Pearce's iterative algorithm, then
/// `CHECK_SCC_ENTRY` over every use.
fn sccs<C>(ssa: &mut Ssa<C>, body: &OpArray) {
    let count = ssa.vars.len();
    let total = i64::try_from(count).unwrap_or(i64::MAX);
    let mut scc: Vec<i64> = vec![-1; count];
    let mut entry: Vec<bool> = vec![false; count];
    let mut walks: Vec<Walk> = vec![Walk::default(); count];
    let mut index = 0i64;
    let mut sccs = total;
    let mut stack: Vec<usize> = Vec::new();
    let get = |v: &[i64], i: usize| v.get(i).copied().unwrap_or(-1);
    for root in 0..count {
        if ssa.vars.get(root).is_none_or(|v| v.no_val) || get(&scc, root) >= 0 {
            continue;
        }
        let mut vstack: Vec<usize> = Vec::new();
        let mut begin = Some(root);
        loop {
            if let Some(var) = begin.take() {
                vstack.push(var);
                if let Some(walk) = walks.get_mut(var) {
                    *walk = Walk {
                        edges: edges(ssa, body, SsaVar::of(var)),
                        next: 0,
                        last: None,
                    };
                }
                if let Some(e) = entry.get_mut(var) {
                    *e = true;
                }
                if let Some(s) = scc.get_mut(var) {
                    *s = index;
                }
                index += 1;
            }
            let Some(&var) = vstack.last() else {
                break;
            };
            while let Some(walk) = walks.get_mut(var) {
                if let Some(last) = walk.last {
                    let (s2, s1) = (get(&scc, last.index()), get(&scc, var));
                    if s2 < s1 {
                        if let Some(s) = scc.get_mut(var) {
                            *s = s2;
                        }
                        if let Some(e) = entry.get_mut(var) {
                            *e = false;
                        }
                    }
                }
                let Some(walk) = walks.get_mut(var) else {
                    break;
                };
                let next = walk.edges.get(walk.next).copied();
                walk.next += 1;
                walk.last = next;
                let Some(next) = next else {
                    break;
                };
                if get(&scc, next.index()) < 0 {
                    begin = Some(next.index());
                    break;
                }
            }
            if begin.is_some() {
                continue;
            }
            vstack.pop();
            if entry.get(var).copied().unwrap_or(false) {
                sccs -= 1;
                while let Some(&other) = stack.last() {
                    if get(&scc, other) < get(&scc, var) {
                        break;
                    }
                    stack.pop();
                    if let Some(s) = scc.get_mut(other) {
                        *s = sccs;
                    }
                    index -= 1;
                }
                if let Some(s) = scc.get_mut(var) {
                    *s = sccs;
                }
                index -= 1;
            } else {
                stack.push(var);
            }
        }
    }
    if sccs != 0 {
        for s in &mut scc {
            if *s >= 0 {
                *s -= sccs;
            }
        }
    }
    ssa.sccs = u32::try_from(total - sccs).unwrap_or(0);
    for (info, (s, e)) in ssa.vars.iter_mut().zip(scc.iter().zip(&entry)) {
        info.scc = u32::try_from(*s).ok();
        info.scc_entry = *e;
    }
    for var in 0..count {
        let Some(own) = ssa.vars.get(var).and_then(|v| v.scc) else {
            continue;
        };
        let id = SsaVar::of(var);
        let mut reached: Vec<SsaVar> = ssa
            .uses(id)
            .flat_map(|at| defined_at(ssa, body, at as usize))
            .collect();
        reached.extend(
            ssa.phi_uses(id)
                .filter_map(|p| ssa.phi(p).map(|p| p.ssa_var)),
        );
        for other in reached {
            if let Some(info) = ssa.vars.get_mut(other.index())
                && info.scc != Some(own)
            {
                info.scc_entry = true;
            }
        }
    }
}
