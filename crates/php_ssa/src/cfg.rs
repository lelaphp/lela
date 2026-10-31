//! `zend_cfg.c` under `zend_jit_build_cfg`'s marks: the basic blocks of an
//! op array with `ZEND_CFG_STACKLESS`, `ZEND_CFG_RECV_ENTRY` and
//! `ZEND_CFG_NO_ENTRY_PREDECESSORS`, which blocks are reachable, their
//! predecessors, the dominator tree of Cooper, Harvey and Kennedy, and the
//! loops Sreedhar's DJ graphs identify.

use php_unit::{OpArray, Opcode, OperandKind, Opline, TryCatch};

use crate::{BlockId, Features, Marks};

/// `zend_jit_build_cfg` declines a function of more blocks than this.
const MOST_BLOCKS: usize = 100_000;

/// `ZEND_THROW_IS_EXPR` in a `THROW`'s or `MATCH_ERROR`'s `extended_value`.
const THROW_IS_EXPR: u32 = 1;
/// `ZEND_LAST_CATCH` in a `CATCH`'s `extended_value`.
const LAST_CATCH: u32 = 1;
/// `ZEND_FREE_ON_RETURN` in an `FE_FREE`'s `extended_value`.
const FREE_ON_RETURN: u32 = 1;
/// `ZEND_FREE_SWITCH` in a `FREE`'s `extended_value`.
const FREE_SWITCH: u32 = 2;
/// `ZEND_FETCH_GLOBAL`, `ZEND_FETCH_LOCAL` and `ZEND_FETCH_GLOBAL_LOCK`
/// in a variable fetch's `extended_value`.
const FETCH_GLOBAL: u32 = 2;
const FETCH_LOCAL: u32 = 4;
const FETCH_GLOBAL_LOCK: u32 = 8;

/// One basic block, `zend_basic_block`.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Block {
    pub marks: Marks,
    /// The block's first opline.
    pub start: u32,
    /// How many oplines it holds; none for the extra entry block.
    pub len: u32,
    /// In php's order (`zend_cfg.h`'s table); none for a block that is
    /// not reachable, as `zend_cfg_build_predecessors` clears them.
    pub successors: Vec<BlockId>,
    /// Each reachable block that ends in an edge to this one, once.
    pub predecessors: Vec<BlockId>,
    pub idom: Option<BlockId>,
    /// The closest loop header this block is in.
    pub loop_header: Option<BlockId>,
    /// Steps away from the entry in the dominator tree; none for a block
    /// that is not reachable.
    pub level: Option<u32>,
    /// The blocks this one immediately dominates, ascending.
    pub children: Vec<BlockId>,
}

impl Block {
    /// The opline index one past the block's last.
    fn end(&self) -> usize {
        self.start as usize + self.len as usize
    }
}

/// `zend_cfg`: the blocks of one op array.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Cfg {
    pub blocks: Vec<Block>,
    /// The block each opline stands in, `cfg->map`.
    pub map: Vec<BlockId>,
    pub features: Features,
    /// The op array's `try`/`catch` table as reachability leaves it:
    /// `zend_mark_reachable_blocks` moves a `try_op` past an unreachable
    /// start, in place in php's op array and here in the CFG's copy.
    pub try_catch: Vec<TryCatch>,
}

impl Cfg {
    pub(crate) fn block(&self, id: BlockId) -> Option<&Block> {
        self.blocks.get(id.index())
    }

    /// The block the opline at `at` stands in.
    pub(crate) fn block_of(&self, at: u32) -> BlockId {
        self.map.get(at as usize).copied().unwrap_or(BlockId(0))
    }

    /// `dominates(blocks, a, b)`: whether `a` dominates `b`, both reachable.
    #[must_use]
    pub fn dominates(&self, a: BlockId, mut b: BlockId) -> bool {
        let level = |id: BlockId| self.block(id).and_then(|block| block.level);
        while let (Some(lb), Some(la)) = (level(b), level(a))
            && lb > la
        {
            match self.block(b).and_then(|block| block.idom) {
                Some(idom) => b = idom,
                None => break,
            }
        }
        a == b
    }
}

