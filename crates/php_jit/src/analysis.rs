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
//! An analysis the epoch keeps names a class by the identity every
//! request of the epoch shares, [`EpochClassId`], and one built for one
//! build alone by the recording request's number, [`ClassId`]
//! (design §5.14, *Identities*): `php_ssa::analyse` is built for each.
//!
//! What a call returns is typed by what php knows of the callee: a
//! native's from php's table, a user function's from what its analysis
//! earlier in the same build found it returns (`func_info->return_info`),
//! or from its declaration. The trace's build reads what each analysis
//! learned ([`Analysed`]) where `zend_jit_trace_build_tssa` reads it.

use std::cell::Cell;
use std::collections::HashMap;
use std::marker::PhantomData;
use std::sync::{Arc, Mutex, PoisonError};

use php_engine::api::abi::EngineState;
use php_engine::tier::{
    Bindings, CallSite, CalledFunction, CalleeBody, ClassId, EngineFuncRef, EpochClassId, FuncRef,
    UnitView,
};
use php_ssa::{
    Analysis, BlockId, Body, Callee, Cfg, ClassConstant, ClassOf, Function, Level, LinkedTable,
    Method, ObjectShape, PhiKind, Position, Property, Range, Returns, Ssa, SsaVar, TraceFrame,
    VarInfo, is_no_val_use,
};
use php_unit::OpArray;
use php_unit::decl::{ClassKind, HookKind, PropertyDecl, Visibility};

use crate::Lines;
use crate::policy::JitDebug;
use crate::record::{Event, TraceRecord};

/// The analysis of one op array the epoch keeps, none where php's fails.
type Kept = Option<Arc<Analysis<EpochClassId>>>;

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
/// finds what php's rebuild would. Engine state by its derive, which
/// holds each field, each analysis it keeps and each class one names by
/// an identity of the epoch's, to the bound by the trait's letter
/// (#4492, #4628).
#[derive(Debug, Default, EngineState)]
pub(crate) struct Analyses(Mutex<HashMap<EngineFuncRef, Vec<(Bindings, Kept)>>>);

impl Analyses {
    /// The analysis of `f` kept under `bindings`, none where none was.
    fn kept(&self, f: EngineFuncRef, bindings: &Bindings) -> Option<Kept> {
        let map = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let under = map.get(&f)?.iter().find(|(held, _)| held == bindings);
        under.map(|(_, kept)| kept.clone())
    }

    /// Keep `made` for `f` under `bindings`, or the analysis kept first
    /// where two builds race.
    fn keep(&self, f: EngineFuncRef, bindings: Bindings, made: Kept) -> Kept {
        let mut map = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let entries = map.entry(f).or_default();
        if let Some((_, kept)) = entries.iter().find(|(held, _)| *held == bindings) {
            return kept.clone();
        }
        entries.push((bindings, made.clone()));
        made
    }
}

/// How one analysis names a class: by the identity every request of the
/// view's epoch shares, in an analysis the epoch keeps
/// ([`EpochClassId`]), or by the recording request's number, in one made
/// for one build alone ([`ClassId`]). The view answers by the request's
/// number, which an analysis takes in its own.
trait Handle: Copy + PartialEq + Into<ClassId> {
    /// `class` as an analysis of this kind names it; none where it
    /// cannot, a request's own class in one the epoch keeps.
    fn of(view: &UnitView, class: ClassId) -> Option<Self>;
}

impl Handle for ClassId {
    fn of(_: &UnitView, class: ClassId) -> Option<ClassId> {
        Some(class)
    }
}

impl Handle for EpochClassId {
    fn of(view: &UnitView, class: ClassId) -> Option<EpochClassId> {
        view.shared_class(class)
    }
}

