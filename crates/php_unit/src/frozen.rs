//! A unit or a declaration the engine published for every request, as a
//! type only a check makes (#4633): [`Frozen`], and the one walk over the
//! values a unit holds ([`unit_values`]). The check reads through it, and
//! so do `php_compile`'s `declared_values` and `holds_counted_value`, the
//! compiler's reuse of an attribute's arguments, and the engine's walks
//! over a constant expression's values and a constant's attributes. One
//! reader does not yet: `php_engine`'s `main_constants` reads an op array's
//! constants and bound variables itself (#4682).
//!
//! A [`PublishedUnit`] cannot tell the engine's unit from a request's:
//! the engine's (`publish_compiled`), a request's own (`compile_for`) and
//! an engine-home unit a request takes as its own (`take_unit`) are one
//! type. So what the engine holds for every request is a [`Frozen`],
//! which [`Frozen::new`] makes only where every value the walk reaches is
//! the engine's and every op array is numbered by the engine.
//!
//! The walk destructures every struct it walks with no `..` and matches
//! every enum with no `_`, so a field or a variant the unit format gains
//! does not compile until the walk names it. A field that holds no value
//! is named `_`: plain data (flags, counts, lines, oplines, indices and
//! tables of them, a doc comment, a diagnostic's text, an attribute's
//! validator), and names and types, of which `Name`, `Type` and
//! `Visibility` are engine state by the derive. The one field that names
//! something beside the walked value is a closure's cell
//! ([`crate::UnitCell`]), which names the unit the closure was declared
//! in: the walk over a unit passes it over, and the walk over a
//! declaration refuses it unless it is empty, as the argument of
//! [`Frozen`]'s `EngineState` impl says.
#![deny(clippy::wildcard_enum_match_arm)]

use std::ops::Deref;

use php_heap::{EngineState, FrozenZval, Zval, ZvalType};

use crate::decl::{
    Attribute, AttributeArgument, ClassConstDecl, ClassDecl, ConstDecl, ConstExpr, FunctionDecl,
    MethodDecl, Parameter, ParameterDefault, PropertyDecl, PropertyHooks,
};
use crate::op::{BoundVar, MapPtrOffset, OpArray};
use crate::unit::{HookBody, MethodBody, PublishedUnit, Unit, UnitCell};

/// What holds a value [`unit_values`] meets, as the engine gives each
/// value of a compile its owner: the main op array, held while it runs;
/// the unit's tables, for a function, a class and its members, held
/// while the unit lives; or the dynamic definition at this place of
/// [`Unit::dynamic`]. The compiler's walk over a unit's names gives
/// each name the same owner.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Holder {
    Main,
    Unit,
    Dynamic(usize),
}

/// Where in what its [`Holder`] holds a value [`unit_values`] meets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Slot {
    /// One of an op array's constants, its `literals`.
    Constant,
    /// The value an op array's bound variable starts with
    /// ([`BoundVar`]).
    Bound,
    /// Inside a tree of an op array's `const_exprs`.
    Tree,
    /// In a table of a declaration's own, as php's compile leaves each
    /// zval: a class's constant, default, static default and attribute
    /// argument, its methods' and hooks' among them, and a function's
    /// attribute argument.
    Declared,
    /// In a declaration that is a view of what an op array holds: a
    /// parameter's default, which its `RECV_INIT` holds, and a `const`'s
    /// value and attributes, which its `DECLARE_CONST` or
    /// `DECLARE_ATTRIBUTED_CONST` holds (`zend_constant_add_attributes`).
    View,
}

/// What [`unit_values`] refuses of a unit apart from its values (#4656):
/// an index past the table it names, a constant no publisher is shown to
/// write, and an op array a request numbered; and what [`Frozen::new`]
/// refuses of a declaration apart from its values, a closure's cell.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stray {
    /// An `IS_CONSTANT_AST` constant past its op array's `const_exprs`.
    PastConstExprs(u32),
    /// An `IS_PTR` constant past the unit's `constants`.
    PastConstants(u32),
    /// An `UNDEF` constant.
    Undef,
    /// An op array whose runtime cache is a request's number
    /// ([`MapPtrOffset::Request`]).
    Request(u32),
    /// A closure's cell in a declaration that is not the empty one, which
    /// names the unit the declaration was compiled in (#4684). Never
    /// [`unit_values`]': in a unit, the cell names the unit walked.
    Cell,
}