/// `zend_jit_build_cfg`: the CFG, its predecessors, its dominator tree and
/// its loops, or none for a function of more than 100,000 blocks.
pub(crate) fn build(body: &OpArray, named: bool) -> Option<Cfg> {
    let mut cfg = split(body, named);
    if cfg.blocks.len() > MOST_BLOCKS {
        return None;
    }
    predecessors(&mut cfg);
    dominators(&mut cfg);
    loops(&mut cfg);
    Some(cfg)
}

/// The opline a `JMP_ADDR` operand of `op` names, read from the field the
/// opcode keeps it in.
fn target(op: &Opline) -> u32 {
    op.jump_target().map_or(0, |at| at.0)
}

/// The targets of a `SWITCH_LONG`'s, a `SWITCH_STRING`'s or a `MATCH`'s
/// jump table, in the table's order.
fn jump_table(body: &OpArray, op: &Opline) -> Vec<u32> {
    let Some(table) = body
        .literals
        .get(op.op2 as usize)
        .and_then(|value| value.as_arr())
    else {
        return Vec::new();
    };
    (&table)
        .into_iter()
        .map(|(_, at)| u32::try_from(at.as_long().unwrap_or(0)).unwrap_or(0))
        .collect()
}

/// `zend_optimizer_is_loop_var_free`.
fn is_loop_var_free(op: &Opline) -> bool {
    (op.opcode == Opcode::FeFree && op.extended_value != FREE_ON_RETURN)
        || (op.opcode == Opcode::Free && op.extended_value == FREE_SWITCH)
}

/// `zend_optimizer_classify_function` for a call to a function of the
/// engine's own, named in lower case: one that reads or writes the
/// caller's variables by name, or reads its arguments.
fn classify(name: &[u8]) -> Features {
    match name {
        b"extract" | b"compact" | b"get_defined_vars" => Features {
            indirect_var_access: true,
            ..Features::default()
        },
        b"func_num_args" | b"func_get_arg" | b"func_get_args" => Features {
            vararg: true,
            ..Features::default()
        },
        _ => Features::default(),
    }
}

/// Sets one of a block's marks.
type Mark = fn(&mut Marks);