/// The classes php's analysis of one op array names, as the view bound
/// them when it accepted the op array ([`UnitView::class_named`]), and
/// what the analysis reads of each from its linked record and
/// declaration, each named as a `C`.
struct Bound<'a, C> {
    view: &'a UnitView,
    /// The op array analysed.
    f: EngineFuncRef,
    /// What the analyses this build made before `f`'s found each op
    /// array returns, `func_info->return_info`, which php clears from
    /// every op array a build analysed once its trace is compiled
    /// (`zend_jit_compile_trace`), so a caller reads only this build's.
    built: &'a [(EngineFuncRef, Option<Returns<ClassId>>)],
    /// Whether the analysis read what another user function returns,
    /// which differs from build to build with the order the op arrays
    /// are analysed in, so the analysis is built anew at each build.
    reads_returns: Cell<bool>,
    /// Whether the analysis met a class it has no `C` for, a request's
    /// own in an analysis the epoch would keep, so it is made again
    /// under the request's numbers and kept for no other build.
    unnamed: Cell<bool>,
    named: PhantomData<C>,
}

impl<'a, C: Handle> Bound<'a, C> {
    fn new(
        view: &'a UnitView,
        f: EngineFuncRef,
        built: &'a [(EngineFuncRef, Option<Returns<ClassId>>)],
    ) -> Bound<'a, C> {
        Bound {
            view,
            f,
            built,
            reads_returns: Cell::new(false),
            unnamed: Cell::new(false),
            named: PhantomData,
        }
    }

    /// `class`, as the view answers it, named as a `C`.
    fn named(&self, class: ClassId) -> Option<C> {
        let named = C::of(self.view, class);
        if named.is_none() {
            self.unnamed.set(true);
        }
        named
    }

    /// What a function returns, as an earlier analysis of this build
    /// found it, its class named as a `C`.
    fn returns(&self, returns: Returns<ClassId>) -> Option<Returns<C>> {
        let class = match returns.class {
            Some(of) => Some(ClassOf {
                class: self.named(of.class)?,
                instanceof: of.instanceof,
            }),
            None => None,
        };
        Some(Returns {
            types: returns.types,
            class,
            range: returns.range,
            has_range: returns.has_range,
        })
    }

    /// `c` and the classes above it, nearest first.
    fn lineage(&self, c: C) -> impl Iterator<Item = C> + '_ {
        std::iter::successors(Some(c), |&c| {
            self.view.parent(c.into()).and_then(|up| self.named(up))
        })
        .take(MAX_DEPTH)
    }

    /// `zend_get_property_info`'s access check for code of `scope`: a
    /// public property, or one `scope` declares; a protected one where
    /// `scope` and the class whose declaration the property's prototype
    /// is are one below the other (`is_protected_compatible_scope`).
    fn reaches(
        &self,
        visibility: Visibility,
        declared_in: C,
        name: &[u8],
        scope: Option<C>,
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
    fn prototype(&self, declared_in: C, name: &[u8]) -> C {
        self.lineage(declared_in)
            .take_while(|&c| self.own_property(c, name).is_some())
            .last()
            .unwrap_or(declared_in)
    }

    /// The property `name` class `c` declares itself.
    fn own_property(&self, c: C, name: &[u8]) -> Option<&PropertyDecl> {
        (self.view.class_decl(c.into())?.properties.iter()).find(|p| *p.name.0 == *name)
    }

    /// `called` as php's call analysis reads it: its body by the kind the
    /// key names it by ([`CalleeBody`]), and another user function's by
    /// its op array only to read what it returns, which marks the
    /// analysis as kept under no key.
    fn callee(&self, called: &'a CalledFunction) -> Callee<'a, C> {
        let body = match called.body_from(self.f) {
            CalleeBody::Native => Body::Native,
            CalleeBody::Itself => Body::Analysed,
            CalleeBody::Another => {
                self.reads_returns.set(true);
                let built =
                    (self.built.iter()).find(|&&(h, _)| Some(FuncRef::from(h)) == called.body);
                let returns = built.and_then(|&(_, returns)| returns);
                Body::User(returns.and_then(|returns| self.returns(returns)))
            }
        };
        Callee {
            name: &called.name.0,
            return_type: called.return_type.as_ref(),
            returns_by_ref: called.returns_by_ref,
            generator: called.generator,
            method: called.method.and_then(|m| {
                Some(Method {
                    scope: self.named(m.scope)?,
                    visibility: m.visibility,
                    is_final: m.is_final,
                    is_abstract: m.is_abstract,
                    tentative_return: m.tentative_return,
                })
            }),
            body,
        }
    }
}

/// The classes above a class the analysis walks, past which it stops.
const MAX_DEPTH: usize = 64;

impl<C: Handle> LinkedTable for Bound<'_, C> {
    type Class = C;

    fn class(&self, lcname: &[u8]) -> Option<C> {
        let class = self.view.class_named(self.f, lcname)?;
        self.named(class)
    }

    fn engine_class(&self, lcname: &[u8]) -> Option<C> {
        let class = self.view.engine_class(lcname)?;
        self.named(class)
    }

    fn scope(&self) -> Option<C> {
        let class = self.view.scope(self.f)?;
        self.named(class)
    }

    fn name(&self, class: C) -> &[u8] {
        self.view
            .class_decl(class.into())
            .map_or(&[][..], |decl| &decl.name.0)
    }

    fn parent(&self, class: C) -> Option<C> {
        let parent = self.view.parent(class.into())?;
        self.named(parent)
    }

    fn instance_of(&self, class: C, of: C) -> bool {
        self.lineage(class).any(|c| c == of)
            || self
                .view
                .class(class.into())
                .is_some_and(|linked| linked.interfaces.contains(&of.into()))
    }

    fn is_final(&self, class: C) -> bool {
        self.view
            .class_decl(class.into())
            .is_some_and(|decl| decl.flags.is_final)
    }

    fn is_trait(&self, class: C) -> bool {
        self.view
            .class_decl(class.into())
            .is_some_and(|decl| decl.kind == ClassKind::Trait)
    }

    /// A class lays its objects down with php's standard handlers and no
    /// `create_object` where no class of its lineage is the engine's own
    /// but `stdClass`, as no program's class sets either.
    fn object(&self, class: C) -> ObjectShape {
        let Some(linked) = self.view.class(class.into()) else {
            return ObjectShape::default();
        };
        let standard = self.lineage(class).all(|c| {
            self.view.class_decl(c.into()).is_some_and(|decl| {
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

    fn property(&self, class: C, name: &[u8], scope: Option<C>) -> Option<Property<'_, C>> {
        let declared =
            |holder: ClassId, at: u32| self.view.class_decl(holder)?.properties.get(at as usize);
        let row = (self.view.class(class.into())?.properties).find(name, |row| {
            declared(row.holder_in(class.into()), row.at).map_or(&[][..], |p| &p.name.0)
        })?;
        let holder = self.named(row.holder_in(class.into()))?;
        let (holder, decl) = match scope {
            // `zend_get_parent_private_property`: code of a class above
            // `class` that declares `name` private reads its own.
            Some(scope)
                if scope != holder
                    && scope != class
                    && self.lineage(class).any(|c| c == scope)
                    && self
                        .own_property(scope, name)
                        .is_some_and(|p| p.visibility == Visibility::Private) =>
            {
                (scope, self.own_property(scope, name)?)
            }
            _ => (holder, declared(holder.into(), row.at)?),
        };
        self.reaches(decl.visibility, holder, name, scope)
            .then_some(Property {
                is_static: !decl.is_instance,
                ty: decl.ty.as_ref(),
                declared_in: holder,
            })
    }

    fn function(&self, lcname: &[u8]) -> Option<Callee<'_, C>> {
        let called = self.view.called(self.f, CallSite::Function(lcname))?;
        Some(self.callee(called))
    }

    fn method(&self, class: C, lcname: &[u8]) -> Option<Callee<'_, C>> {
        let called = (self.view).called(self.f, CallSite::Method(class.into(), lcname))?;
        Some(self.callee(called))
    }

    fn frameless(&self, index: u32) -> Option<Callee<'_, C>> {
        let called = self.view.called(self.f, CallSite::Frameless(index))?;
        Some(self.callee(called))
    }

    fn parent_hook(&self, name: &[u8], kind: HookKind) -> Option<Callee<'_, C>> {
        let called = self.view.called(self.f, CallSite::ParentHook(name, kind))?;
        Some(self.callee(called))
    }

    fn constant(&self, class: C, name: &[u8]) -> Option<ClassConstant<'_, C>> {
        let declared =
            |holder: ClassId, at: u32| self.view.class_decl(holder)?.constants.get(at as usize);
        let row = (self.view.class(class.into())?.constants).find(name, |row| {
            declared(row.holder_in(class.into()), row.at).map_or(&[][..], |c| &c.name.0)
        })?;
        let decl = declared(row.holder_in(class.into()), row.at)?;
        Some(ClassConstant {
            ty: decl.ty.as_ref(),
            is_public: decl.visibility == Visibility::Public,
            is_deprecated: (decl.attributes.iter())
                .any(|a| a.name.0.eq_ignore_ascii_case(b"Deprecated")),
            declared_in: self.named(row.holder_in(class.into()))?,
        })
    }
}

