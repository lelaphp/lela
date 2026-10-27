//! php's function-level analysis of the op arrays a trace passes through
//! (ADR-0015, `jit-design.md` §4.6): `php_ssa`'s CFG, DFG, SSA and the
//! types and ranges inferred over them, asked for where
//! `zend_jit_trace_build_tssa` builds them, for the op arrays it builds
//! them for, built once per op array and binding set of the epoch and
//! kept in its table, as php's rebuild at every build would find it
//! (built anew at each build where it binds a class only the request
//! holds, or reads what another function returns), and written under
//! `ZEND_JIT_DEBUG_SSA` at each trace's build, as php writes them each
//! time it builds them anew.
//!
//! What a call returns is typed by what php knows of the callee: a
//! native's from php's table, a user function's from what its analysis
//! earlier in the same build found it returns (`func_info->return_info`),
//! or from its declaration. The trace's build reads what each analysis
//! learned ([`Analysed`]) where `zend_jit_trace_build_tssa` reads it.

use std::cell::Cell;
use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use php_engine::tier::{Bindings, CallSite, CalledFunction, ClassId, FuncRef, UnitView};
use php_ssa::{
    Analysis, BlockId, Body, Callee, Cfg, ClassConstant, Function, Level, LinkedTable, Method,
    ObjectShape, PhiKind, Position, Property, Range, Returns, Ssa, SsaVar, TraceFrame, VarInfo,
};
use php_unit::decl::{ClassKind, HookKind, PropertyDecl, Visibility};

use crate::Lines;
use crate::policy::JitDebug;
use crate::record::{Event, TraceRecord};

/// The analysis of one op array, none where php's fails.
type Kept = Option<Arc<Analysis<ClassId>>>;

/// `ZEND_JIT_LEVEL_OPT_FUNC`: the `O` digit of `opcache.jit` from which
/// php analyses the op arrays a trace passes through (`zend_jit.h`).
const OPT_FUNC: u8 = 3;

/// `ZEND_JIT_LEVEL_OPT_FUNCS`: the `O` digit from which php reads the
/// calls an op array makes and its declared return type beside it.
const OPT_FUNCS: u8 = 4;

/// `ZEND_JIT_TRACE_MAX_FUNCS`: the op arrays one trace's build analyses,
/// past which php refuses to compile it (`zend_jit.h`).
const MAX_FUNCS: usize = 30;

/// An epoch's analyses by op array and the classes and functions its
/// view bound for it, each written once (`jit-design.md` §4.6; ADR-0015
/// as amended on #3571): php rebuilds an op array's analysis at every
/// trace's build, and a build that binds what an earlier one bound
/// finds what php's rebuild would.
#[derive(Debug, Default)]
pub(crate) struct Analyses(Mutex<HashMap<FuncRef, Vec<(Bindings, Kept)>>>);

impl Analyses {
    /// The analysis of `f` kept under `bindings`, none where none was.
    fn kept(&self, f: FuncRef, bindings: &Bindings) -> Option<Kept> {
        let map = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let under = map.get(&f)?.iter().find(|(held, _)| held == bindings);
        under.map(|(_, kept)| kept.clone())
    }

    /// Keep `made` for `f` under `bindings`, or the analysis kept first
    /// where two builds race.
    fn keep(&self, f: FuncRef, bindings: Bindings, made: Kept) -> Kept {
        let mut map = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let entries = map.entry(f).or_default();
        if let Some((_, kept)) = entries.iter().find(|(held, _)| *held == bindings) {
            return kept.clone();
        }
        entries.push((bindings, made.clone()));
        made
    }
}