/// `zend_build_cfg`, steps 1 to 4: where blocks start, the blocks, their
/// successors and which are reachable. `named` is whether the op array
/// has a `function_name`, which a global fetch tests.
fn split(body: &OpArray, named: bool) -> Cfg {
    let ops = &body.oplines;
    let last = ops.len();
    let mut starts = vec![0u32; last.max(1)];
    let mut features = Features::default();
    let mut start = |at: usize| {
        if let Some(n) = starts.get_mut(at) {
            *n += 1;
        }
    };
    start(0);
    for (i, op) in ops.iter().enumerate() {
        let more = i + 1 < last;
        match op.opcode {
            Opcode::Return
            | Opcode::ReturnByRef
            | Opcode::GeneratorReturn
            | Opcode::VerifyNeverType
            | Opcode::FastRet
                if more =>
            {
                start(i + 1);
            }
            // A `THROW` in an expression ends no block, so no live range
            // is lost where unreachable code is dropped.
            Opcode::MatchError | Opcode::Throw if op.extended_value != THROW_IS_EXPR && more => {
                start(i + 1);
            }
            Opcode::IncludeOrEval => {
                features.indirect_var_access = true;
                start(i + 1);
            }
            Opcode::Recv
            | Opcode::RecvInit
            | Opcode::GeneratorCreate
            | Opcode::Yield
            | Opcode::YieldFrom => start(i + 1),
            Opcode::DoFcall | Opcode::DoUcall | Opcode::DoFcallByName => {
                features.has_calls = true;
                start(i + 1);
            }
            Opcode::DoIcall => features.has_calls = true,
            Opcode::InitFcall | Opcode::InitNsFcallByName => {
                // The third constant of an `INIT_NS_FCALL_BY_NAME` is the
                // unqualified name in lower case.
                let at = op.op2 as usize + if op.opcode == Opcode::InitFcall { 0 } else { 2 };
                if let Some(name) = body.literals.get(at).and_then(|value| value.as_str()) {
                    features.join(classify(name.bytes()));
                }
            }
            Opcode::FastCall | Opcode::Jmp => {
                start(target(op) as usize);
                if op.opcode == Opcode::FastCall || more {
                    start(i + 1);
                }
            }
            Opcode::Jmpz
            | Opcode::Jmpnz
            | Opcode::JmpzEx
            | Opcode::JmpnzEx
            | Opcode::JmpSet
            | Opcode::Coalesce
            | Opcode::AssertCheck
            | Opcode::JmpNull
            | Opcode::BindInitStaticOrJmp
            | Opcode::JmpFrameless
            | Opcode::FeFetchR
            | Opcode::FeFetchRw
            | Opcode::FeResetR
            | Opcode::FeResetRw => {
                start(target(op) as usize);
                start(i + 1);
            }
            Opcode::Catch => {
                if op.extended_value & LAST_CATCH == 0 {
                    start(op.op2 as usize);
                }
                start(i + 1);
            }
            Opcode::SwitchLong | Opcode::SwitchString | Opcode::Match => {
                for at in jump_table(body, op) {
                    start(at as usize);
                }
                start(op.extended_value as usize);
                start(i + 1);
            }
            Opcode::FetchR
            | Opcode::FetchW
            | Opcode::FetchRw
            | Opcode::FetchFuncArg
            | Opcode::FetchIs
            | Opcode::FetchUnset
            | Opcode::UnsetVar
            | Opcode::IssetIsemptyVar
                if op.extended_value & FETCH_LOCAL != 0
                    || (op.extended_value & (FETCH_GLOBAL | FETCH_GLOBAL_LOCK) != 0 && !named) =>
            {
                features.indirect_var_access = true;
            }
            Opcode::FuncGetArgs => features.vararg = true,
            Opcode::ExtStmt => features.extended_stmt = true,
            Opcode::ExtFcallBegin | Opcode::ExtFcallEnd => features.extended_fcall = true,
            Opcode::Free | Opcode::FeFree => {
                let thrown = i
                    .checked_sub(1)
                    .and_then(|before| ops.get(before))
                    .is_some_and(|before| {
                        before.opcode == Opcode::MatchError
                            && before.extended_value == THROW_IS_EXPR
                    });
                if is_loop_var_free(op) && !thrown {
                    start(i);
                    features.free_loop_var = true;
                }
            }
            _ => {}
        }
    }
    // The entry block has predecessors where something jumps to the first
    // opline: it is split off an empty block of its own.
    let extra_entry = last > 0 && starts.first().is_some_and(|n| *n > 1);
    let mut start = |at: usize| {
        if let Some(n) = starts.get_mut(at) {
            *n += 1;
        }
    };
    for tc in &body.try_catch {
        start(tc.try_op.0 as usize);
        for at in [tc.catch_op, tc.finally_op, tc.finally_end] {
            if at.0 != 0 {
                start(at.0 as usize);
            }
        }
    }

    let mut blocks: Vec<Block> = Vec::new();
    if extra_entry {
        blocks.push(Block::default());
    }
    let mut map = Vec::with_capacity(last);
    for (i, n) in starts.iter().enumerate().take(last) {
        if *n != 0 {
            let at = u32::try_from(i).unwrap_or(u32::MAX);
            if let Some(open) = blocks.last_mut() {
                open.len = at - open.start;
            }
            blocks.push(Block {
                start: at,
                ..Block::default()
            });
        }
        map.push(BlockId::of(blocks.len().saturating_sub(1)));
    }
    if let Some(open) = blocks.last_mut() {
        open.len = u32::try_from(last).unwrap_or(u32::MAX) - open.start;
    }
    if blocks.is_empty() {
        blocks.push(Block::default());
    }

    let block_at = |at: u32| map.get(at as usize).copied().unwrap_or(BlockId(0));
    let count = blocks.len();
    for j in 0..count {
        let next = BlockId::of(j + 1);
        let Some(block) = blocks.get_mut(j) else {
            continue;
        };
        if block.len == 0 {
            block.successors = vec![next];
            continue;
        }
        let Some(op) = ops.get(block.end() - 1) else {
            continue;
        };
        block.successors = match op.opcode {
            Opcode::FastRet
            | Opcode::Return
            | Opcode::ReturnByRef
            | Opcode::GeneratorReturn
            | Opcode::Throw
            | Opcode::MatchError
            | Opcode::VerifyNeverType => Vec::new(),
            Opcode::Jmp => vec![block_at(target(op))],
            Opcode::Jmpz
            | Opcode::Jmpnz
            | Opcode::JmpzEx
            | Opcode::JmpnzEx
            | Opcode::JmpSet
            | Opcode::Coalesce
            | Opcode::AssertCheck
            | Opcode::JmpNull
            | Opcode::BindInitStaticOrJmp
            | Opcode::JmpFrameless
            | Opcode::FeFetchR
            | Opcode::FeFetchRw
            | Opcode::FeResetR
            | Opcode::FeResetRw
            | Opcode::FastCall => vec![block_at(target(op)), next],
            Opcode::Catch if op.extended_value & LAST_CATCH == 0 => {
                vec![block_at(op.op2), next]
            }
            Opcode::SwitchLong | Opcode::SwitchString | Opcode::Match => {
                let mut to: Vec<BlockId> = jump_table(body, op).into_iter().map(block_at).collect();
                to.push(block_at(op.extended_value));
                if op.opcode != Opcode::Match {
                    to.push(next);
                }
                to
            }
            _ => vec![next],
        };
    }

    let mut cfg = Cfg {
        blocks,
        map,
        features,
        try_catch: body.try_catch.clone(),
    };
    mark_reachable_blocks(&mut cfg, body);
    cfg
}