/// The op array `f` names and what php reads beside it, from `view`.
fn function(view: &UnitView, f: EngineFuncRef) -> Option<Function<'_>> {
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
fn passed(record: &TraceRecord) -> Vec<EngineFuncRef> {
    let mut order = vec![record.root];
    let mut build = |f: EngineFuncRef| {
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

/// One analysis a trace's build reads: one the epoch keeps, its classes
/// named as every request of the epoch names them, or one made for this
/// build alone, named by the recording request's numbers.
#[derive(Debug)]
enum Read {
    Kept(Arc<Analysis<EpochClassId>>),
    Own(Arc<Analysis<ClassId>>),
}

impl Read {
    /// `func_info->return_info`, its class by the recording request's
    /// number.
    fn returns(&self) -> Returns<ClassId> {
        match self {
            Read::Kept(analysis) => widened_returns(analysis.returns),
            Read::Own(analysis) => analysis.returns,
        }
    }

    /// Whether php built an SSA of the op array.
    fn has_ssa(&self) -> bool {
        match self {
            Read::Kept(analysis) => analysis.ssa.is_some(),
            Read::Own(analysis) => analysis.ssa.is_some(),
        }
    }
}

/// `info`, its class by the recording request's number.
fn widened<C: Into<ClassId>>(info: VarInfo<C>) -> VarInfo<ClassId> {
    VarInfo {
        types: info.types,
        class: info.class.map(widened_class),
        range: info.range,
    }
}

/// `returns`, its class by the recording request's number.
fn widened_returns<C: Into<ClassId>>(returns: Returns<C>) -> Returns<ClassId> {
    Returns {
        types: returns.types,
        class: returns.class.map(widened_class),
        range: returns.range,
        has_range: returns.has_range,
    }
}

/// `of`, its class by the recording request's number.
fn widened_class<C: Into<ClassId>>(of: ClassOf<C>) -> ClassOf<ClassId> {
    ClassOf {
        class: of.class.into(),
        instanceof: of.instanceof,
    }
}

/// The analyses of the op arrays one trace's build passes through, as
/// `zend_jit_trace_build_tssa` reads each one's `func_info.ssa`: none for
/// an op array php does not analyse, or analyses with no SSA; and what
/// php's inference over the trace reads beside them, the level they were
/// built at and what each op array analysed returns.
#[derive(Debug)]
pub(crate) struct Analysed {
    analyses: Vec<(EngineFuncRef, Read)>,
    built: Vec<(EngineFuncRef, Option<Returns<ClassId>>)>,
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

/// Which operand of an opline: php's `op1_def`, `op2_def` or
/// `result_def` where it defines, its `op1_use`, `op2_use` or
/// `result_use` where it reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Defined {
    Op1,
    Op2,
    Out,
}

/// `$read` over the CFG and the SSA of the analysis of `$f` that
/// `$analysed` made or read, whichever identity names its classes; none
/// where it has none, or no SSA.
macro_rules! over {
    ($analysed:expr, $f:expr, |$cfg:pat_param, $ssa:ident| $read:expr) => {
        match &$analysed.analyses.iter().find(|(g, _)| *g == $f)?.1 {
            Read::Kept(analysis) => {
                let ($cfg, $ssa) = (&analysis.cfg, analysis.ssa.as_ref()?);
                $read
            }
            Read::Own(analysis) => {
                let ($cfg, $ssa) = (&analysis.cfg, analysis.ssa.as_ref()?);
                $read
            }
        }
    };
}

impl Analysed {
    /// What php's analysis of `f` knows of the variable opline `at`
    /// defines as `which`, the op array's `var_info` that
    /// `zend_jit_trace_restrict_ssa_var_info` and
    /// `zend_jit_trace_copy_ssa_var_range` read
    /// (`Z/ext/opcache/jit/zend_jit_trace.c:1009-1100`).
    pub(crate) fn defined(
        &self,
        f: EngineFuncRef,
        at: u32,
        which: Defined,
    ) -> Option<VarInfo<ClassId>> {
        over!(self, f, |_, ssa| defined_in(ssa, at, which))
    }

    /// Whether php's analysis of `f` holds what opline `at` defines as
    /// `which` reachable other than through its slot (a symbol table, a
    /// `compact`, a reference taken by name), the `alias` of its SSA
    /// variable, which `zend_jit_trace_copy_ssa_var_range` copies before
    /// it propagates the range (`Z/ext/opcache/jit/zend_jit_trace.c:996-1050`).
    pub(crate) fn aliased(&self, f: EngineFuncRef, at: u32, which: Defined) -> bool {
        let aliased = || over!(self, f, |_, ssa| aliased_in(ssa, at, which));
        aliased().unwrap_or(false)
    }

    /// What php's analysis of `f` knows of variable `var` where opline
    /// `at` begins, `zend_jit_trace_copy_ssa_var_info`'s search of the
    /// dominator tree from the opline a trace starts at
    /// (`Z/ext/opcache/jit/zend_jit_trace.c:917-965`): the last definition
    /// before it in its block, else the block's phi of the variable, a
    /// pi only where no phi is, and so up each immediate dominator; none
    /// where no definition dominates it, as php then copies what its first
    /// use reads, which is what the variable entered the function with.
    pub(crate) fn reaching(&self, f: EngineFuncRef, at: u32, var: u32) -> Option<VarInfo<ClassId>> {
        over!(self, f, |cfg, ssa| reaching_in(cfg, ssa, at, var))
    }

    /// `zend_ssa_is_last_use` (`Z/ext/opcache/jit/zend_jit.c:125-172`):
    /// whether opline `at` of `f`, whose oplines are `code`'s, reads the
    /// value its `which` operand names for the last time, as php's
    /// analysis of the op array sees it: no phi takes the value on, no
    /// use the loop around it dominates reads it again, and no opline
    /// after it reads it but to overwrite it. None where the analysis has
    /// no SSA.
    pub(crate) fn is_last_use(
        &self,
        f: EngineFuncRef,
        code: &OpArray,
        at: u32,
        which: Defined,
    ) -> Option<bool> {
        over!(self, f, |cfg, ssa| last_use_in((cfg, ssa), code, at, which))
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
        f: EngineFuncRef,
        start: u32,
        last: Option<u32>,
        var: u32,
    ) -> Option<VarInfo<ClassId>> {
        over!(self, f, |cfg, ssa| header_in((cfg, ssa), start, last, var))
    }
}

/// [`Analysed::defined`] over one analysis's SSA.
fn defined_in<C: Copy + Into<ClassId>>(
    ssa: &Ssa<C>,
    at: u32,
    which: Defined,
) -> Option<VarInfo<ClassId>> {
    let op = ssa.ops.get(at as usize)?;
    let var = match which {
        Defined::Op1 => op.op1_def,
        Defined::Op2 => op.op2_def,
        Defined::Out => op.out_def,
    }?;
    ssa.info.get(var.index()).copied().map(widened)
}

/// [`Analysed::aliased`] over one analysis's SSA; none where the opline
/// defines nothing as `which`.
fn aliased_in<C>(ssa: &Ssa<C>, at: u32, which: Defined) -> Option<bool> {
    let op = ssa.ops.get(at as usize)?;
    let var = match which {
        Defined::Op1 => op.op1_def,
        Defined::Op2 => op.op2_def,
        Defined::Out => op.out_def,
    }?;
    Some(ssa.vars.get(var.index())?.alias != php_ssa::Alias::None)
}

/// [`Analysed::reaching`] over one analysis's CFG and SSA.
fn reaching_in<C: Copy + Into<ClassId>>(
    cfg: &Cfg,
    ssa: &Ssa<C>,
    at: u32,
    var: u32,
) -> Option<VarInfo<ClassId>> {
    let info = |v: SsaVar| ssa.info.get(v.index()).copied().map(widened);
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
                return info(v);
            }
        }
        if let Some(v) = block_phi(ssa, block, var) {
            return info(v);
        }
        block = b.idom?;
        let up = cfg.blocks.get(block.index())?;
        op_num = up.start + up.len;
    }
}

