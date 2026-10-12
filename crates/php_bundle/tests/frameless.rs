//! The bundle's frameless table against php's `zend_flf_functions`: the
//! rows `zend_register_functions` appends as each extension registers its
//! functions, Core's first and then the bundle's extensions in
//! `internal_functions.c`'s order, each native's arities in its
//! `frameless_function_infos_<name>` order, and none for an alias. A
//! `FRAMELESS_ICALL_*` carries its row in `extended_value`, so a row out
//! of php's place is a unit whose oplines are not php's (#2673).

use php_ext::test::Engine;

/// php 8.5's 28 rows, read off `Zend/zend_builtin_functions_arginfo.h`,
/// `ext/pcre/php_pcre_arginfo.h` and `ext/standard/basic_functions_arginfo.h`
/// at the pinned tag: the native each row serves, in the table's order.
const PHP: [&str; 28] = [
    "property_exists",
    "class_exists",
    "class_exists",
    "preg_match",
    "preg_replace",
    "min",
    "max",
    "in_array",
    "in_array",
    "trim",
    "trim",
    "implode",
    "implode",
    "dirname",
    "dirname",
    "strstr",
    "strstr",
    "strpos",
    "strpos",
    "str_contains",
    "str_starts_with",
    "substr",
    "substr",
    "strtr",
    "strtr",
    "str_replace",
    "dechex",
    "is_numeric",
];

#[test]
fn the_bundles_frameless_table_is_zend_flf_functions_in_php_order() {
    let extensions: Vec<_> = php_bundle::enabled().iter().map(|made| made()).collect();
    let engine = Engine::builder().extensions(&extensions).build();
    let rows: Vec<String> = engine
        .frameless()
        .iter()
        .map(|name| String::from_utf8_lossy(&name.0).into_owned())
        .collect();
    assert_eq!(rows, PHP);
}
