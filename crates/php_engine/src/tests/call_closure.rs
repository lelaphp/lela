//! A call's owner of the `Closure` it was made through, taken off the
//! call for the closure `CALLABLE_CONVERT` writes (#3335): the header holds none
//! once it is taken, so a second take finds none and the call's pop
//! gives up no owner of the closure.

use php_unit::decl::Bare;

use crate::exec::Header;

#[test]
fn a_call_s_closure_taken_leaves_the_call_holding_none() {
    let mut req = crate::test::request();
    let closure = req.new_object_of("stdClass").expect("a stdClass");
    let function = req
        .function_named(Bare::resolved(b"strlen"))
        .expect("strlen is declared");
    let mut header = Header::call(function, 0);
    header.set_closure(Some(closure));
    let call = req.push_call(header).expect("the call is pushed");

    let taken = req
        .take_call_closure(call)
        .map(|taken| taken.as_value().to_zval());
    assert!(
        taken == Some(closure.as_value().to_zval()),
        "the call's closure"
    );
    assert!(
        req.frame_at(call).is_some_and(|at| at.closure().is_none()),
        "the header's closure word is cleared"
    );
    assert!(req.take_call_closure(call).is_none(), "taken once");

    req.pop_call();
    assert_eq!(
        closure.as_value().refcount(),
        1,
        "the pop gave up no owner of it"
    );
    req.release(closure.as_value());
}
