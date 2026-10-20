//! `debug_backtrace` and `debug_print_backtrace`, the two natives of
//! `zend_builtin_functions.c` over `zend_fetch_debug_backtrace`: the
//! walk a throwable's trace is built by ([`Request::fetch_debug_backtrace`]),
//! with the native's own link left out and php's options and limit, and
//! the rendering its trace string is made by.

use php_macros::php_function;

use crate::api::raising::Thrown;
use crate::api::request::Request;
use crate::api::values::{Owner, PhpArray, Value};

/// `debug_backtrace(int $options = DEBUG_BACKTRACE_PROVIDE_OBJECT, int
/// $limit = 0): array`, php's `ZEND_FUNCTION(debug_backtrace)`
/// (`Z/Zend/zend_builtin_functions.c:2230`), `zend_fetch_debug_backtrace`
/// with `skip_last`: one row per link below this native, each `file`
/// and `line` where it was called, `function`, and for a method
/// `class`, its receiver as `object` under
/// `DEBUG_BACKTRACE_PROVIDE_OBJECT` and `type`, then `args`. The object
/// stands between `class` and `type`, where php appends it.
///
/// ```php
/// class C { function m($x) { debug_print_backtrace(); return debug_backtrace(); } } function f() { return (new C)->m([1]); } var_dump(f());
/// ```
/// ```expect
/// #0 %s(2): C->m(Array)
/// #1 %s(2): f()
/// array(2) {
///   [0]=>
///   array(7) {
///     ["file"]=>
///     string(%d) "%s"
///     ["line"]=>
///     int(2)
///     ["function"]=>
///     string(1) "m"
///     ["class"]=>
///     string(1) "C"
///     ["object"]=>
///     object(C)#1 (0) {
///     }
///     ["type"]=>
///     string(2) "->"
///     ["args"]=>
///     array(1) {
///       [0]=>
///       array(1) {
///         [0]=>
///         int(1)
///       }
///     }
///   }
///   [1]=>
///   array(4) {
///     ["file"]=>
///     string(%d) "%s"
///     ["line"]=>
///     int(2)
///     ["function"]=>
///     string(1) "f"
///     ["args"]=>
///     array(0) {
///     }
///   }
/// }
/// ```
///
/// `DEBUG_BACKTRACE_IGNORE_ARGS` leaves `args` out, a `limit` of one
/// keeps the first row, options of zero keep the receiver out, and a
/// call at the top level has no row at all.
///
/// ```php
/// class C { static function s($a) { return debug_backtrace(DEBUG_BACKTRACE_IGNORE_ARGS, 1); } function m() { return debug_backtrace(0); } } function f($x) { return C::s($x); }
/// var_dump(f(7), (new C)->m(), debug_backtrace());
/// ```
/// ```expect
/// array(1) {
///   [0]=>
///   array(5) {
///     ["file"]=>
///     string(%d) "%s"
///     ["line"]=>
///     int(2)
///     ["function"]=>
///     string(1) "s"
///     ["class"]=>
///     string(1) "C"
///     ["type"]=>
///     string(2) "::"
///   }
/// }
/// array(1) {
///   [0]=>
///   array(6) {
///     ["file"]=>
///     string(%d) "%s"
///     ["line"]=>
///     int(3)
///     ["function"]=>
///     string(1) "m"
///     ["class"]=>
///     string(1) "C"
///     ["type"]=>
///     string(2) "->"
///     ["args"]=>
///     array(0) {
///     }
///   }
/// }
/// array(0) {
/// }
/// ```
#[php_function]
fn debug_backtrace(
    req: &mut Request,
    #[php(default = DEBUG_BACKTRACE_PROVIDE_OBJECT)] options: i64,
    #[php(default = 0)] limit: i64,
) -> Result<Owner<PhpArray>, Thrown> {
    let made = req.fetch_debug_backtrace(true, options, limit);
    crate::api::values::adopted(made)
}

/// `debug_print_backtrace(int $options = 0, int $limit = 0): void`,
/// php's `ZEND_FUNCTION(debug_print_backtrace)`
/// (`Z/Zend/zend_builtin_functions.c:1887`): the same rows rendered by
/// `zend_trace_to_string` without `{main}`, each line closed by its
/// newline, and written; nothing at the top level.
///
/// ```php
/// function g() { debug_print_backtrace(DEBUG_BACKTRACE_IGNORE_ARGS); debug_print_backtrace(0, 1); debug_print_backtrace(); }
/// function h() { g(); } h("a"); debug_print_backtrace(); echo "|\n";
/// ```
/// ```expect
/// #0 %s(3): g()
/// #1 %s(3): h()
/// #0 %s(3): g()
/// #0 %s(3): g()
/// #1 %s(3): h('a')
/// |
/// ```
#[php_function]
fn debug_print_backtrace(
    req: &mut Request,
    #[php(default = 0)] options: i64,
    #[php(default = 0)] limit: i64,
) -> Result<(), Thrown> {
    let trace = req.fetch_debug_backtrace(true, options, limit)?;
    let text = req.trace_to_string(trace, false);
    req.release(Value::array(trace));
    req.response_mut().write(text.as_bytes());
    Ok(())
}