/// The classes php's analysis of one op array names, as the view bound
/// them when it accepted the op array ([`UnitView::class_named`]), and
/// what the analysis reads of each from its linked record and
/// declaration.
struct Bound<'a> {
    view: &'a UnitView,
    /// The op array analysed.
    f: FuncRef,
    /// What the analyses this build made before `f`'s found each op
    /// array returns, `func_info->return_info`, which php clears from
    /// every op array a build analysed once its trace is compiled
    /// (`zend_jit_compile_trace`), so a caller reads only this build's.
    built: &'a [(FuncRef, Option<Returns<ClassId>>)],
    /// Whether the analysis read what another user function returns,
    /// which differs from build to build with the order the op arrays
    /// are analysed in, so the analysis is built anew at each build.
    reads_returns: Cell<bool>,
}

impl Bound<'_> {
    /// `c` and the classes above it, nearest first.
    fn lineage(&self, c: ClassId) -> impl Iterator<Item = ClassId> + '_ {
        std::iter::successors(Some(c), |&c| self.view.parent(c)).take(MAX_DEPTH)
    }

    /// `zend_get_property_info`'s access check for code of `scope`: a
    /// public property, or one `scope` declares; a protected one where
    /// `scope` and the class whose declaration the property's prototype
    /// is are one below the other (`is_protected_compatible_scope`).
    fn reaches(
        &self,
        visibility: Visibility,
        declared_in: ClassId,
        name: &[u8],
        scope: Option<ClassId>,
    ) -> bool {
        match (visibility, scope) {
            (Visibility::Public, _) => true,
            (_, Some(scope)) if scope == declared_in => true,
            (Visibility::Protected, Some(scope)) => {
                let root = self.prototype(declared_in, name);
                self.instance_of(scope, root) || self.instance_of(root, scope)
            }
            _ => false,
        }
    }

    /// `property_info->prototype->ce`: the class above `declared_in`
    /// furthest up whose properties still hold `name`, where an
    /// inherited protected property was first declared.
    fn prototype(&self, declared_in: ClassId, name: &[u8]) -> ClassId {
        self.lineage(declared_in)
            .take_while(|&c| self.own_property(c, name).is_some())
            .last()
            .unwrap_or(declared_in)
    }

    /// The property `name` class `c` declares itself.
    fn own_property(&self, c: ClassId, name: &[u8]) -> Option<&PropertyDecl> {
        (self.view.class_decl(c)?.properties.iter()).find(|p| *p.name.0 == *name)
    }
}

impl<'a> Bound<'a> {
    /// `called` as php's call analysis reads it.
    fn callee(&self, called: &'a CalledFunction) -> Callee<'a, ClassId> {
        let body = match called.body {
            None => Body::Native,
            Some(g) if g == self.f => Body::Analysed,
            Some(g) => {
                self.reads_returns.set(true);
                let built = self.built.iter().find(|(h, _)| *h == g);
                Body::User(built.and_then(|&(_, returns)| returns))
            }
        };
        Callee {
            name: &called.name.0,
            return_type: called.return_type.as_ref(),
            returns_by_ref: called.returns_by_ref,
            generator: called.generator,
            method: called.method.map(|m| Method {
                scope: m.scope,
                visibility: m.visibility,
                is_final: m.is_final,
                is_abstract: m.is_abstract,
                tentative_return: m.tentative_return,
            }),
            body,
        }
    }
}

/// The classes above a class the analysis walks, past which it stops.
const MAX_DEPTH: usize = 64;

impl LinkedTable for Bound<'_> {
    type Class = ClassId;

    fn class(&self, lcname: &[u8]) -> Option<ClassId> {
        self.view.class_named(self.f, lcname)
    }

    fn engine_class(&self, lcname: &[u8]) -> Option<ClassId> {
        self.view.engine_class(lcname)
    }

    fn scope(&self) -> Option<ClassId> {
        self.view.scope(self.f)
    }

    fn name(&self, class: ClassId) -> &[u8] {
        self.view
            .class_decl(class)
            .map_or(&[][..], |decl| &decl.name.0)
    }

    fn parent(&self, class: ClassId) -> Option<ClassId> {
        self.view.parent(class)
    }

