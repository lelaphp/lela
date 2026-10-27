//! php's function-level analysis of the op arrays a trace passes through
//! (ADR-0015, `jit-design.md` §4.6): `php_ssa`'s CFG, DFG, SSA and the
//! types and ranges inferred over them, asked for where
//! `zend_jit_trace_build_tssa` builds them, for the op arrays it builds
//! them for, built once per op array of the epoch and kept in its table
//! (built anew at each build where it binds a class only the request
//! holds, as php builds every one), and written under
//! `ZEND_JIT_DEBUG_SSA` at each trace's build, as php writes them each
//! time it builds them anew.
//!
//! Nothing in the trace reads the analysis yet (#3575); what a call returns
//! is typed as php types an opline it knows nothing of until the analysis
//! reads the callee (#3574).

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use php_engine::tier::{ClassId, FuncRef, UnitView};
use php_ssa::{Analysis, ClassConstant, Function, Level, LinkedTable, ObjectShape, Property};
use php_unit::decl::{ClassKind, PropertyDecl, Visibility};

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

/// The classes php's analysis of one op array names, as the view bound
/// them when it accepted the op array ([`UnitView::class_named`]), and
/// what the analysis reads of each from its linked record and
/// declaration.
struct Bound<'a> {
    view: &'a UnitView,
    /// The op array analysed.
    f: FuncRef,
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

/// The analyses of the op arrays `record` passes through, at an
/// `opt_level` of [`OPT_FUNC`] or more as php builds them, built where
/// `analyses` keeps none, each written to `lines` where they ask for
/// `ZEND_JIT_DEBUG_SSA`. `false` where the trace passes through more op
/// arrays than [`MAX_FUNCS`]: php builds the first thirty and its
/// `zend_jit_trace_build_tssa` fails, so the trace does not compile.
#[must_use]
pub(crate) fn build(
    record: &TraceRecord,
    view: &UnitView,
    analyses: &Analyses,
    opt_level: u8,
    lines: &mut Lines,
) -> bool {
    if opt_level < OPT_FUNC {
        return true;
    }
    let dump = lines.debug().any(JitDebug::SSA);
    let passed = passed(record);
    let level = if opt_level >= OPT_FUNCS {
        Level::Funcs
    } else {
        Level::Func
    };
    for &f in passed.iter().take(MAX_FUNCS) {
        let Some(function) = function(view, f) else {
            continue;
        };
        let table = Bound { view, f };
        let analyse = || php_ssa::analyse(&function, &table, level).map(Arc::new);
        let kept = if view.binds_request_class(f) {
            analyse()
        } else {
            analyses.get_or_build(f, analyse)
        };
        if dump && let Some(analysis) = kept {
            let mut written = Vec::new();
            analysis.dump(&mut written, &function, &table, view.frameless_names());
            lines.push(&written);
        }
    }
    passed.len() <= MAX_FUNCS
}

#[cfg(test)]
mod tests;