/// `zend_mark_reachable`: every block reachable from `from`, marked
/// reachable, with how each was entered.
fn mark_reachable(cfg: &mut Cfg, ops: &[Opline], from: BlockId) {
    let mut visited = vec![false; cfg.blocks.len()];
    let mut work = vec![from];
    if let Some(seen) = visited.get_mut(from.index()) {
        *seen = true;
    }
    while let Some(b) = work.pop() {
        let Some(block) = cfg.blocks.get_mut(b.index()) else {
            continue;
        };
        block.marks.reachable = true;
        if block.successors.is_empty() {
            block.marks.exit = true;
            continue;
        }
        let successors = block.successors.clone();
        let last = (block.len != 0)
            .then(|| ops.get(block.end() - 1).map(|op| op.opcode))
            .flatten();
        let count = successors.len();
        for (i, succ) in successors.into_iter().enumerate() {
            let Some(to) = cfg.blocks.get_mut(succ.index()) else {
                continue;
            };
            match last {
                Some(Opcode::Match) => to.marks.target = true,
                Some(Opcode::SwitchLong | Opcode::SwitchString) => {
                    to.marks.target = true;
                    if i == count - 1 {
                        to.marks.follow = true;
                    }
                }
                Some(Opcode::Jmp) if count == 1 => to.marks.target = true,
                Some(opcode) if count == 1 => {
                    to.marks.follow = true;
                    if matches!(
                        opcode,
                        Opcode::IncludeOrEval
                            | Opcode::GeneratorCreate
                            | Opcode::Yield
                            | Opcode::YieldFrom
                            | Opcode::DoFcall
                            | Opcode::DoUcall
                            | Opcode::DoFcallByName
                    ) {
                        to.marks.entry = true;
                    }
                    if matches!(opcode, Opcode::Recv | Opcode::RecvInit) {
                        to.marks.recv_entry = true;
                    }
                }
                Some(_) => {
                    if i == 0 {
                        to.marks.target = true;
                    } else {
                        to.marks.follow = true;
                    }
                }
                None => to.marks.follow = true,
            }
            if !to.marks.reachable
                && let Some(seen) = visited.get_mut(succ.index())
                && !*seen
            {
                *seen = true;
                work.push(succ);
            }
        }
    }
}

