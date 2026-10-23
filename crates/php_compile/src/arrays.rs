//! The arrays a unit holds as values ([`ConstExpr::UnitArray`]): the
//! walks over its declarations and its constant expressions that the
//! engine gives the compile's owner of each array by, as
//! [`crate::declared_names`] walks the names a unit's declarations are
//! read by.

use php_heap::Zval;
use php_unit::Unit;
use php_unit::decl::{Attribute, ConstExpr, FunctionDecl};

/// What holds an array of [`declared_arrays`]: the unit, for a function,
/// a class and its members, or the dynamic definition at its place.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Holder {
    /// The unit's tables, held while the unit lives.
    Unit,
    /// The dynamic definition at this place of [`Unit::dynamic`].
    Dynamic(usize),
}

/// The arrays `unit`'s declarations hold as values
/// ([`ConstExpr::UnitArray`]), each beside what holds it, as php's
/// compile leaves each zval in a table of the declaration's: a class's
/// constant, default, static default and attribute argument, its
/// methods' and hooks' among them, the class's; a function's attribute
/// argument the unit's, and a dynamic definition's its own. A
/// parameter's default and a `const`'s value and attributes are not
/// among them: the `RECV_INIT` or `DECLARE_CONST` constant, or the
/// `DECLARE_ATTRIBUTED_CONST`'s data, holds the array, of which the
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
pub fn declared_arrays(unit: &Unit) -> Vec<(Zval, Holder)> {
    let mut found = Vec::new();
    let function = |decl: &FunctionDecl, holder: Holder, found: &mut Vec<(Zval, Holder)>| {
        let attributes = (decl.parameters.iter().map(|p| &p.attributes)).chain([&decl.attributes]);
        for list in attributes {
            attribute_arrays(list, &mut |z| found.push((z, holder)));
        }
    };
    for (decl, _) in &unit.functions {
        function(decl, Holder::Unit, &mut found);
    }
    for (at, (decl, _)) in unit.dynamic.iter().enumerate() {
        function(decl, Holder::Dynamic(at), &mut found);
    }
    for class in &unit.classes {
        let mut push = |z| found.push((z, Holder::Unit));
        attribute_arrays(&class.attributes, &mut push);
        for constant in &class.constants {
            unit_arrays(&constant.value, &mut push);
            attribute_arrays(&constant.attributes, &mut push);
        }
        for property in &class.properties {
            property
                .default
                .iter()
                .for_each(|d| unit_arrays(d, &mut push));
            attribute_arrays(&property.attributes, &mut push);
        }
        let hooks = (class.properties.iter())
            .filter_map(|property| property.hooks.as_deref())
            .flat_map(|hooks| hooks.get.iter().chain(&hooks.set));
        for method in class.methods.iter().chain(hooks) {
            function(&method.function, Holder::Unit, &mut found);
        }
    }
    found
}

/// Each array `expr` holds ([`ConstExpr::UnitArray`]) anywhere in its tree.
pub fn unit_arrays(expr: &ConstExpr, found: &mut dyn FnMut(Zval)) {
    let mut each = |expr: &ConstExpr| unit_arrays(expr, found);
    match expr {
        ConstExpr::UnitArray(array) => found(array.value()),
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

/// Each array the arguments of `attributes` hold.
pub fn attribute_arrays(attributes: &[Attribute], found: &mut dyn FnMut(Zval)) {
    for argument in attributes.iter().flat_map(|attribute| &attribute.arguments) {
        unit_arrays(&argument.value, found);
    }
}

/// Whether `unit` holds a counted array anywhere: among an op array's
/// constants, inside a constant expression one keeps, or in a
/// declaration ([`declared_arrays`]). A unit the cache keeps holds none
/// (design §3.4).
pub fn holds_counted_array(unit: &Unit) -> bool {
    let mut counted = false;
    let mut note = |z: Zval| counted |= z.as_arr().is_some() && z.is_refcounted();
    let bodies = (unit.functions.iter().chain(&unit.dynamic))
        .map(|(_, body)| body)
        .chain(unit.methods.iter().map(|method| &method.body))
        .chain(unit.hooks.iter().map(|hook| &hook.body));
    for body in std::iter::once(&unit.main).chain(bodies) {
        body.literals.iter().for_each(|z| note(*z));
        body.const_exprs
            .iter()
            .for_each(|expr| unit_arrays(expr, &mut note));
    }
    for constant in &unit.constants {
        attribute_arrays(&constant.attributes, &mut note);
    }
    for (z, _) in declared_arrays(unit) {
        note(z);
    }
    counted
}
