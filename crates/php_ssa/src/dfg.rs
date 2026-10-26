//! `zend_dfg.c`: which variables each block defines and uses before it
//! defines them, and which are live into and out of it, under
//! `ZEND_SSA_RC_INFERENCE` and `ZEND_SSA_USE_CV_RESULTS` as the compiled
//! tier builds it. A variable is its frame slot, `EX_VAR_TO_NUM`.

use php_unit::{OpArray, Opcode, OperandKind, Opline};

use crate::bits::Bits;
use crate::cfg::Cfg;

/// `zend_dfg`: a set per block, each over every variable of the frame.
pub(crate) struct Dfg {
    pub(crate) def: Vec<Bits>,
    pub(crate) uses: Vec<Bits>,
    pub(crate) live_in: Vec<Bits>,
}

/// Whether an operand of `kind` names a variable.
pub(crate) fn is_var(kind: OperandKind) -> bool {
    matches!(kind, OperandKind::Cv | OperandKind::Tmp | OperandKind::Var)
}

/// One opline's part of `_zend_dfg_add_use_def_op`: `next` is the opline
/// after it, which an assignment's `OP_DATA` is.
fn use_def(op: &Opline, next: Option<&Opline>, uses: &mut Bits, def: &mut Bits) {
    let read = |uses: &mut Bits, def: &Bits, var: u32| {
        if !def.has(var as usize) {
            uses.incl(var as usize);
        }
    };
    if is_var(op.op1_type) {
        read(uses, def, op.op1);
    }
    if (matches!(op.op2_type, OperandKind::Tmp | OperandKind::Var)
        && !matches!(op.opcode, Opcode::FeFetchR | Opcode::FeFetchRw))
        || op.op2_type == OperandKind::Cv
    {
        read(uses, def, op.op2);
    }
    if op.out_type == OperandKind::Cv && op.opcode != Opcode::Recv {
        read(uses, def, op.out);
    }
    let op1_cv = op.op1_type == OperandKind::Cv;
    let data = next.filter(|next| is_var(next.op1_type));
    let mut def_op1 = false;
    match op.opcode {
        // Under `ZEND_SSA_RC_INFERENCE` an `ASSIGN` defines its CV
        // source as an `ASSIGN_REF` does.
        Opcode::Assign | Opcode::AssignRef => {
            if op.op2_type == OperandKind::Cv {
                def.incl(op.op2 as usize);
            }
            def_op1 = op1_cv;
        }
        Opcode::AssignDim | Opcode::AssignObj | Opcode::AssignObjRef => {
            if let Some(next) = data {
                read(uses, def, next.op1);
                if next.op1_type == OperandKind::Cv {
                    def.incl(next.op1 as usize);
                }
            }
            def_op1 = op1_cv;
        }
        Opcode::AssignStaticProp | Opcode::AssignStaticPropRef => {
            if let Some(next) = data {
                read(uses, def, next.op1);
                if next.op1_type == OperandKind::Cv {
                    def.incl(next.op1 as usize);
                }
            }
        }
        Opcode::AssignStaticPropOp | Opcode::FramelessIcall3 => {
            if let Some(next) = data {
                read(uses, def, next.op1);
            }
        }
        Opcode::AssignDimOp | Opcode::AssignObjOp => {
            if let Some(next) = data {
                read(uses, def, next.op1);
            }
            def_op1 = op1_cv;
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
        // Under `ZEND_SSA_RC_INFERENCE` whatever the return's kind.
        | Opcode::Yield => def_op1 = op1_cv,
        Opcode::AddArrayUnpack => read(uses, def, op.out),
        Opcode::AddArrayElement => {
            read(uses, def, op.out);
            def_op1 = op1_cv;
        }
        Opcode::UnsetCv => def_op1 = true,
        Opcode::VerifyReturnType => def_op1 = is_var(op.op1_type),
        Opcode::FeFetchR | Opcode::FeFetchRw | Opcode::BindLexical => {
            def.incl(op.op2 as usize);
        }
        _ => {}
    }
    if def_op1 {
        def.incl(op.op1 as usize);
    }
    if is_var(op.out_type) {
        def.incl(op.out as usize);
    }
}

/// `zend_build_dfg`: `def` and `use` per reachable block, then `in` and
/// `out` to their fixed point, taking the last block of the worklist
/// first.
pub(crate) fn build(body: &OpArray, cfg: &Cfg) -> Dfg {
    let vars = (body.num_cvs + body.num_tmps) as usize;
    let count = cfg.blocks.len();
    let mut def = vec![Bits::new(vars); count];
    let mut uses = vec![Bits::new(vars); count];
    let mut live_in = vec![Bits::new(vars); count];
    let ops = &body.oplines;
    for (j, block) in cfg.blocks.iter().enumerate() {
        if !block.marks.reachable {
            continue;
        }
        let (Some(def), Some(uses)) = (def.get_mut(j), uses.get_mut(j)) else {
            continue;
        };
        for at in block.start as usize..block.start as usize + block.len as usize {
            let Some(op) = ops.get(at) else {
                continue;
            };
            if op.opcode != Opcode::OpData {
                use_def(op, ops.get(at + 1), uses, def);
            }
        }
    }

    let mut worklist = Bits::new(count);
    for j in 0..count {
        worklist.incl(j);
    }
    let mut tmp = Bits::new(vars);
    while let Some(j) = worklist.last() {
        worklist.excl(j);
        let Some(block) = cfg.blocks.get(j) else {
            continue;
        };
        if !block.marks.reachable {
            continue;
        }
        let mut out = Bits::new(vars);
        for succ in &block.successors {
            if let Some(live) = live_in.get(succ.index()) {
                out.union(live);
            }
        }
        let (Some(uses), Some(def)) = (uses.get(j), def.get(j)) else {
            continue;
        };
        tmp.set_union_with_difference(uses, &out, def);
        if let Some(live) = live_in.get_mut(j)
            && *live != tmp
        {
            live.clone_from(&tmp);
            for pred in &block.predecessors {
                worklist.incl(pred.index());
            }
        }
    }
    Dfg { def, uses, live_in }
}