/// `zend_mark_reachable_blocks`: reachability from the entry, then from
/// each live `try`'s handlers until nothing changes, then the unreachable
/// blocks that free a loop variable a reachable block defined.
fn mark_reachable_blocks(cfg: &mut Cfg, body: &OpArray) {
    let ops = &body.oplines;
    if let Some(entry) = cfg.blocks.first_mut() {
        entry.marks = Marks {
            start: true,
            ..Marks::default()
        };
    }
    mark_reachable(cfg, ops, BlockId(0));
    let reachable = |cfg: &Cfg, id: BlockId| cfg.block(id).is_some_and(|b| b.marks.reachable);

    if !cfg.try_catch.is_empty() {
        loop {
            let mut changed = false;
            for j in 0..cfg.try_catch.len() {
                let Some(tc) = cfg.try_catch.get(j).copied() else {
                    continue;
                };
                // A jump into the middle of a try block.
                let mut b = cfg.block_of(tc.try_op.0);
                if !reachable(cfg, b) {
                    if tc.catch_op.0 != 0 {
                        let end = cfg.block_of(tc.catch_op.0);
                        while b < end {
                            if reachable(cfg, b) {
                                let start = cfg.block(b).map_or(0, |b| b.start);
                                if let Some(tc) = cfg.try_catch.get_mut(j) {
                                    tc.try_op.0 = start;
                                }
                                break;
                            }
                            b = BlockId(b.0 + 1);
                        }
                    }
                    let tc = cfg.try_catch.get(j).copied().unwrap_or(tc);
                    b = cfg.block_of(tc.try_op.0);
                    if !reachable(cfg, b) && tc.finally_op.0 != 0 {
                        let end = cfg.block_of(tc.finally_op.0);
                        while b < end {
                            if reachable(cfg, b) {
                                let start = cfg.block(b).map_or(0, |b| b.start);
                                let try_op = if tc.catch_op.0 != 0 {
                                    tc.catch_op.0
                                } else {
                                    start
                                };
                                if let Some(tc) = cfg.try_catch.get_mut(j) {
                                    tc.try_op.0 = try_op;
                                }
                                changed = true;
                                let from = cfg.block_of(try_op);
                                mark_reachable(cfg, ops, from);
                                break;
                            }
                            b = BlockId(b.0 + 1);
                        }
                    }
                }

                let tc = cfg.try_catch.get(j).copied().unwrap_or(tc);
                let b = cfg.block_of(tc.try_op.0);
                if !reachable(cfg, b) {
                    continue;
                }
                if let Some(block) = cfg.blocks.get_mut(b.index()) {
                    block.marks.try_ = true;
                }
                let handlers: [(u32, Mark); 3] = [
                    (tc.catch_op.0, |m| m.catch = true),
                    (tc.finally_op.0, |m| m.finally = true),
                    (tc.finally_end.0, |m| m.finally_end = true),
                ];
                for (at, mark) in handlers {
                    if at == 0 {
                        continue;
                    }
                    let h = cfg.block_of(at);
                    let Some(block) = cfg.blocks.get_mut(h.index()) else {
                        continue;
                    };
                    mark(&mut block.marks);
                    if !block.marks.reachable {
                        changed = true;
                        mark_reachable(cfg, ops, h);
                    }
                }
            }
            if !changed {
                break;
            }
        }
    }

    if cfg.features.free_loop_var {
        for n in 0..cfg.blocks.len() {
            let Some(block) = cfg.blocks.get(n) else {
                continue;
            };
            if block.marks.reachable {
                continue;
            }
            let frees_live = (block.start as usize..block.end()).any(|j| {
                ops.get(j).is_some_and(|op| {
                    is_loop_var_free(op)
                        && loop_var_def(ops, j, op.op1)
                            .is_some_and(|def| reachable(cfg, cfg.block_of(def)))
                })
            });
            if frees_live && let Some(block) = cfg.blocks.get_mut(n) {
                block.marks.unreachable_free = true;
            }
        }
    }
}

/// `zend_optimizer_get_loop_var_def`: the last opline before `free` whose
/// `TMP` or `VAR` out is `var`.
fn loop_var_def(ops: &[Opline], free: usize, var: u32) -> Option<u32> {
    let at = ops.get(..free)?.iter().rposition(|op| {
        matches!(op.out_type, OperandKind::Tmp | OperandKind::Var) && op.out == var
    })?;
    u32::try_from(at).ok()
}

/// `zend_cfg_build_predecessors`: an unreachable block loses its
/// successors, and each reachable block's distinct successors name it.
fn predecessors(cfg: &mut Cfg) {
    for block in &mut cfg.blocks {
        block.predecessors.clear();
        if !block.marks.reachable {
            block.successors.clear();
        }
    }
    for j in 0..cfg.blocks.len() {
        let Some(block) = cfg.blocks.get(j) else {
            continue;
        };
        if !block.marks.reachable {
            continue;
        }
        let successors = block.successors.clone();
        for (s, succ) in successors.iter().enumerate() {
            if successors
                .get(..s)
                .is_some_and(|before| before.contains(succ))
            {
                continue;
            }
            if let Some(to) = cfg.blocks.get_mut(succ.index()) {
                to.predecessors.push(BlockId::of(j));
            }
        }
    }
}