/// [`Analysed::is_last_use`] over one analysis's CFG and SSA.
fn last_use_in<C>(
    (cfg, ssa): (&Cfg, &Ssa<C>),
    code: &OpArray,
    at: u32,
    which: Defined,
) -> Option<bool> {
    let op = ssa.ops.get(at as usize)?;
    let var = match which {
        Defined::Op1 => op.op1_use,
        Defined::Op2 => op.op2_use,
        Defined::Out => op.out_use,
    }?;
    let info = ssa.var(var)?;
    let mut phi = info.phi_use_chain;
    while let Some(id) = phi {
        if !ssa.var(ssa.phi(id)?.ssa_var)?.no_val {
            return Some(false);
        }
        phi = ssa.next_use_phi(var, id);
    }
    let no_val = |at: u32| {
        let (op, ssa_op) = (code.oplines.get(at as usize)?, ssa.ops.get(at as usize)?);
        Some(is_no_val_use(op, ssa_op, var))
    };
    let b = *cfg.map.get(at as usize)?;
    let block = cfg.blocks.get(b.index())?;
    if block.loop_header.is_some_and(|h| h.0 > 0) || block.marks.loop_header {
        let defined_in = match info.definition {
            Some(def) => *cfg.map.get(def as usize)?,
            None => ssa.phi(info.definition_phi?)?.block,
        };
        let header = if block.marks.loop_header {
            b
        } else {
            block.loop_header?
        };
        if cfg.dominates(defined_in, header) {
            return Some(false);
        }
        let mut prev = info.use_chain;
        while let Some(p) = prev.filter(|p| *p != at) {
            let pb = *cfg.map.get(p as usize)?;
            if b != pb && cfg.dominates(b, pb) && !no_val(p)? {
                return Some(false);
            }
            prev = ssa.next_use(var, p);
        }
    }
    match ssa.next_use(var, at) {
        None => Some(true),
        Some(next) => no_val(next),
    }
}

