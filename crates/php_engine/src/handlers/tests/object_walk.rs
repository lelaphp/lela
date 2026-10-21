//! A `foreach` over an object: the properties the scope may see, walked
//! in order.

use super::*;

/// #367, the first acceptance: a `foreach` by value over a plain
/// object walks the properties the scope may see, the private one
/// stepped over from outside the class, the key the name as
/// written. The witness beside [`fe_reset_r`] pins the same line
/// against the oracle; this holds the engine to it.
#[test]
fn a_foreach_over_an_object_walks_the_properties_the_scope_may_see() {
    let served = Engine::cli_builder().build().serve_capture(
        "<?php class P { public $a = 1; private $b = 2; public $c = 3; } foreach (new P as $k => $v) echo \"$k=$v \";",
        crate::test::Capture::default(),
    );
    served.assert_clean("a=1 c=3 ");
}

/// The same loop from inside the class sees every property, as
/// `zend_check_property_access` lets the declaring scope through.
#[test]
fn a_foreach_over_this_walks_the_private_properties_too() {
    let served = Engine::cli_builder().build().serve_capture(
        "<?php class C { public $n = 1; protected $p = 2; public $s = 'x'; private $q = 3; public function all() { foreach ($this as $k => $v) echo \"$k=$v \"; } } $c = new C; foreach ($c as $k => $v) echo \"$k=$v \"; $c->all();",
        crate::test::Capture::default(),
    );
    served.assert_clean("n=1 s=x n=1 p=2 s=x q=3 ");
}

/// #367, the second acceptance: an object of no properties runs the
/// body no time, and a property declared with no default is the
/// `null` `default_properties_table` put there, walked like any
/// other.
#[test]
fn a_foreach_over_an_object_of_no_properties_runs_its_body_no_time() {
    let served = Engine::cli_builder().build().serve_capture(
        "<?php foreach (new stdClass as $v) echo 'no'; class U { public $kept = 2; public $null; } foreach (new U as $k => $v) echo $k, ' ';",
        crate::test::Capture::default(),
    );
    served.assert_clean("kept null ");
}

/// The walk itself: every property `Exception` declares is
/// protected or private, so the global scope may see none of them
/// and `zend_fe_fetch_object_helper` steps over the whole table,
/// which is the jump past the loop.
#[test]
fn the_walk_over_an_object_of_properties_the_scope_may_not_see_finds_nothing() {
    let engine = Engine::cli_builder().build();
    let mut req = Request::new(Arc::clone(&engine.shared), RequestDescription::default());
    let class = req
        .class_named(Bare::resolved(b"exception"))
        .expect("the engine declares Exception");
    let object =
        crate::api::call::new_object_made(&mut req, class).expect("Exception is instantiable");
    let idx = req
        .object_walk_start(object)
        .expect("the walk starts")
        .expect("Exception declares properties, so the walk is registered");
    assert!(
        matches!(req.object_walk(object, idx, false), Ok(None)),
        "the global scope sees none of Exception's properties"
    );
    req.heap_mut().iterator_del(idx);
    req.release(object.as_value());
}