/// `compute_postnum_recursive`, kept on a stack of its own: each block's
/// number in a depth-first postorder from the entry, successors in order.
fn postorder(cfg: &Cfg) -> Vec<Option<u32>> {
    let mut postnum: Vec<Option<u32>> = vec![None; cfg.blocks.len()];
    let mut entered = vec![false; cfg.blocks.len()];
    let mut next = 0u32;
    // A block and the index of the successor to visit next.
    let mut stack = vec![(BlockId(0), 0usize)];
    if let Some(seen) = entered.first_mut() {
        *seen = true;
    }
    while let Some((b, s)) = stack.last_mut() {
        let succ = cfg
            .block(*b)
            .and_then(|block| block.successors.get(*s))
            .copied();
        *s += 1;
        if let Some(succ) = succ {
            if let Some(seen) = entered.get_mut(succ.index())
                && !*seen
            {
                *seen = true;
                stack.push((succ, 0));
            }
        } else {
            let b = *b;
            stack.pop();
            if let Some(n) = postnum.get_mut(b.index()) {
                *n = Some(next);
            }
            next += 1;
        }
    }
    postnum
}

/// `zend_cfg_compute_dominators_tree`.
fn dominators(cfg: &mut Cfg) {
    if cfg.blocks.len() == 1 {
        if let Some(entry) = cfg.blocks.first_mut() {
            entry.level = Some(0);
        }
        return;
    }
    let postnum = postorder(cfg);
    let num = |b: BlockId| postnum.get(b.index()).copied().flatten();
    let mut idom: Vec<Option<BlockId>> = vec![None; cfg.blocks.len()];
    if let Some(entry) = idom.first_mut() {
        *entry = Some(BlockId(0));
    }
    loop {
        let mut changed = false;
        for j in 1..cfg.blocks.len() {
            let Some(block) = cfg.blocks.get(j) else {
                continue;
            };
            if !block.marks.reachable {
                continue;
            }
            let mut found: Option<BlockId> = None;
            for &pred in &block.predecessors {
                if idom.get(pred.index()).copied().flatten().is_none() {
                    continue;
                }
                let Some(mut at) = found else {
                    found = Some(pred);
                    continue;
                };
                let mut pred = pred;
                while at != pred {
                    while num(pred) < num(at) {
                        match idom.get(pred.index()).copied().flatten() {
                            Some(up) => pred = up,
                            None => break,
                        }
                    }
                    while num(at) < num(pred) {
                        match idom.get(at.index()).copied().flatten() {
                            Some(up) => at = up,
                            None => break,
                        }
                    }
                    if num(pred).is_none() || num(at).is_none() {
                        break;
                    }
                }
                found = Some(at);
            }
            if let Some(found) = found
                && let Some(slot) = idom.get_mut(j)
                && *slot != Some(found)
            {
                *slot = Some(found);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
    if let Some(entry) = idom.first_mut() {
        *entry = None;
    }
    for (block, idom) in cfg.blocks.iter_mut().zip(&idom) {
        block.idom = if block.marks.reachable { *idom } else { None };
    }
    if let Some(entry) = cfg.blocks.first_mut() {
        entry.idom = None;
    }
    for j in 1..cfg.blocks.len() {
        let Some(parent) = cfg.blocks.get(j).and_then(|b| b.idom) else {
            continue;
        };
        if let Some(parent) = cfg.blocks.get_mut(parent.index()) {
            // Ascending, as j ascends.
            parent.children.push(BlockId::of(j));
        }
    }
    for j in 0..cfg.blocks.len() {
        let Some(block) = cfg.blocks.get(j) else {
            continue;
        };
        if !block.marks.reachable {
            continue;
        }
        let mut level = 0u32;
        let mut up = block.idom;
        while let Some(at) = up {
            level += 1;
            match cfg.block(at) {
                Some(above) if above.level.is_some() => {
                    level += above.level.unwrap_or(0);
                    break;
                }
                Some(above) => up = above.idom,
                None => break,
            }
        }
        if let Some(block) = cfg.blocks.get_mut(j) {
            block.level = Some(level);
        }
    }
}

/// `zend_worklist`: a stack whose pushes each number once until the
/// visited set is cleared.
struct Worklist {
    visited: Vec<bool>,
    stack: Vec<BlockId>,
}

impl Worklist {
    fn push(&mut self, b: BlockId) -> bool {
        match self.visited.get_mut(b.index()) {
            Some(seen) if !*seen => {
                *seen = true;
                self.stack.push(b);
                true
            }
            _ => false,
        }
    }
}

/// `zend_cfg_identify_loops`: loop headers, each block's closest one, and
/// irreducible loops, by Sreedhar's DJ graphs; the function's
/// `no_loops` and `irreducible` features.
fn loops(cfg: &mut Cfg) {
    let count = cfg.blocks.len();
    if count == 1 {
        cfg.features.no_loops = true;
        return;
    }
    let mut no_loops = true;
    let mut work = Worklist {
        visited: vec![false; count],
        stack: Vec::new(),
    };
    let mut entry_times: Vec<Option<u32>> = vec![None; count];
    let mut exit_times: Vec<Option<u32>> = vec![None; count];
    work.push(BlockId(0));
    let mut time = 0u32;
    'dfs: while let Some(&i) = work.stack.last() {
        if let Some(entry) = entry_times.get_mut(i.index())
            && entry.is_none()
        {
            *entry = Some(time);
            time += 1;
        }
        let Some(block) = cfg.block(i) else {
            work.stack.pop();
            continue;
        };
        for &child in &block.children {
            if work.push(child) {
                continue 'dfs;
            }
        }
        for &succ in &block.successors {
            if cfg.block(succ).and_then(|s| s.idom) == Some(i) {
                continue;
            }
            if work.push(succ) {
                continue 'dfs;
            }
        }
        if let Some(exit) = exit_times.get_mut(i.index()) {
            *exit = Some(time);
        }
        time += 1;
        work.stack.pop();
    }

    // The blocks by level, the reverse of the order they are taken in.
    let mut sorted = vec![BlockId(0)];
    let mut j = 0;
    while j < sorted.len() {
        let children = sorted
            .get(j)
            .and_then(|b| cfg.block(*b))
            .map(|b| b.children.clone())
            .unwrap_or_default();
        sorted.extend(children);
        j += 1;
    }

    while let Some(i) = sorted.pop() {
        let Some(block) = cfg.block(i) else {
            continue;
        };
        if block.predecessors.len() < 2 {
            continue;
        }
        let preds = block.predecessors.clone();
        let idom = block.idom;
        for pred in preds {
            if idom == Some(pred) {
                continue;
            }
            if cfg.dominates(i, pred) {
                if let Some(block) = cfg.blocks.get_mut(i.index()) {
                    block.marks.loop_header = true;
                }
                no_loops = false;
                if work.stack.is_empty() {
                    work.visited.fill(false);
                }
                work.push(pred);
            } else {
                let at =
                    |times: &[Option<u32>], b: BlockId| times.get(b.index()).copied().flatten();
                let ancestor = at(&entry_times, pred) > at(&entry_times, i)
                    && at(&exit_times, pred) < at(&exit_times, i);
                if ancestor {
                    if let Some(block) = cfg.blocks.get_mut(i.index()) {
                        block.marks.irreducible = true;
                    }
                    cfg.features.irreducible = true;
                    no_loops = false;
                }
            }
        }
        while let Some(mut j) = work.stack.pop() {
            while let Some(up) = cfg.block(j).and_then(|b| b.loop_header) {
                j = up;
            }
            if j == i {
                continue;
            }
            let Some(block) = cfg.blocks.get_mut(j.index()) else {
                continue;
            };
            if block.idom.is_none() && j != BlockId(0) {
                // Unreachable, or reachable only abnormally.
                continue;
            }
            block.loop_header = Some(i);
            let preds = block.predecessors.clone();
            for pred in preds {
                work.push(pred);
            }
        }
    }
    cfg.features.no_loops = no_loops;
}