/// Each value `unit` holds, handed to `found` beside its [`Holder`] and
/// its [`Slot`]: the main op array's, then each function's declaration
/// and op array, each dynamic definition's, each class's, each `const`'s,
/// and each method's and hook's op array. An op array's constant that is
/// an index is decided here and never handed on (#4656): an
/// `IS_CONSTANT_AST` against that op array's `const_exprs` and an `IS_PTR`
/// against the unit's `constants`, each by its bound, and an `UNDEF`
/// refused.
///
/// # Errors
///
/// The first [`Stray`] the walk met, after it has handed on every value.
pub fn unit_values(unit: &Unit, found: &mut dyn FnMut(Holder, Slot, Zval)) -> Result<(), Stray> {
    let Unit {
        main,
        functions,
        classes,
        runtime_keys: _,
        top_level: _,
        constants,
        methods,
        hooks,
        completed: _,
        dynamic,
        diagnostics: _,
        arms: _,
        halt_offset: _,
        loop_vars: _,
        delayed_fetches: _,
        seen_symbols: _,
        cell: _,
    } = unit;
    let mut walk = Walk {
        found,
        constants: constants.len(),
        cells: false,
        stray: None,
    };
    walk.op_array(Holder::Main, main);
    for (decl, body) in functions {
        walk.function(Holder::Unit, decl);
        walk.op_array(Holder::Unit, body);
    }
    for (at, (decl, body)) in dynamic.iter().enumerate() {
        walk.function(Holder::Dynamic(at), decl);
        walk.op_array(Holder::Dynamic(at), body);
    }
    for class in classes {
        walk.class(class);
    }
    for ConstDecl {
        name: _,
        value,
        attributes,
    } in constants
    {
        walk.expr(Holder::Unit, Slot::View, value);
        walk.attributes(Holder::Unit, Slot::View, attributes);
    }
    for MethodBody {
        class: _,
        method: _,
        body,
    } in methods
    {
        walk.op_array(Holder::Unit, body);
    }
    for HookBody {
        class: _,
        property: _,
        kind: _,
        body,
    } in hooks
    {
        walk.op_array(Holder::Unit, body);
    }
    walk.stray.map_or(Ok(()), Err)
}

/// Each value the arguments of `attributes` hold ([`ConstExpr::UnitValue`]).
pub fn attribute_values(attributes: &[Attribute], found: &mut dyn FnMut(Zval)) {
    attribute_parts(attributes, found, &mut |_| {});
}

/// Each value the arguments of `attributes` hold, handed to `found`, and
/// each closure's cell in them, handed to `cell`.
fn attribute_parts(
    attributes: &[Attribute],
    found: &mut dyn FnMut(Zval),
    cell: &mut dyn FnMut(&UnitCell),
) {
    for Attribute {
        name: _,
        arguments,
        line: _,
        strict: _,
        own_name: _,
    } in attributes
    {
        for AttributeArgument { name: _, value } in arguments.as_slice() {
            expr_parts(value, found, cell);
        }
    }
}

/// Each value `expr` holds ([`ConstExpr::UnitValue`]) anywhere in its
/// tree.
pub fn expr_values(expr: &ConstExpr, found: &mut dyn FnMut(Zval)) {
    expr_parts(expr, found, &mut |_| {});
}

