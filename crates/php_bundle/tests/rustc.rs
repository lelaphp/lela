//! What the expansion relies on rustc for, pinned by name: a body-less
//! `fn` and a `const` with no value in an `impl` reach the attribute as
//! tokens and are not refused by the parser first. The workspace pins its
//! toolchain; a bump that changes this fails here.

use php_ext::abi::{Members, This};
use php_ext::attr::php_class;
use php_ext::values::PhpString;

#[php_class(interface)]
pub(crate) struct Shape;

#[php_class]
impl Shape {
    fn area(this: This<Self>) -> f64;
}

#[php_class(abstract)]
pub(crate) struct Base;

#[php_class]
impl Base {
    #[php(property)]
    const label: PhpString;

    fn make(this: This<Self>) -> i64;
    fn real(this: This<Self>) -> i64 {
        let _ = this;
        1
    }
}

#[php_class(extends(Base))]
pub(crate) struct Leaf;

#[php_class]
impl Leaf {
    #[php(implementation = Base::real)]
    fn make(this: This<Self>) -> i64;
}

#[test]
fn a_bodyless_fn_and_const_reach_the_attribute() {
    let methods = <Shape as Members>::METHODS;
    assert!(methods.len() == 1 && methods[0].is_abstract);
    let properties = <Base as Members>::PROPERTIES;
    assert_eq!(properties.len(), 1);
    assert!(properties[0].default.is_none());
    let methods = <Base as Members>::METHODS;
    assert!(methods[0].is_abstract && !methods[1].is_abstract);
    assert!(!<Leaf as Members>::METHODS[0].is_abstract);
}