    fn instance_of(&self, class: ClassId, of: ClassId) -> bool {
        self.lineage(class).any(|c| c == of)
            || self
                .view
                .class(class)
                .is_some_and(|linked| linked.interfaces.contains(&of))
    }

    fn is_final(&self, class: ClassId) -> bool {
        self.view
            .class_decl(class)
            .is_some_and(|decl| decl.flags.is_final)
    }

    fn is_trait(&self, class: ClassId) -> bool {
        self.view
            .class_decl(class)
            .is_some_and(|decl| decl.kind == ClassKind::Trait)
    }

    /// A class lays its objects down with php's standard handlers and no
    /// `create_object` where no class of its lineage is the engine's own
    /// but `stdClass`, as no program's class sets either.
    fn object(&self, class: ClassId) -> ObjectShape {
        let Some(linked) = self.view.class(class) else {
            return ObjectShape::default();
        };
        let standard = self.lineage(class).all(|c| {
            self.view.class_decl(c).is_some_and(|decl| {
                !decl.file.0.is_empty() || decl.name.0.eq_ignore_ascii_case(b"stdClass")
            })
        });
        ObjectShape {
            constructor: linked.constructor.is_some(),
            get: linked.accessors.get.is_some(),
            set: linked.accessors.set.is_some(),
            standard,
        }
    }

    fn property(
        &self,
        class: ClassId,
        name: &[u8],
        scope: Option<ClassId>,
    ) -> Option<Property<'_, ClassId>> {
        let declared =
            |holder: ClassId, at: u32| self.view.class_decl(holder)?.properties.get(at as usize);
        let row = (self.view.class(class)?.properties).find(name, |row| {
            declared(row.holder_in(class), row.at).map_or(&[][..], |p| &p.name.0)
        })?;
        let (holder, decl) = match scope {
            // `zend_get_parent_private_property`: code of a class above
            // `class` that declares `name` private reads its own.
            Some(scope)
                if scope != row.holder_in(class)
                    && scope != class
                    && self.lineage(class).any(|c| c == scope)
                    && self
                        .own_property(scope, name)
                        .is_some_and(|p| p.visibility == Visibility::Private) =>
            {
                (scope, self.own_property(scope, name)?)
            }
            _ => (
                row.holder_in(class),
                declared(row.holder_in(class), row.at)?,
            ),
        };
        self.reaches(decl.visibility, holder, name, scope)
            .then_some(Property {
                is_static: !decl.is_instance,
                ty: decl.ty.as_ref(),
                declared_in: holder,
            })
    }

    fn function(&self, lcname: &[u8]) -> Option<Callee<'_, ClassId>> {
        let called = self.view.called(self.f, CallSite::Function(lcname))?;
        Some(self.callee(called))
    }

    fn method(&self, class: ClassId, lcname: &[u8]) -> Option<Callee<'_, ClassId>> {
        let called = self.view.called(self.f, CallSite::Method(class, lcname))?;
        Some(self.callee(called))
    }

    fn frameless(&self, index: u32) -> Option<Callee<'_, ClassId>> {
        let called = self.view.called(self.f, CallSite::Frameless(index))?;
        Some(self.callee(called))
    }

    fn parent_hook(&self, name: &[u8], kind: HookKind) -> Option<Callee<'_, ClassId>> {
        let called = self.view.called(self.f, CallSite::ParentHook(name, kind))?;
        Some(self.callee(called))
    }

    fn constant(&self, class: ClassId, name: &[u8]) -> Option<ClassConstant<'_, ClassId>> {
        let declared =
            |holder: ClassId, at: u32| self.view.class_decl(holder)?.constants.get(at as usize);
        let row = (self.view.class(class)?.constants).find(name, |row| {
            declared(row.holder_in(class), row.at).map_or(&[][..], |c| &c.name.0)
        })?;
        let holder = row.holder_in(class);
        let decl = declared(holder, row.at)?;
        Some(ClassConstant {
            ty: decl.ty.as_ref(),
            is_public: decl.visibility == Visibility::Public,
            is_deprecated: (decl.attributes.iter())
                .any(|a| a.name.0.eq_ignore_ascii_case(b"Deprecated")),
            declared_in: holder,
        })
    }
}