/// Each value `expr` holds anywhere in its tree, handed to `found`, and
/// each closure's cell, a `static function` node's or a `name(...)`
/// node's, handed to `cell`.
#[allow(clippy::too_many_lines)] // one arm per variant, its fields a line each
fn expr_parts(expr: &ConstExpr, found: &mut dyn FnMut(Zval), cell: &mut dyn FnMut(&UnitCell)) {
    match expr {
        ConstExpr::UnitValue(held) => found(held.value()),
        ConstExpr::Closure {
            at: _,
            name: _,
            unit,
        }
        | ConstExpr::FunctionCallable {
            name: _,
            name_kind: _,
            line: _,
            at: _,
            unit,
        } => cell(unit),
        ConstExpr::Unary {
            op: _,
            operand,
            line: _,
        }
        | ConstExpr::Unpack(operand)
        | ConstExpr::Cast {
            to: _,
            operand,
            line: _,
        }
        | ConstExpr::DynamicClassConstant {
            class: _,
            class_kind: _,
            name: operand,
            line: _,
        } => expr_parts(operand, found, cell),
        ConstExpr::Binary {
            op: _,
            lhs,
            rhs,
            line: _,
        } => {
            expr_parts(lhs, found, cell);
            expr_parts(rhs, found, cell);
        }
        ConstExpr::Dim {
            container: a,
            dim: b,
            line: _,
        }
        | ConstExpr::Prop {
            object: a,
            name: b,
            nullsafe: _,
            line: _,
        } => {
            expr_parts(a, found, cell);
            expr_parts(b, found, cell);
        }
        ConstExpr::Conditional {
            cond,
            then,
            otherwise,
        } => {
            for expr in [Some(cond), then.as_ref(), Some(otherwise)]
                .into_iter()
                .flatten()
            {
                expr_parts(expr, found, cell);
            }
        }
        ConstExpr::Array { elements, line: _ } => {
            for (key, value) in elements {
                for expr in key.iter().chain([value]) {
                    expr_parts(expr, found, cell);
                }
            }
        }
        ConstExpr::New {
            class: _,
            arguments,
            line: _,
        } => arguments
            .iter()
            .for_each(|(_, value)| expr_parts(value, found, cell)),
        ConstExpr::Null
        | ConstExpr::Bool(_)
        | ConstExpr::Long { value: _, radix: _ }
        | ConstExpr::Double(_)
        | ConstExpr::Str(_)
        | ConstExpr::Constant { name: _, line: _ }
        | ConstExpr::ClassConstant {
            class: _,
            class_kind: _,
            name: _,
            line: _,
        }
        | ConstExpr::EnumCase {
            class: _,
            case: _,
            line: _,
        }
        | ConstExpr::ClassName { class: _, line: _ }
        | ConstExpr::MethodCallable {
            class: _,
            method: _,
            line: _,
        }
        | ConstExpr::ScopeClass => {}
    }
}

/// The walk over one unit or one declaration: where it hands each value,
/// the bound of the unit's `constants`, whether a closure's cell that is
/// not empty is refused, and the first [`Stray`] it met.
struct Walk<'f> {
    found: &'f mut dyn FnMut(Holder, Slot, Zval),
    constants: usize,
    cells: bool,
    stray: Option<Stray>,
}

