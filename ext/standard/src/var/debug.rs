//! `debug_zval_dump()`, php's `php_debug_zval_dump`
//! (`Z/ext/standard/var.c:301`): `var_dump`'s walk with each counted
//! value's refcount beside it, a string the program wrote `interned`,
//! and a list
//! `packed`.

use php_ext::abi::{PropertyPurpose, Variadic};
use php_ext::attr::php_function;
use php_ext::coerce::{Exponent, float_format};
use php_ext::raising::{Thrown, call_stack_end};
use php_ext::request::Request;
use std::ops::ControlFlow;

use php_ext::values::walk::{self, GiveUp, Guarded, Hold, Row, Unwritten};
use php_ext::values::{Key, Laziness, PhpArray, PhpObject, RecursionGuard, Value, ValueRef};

/// `debug_zval_dump(mixed $value, mixed ...$values): void`, php's
/// `PHP_FUNCTION(debug_zval_dump)` (`Z/ext/standard/var.c:434`): each
/// argument dumped at the top level. The refcounts are the engine's, and
/// read as php's where the engine holds a value as php does. A string or
/// an array is `interned` where it is not refcounted (`Z_REFCOUNTED_P`),
/// an interned string or an immutable array, and a temporary of one
/// owner is `refcount(1)`.
///
/// ```php
/// debug_zval_dump(1, 1.5, "lit", null, true, false);
/// debug_zval_dump(array_fill(0, 2, 0), range(1, 2), str_repeat("ab", 2));
/// $s = str_repeat("x", 3); debug_zval_dump($s);
/// ```
/// ```expect
/// int(1)
/// float(1.5)
/// string(3) "lit" interned
/// NULL
/// bool(true)
/// bool(false)
/// array(2) packed refcount(1){
///   [0]=>
///   int(0)
///   [1]=>
///   int(0)
/// }
/// array(2) packed refcount(1){
///   [0]=>
///   int(1)
///   [1]=>
///   int(2)
/// }
/// string(4) "abab" refcount(1)
/// string(3) "xxx" refcount(2)
/// ```
///
/// The empty array written `[]` is php's one immutable `zend_empty_array`,
/// never counted, and an enum case's name is its constant's interned
/// name.
///
/// ```php
/// enum E { case Bar; }
/// debug_zval_dump([], E::Bar);
/// ```
/// ```expect
/// array(0) interned {
/// }
/// object(E)#1 (1) refcount(2){
///   ["name"]=>
///   string(3) "Bar" interned
/// }
/// ```
///
/// Any other array written in the source is immutable under opcache's model, so it
/// is `interned` where `php -n` counts its owners.
///
/// ```php
/// debug_zval_dump([1, 2]);
/// ```
/// ```expect regime=immutable-literals
/// array(2) packed interned {
///   [0]=>
///   int(1)
///   [1]=>
///   int(2)
/// }
/// ```
///
/// `packed` is `HT_IS_PACKED`, how the table is laid out, not whether
/// its keys count from zero: a hash whose one key is 0 is not packed, and a
/// packed list with its first slot removed still is.
///
/// ```php
/// $h = ["a" => 1]; unset($h["a"]); $h[] = 2; $p = [1, 2, 3]; unset($p[0]);
/// debug_zval_dump($h, $p);
/// ```
/// ```expect
/// array(1) refcount(2){
///   [0]=>
///   int(2)
/// }
/// array(2) packed refcount(2){
///   [1]=>
///   int(2)
///   [2]=>
///   int(3)
/// }
/// ```
///
/// A class that builds its table for the reader holds one owner of each
/// row while the dump reads it, as `zend_get_properties_for` hands php's
/// dump a temporary table until `zend_release_properties`, so an
/// `SplFixedArray` that holds itself counts that row, and a string
/// element one owner more.
///
/// ```php
/// $a = new SplFixedArray(1); $a[0] = $a; debug_zval_dump($a);
/// $b = new SplFixedArray(2); $b[0] = "x" . rand(1, 1); debug_zval_dump($b);
/// ```
/// ```expect
/// object(SplFixedArray)#1 (1) refcount(4){
///   [0]=>
///   *RECURSION*
/// }
/// object(SplFixedArray)#2 (2) refcount(2){
///   [0]=>
///   string(2) "x1" refcount(2)
///   [1]=>
///   NULL
/// }
/// ```
///
/// A typed property nothing has written is a row of its own,
/// `uninitialized(T)`, left out of the count, as
/// `zval_object_property_dump` prints the slot
/// `zend_get_typed_property_info_for_slot` finds a type for; and an
/// initialised proxy shows its instance in a table of its own, which
/// holds one more owner of it while it prints
/// (`zend_lazy_object_debug_info`).
///
/// ```php
/// class T { public int $a; protected ?float $b; private string $c = "x"; }
/// $t = new T; debug_zval_dump($t);
/// $r = new ReflectionClass(T::class);
/// $p = $r->newLazyProxy(fn () => new T);
/// debug_zval_dump($p); $r->initializeLazyObject($p); debug_zval_dump($p);
/// ```
/// ```expect
/// object(T)#1 (1) refcount(2){
///   ["a"]=>
///   uninitialized(int)
///   ["b":protected]=>
///   uninitialized(?float)
///   ["c":"T":private]=>
///   string(1) "x" interned
/// }
/// lazy proxy object(T)#4 (0) refcount(2){
///   ["a"]=>
///   uninitialized(int)
///   ["b":protected]=>
///   uninitialized(?float)
///   ["c":"T":private]=>
///   uninitialized(string)
/// }
/// lazy proxy object(T)#4 (1) refcount(2){
///   ["instance"]=>
///   object(T)#5 (1) refcount(2){
///     ["a"]=>
///     uninitialized(int)
///     ["b":protected]=>
///     uninitialized(?float)
///     ["c":"T":private]=>
///     string(1) "x" interned
///   }
/// }
/// ```
#[php_function]
fn debug_zval_dump(req: &mut Request, value: Value, values: Variadic<Value>) -> Result<(), Thrown> {
    for value in std::iter::once(value).chain(values) {
        dump(req, &value, 1)?;
    }
    Ok(())
}