/// The op array `f` names and what php reads beside it, from `view`.
fn function(view: &UnitView, f: FuncRef) -> Option<Function<'_>> {
    let body = view.op_array(f)?;
    let declared = view.declaration(f)?;
    Some(Function {
        body,
        decl: declared.function,
        scope: declared.scope.map(|name| &*name.0),
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

/// The analyses of the op arrays one trace's build passes through, as
/// `zend_jit_trace_build_tssa` reads each one's `func_info.ssa`: none for
/// an op array php does not analyse, or analyses with no SSA; and what
/// php's inference over the trace reads beside them, the level they were
/// built at and what each op array analysed returns.
#[derive(Debug)]
pub(crate) struct Analysed {
    analyses: Vec<(FuncRef, Arc<Analysis<ClassId>>)>,
    built: Vec<(FuncRef, Option<Returns<ClassId>>)>,
    level: Level,
}

impl Default for Analysed {
    fn default() -> Analysed {
        Analysed {
            analyses: Vec::new(),
            built: Vec::new(),
            level: Level::Func,
        }
    }
}

/// Which variable an opline defines: php's `op1_def`, `op2_def` or
/// `result_def`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Defined {
    Op1,
    Op2,
    Out,
}

impl Analysed {
    /// The analysis of `f` this build made or read, and its SSA, where it
    /// has one.
    fn of(&self, f: FuncRef) -> Option<(&Cfg, &Ssa<ClassId>)> {
        let (_, analysis) = self.analyses.iter().find(|(g, _)| *g == f)?;
        Some((&analysis.cfg, analysis.ssa.as_ref()?))
    }

    /// What php's analysis of `f` knows of the variable opline `at`
    /// defines as `which`, the op array's `var_info` that
    /// `zend_jit_trace_restrict_ssa_var_info` and
    /// `zend_jit_trace_copy_ssa_var_range` read
    /// (`Z/ext/opcache/jit/zend_jit_trace.c:1009-1100`).
    pub(crate) fn defined(&self, f: FuncRef, at: u32, which: Defined) -> Option<&VarInfo<ClassId>> {
        let (_, ssa) = self.of(f)?;
        let op = ssa.ops.get(at as usize)?;
        let var = match which {
            Defined::Op1 => op.op1_def,
            Defined::Op2 => op.op2_def,
            Defined::Out => op.out_def,
        }?;
        ssa.info.get(var.index())
    }

    /// What php's analysis of `f` knows of variable `var` where opline
    /// `at` begins, `zend_jit_trace_copy_ssa_var_info`'s search of the
    /// dominator tree from the opline a trace starts at
    /// (`Z/ext/opcache/jit/zend_jit_trace.c:917-965`): the last definition
    /// before it in its block, else the block's phi of the variable, a
    /// pi only where no phi is, and so up each immediate dominator; none
    /// where no definition dominates it, as php then copies what its first
    /// use reads, which is what the variable entered the function with.
    pub(crate) fn reaching(&self, f: FuncRef, at: u32, var: u32) -> Option<&VarInfo<ClassId>> {
        let (cfg, ssa) = self.of(f)?;
        let of_var = |v: Option<SsaVar>| v.filter(|v| ssa.var(*v).is_some_and(|i| i.var.0 == var));
        let mut block = *cfg.map.get(at as usize)?;
        let mut op_num = at;
        loop {
            let b = cfg.blocks.get(block.index())?;
            while op_num > b.start {
                op_num -= 1;
                let op = ssa.ops.get(op_num as usize)?;
                if let Some(v) = of_var(op.out_def)
                    .or_else(|| of_var(op.op2_def))
                    .or_else(|| of_var(op.op1_def))
                {
                    return ssa.info.get(v.index());
                }
            }
            if let Some(v) = block_phi(ssa, block, var) {
                return ssa.info.get(v.index());
            }
            block = b.idom?;
            let up = cfg.blocks.get(block.index())?;
            op_num = up.start + up.len;
        }
    }

    /// What php's analysis of `f` knows of variable `var` at the phi of the
    /// loop a trace that starts at opline `start` and whose last opline of
    /// `f` is `last` repeats, `zend_jit_trace_copy_ssa_var_info` of a trace
    /// phi (`Z/ext/opcache/jit/zend_jit_trace.c:832-856`): the header's phi
    /// of the variable, a pi only where no phi is; none where the start is
    /// no loop's header, or the trace's loop is not the op array's
    /// (`zend_jit_trace_is_false_loop`).
    pub(crate) fn header(
        &self,
        f: FuncRef,
        start: u32,
        last: Option<u32>,
        var: u32,
    ) -> Option<&VarInfo<ClassId>> {
        let (cfg, ssa) = self.of(f)?;
        let block = *cfg.map.get(start as usize)?;
        if !cfg.blocks.get(block.index())?.marks.loop_header {
            return None;
        }
        let false_loop = last
            .and_then(|last| cfg.map.get(last as usize))
            .and_then(|b| cfg.blocks.get(b.index()))
            .is_some_and(|b| b.loop_header != Some(block));
        if false_loop {
            return None;
        }
        ssa.info.get(block_phi(ssa, block, var)?.index())
    }
}

impl Analysed {
    /// What php's inference over a trace reads of each op array `record`
    /// passes through: the op array, and the classes and functions its view
    /// binds, as the analyses read them.
    pub(crate) fn tables<'a>(&'a self, record: &TraceRecord, view: &'a UnitView) -> Tables<'a> {
        let mut funcs = vec![record.root];
        for event in &record.events {
            if let Event::Op { func, .. } | Event::Enter { func } = *event
                && !funcs.contains(&func)
            {
                funcs.push(func);
            }
        }
        let tables = (funcs.into_iter())
            .filter_map(|f| {
                let table = Bound {
                    view,
                    f,
                    built: &self.built,
                    reads_returns: Cell::new(false),
                };
                Some((f, function(view, f)?, table))
            })
            .collect();
        Tables {
            tables,
            level: self.level,
        }
    }
}

/// What php's inference over a trace reads of the op arrays it passes
/// through
/// ([`Analysed::tables`]), from which its frames are made
/// ([`Tables::frames`]).
pub(crate) struct Tables<'a> {
    tables: Vec<(FuncRef, Function<'a>, Bound<'a>)>,
    level: Level,
}