impl Walk<'_> {
    fn stray(&mut self, stray: Stray) {
        self.stray = self.stray.or(Some(stray));
    }

    /// A cell that is not the empty one, refused where the walk refuses
    /// cells. Not bound is not empty: a shared cell is bound when its unit
    /// is published.
    fn cell(&mut self, met: bool) {
        if met && self.cells {
            self.stray(Stray::Cell);
        }
    }

    /// The index `at` a constant holds, refused as `past` names it where it is not
    /// below `bound`.
    fn index(&mut self, at: Option<u32>, bound: usize, past: fn(u32) -> Stray) {
        let at = at.unwrap_or(u32::MAX);
        if !usize::try_from(at).is_ok_and(|at| at < bound) {
            self.stray(past(at));
        }
    }

    fn op_array(&mut self, holder: Holder, code: &OpArray) {
        let OpArray {
            oplines: _,
            literals,
            const_exprs,
            vars: _,
            num_cvs: _,
            num_tmps: _,
            try_catch: _,
            live_ranges: _,
            safepoints: _,
            filename: _,
            line_start: _,
            line_end: _,
            cache_size: _,
            cache_room: _,
            runtime_cache,
            dynamic_func_defs: _,
            nested: _,
            bound_vars,
            implicit_uses: _,
            loops: _,
            no_receiver: _,
            strict_types: _,
            return_mask: _,
            numeric_dims: _,
        } = code;
        match *runtime_cache {
            MapPtrOffset::Engine(_) => {}
            MapPtrOffset::Request(number) => self.stray(Stray::Request(number)),
        }
        for &z in &**literals {
            match z.kind() {
                ZvalType::Undef => self.stray(Stray::Undef),
                ZvalType::ConstExpr => {
                    self.index(z.as_const_expr(), const_exprs.len(), Stray::PastConstExprs);
                }
                ZvalType::Ptr => self.index(z.as_ptr(), self.constants, Stray::PastConstants),
                ZvalType::Null
                | ZvalType::False
                | ZvalType::True
                | ZvalType::Long
                | ZvalType::Double
                | ZvalType::String
                | ZvalType::Array
                | ZvalType::Object
                | ZvalType::Resource
                | ZvalType::Reference
                | ZvalType::Indirect => (self.found)(holder, Slot::Constant, z),
            }
        }
        for BoundVar {
            name: _,
            value,
            lexical: _,
        } in bound_vars
        {
            (self.found)(holder, Slot::Bound, *value);
        }
        for expr in const_exprs {
            self.expr(holder, Slot::Tree, expr);
        }
    }

    fn function(&mut self, holder: Holder, decl: &FunctionDecl) {
        let FunctionDecl {
            name: _,
            parameters,
            return_type: _,
            returns_by_ref: _,
            is_generator: _,
            attributes,
            doc_comment: _,
            file: _,
            line: _,
            end_line: _,
            frameless: _,
        } = decl;
        // The slots through their `Deref`, since their fields are the
        // declaration module's, where they are built: a program's always
        // are. A native's that no reader has built are its entries,
        // which are left unbuilt and not read: each is the binary's own,
        // its attributes `'static` and its default made by a `fn` where a
        // reader builds the slots, so none holds a value a unit or a
        // request laid.
        for Parameter {
            name: _,
            ty: _,
            by_ref: _,
            prefer_ref: _,
            promoted: _,
            variadic: _,
            default,
            attributes,
        } in parameters.unbuilt().map_or_else(|| &**parameters, |_| &[])
        {
            match default {
                ParameterDefault::None | ParameterDefault::Unshown => {}
                ParameterDefault::Expr(expr) => self.expr(holder, Slot::View, expr),
            }
            self.attributes(holder, Slot::Declared, attributes);
        }
        self.attributes(holder, Slot::Declared, attributes);
    }

    fn class(&mut self, class: &ClassDecl) {
        let ClassDecl {
            name: _,
            kind: _,
            flags: _,
            parent: _,
            interfaces: _,
            implementors: _,
            aliases: _,
            traits: _,
            adaptations: _,
            backing: _,
            constants,
            properties,
            methods,
            attributes,
            doc_comment: _,
            file: _,
            line: _,
            end_line: _,
            object_size: _,
            object_beside: _,
            attribute_validator: _,
        } = class;
        let unit = Holder::Unit;
        self.attributes(unit, Slot::Declared, attributes);
        for ClassConstDecl {
            name: _,
            visibility: _,
            is_final: _,
            is_case: _,
            ty: _,
            value,
            attributes,
            doc_comment: _,
        } in constants
        {
            self.expr(unit, Slot::Declared, value);
            self.attributes(unit, Slot::Declared, attributes);
        }
        for PropertyDecl {
            name: _,
            visibility: _,
            set_visibility: _,
            is_instance: _,
            is_readonly: _,
            is_final: _,
            is_promoted: _,
            is_virtual: _,
            ty: _,
            default,
            attributes,
            doc_comment: _,
            hooks,
        } in properties.as_slice()
        {
            if let Some(default) = default {
                self.expr(unit, Slot::Declared, default);
            }
            self.attributes(unit, Slot::Declared, attributes);
            if let Some(hooks) = hooks.as_deref() {
                let PropertyHooks {
                    get,
                    set,
                    is_abstract: _,
                } = hooks;
                get.iter().chain(set).for_each(|hook| self.method(hook));
            }
        }
        for method in methods {
            self.method(method);
        }
    }

    fn method(&mut self, method: &MethodDecl) {
        let MethodDecl {
            function,
            visibility: _,
            is_instance: _,
            is_abstract: _,
            is_final: _,
            tentative_return: _,
        } = method;
        self.function(Holder::Unit, function);
    }

    fn expr(&mut self, holder: Holder, slot: Slot, expr: &ConstExpr) {
        let (found, mut met) = (&mut *self.found, false);
        let empty = UnitCell::default();
        expr_parts(expr, &mut |z| found(holder, slot, z), &mut |cell| {
            met |= *cell != empty;
        });
        self.cell(met);
    }

    fn attributes(&mut self, holder: Holder, slot: Slot, attributes: &[Attribute]) {
        let (found, mut met) = (&mut *self.found, false);
        let empty = UnitCell::default();
        attribute_parts(attributes, &mut |z| found(holder, slot, z), &mut |cell| {
            met |= *cell != empty;
        });
        self.cell(met);
    }
}

mod sealed {
    /// What [`super::Values`] asks and no type outside the crate answers:
    /// whether every value the walk reaches is the engine's.
    pub trait Walked {
        fn frozen(&self) -> bool;
    }
}

/// What a [`Frozen`] can hold: a unit or a class declaration, the two
/// the engine publishes for every request, and the walk over each. Sealed,
/// so no type outside this crate is one.
pub trait Values: sealed::Walked {}