/// php's `php_printf("%*c", width, ' ')`, `width` blanks as one piece,
/// ahead of `rest` where php's one call writes both.
fn indented(req: &mut Request, width: usize, rest: &[u8]) {
    let mut piece = vec![b' '; width];
    piece.extend_from_slice(rest);
    if !piece.is_empty() {
        req.response_mut().write(&piece);
    }
}

/// `zval_array_element_dump`'s and `zval_object_property_dump`'s
/// `ZEND_CHECK_STACK_LIMIT` arm, php 8.5.12's (`Z/ext/standard/var.c:267`,
/// `:288`): where the stack has passed `zend.max_allowed_stack_size`, the
/// row is `nesting level too deep` after its indent, with no newline, in
/// place of its key and value, and the walk goes on to the next row.
///
/// `var_dump_stack_limit.phpt`'s shape under the small-stack regime,
/// the dump handed to an `ob_start` callback that answers only whether
/// the refusal is in it, since the depth each engine reaches is its own
/// frames':
///
/// ```php
/// $a = [];
/// for ($i = 0; $i < 50000; $i++) { $a = [$a]; }
/// $o = $first = new stdClass;
/// for ($i = 0; $i < 50000; $i++) { $o->next = new stdClass; $o = $o->next; }
/// $guarded = fn ($chunk) => str_contains($chunk, 'nesting level too deep') ? "guarded\n" : "whole\n";
/// ob_start($guarded); debug_zval_dump($a); ob_end_flush();
/// ob_start($guarded); debug_zval_dump($first); ob_end_flush();
/// while (is_array($a) && isset($a[0])) { $a = $a[0]; }
/// while ($next = $first->next ?? null) { $first->next = $next->next ?? null; }
/// ```
/// ```expect regime=small-stack
/// guarded
/// guarded
/// ```
fn too_deep(req: &mut Request, level: usize) -> bool {
    if !req.limits().stack_overflowed() {
        return false;
    }
    indented(req, level + 1, b"nesting level too deep");
    true
}