impl Tables<'_> {
    /// php's inference over a trace, one frame per op array.
    pub(crate) fn frames(&self) -> Inference<'_> {
        Inference(
            (self.tables.iter())
                .map(|(f, function, table)| (*f, TraceFrame::new(function, table, self.level)))
                .collect(),
        )
    }
}

/// php's inference over a trace's SSA, `zend_update_type_info` and
/// `zend_inference_propagate_range` as `zend_jit_trace_build_tssa` calls
/// them, each op read in the op array it is of.
pub(crate) struct Inference<'a>(Vec<(FuncRef, TraceFrame<'a, Bound<'a>>)>);

impl<'a> Inference<'a> {
    fn frame(&self, f: FuncRef) -> Option<&TraceFrame<'a, Bound<'a>>> {
        self.0.iter().find(|(g, _)| *g == f).map(|(_, frame)| frame)
    }

    /// `zend_update_type_info` at the op `at` of `tssa`, which is of
    /// `f`; false where `f` is no op array the trace passes through, or php's
    /// would narrow what it wrote.
    pub(crate) fn update(
        &self,
        f: FuncRef,
        tssa: &mut Ssa<ClassId>,
        positions: &[Position],
        at: usize,
    ) -> bool {
        self.frame(f)
            .is_some_and(|frame| frame.update_type_info(tssa, positions, at))
    }

