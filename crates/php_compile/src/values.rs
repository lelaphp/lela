//! The values a unit's declarations hold, its arrays and its strings
//! ([`ConstExpr::UnitValue`]): the walks over its
//! declarations and its constant expressions that the engine gives the
//! compile's owner of each value by, as [`crate::named`] walks the
//! names a unit's declarations own.

use php_heap::Zval;
use php_unit::Unit;
use php_unit::decl::{Attribute, ConstExpr, FunctionDecl};

/// What holds a value of [`declared_values`] or a name of
/// [`crate::named`]: the unit, for a function, a class and its members,
/// the dynamic definition at its place, or the main op array, whose
/// variables' names alone it holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Holder {
    /// The main op array, held while it runs.
    Main,
    /// The unit's tables, held while the unit lives.
    Unit,
    /// The dynamic definition at this place of [`Unit::dynamic`].
    Dynamic(usize),
}

/// The values `unit`'s declarations hold ([`ConstExpr::UnitValue`]), each
/// beside what holds it, as php's
/// compile leaves each zval in a table of the declaration's: a class's
/// constant, default, static default and attribute argument, its
/// methods' and hooks' among them, the class's; a function's attribute
/// argument the unit's, and a dynamic definition's its own. A
/// parameter's default and a `const`'s value and attributes are not
/// among them: the `RECV_INIT` or `DECLARE_CONST` constant, or the
/// `DECLARE_ATTRIBUTED_CONST`'s data, holds the value, of which the
/// declaration is a view, and the constant takes its own owner as it is
/// defined (`zend_constant_add_attributes`).
///
/// So each outlives the `eval` that declared it, read after the heap is
/// churned:
///
/// ```php
/// eval('class K { const C = [1, 2]; public $p = [3, 4]; public static $s = [5, 6]; }');
/// eval('enum E1 { case A; const L = [7, 8]; }');
/// eval('#[Attribute(Attribute::TARGET_ALL)] class At { function __construct(public $v) {} }');
/// eval('#[At([9, 10])] class Q { #[At([11])] function m(#[At([12])] $a) {} }');
/// eval('$GLOBALS["cl"] = #[At([13])] function () {};');
/// eval('#[At([14])] const Z = 1;');
/// $junk = array_fill(0, 1000, str_repeat("x", 100));
/// echo json_encode([K::C, (new K)->p, K::$s, E1::L]), "\n";
/// $m = new ReflectionMethod('Q', 'm');
/// echo json_encode([
///     (new ReflectionClass('Q'))->getAttributes()[0]->newInstance()->v,
///     $m->getAttributes()[0]->newInstance()->v,
///     $m->getParameters()[0]->getAttributes()[0]->newInstance()->v,
///     (new ReflectionFunction($cl))->getAttributes()[0]->newInstance()->v,
///     (new ReflectionConstant('Z'))->getAttributes()[0]->newInstance()->v,
/// ]), "\n";
/// ```
/// ```expect regime=jit-off
/// [[1,2],[3,4],[5,6],[7,8]]
/// [[9,10],[11],[12],[13],[14]]
/// ```
pub fn declared_values(unit: &Unit) -> Vec<(Zval, Holder)> {
    let mut found = Vec::new();
    for (holder, decl) in function_decls(unit) {
        let attributes = (decl.parameters.iter().map(|p| &p.attributes)).chain([&decl.attributes]);
        for list in attributes {
            attribute_values(list, &mut |z| found.push((z, holder)));
        }
    }
    let mut push = |z| found.push((z, Holder::Unit));
    for class in &unit.classes {
        attribute_values(&class.attributes, &mut push);
        for constant in &class.constants {
            unit_values(&constant.value, &mut push);
            attribute_values(&constant.attributes, &mut push);
        }
        for property in &class.properties {
            property
                .default
                .iter()
                .for_each(|d| unit_values(d, &mut push));
            attribute_values(&property.attributes, &mut push);
        }
    }
    found
}

/// Each function `unit` declares beside what holds it: a function, a
/// method and a hook the unit, a dynamic definition its own.
fn function_decls(unit: &Unit) -> impl Iterator<Item = (Holder, &FunctionDecl)> {
    let members = unit.classes.iter().flat_map(|class| {
        let hooks = (class.properties.iter())
            .filter_map(|property| property.hooks.as_deref())
            .flat_map(|hooks| hooks.get.iter().chain(&hooks.set));
        class
            .methods
            .iter()
            .chain(hooks)
            .map(|method| &method.function)
    });
    (unit.functions.iter().map(|(decl, _)| (Holder::Unit, decl)))
        .chain((unit.dynamic.iter().enumerate()).map(|(at, (decl, _))| (Holder::Dynamic(at), decl)))
        .chain(members.map(|decl| (Holder::Unit, decl)))
}

/// Each value of the unit's `expr` holds ([`ConstExpr::UnitValue`])
/// anywhere in its tree.
pub fn unit_values(expr: &ConstExpr, found: &mut dyn FnMut(Zval)) {
    let mut each = |expr: &ConstExpr| unit_values(expr, found);
    match expr {
        ConstExpr::UnitValue(held) => found(held.value()),
        ConstExpr::Unary { operand, .. }
        | ConstExpr::Unpack(operand)
        | ConstExpr::Cast { operand, .. }
        | ConstExpr::DynamicClassConstant { name: operand, .. } => each(operand),
        ConstExpr::Binary { lhs: a, rhs: b, .. }
        | ConstExpr::Dim {
            container: a,
            dim: b,
            ..
        }
        | ConstExpr::Prop {
            object: a, name: b, ..
        } => [a, b].into_iter().for_each(|e| each(e)),
        ConstExpr::Conditional {
            cond,
            then,
            otherwise,
        } => {
            [Some(cond), then.as_ref(), Some(otherwise)]
                .into_iter()
                .flatten()
                .for_each(|e| each(e));
        }
        ConstExpr::Array { elements, .. } => {
            for (key, value) in elements {
                key.iter().chain([value]).for_each(&mut each);
            }
        }
        ConstExpr::New { arguments, .. } => arguments.iter().for_each(|(_, value)| each(value)),
        _ => {}
    }
}

/// Each value of the unit's the arguments of `attributes` hold.
pub fn attribute_values(attributes: &[Attribute], found: &mut dyn FnMut(Zval)) {
    for argument in attributes.iter().flat_map(|attribute| &attribute.arguments) {
        unit_values(&argument.value, found);
    }
}

/// Whether `unit` holds a counted value anywhere: among an op array's
/// constants, inside a constant expression one keeps, or in a
/// declaration ([`declared_values`]). A unit the cache keeps holds none
/// (design §3.4).
pub fn holds_counted_value(unit: &Unit) -> bool {
    let mut counted = false;
    let mut note = |z: Zval| counted |= z.is_refcounted();
    let bodies = (unit.functions.iter().chain(&unit.dynamic))
        .map(|(_, body)| body)
        .chain(unit.methods.iter().map(|method| &method.body))
        .chain(unit.hooks.iter().map(|hook| &hook.body));
    for body in std::iter::once(&unit.main).chain(bodies) {
        body.literals.iter().for_each(|z| note(*z));
        body.const_exprs
            .iter()
            .for_each(|expr| unit_values(expr, &mut note));
    }
    for constant in &unit.constants {
        attribute_values(&constant.attributes, &mut note);
    }
    for (z, _) in declared_values(unit) {
        note(z);
    }
    counted
}