/// `zval_object_property_dump`'s value: the row's value dumped, its
/// count read as php reads it, or for a typed property nothing has
/// written `uninitialized(T)` at the indent the value would take.
fn dump_row(req: &mut Request, row: &Row, level: usize) -> Result<(), Thrown> {
    match row.uninitialized() {
        None => dump(req, row.value(), level),
        Some(ty) => {
            indented(req, level - 1, format!("uninitialized({ty})\n").as_bytes());
            Ok(())
        }
    }
}

/// `php_debug_zval_dump` (`Z/ext/standard/var.c:301`), one arm per
/// type as php's switch has them, each count read as php reads it, and
/// each piece written to the request as php's `php_printf`, `PHPWRITE`
/// and `PUTS` write it, one write per call of php's, so what code the
/// dump runs writes lands where php's lands, and an `ob_start` callback
/// with a chunk size of one is handed php's pieces:
///
/// ```php
/// class N { function __debugInfo() { echo "inside\n"; return []; } }
/// debug_zval_dump([new N, 1]);
/// ob_start(function ($chunk) { return $chunk === '' ? '' : "<$chunk>"; }, 1);
/// debug_zval_dump([1, str_repeat("a", 1)]);
/// ob_end_flush();
/// ```
/// ```expect
/// array(2) packed refcount(1){
///   [0]=>
///   inside
/// object(N)#1 (0) refcount(1){
///   }
///   [1]=>
///   int(1)
/// }
/// <array(2) packed refcount(1){
/// ><  [0]=>
/// ><  ><int(1)
/// ><  [1]=>
/// ><  ><string(1) "><a><" refcount(1)
/// ><}
/// >
/// ```
///
/// An array is held while its rows are walked, so an element's
/// `__debugInfo` that frees it through another name (GH-19801) leaves
/// it standing until the walk is done:
///
/// ```php
/// $a = [new class { function __debugInfo() { global $b; $b->a = null; gc_collect_cycles(); return []; } }];
/// $b = new stdClass; $b->a = &$a;
/// debug_zval_dump($b);
/// ```
/// ```expect
/// object(stdClass)#2 (1) refcount(2){
///   ["a"]=>
///   reference refcount(2) {
///     array(1) packed refcount(1){
///       [0]=>
///       object(class@anonymous)#1 (0) refcount(1){
///       }
///     }
///   }
/// }
/// ```
///
/// A float is `%.*H` at `serialize_precision`, which writes a non-finite
/// one whole however short the precision:
///
/// ```php
/// ini_set('serialize_precision', 1);
/// var_dump(INF, -INF, NAN, 1.5);
/// ```
/// ```expect
/// float(INF)
/// float(-INF)
/// float(NAN)
/// float(2)
/// ```
fn dump(req: &mut Request, value: &Value, level: usize) -> Result<(), Thrown> {
    // Unguarded in php, held to the thread's floor here.
    call_stack_end(req)?;
    indented(req, level - 1, b"");
    match value.as_ref() {
        ValueRef::Bool(false) => req.response_mut().write(b"bool(false)\n"),
        ValueRef::Bool(true) => req.response_mut().write(b"bool(true)\n"),
        ValueRef::Null | ValueRef::Undef => req.response_mut().write(b"NULL\n"),
        ValueRef::Int(n) => req.response_mut().write(format!("int({n})\n").as_bytes()),
        ValueRef::Float(f) => dump_double(req, f),
        ValueRef::Str(s) => {
            req.response_mut()
                .write(format!("string({}) \"", s.len()).as_bytes());
            req.response_mut().write(s.as_bytes());
            if value.is_refcounted() {
                let count = value.refcount();
                req.response_mut()
                    .write(format!("\" refcount({count})\n").as_bytes());
            } else {
                req.response_mut().write(b"\" interned\n");
            }
        }
        ValueRef::Array(array) => dump_array(req, value, array, level)?,
        ValueRef::Object(object) => dump_object(req, value, object, level)?,
        ValueRef::Resource(resource) => {
            let kind = resource.type_name(req).map_or_else(
                || String::from("Unknown"),
                |k| String::from_utf8_lossy(k).into_owned(),
            );
            req.response_mut().write(
                format!(
                    "resource({}) of type ({kind}) refcount({})\n",
                    resource.id(),
                    value.refcount()
                )
                .as_bytes(),
            );
        }
        ValueRef::Reference(cell) => {
            let count = value.refcount();
            req.response_mut()
                .write(format!("reference refcount({count}) {{\n").as_bytes());
            dump(req, &cell.get(), level + 2)?;
            indented(req, level - 1, b"}\n");
        }
    }
    Ok(())
}