    /// `zend_inference_propagate_range` of `var`, which the op `at` of
    /// `tssa`, running in `f`, defines.
    pub(crate) fn propagate_range(
        &self,
        f: FuncRef,
        tssa: &mut Ssa<ClassId>,
        positions: &[Position],
        at: usize,
        var: SsaVar,
    ) -> Option<Range> {
        self.frame(f)?.propagate_range(tssa, positions, at, var)
    }
}

/// The phi of `var` at `block`, a pi only where the block has no phi of
/// it, as `zend_jit_trace_copy_ssa_var_info` walks `ssa->blocks[b].phis`.
fn block_phi(ssa: &Ssa<ClassId>, block: BlockId, var: u32) -> Option<SsaVar> {
    let phis = ssa.block_phis.get(block.index())?;
    let mut pi = None;
    for phi in phis.iter().filter_map(|id| ssa.phi(*id)) {
        if phi.var.0 != var {
            continue;
        }
        match phi.kind {
            PhiKind::Phi => return Some(phi.ssa_var),
            PhiKind::Pi { .. } => pi = Some(phi.ssa_var),
        }
    }
    pi
}

/// The analyses of the op arrays `record` passes through, at an
/// `opt_level` of [`OPT_FUNC`] or more as php builds them, built where
/// `analyses` keeps none, each written to `lines` where they ask for
/// `ZEND_JIT_DEBUG_SSA`. None where the trace passes through more op
/// arrays than [`MAX_FUNCS`]: php builds the first thirty and its
/// `zend_jit_trace_build_tssa` fails, so the trace does not compile.
#[must_use]
pub(crate) fn build(
    record: &TraceRecord,
    view: &UnitView,
    analyses: &Analyses,
    opt_level: u8,
    lines: &mut Lines,
) -> Option<Analysed> {
    if opt_level < OPT_FUNC {
        return Some(Analysed::default());
    }
    let dump = lines.debug().any(JitDebug::SSA);
    let passed = passed(record);
    let level = if opt_level >= OPT_FUNCS {
        Level::Funcs
    } else {
        Level::Func
    };
    let mut built: Vec<(FuncRef, Option<Returns<ClassId>>)> = Vec::new();
    let mut found = Vec::new();
    for &f in passed.iter().take(MAX_FUNCS) {
        let Some(function) = function(view, f) else {
            continue;
        };
        let table = Bound {
            view,
            f,
            built: &built,
            reads_returns: Cell::new(false),
        };
        // Kept for the epoch only where what it read stays: no class
        // only the request holds, and no other function's return.
        let bindings = (!view.binds_request_class(f)).then(|| view.bindings(f));
        let kept = (bindings.as_ref())
            .and_then(|b| analyses.kept(f, b))
            .unwrap_or_else(|| {
                let made = php_ssa::analyse(&function, &table, level).map(Arc::new);
                match bindings {
                    Some(bindings) if !table.reads_returns.get() => {
                        analyses.keep(f, bindings, made)
                    }
                    _ => made,
                }
            });
        if dump && let Some(analysis) = &kept {
            let mut written = Vec::new();
            analysis.dump(&mut written, &function, &table, view.frameless_names());
            lines.push(&written);
        }
        built.push((f, kept.as_ref().map(|a| a.returns)));
        if let Some(analysis) = kept.filter(|a| a.ssa.is_some()) {
            found.push((f, analysis));
        }
    }
    (passed.len() <= MAX_FUNCS).then_some(Analysed {
        analyses: found,
        built,
        level,
    })
}

/// The analyses a trace's build reads, made afresh as the tier makes
/// them under `opcache.jit=tracing`, for a test that builds a record
/// with no epoch's table.
#[cfg(test)]
pub(crate) fn analysed(record: &TraceRecord, view: &UnitView) -> Analysed {
    let mut lines = Lines::default();
    build(record, view, &Analyses::default(), OPT_FUNCS, &mut lines).unwrap_or_default()
}

#[cfg(test)]
mod tests;