/// [`Analysed::header`] over one analysis's CFG and SSA.
fn header_in<C: Copy + Into<ClassId>>(
    (cfg, ssa): (&Cfg, &Ssa<C>),
    start: u32,
    last: Option<u32>,
    var: u32,
) -> Option<VarInfo<ClassId>> {
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
    let phi = block_phi(ssa, block, var)?;
    ssa.info.get(phi.index()).copied().map(widened)
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
            .filter_map(|f| Some((f, function(view, f)?, Bound::new(view, f, &self.built))))
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
    tables: Vec<(EngineFuncRef, Function<'a>, Bound<'a, ClassId>)>,
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
pub(crate) struct Inference<'a>(Vec<(EngineFuncRef, TraceFrame<'a, Bound<'a, ClassId>>)>);

impl<'a> Inference<'a> {
    fn frame(&self, f: EngineFuncRef) -> Option<&TraceFrame<'a, Bound<'a, ClassId>>> {
        self.0.iter().find(|(g, _)| *g == f).map(|(_, frame)| frame)
    }

    /// `zend_update_type_info` at the op `at` of `tssa`, which is of
    /// `f`; false where `f` is no op array the trace passes through, or php's
    /// would narrow what it wrote.
    pub(crate) fn update(
        &self,
        f: EngineFuncRef,
        tssa: &mut Ssa<ClassId>,
        positions: &[Position],
        at: usize,
    ) -> bool {
        self.frame(f)
            .is_some_and(|frame| frame.update_type_info(tssa, positions, at))
    }

    /// `zend_may_throw_ex` at the op `at` of `tssa`, which is of `f`, its
    /// operands of `t1` and `t2`; true where `f` is no op array the trace
    /// passes through.
    pub(crate) fn may_throw(
        &self,
        f: EngineFuncRef,
        tssa: &mut Ssa<ClassId>,
        positions: &[Position],
        at: usize,
        operands: (php_ssa::Types, php_ssa::Types),
    ) -> bool {
        self.frame(f)
            .is_none_or(|frame| frame.may_throw(tssa, positions, at, operands))
    }

    /// `zend_inference_propagate_range` of `var`, which the op `at` of
    /// `tssa`, running in `f`, defines.
    pub(crate) fn propagate_range(
        &self,
        f: EngineFuncRef,
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
fn block_phi<C>(ssa: &Ssa<C>, block: BlockId, var: u32) -> Option<SsaVar> {
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

/// The analysis of `f` a trace's build reads: the one the epoch keeps
/// under what the view bound for it, made and kept where none is; or one
/// made for this build alone where it binds a class only the request
/// holds, or met one with no identity the epoch shares, and made for the
/// epoch but kept for no other build where it read another function's
/// return. None where php's analysis fails.
fn analysis_of(
    view: &UnitView,
    analyses: &Analyses,
    (f, function): (EngineFuncRef, &Function<'_>),
    built: &[(EngineFuncRef, Option<Returns<ClassId>>)],
    level: Level,
) -> Option<Read> {
    if let Some(bindings) = view.bindings(f) {
        if let Some(kept) = analyses.kept(f, &bindings) {
            return kept.map(Read::Kept);
        }
        let table = Bound::<EpochClassId>::new(view, f, built);
        let made = php_ssa::analyse(function, &table, level).map(Arc::new);
        if !table.unnamed.get() {
            let made = if table.reads_returns.get() {
                made
            } else {
                analyses.keep(f, bindings, made)
            };
            return made.map(Read::Kept);
        }
    }
    let table = Bound::<ClassId>::new(view, f, built);
    php_ssa::analyse(function, &table, level).map(|made| Read::Own(Arc::new(made)))
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
    let mut built: Vec<(EngineFuncRef, Option<Returns<ClassId>>)> = Vec::new();
    let mut found = Vec::new();
    for &f in passed.iter().take(MAX_FUNCS) {
        let Some(function) = function(view, f) else {
            continue;
        };
        let read = analysis_of(view, analyses, (f, &function), &built, level);
        if dump && let Some(read) = &read {
            let mut written = Vec::new();
            let frameless = view.frameless_names();
            match read {
                Read::Kept(kept) => {
                    let table = Bound::<EpochClassId>::new(view, f, &built);
                    kept.dump(&mut written, &function, &table, frameless);
                }
                Read::Own(own) => {
                    let table = Bound::<ClassId>::new(view, f, &built);
                    own.dump(&mut written, &function, &table, frameless);
                }
            }
            lines.push(&written);
        }
        built.push((f, read.as_ref().map(Read::returns)));
        if let Some(read) = read.filter(Read::has_ssa) {
            found.push((f, read));
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