/// Every value the unit holds passes [`FrozenZval::of`], each of its op
/// arrays' constants that is an index, an `IS_CONSTANT_AST` or an
/// `IS_PTR`, names an entry of its own tables, and every op array is
/// numbered by the engine.
impl sealed::Walked for PublishedUnit {
    fn frozen(&self) -> bool {
        let mut values = true;
        let walked = unit_values(self, &mut |_, _, z| values &= FrozenZval::of(z).is_some());
        values && walked.is_ok()
    }
}

/// Every value the declaration holds passes [`FrozenZval::of`], and every
/// closure's cell in it is empty ([`Stray::Cell`]); a declaration holds no
/// index, which only an op array's constant is.
impl sealed::Walked for ClassDecl {
    fn frozen(&self) -> bool {
        let mut values = true;
        let mut walk = Walk {
            found: &mut |_, _, z| values &= FrozenZval::of(z).is_some(),
            constants: 0,
            cells: true,
            stray: None,
        };
        walk.class(self);
        let walked = walk.stray.is_none();
        values && walked
    }
}

impl Values for PublishedUnit {}
impl Values for ClassDecl {}

/// A unit or a class declaration the engine published for every request,
/// read and never written: made only by [`Frozen::new`], which walks every
/// value it holds. A request's own unit is never one.
#[derive(Debug, Clone)]
pub struct Frozen<T>(T);

impl<T: Values> Frozen<T> {
    /// `value`, where every value the walk reaches is a scalar or a
    /// published string or constant array of the engine's table
    /// ([`FrozenZval::of`]), and, for a unit, each of its constants that is
    /// an index, an `IS_CONSTANT_AST` or an `IS_PTR`, is below its table's
    /// bound, none is `UNDEF`, and every op array's
    /// offset is [`MapPtrOffset::Engine`] ([`unit_values`]); for a
    /// declaration, every closure's cell in it is empty ([`Stray::Cell`]).
    ///
    /// # Errors
    ///
    /// `value` back, where any of those does not hold.
    pub fn new(value: T) -> Result<Frozen<T>, T> {
        if value.frozen() {
            Ok(Frozen(value))
        } else {
            Err(value)
        }
    }
}

impl<T> Deref for Frozen<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.0
    }
}

/// The one hand impl `state.engine` admits outside the homes (#4633,
/// ADR-0005). A [`Frozen`] is made by [`Frozen::new`] alone, which has
/// read every value the unit or declaration holds through the walk above,
/// which names every field and variant of the unit format, and admitted
/// each only where [`FrozenZval::of`] does: a scalar, or a string or
/// constant array the engine's table published, which no request owns or
/// counts and which lives as long as the engine. Each of a unit's
/// constants that is an index names an entry of its own tables, an
/// `IS_CONSTANT_AST` of its op array's `const_exprs` and an `IS_PTR` of
/// the unit's `constants`; no other index the unit holds, an opline's or
/// a table's, is checked, since none reaches a request's heap (#4684).
/// Each op array is numbered by the engine's map-pointer space, not a
/// request's.
/// It is read and never written, so what the check read stands while it
/// is held.
///
/// No field the walk reads names anything beside the value walked but
/// one, a closure's cell ([`crate::UnitCell`]) in a constant expression,
/// which names a unit. In a unit, every closure's cell is a
/// share of the one cell its maker laid, the compiler's
/// ([`crate::UnitCell::share`]) or the cache reader's, which
/// [`Unit::published`] binds to the published copy the first time the
/// unit is published; a copy published again keeps that first binding.
/// The engine publishes only a unit fresh from a compile or from the
/// cache's reader (`publish_compiled`), once, so the cell names the unit
/// walked: that holds by how the engine makes a unit, and the walk does
/// not check it. So no pointer into a request's heap is reachable through
/// a frozen unit, and it may be moved and shared as engine state.
///
/// A declaration is walked apart from any unit, so a cell in it that is
/// not empty names the unit the declaration was compiled in, which may be
/// a request's own. No frozen declaration holds one: the walk over a
/// declaration refuses every cell that is not the empty one, bound or not
/// ([`Stray::Cell`]), wherever a constant expression sits in it, a class
/// constant, a property's default, an attribute's argument or a
/// parameter's default. So no unit is reachable through a frozen
/// declaration, by the type and the check alone, whoever makes one.
impl<T: Values + Send + Sync + 'static> EngineState for Frozen<T> {}