/// `php_debug_zval_dump`'s `IS_ARRAY` arm: `GC_IS_RECURSIVE`, then
/// `GC_ADDREF(myht)` (`Z/ext/standard/var.c:347`) beside
/// `GC_PROTECT_RECURSION(myht)` (GH-19801) and
/// `ZEND_HASH_FOREACH_KEY_VAL`: a foreach walk holding the array and
/// guarding its recursion, so an element's `__debugInfo` that gives up
/// the array's last other owner leaves it standing until the walk is
/// done; the walk's owner is given up as `GC_DTOR_NO_REF(myht)` frees the
/// array where it was the last. An immutable array is held and marked by
/// neither. The mark is asked before the header is written, as php asks
/// it, and the count printed is the one before the walk's owner, php's
/// `Z_REFCOUNT_P(struc) - 1`.
fn dump_array(
    req: &mut Request,
    value: &Value,
    array: PhpArray,
    level: usize,
) -> Result<(), Thrown> {
    if array.is_recursive() {
        req.response_mut().write(b"*RECURSION*\n");
        return Ok(());
    }
    let packed = if array.is_packed() { "packed " } else { "" };
    if value.is_refcounted() {
        let count = value.refcount();
        req.response_mut()
            .write(format!("array({}) {packed}refcount({count}){{\n", array.len()).as_bytes());
    } else {
        req.response_mut()
            .write(format!("array({}) {packed}interned {{\n", array.len()).as_bytes());
    }
    // `zval_array_element_dump`, under `GC_ADDREF(myht)` given up as
    // `GC_DTOR_NO_REF(myht)` (`Z/ext/standard/var.c:347`, `:363`).
    let held = Hold::Owner(GiveUp::Dtor);
    let walked = walk::foreach_guarded(req, array, held, |req, row| -> walk::Step {
        if too_deep(req, level) {
            return Ok(ControlFlow::Continue(()));
        }
        match row.key() {
            Key::Int(n) => indented(req, level + 1, format!("[{n}]=>\n").as_bytes()),
            Key::Str(s) => {
                indented(req, level + 1, b"[\"");
                req.response_mut().write(s.as_bytes());
                req.response_mut().write(b"\"]=>\n");
            }
        }
        dump(req, row.value(), level + 2)?;
        Ok(ControlFlow::Continue(()))
    })?;
    match walked {
        // Asked above, before anything was written.
        Guarded::Recursive | Guarded::Walked(_) => {}
    }
    indented(req, level - 1, b"}\n");
    Ok(())
}

