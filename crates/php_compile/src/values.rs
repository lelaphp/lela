//! The values a unit's declarations hold, its arrays and its strings
//! ([`ConstExpr::UnitValue`]), and whether a unit holds a counted one:
//! each read through `php_unit`'s one walk over a unit's values
//! ([`frozen::unit_values`]), as [`crate::named`] walks the names a
//! unit's declarations own.
//!
//! [`ConstExpr::UnitValue`]: php_unit::decl::ConstExpr::UnitValue

use php_heap::Zval;
use php_unit::Unit;
use php_unit::frozen::{self, Holder, Slot};

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
    // An index or an op array the walk refuses is a publisher's to
    // refuse; what it hands on is every value a declaration holds.
    frozen::unit_values(unit, &mut |holder, slot, z| {
        if slot == Slot::Declared {
            found.push((z, holder));
        }
    })
    .ok();
    found
}

/// Whether `unit` holds a counted value anywhere the walk reaches: among
/// an op array's constants and bound variables, inside a constant
/// expression one keeps, or in a declaration ([`declared_values`]). A
/// unit the cache keeps holds none (design §3.4). The engine asks it
/// before its compile's strings are published, where a cache-bound
/// compile's strings do not yet carry the bit `FrozenZval::of` reads
/// (#4656), so it asks each value whether it is counted, which no string
/// or constant array of such a compile is.
pub fn holds_counted_value(unit: &Unit) -> bool {
    let mut counted = false;
    frozen::unit_values(unit, &mut |_, _, z| counted |= z.is_refcounted()).ok();
    counted
}
