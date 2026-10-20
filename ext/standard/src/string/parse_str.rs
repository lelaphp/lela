//! `parse_str` over `php_default_treat_data(PARSE_STRING, ...)`
//! (`Z/main/php_variables.c:462`), which is the engine's variable
//! registration, the one `$_GET` is built by.

use php_ext::abi::{Ref, Untyped};
use php_ext::attr::php_function;
use php_ext::raising::Thrown;
use php_ext::request::Request;
use php_ext::values::{PhpString, Value};

/// `parse_str(string $string, &$result): void`: the variables of the
/// query string as an array written through `$result`.
///
/// ```php
/// parse_str("first=value&arr[]=foo+bar&arr[]=baz", $o); var_dump($o);
/// parse_str("a.b=1&c d=2&e[x][y]=3&e[x][]=4&f[=5&g]=6&h[a=7&%61%62=%3D&&=x&i", $p); var_dump($p);
/// parse_str(" lead=1&a[ b]=2&n[1]=x&n[01]=y&z[]]=q", $q); var_dump($q);
/// try { parse_str("a=1"); } catch (ArgumentCountError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// array(2) {
///   ["first"]=>
///   string(5) "value"
///   ["arr"]=>
///   array(2) {
///     [0]=>
///     string(7) "foo bar"
///     [1]=>
///     string(3) "baz"
///   }
/// }
/// array(8) {
///   ["a_b"]=>
///   string(1) "1"
///   ["c_d"]=>
///   string(1) "2"
///   ["e"]=>
///   array(1) {
///     ["x"]=>
///     array(2) {
///       ["y"]=>
///       string(1) "3"
///       [0]=>
///       string(1) "4"
///     }
///   }
///   ["f_"]=>
///   string(1) "5"
///   ["g]"]=>
///   string(1) "6"
///   ["h_a"]=>
///   string(1) "7"
///   ["ab"]=>
///   string(1) "="
///   ["i"]=>
///   string(0) ""
/// }
/// array(4) {
///   ["lead"]=>
///   string(1) "1"
///   ["a"]=>
///   array(1) {
///     [" b"]=>
///     string(1) "2"
///   }
///   ["n"]=>
///   array(2) {
///     [1]=>
///     string(1) "x"
///     ["01"]=>
///     string(1) "y"
///   }
///   ["z"]=>
///   array(1) {
///     [0]=>
///     string(1) "q"
///   }
/// }
/// parse_str() expects exactly 2 arguments, 1 given
/// ```
#[php_function]
pub(super) fn parse_str(
    req: &mut Request,
    string: PhpString,
    result: Ref<Untyped<Value>>,
) -> Result<(), Thrown> {
    let array = req.variables_mut().parse_query(string.as_bytes())?;
    result.set(req, Untyped(array.into_value()))
}