/// `php_debug_zval_dump`'s `IS_OBJECT` arm over the table
/// `zend_get_properties_for(struc, ZEND_PROP_PURPOSE_DEBUG)` holds, each
/// row as `zval_object_property_dump` prints it, then
/// `zend_release_properties`, the closing brace and the unprotect, in
/// php's order.
fn dump_object(
    req: &mut Request,
    value: &Value,
    object: PhpObject,
    level: usize,
) -> Result<(), Thrown> {
    if object.is_recursive_in(req, RecursionGuard::Debug) {
        req.response_mut().write(b"*RECURSION*\n");
        return Ok(());
    }
    object.protect_in(req, RecursionGuard::Debug);
    // Held from here to the closing brace, as php holds the table from
    // `zend_get_properties_for` to `zend_release_properties`, so the
    // refcount printed counts the owners a class's own table takes.
    let held = match object.properties_for(req, PropertyPurpose::Debug) {
        Ok(held) => held,
        Err(thrown) => {
            object.unprotect_in(req, RecursionGuard::Debug);
            return Err(thrown);
        }
    };
    // `ZSTR_VAL(class_name)` under `%s`: up to the NUL of an anonymous
    // class's name.
    let full = object.handlers(req).get_class_name(req, object);
    let name = php_ext::coerce::c_str(&full);
    // `php_var_dump_object_prefix`: what a lazy object is.
    let prefix = match object.laziness() {
        Laziness::Eager => "",
        Laziness::Ghost => "lazy ghost ",
        Laziness::Proxy | Laziness::Forwarding => "lazy proxy ",
    };
    let mut header = format!("{prefix}object(").into_bytes();
    header.extend_from_slice(name);
    header.extend_from_slice(
        format!(
            ")#{} ({}) refcount({}){{\n",
            object.index(),
            held.count(),
            value.refcount()
        )
        .as_bytes(),
    );
    req.response_mut().write(&header);
    let dumped = walk::properties(req, &held, Unwritten::Shown, |req, row| -> walk::Step {
        if too_deep(req, level) {
            return Ok(ControlFlow::Continue(()));
        }
        // An element a class shows among its properties is an integer
        // key, `[%ld]=>`.
        let prop = match row.key() {
            Key::Int(n) => {
                indented(req, level + 1, format!("[{n}]=>\n").as_bytes());
                dump_row(req, row, level + 2)?;
                return Ok(ControlFlow::Continue(()));
            }
            Key::Str(prop) => *prop,
        };
        let bytes = prop.as_bytes();
        indented(req, level + 1, b"[");
        let mut shown = b"\"".to_vec();
        match super::unmangle(bytes) {
            (Some(b"*"), name) => {
                shown.extend_from_slice(name);
                shown.extend_from_slice(b"\":protected");
            }
            (Some(class), name) => {
                shown.extend_from_slice(name);
                shown.extend_from_slice(b"\":\"");
                shown.extend_from_slice(php_ext::coerce::c_str(class));
                shown.extend_from_slice(b"\":private");
            }
            (None, _) => {
                shown.extend_from_slice(bytes);
                shown.push(b'"');
            }
        }
        req.response_mut().write(&shown);
        req.response_mut().write(b"]=>\n");
        dump_row(req, row, level + 2)?;
        Ok(ControlFlow::Continue(()))
    })
    .map(|_| ());
    // What the class's debug table left pending was raised before any
    // row's, so a row's is chained onto it.
    let left = held.release(req);
    indented(req, level - 1, b"}\n");
    object.unprotect_in(req, RecursionGuard::Debug);
    match (left, dumped) {
        (Err(earlier), Err(later)) => Err(earlier.chain(req, later)),
        (Err(thrown), Ok(())) | (Ok(()), Err(thrown)) => Err(thrown),
        (Ok(()), Ok(())) => Ok(()),
    }
}

/// `var_dump`'s `float(%.*H)` line. `%.*H` writes a non-finite double
/// whole, as `zend_gcvt` does at -1 (`Z/main/snprintf.c:886-899`). Out of
/// line, so the text's buffer stands in this leaf's frame and not in
/// every nesting level of [`dump`], as php's `num_buf` stands in
/// `format_converter`'s and not in `php_var_dump`'s.
#[inline(never)]
fn dump_double(req: &mut Request, f: f64) {
    let precision = if f.is_finite() {
        req.ini().serialize_precision()
    } else {
        -1
    };
    let text = float_format(f, precision, Exponent::Upper);
    req.response_mut()
        .write(format!("float({text})\n").as_bytes());
}
