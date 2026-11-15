//! `serialize()`, php's `php_var_serialize_intern` (`Z/ext/standard/var.c:1049`):
//! the byte format php writes, with the var hash that numbers every
//! value written, so a second sight of an object is `r:n;` and a second
//! sight of a reference `R:n;`; an object's `__serialize()`, `__sleep()`
//! and enum cases as php reads them, and a float at
//! `serialize_precision` through `zend_gcvt`.

use std::ops::ControlFlow;

use php_ext::abi::PropertyPurpose;
use php_ext::attr::php_function;
use php_ext::call::call_method;
use php_ext::coerce::{Exponent, gcvt, with_string};
use php_ext::decl::Visibility;
use php_ext::raising::{Level, Thrown, call_stack_limit, docref, throw_exception_ex, type_error};
use php_ext::request::Request;
use php_ext::values::walk::{self, Hold};
use php_ext::values::{Key, Owner, PhpArray, PhpObject, PhpRef, PhpString, Value, ValueRef};

use super::{
    Holder, INCOMPLETE_CLASS, MAGIC_MEMBER, MAGIC_MEMBER_ENUM_CASE, MAGIC_MEMBER_SERIALIZED,
    Shared, class_name, has_method, is_enum, is_serializable, locked, not_serializable,
};
use crate::string::Decimal;
use crate::throwables::Throwables;

/// What the var hash keys an entry by: the object's identity, or the
/// reference's.
#[derive(Clone, Copy, PartialEq)]
enum Held {
    Object(u32),
    Reference(PhpRef),
}

/// `php_serialize_data`: `n`, the count of values written, the number
/// each object and reference was first written at, `-1` for an object
/// whose serialization failed, and an owner of each, which php's
/// `Z_ADDREF_P` takes so a value a `__serialize()` frees cannot have its
/// place reused by a later one while the walk goes on.
#[derive(Default)]
pub(crate) struct VarHash {
    n: i64,
    seen: Vec<(Held, i64)>,
    owners: Vec<Owner<Value>>,
    /// `incomplete_classes_only` (8.5.12): the `C:` and `E:` payloads an
    /// unserializer reading incomplete classes only kept are written back
    /// as they were read.
    incomplete_classes_only: bool,
}

impl VarHash {
    /// `PHP_VAR_SERIALIZE_DESTROY`'s `zend_hash_destroy`: the owners given
    /// up.
    fn destroy(self, req: &mut Request) {
        for owner in self.owners {
            owner.give_up(req);
        }
    }
}

/// `serialize(mixed $value): string`, php's `PHP_FUNCTION(serialize)`
/// (`Z/ext/standard/var.c:1358`).
///
/// ```php
/// var_dump(serialize(null), serialize(true), serialize(-5), serialize(1.5), serialize(0.1 + 0.2), serialize(1e100), serialize(-0.0), serialize(INF), serialize(NAN), serialize("a\"b"), serialize([1, "k" => [2]]));
/// ```
/// ```expect
/// string(2) "N;"
/// string(4) "b:1;"
/// string(5) "i:-5;"
/// string(6) "d:1.5;"
/// string(22) "d:0.30000000000000004;"
/// string(11) "d:1.0E+100;"
/// string(5) "d:-0;"
/// string(6) "d:INF;"
/// string(6) "d:NAN;"
/// string(10) "s:3:"a"b";"
/// string(36) "a:2:{i:0;i:1;s:1:"k";a:1:{i:0;i:2;}}"
/// ```
///
/// An object is its class and its properties, a protected or private
/// one under the name php mangles it to; a second sight of the same
/// object is `r:n;`, and a reference seen twice is `R:n;`, `n` the
/// number the var hash gave its first sight.
///
/// ```php
/// class P { public $a = 1; protected $b = 2; private $c = 3; public $d; }
/// $p = new P; $p->d = $p; echo json_encode(serialize([$p, $p])), "\n";
/// $x = 1; $arr = []; $arr[] = &$x; $arr[] = &$x; $arr[] = 2; echo serialize($arr), "\n";
/// $o = new stdClass; $o->self = $o; $o->list = [1]; echo serialize($o), "\n";
/// ```
/// ```expect
/// "a:2:{i:0;O:1:\"P\":4:{s:1:\"a\";i:1;s:4:\"\u0000*\u0000b\";i:2;s:4:\"\u0000P\u0000c\";i:3;s:1:\"d\";r:2;}i:1;r:2;}"
/// a:3:{i:0;i:1;i:1;R:2;i:2;i:2;}
/// O:8:"stdClass":2:{s:4:"self";r:1;s:4:"list";a:1:{i:0;i:1;}}
/// ```
///
/// `__serialize()` answers the data, which must be an array; `__sleep()`
/// names the properties, each looked for as written, then as private,
/// then as protected, with php's warnings for a name that is not there
/// or twice.
///
/// ```php
/// class S { public $a = 1; function __serialize(): array { return ["x" => $this->a, 5 => "y"]; } }
/// class L { public $a = 1; protected $b = 2; private $c = 3; public $d = 4; function __sleep() { return ["a", "b", "c", "nope", "a"]; } }
/// class Bad { function __serialize() { return 1; } }
/// echo serialize(new S), "\n", json_encode(serialize(new L)), "\n";
/// try { serialize(new Bad); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// O:1:"S":2:{s:1:"x";i:1;i:5;s:1:"y";}
///
/// Warning: serialize(): "nope" returned as member variable from __sleep() but does not exist in %s on line 5
///
/// Warning: serialize(): "a" is returned from __sleep() multiple times in %s on line 5
/// "O:1:\"L\":3:{s:1:\"a\";i:1;s:4:\"\u0000*\u0000b\";i:2;s:4:\"\u0000L\u0000c\";i:3;}"
/// Bad::__serialize() must return an array
/// ```
///
/// A closure is not serializable; an enum case is `E:` and its name.
///
/// ```php
/// try { serialize(function () {}); } catch (Exception $e) { echo get_class($e), ": ", $e->getMessage(), "\n"; }
/// echo serialize(RoundingMode::HalfEven), "\n";
/// ```
/// ```expect
/// Exception: Serialization of 'Closure' is not allowed
/// E:21:"RoundingMode:HalfEven";
/// ```
#[php_function]
fn serialize(req: &mut Request, value: Value) -> Result<Owner<PhpString>, Thrown> {
    let mut serializer = Serializer::new(req);
    let mut out = Vec::new();
    let written = serializer.write(req, &mut out, &value);
    serializer.finish(req);
    written?;
    PhpString::new(req, &out)
}

/// `php_serialize_data_t` with `php_var_serialize`, `PHPAPI` in
/// `php_var.h`: one var hash across several values, so a second sight of
/// an object or a reference in a later value is the `r:n;` or `R:n;` of
/// its first. What ext/session's encoders write every session variable
/// through, and what `serialize()` is for one value.
///
/// ```php
/// ob_start(); session_start(["save_path" => __DIR__, "use_cookies" => 0, "cache_limiter" => ""]);
/// session_decode('a|O:8:"stdClass":0:{}b|r:1;c|a:1:{i:0;r:1;}'); echo session_encode(), "\n";
/// session_write_close(); ob_end_flush();
/// ```
/// ```expect
/// a|O:8:"stdClass":0:{}b|r:1;c|a:1:{i:0;r:1;}
/// ```
pub struct Serializer {
    hash: VarHash,
    holder: Holder,
}

impl Serializer {
    /// `PHP_VAR_SERIALIZE_INIT`: the var hash a serializer already open
    /// shares, where one is and `BG(serialize_lock)` is not held, so what
    /// a `Serializable::serialize()` writes of its parts is numbered in
    /// the string around it; a fresh one otherwise.
    #[must_use]
    pub fn new(req: &mut Request) -> Serializer {
        let nesting = &mut req.state::<crate::State>().var;
        let (hash, holder) = nesting.serialize.open(nesting.lock, VarHash::default());
        Serializer { hash, holder }
    }

    /// `php_var_serialize(buf, struc, &var_hash)`: `value` appended to
    /// `out` as a root.
    pub fn write(
        &mut self,
        req: &mut Request,
        out: &mut Vec<u8>,
        value: &Value,
    ) -> Result<(), Thrown> {
        write(req, out, value, self, false, true)
    }

    /// `php_var_serialize_set_incomplete_classes_only`: whether an
    /// `__PHP_Incomplete_Class` holding the payload of a `C:` record or
    /// the case of an `E:` value, as an [`Unserializer`] reading
    /// incomplete classes only made it, is written back as those bytes,
    /// which is how ext/session's upload progress rewrites a session no
    /// class of which it may load.
    ///
    /// [`Unserializer`]: crate::Unserializer
    pub fn set_incomplete_classes_only(&mut self, on: bool) {
        self.hash.incomplete_classes_only = on;
    }

    /// `PHP_VAR_SERIALIZE_DESTROY`: the var hash handed back to the
    /// serializer it is shared with, or destroyed where this was its last
    /// holder, the owners it took given up.
    pub fn finish(self, req: &mut Request) {
        let Serializer { hash, holder } = self;
        if let Some(hash) = req
            .state::<crate::State>()
            .var
            .serialize
            .close(hash, holder)
        {
            hash.destroy(req);
        }
    }

    /// `f`, a call that may open another serializer, with the var hash
    /// set down where that one finds it.
    fn out<R>(&mut self, req: &mut Request, f: impl FnOnce(&mut Request) -> R) -> R {
        let hash = std::mem::take(&mut self.hash);
        let (hash, answered) =
            Shared::set_down(req, |nesting| &mut nesting.serialize, hash, self.holder, f);
        self.hash = hash;
        answered
    }

    /// `php_add_var_hash`: the number an earlier sight gave `value`, or
    /// zero after recording this one with an owner of it. A scalar and an
    /// array are never recorded, a reference to an object is the object,
    /// and an object nothing else holds, in no array more than one holds,
    /// whose property table nothing else holds either
    /// ([`PhpObject::properties_shared`]) and of a class with neither
    /// `__serialize()` nor `__sleep()`, is counted without being
    /// recorded, since nothing can show it again.
    /// The owner keeps an object a `__serialize()` made and let go alive
    /// until the serializer is done, so the next one it makes is another
    /// object and not an `r:` to the first; one nothing records is freed
    /// as soon as what held it is.
    ///
    /// ```php
    /// class P { function __serialize(): array { return []; } function __destruct() { echo "P gone\n"; } }
    /// class Q { function __serialize(): array { return [new P]; } }
    /// class D { function __destruct() { echo "D gone\n"; } }
    /// class R { function __serialize(): array { echo "R\n"; return [new D]; } }
    /// echo serialize([new Q, new Q]), "\n";
    /// echo serialize([new R, new R]), "\n";
    /// ```
    /// ```expect
    /// P gone
    /// P gone
    /// a:2:{i:0;O:1:"Q":1:{i:0;O:1:"P":0:{}}i:1;O:1:"Q":1:{i:0;O:1:"P":0:{}}}
    /// R
    /// D gone
    /// R
    /// D gone
    /// a:2:{i:0;O:1:"R":1:{i:0;O:1:"D":0:{}}i:1;O:1:"R":1:{i:0;O:1:"D":0:{}}}
    /// ```
    fn add(&mut self, req: &mut Request, value: &Value, in_rcn_array: bool) -> i64 {
        self.hash.n += 1;
        let (held, is_ref, recorded) = match value.as_ref() {
            ValueRef::Reference(cell) => match cell.get().as_object() {
                Some(object) => (Held::Object(object.index()), true, cell.get()),
                None => (Held::Reference(cell), true, *value),
            },
            ValueRef::Object(object) => {
                if !in_rcn_array
                    && value.refcount() == 1
                    && !object.properties_shared()
                    && !has_method(req, object.class(), b"__serialize")
                    && !has_method(req, object.class(), b"__sleep")
                {
                    return 0;
                }
                (Held::Object(object.index()), false, *value)
            }
            _ => return 0,
        };
        if let Some((_, number)) = self.hash.seen.iter().find(|(h, _)| *h == held) {
            // References are counted once; undo the increment above.
            if is_ref && *number != -1 {
                self.hash.n -= 1;
            }
            return *number;
        }
        self.hash.seen.push((held, self.hash.n));
        let recorded = Owner::take(req, recorded);
        self.hash.owners.push(recorded);
        0
    }

    /// The entry of an object whose serialization failed marked `-1`, so
    /// a later sight of it is written `N;`.
    fn fail(&mut self, object: PhpObject) {
        if let Some((_, number)) = self
            .hash
            .seen
            .iter_mut()
            .find(|(h, _)| *h == Held::Object(object.index()))
        {
            *number = -1;
        }
    }
}

fn write_long(out: &mut Vec<u8>, n: i64) {
    out.extend_from_slice(b"i:");
    out.extend_from_slice(Decimal::of(n).as_bytes());
    out.push(b';');
}

fn write_string(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(b"s:");
    out.extend_from_slice(Decimal::size(bytes.len()).as_bytes());
    out.extend_from_slice(b":\"");
    out.extend_from_slice(bytes);
    out.extend_from_slice(b"\";");
}

/// `php_var_serialize_class_name`: `O:len:"name":`, the name an
/// incomplete object carries in place of its own; whether it was one.
fn write_class_name(req: &mut Request, out: &mut Vec<u8>, object: PhpObject) -> bool {
    let mut name = class_name(req, object);
    let incomplete = name == INCOMPLETE_CLASS;
    if incomplete
        && let Some(stored) = object
            .prop(req, MAGIC_MEMBER)
            .and_then(|v| v.deref().as_str_bytes().map(<[u8]>::to_vec))
    {
        name = stored;
    }
    out.extend_from_slice(b"O:");
    out.extend_from_slice(Decimal::size(name.len()).as_bytes());
    out.extend_from_slice(b":\"");
    out.extend_from_slice(&name);
    out.extend_from_slice(b"\":");
    incomplete
}

/// `php_var_serialize_intern`, which opens with
/// `php_serialize_check_stack_limit` (`Z/ext/standard/var.c:1058`): a
/// value nested past `zend.max_allowed_stack_size` is php's `Error` at
/// the level the stack ran out, before anything of it is written. Under
/// the small-stack regime php's own frames use up 256K within a few
/// hundred levels of an object holding the next in an array, so five
/// thousand are refused in both engines; the size is 256K less the
/// platform's least reserve, so the number is masked. The chain is taken
/// apart a link at a time after, so its release is no deeper than the
/// stack either.
///
/// ```php
/// $o = $first = new stdClass;
/// for ($i = 0; $i < 5000; $i++) { $o->next = [new stdClass]; $o = $o->next[0]; }
/// try { serialize([$first]); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// while ($next = $first->next[0] ?? null) { $first->next = $next->next ?? null; }
/// ```
/// ```expect regime=small-stack
/// Maximum call stack size of %d bytes (zend.max_allowed_stack_size - zend.reserved_stack_size) reached. Infinite recursion?
/// ```
fn write(
    req: &mut Request,
    out: &mut Vec<u8>,
    value: &Value,
    hash: &mut Serializer,
    in_rcn_array: bool,
    is_root: bool,
) -> Result<(), Thrown> {
    // `php_serialize_check_stack_limit`.
    call_stack_limit(req)?;
    let already = hash.add(req, value, in_rcn_array);
    if already != 0 {
        if already == -1 {
            out.extend_from_slice(b"N;");
            return Ok(());
        }
        if value.as_php_ref().is_some() {
            out.extend_from_slice(b"R:");
            out.extend_from_slice(Decimal::of(already).as_bytes());
            out.push(b';');
            return Ok(());
        }
        if value.as_object().is_some() {
            out.extend_from_slice(b"r:");
            out.extend_from_slice(Decimal::of(already).as_bytes());
            out.push(b';');
            return Ok(());
        }
    }
    // `struc = Z_REFVAL_P(struc)`: what a reference holds is counted as
    // the reference holds it.
    let value = value.deref();
    match value.as_ref() {
        ValueRef::Bool(false) => out.extend_from_slice(b"b:0;"),
        ValueRef::Bool(true) => out.extend_from_slice(b"b:1;"),
        ValueRef::Null | ValueRef::Undef => out.extend_from_slice(b"N;"),
        ValueRef::Int(n) => write_long(out, n),
        ValueRef::Float(f) => {
            out.extend_from_slice(b"d:");
            write_double(out, f, req.ini().serialize_precision());
            out.push(b';');
        }
        ValueRef::Str(s) => write_string(out, s.as_bytes()),
        ValueRef::Array(array) => {
            out.extend_from_slice(b"a:");
            let shared = !is_root && (in_rcn_array || value.refcount() > 1);
            nested(req, out, &value, array, hash, shared, false)?;
        }
        ValueRef::Object(object) => object_value(req, out, object, hash)?,
        _ => out.extend_from_slice(b"i:0;"),
    }
    Ok(())
}

/// `php_var_serialize_nested_data`: `count:{`, each key and value, `}`;
/// a reference with one owner is written as what it holds. An element
/// that is the table being written, `Z_ARR_P(data) == Z_ARR_P(struc)`,
/// is `N;`: php compares the pointers, so a nest of distinct tables is
/// never read twice, and the stack limit at the top of [`write`] is
/// what refuses one too deep. Under the small-stack regime a plain nest
/// a few thousand deep reaches the ini's 256K in both engines:
///
/// ```php
/// $a = [];
/// for ($i = 0; $i < 3000; $i++) { $a = [$a]; }
/// try { serialize($a); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// while ($a) { $a = $a[0]; }
/// ```
/// ```expect regime=small-stack
/// Maximum call stack size of %d bytes (zend.max_allowed_stack_size - zend.reserved_stack_size) reached. Infinite recursion?
/// ```
///
/// The walk holds no owner of the table, as php's holds none: a
/// row an element's `__serialize` rewrites in place through the
/// reference that holds the table is written as it now is, a table that
/// assignment gives up is destroyed there, its other elements' last
/// owner with it, and `debug_zval_dump` from inside `__serialize` counts
/// the table's owners as php does.
///
/// ```php
/// class C { function __serialize(): array { $GLOBALS['r'][1] = 'new'; return []; } }
/// $v = 'vvvvv';
/// $r = [new C, $v . $v];
/// $outer = [&$r];
/// echo serialize($outer), "\n";
/// ```
/// ```expect
/// a:1:{i:0;a:2:{i:0;O:1:"C":0:{}i:1;s:3:"new";}}
/// ```
///
/// Nor does it hold an owner of a row, so two tables down a row the
/// program rewrites through its reference is rewritten in place and
/// written as it now is, as php writes it:
///
/// ```php
/// class C { function __serialize(): array { $GLOBALS['r'][0][1] = 'new'; return []; } }
/// $r = [[new C, 1]];
/// $o = [&$r];
/// echo serialize($o), "\n";
/// ```
/// ```expect
/// a:1:{i:0;a:1:{i:0;a:2:{i:0;O:1:"C":0:{}i:1;s:3:"new";}}}
/// ```
///
/// ```php
/// class D { function __destruct() { echo "D gone\n"; } }
/// class C { function __serialize(): array { $GLOBALS['r'] = 0; echo "after assign\n"; return []; } }
/// $r = [new D, new C];
/// $outer = [&$r];
/// echo serialize($outer), "\n";
/// ```
/// ```expect
/// D gone
/// after assign
/// a:1:{i:0;a:2:{i:0;O:1:"D":0:{}i:1;O:1:"C":0:{}}}
/// ```
///
/// ```php
/// class C { function __serialize(): array { debug_zval_dump($GLOBALS['r']); return []; } }
/// $v = 'vvvvv';
/// $r = [new C, $v . $v];
/// $outer = [&$r];
/// echo serialize($outer), "\n";
/// ```
/// ```expect
/// array(2) packed refcount(2){
///   [0]=>
///   object(C)#1 (0) refcount(3){
///   }
///   [1]=>
///   string(10) "vvvvvvvvvv" refcount(1)
/// }
/// a:1:{i:0;a:2:{i:0;O:1:"C":0:{}i:1;s:10:"vvvvvvvvvv";}}
/// ```
fn nested(
    req: &mut Request,
    out: &mut Vec<u8>,
    outer: &Value,
    table: PhpArray,
    hash: &mut Serializer,
    in_rcn_array: bool,
    incomplete: bool,
) -> Result<(), Thrown> {
    let magic = incomplete && walk::rows(req, table).any(|(k, _)| is_magic(&k));
    let count = table.len() - usize::from(magic);
    out.extend_from_slice(Decimal::size(count).as_bytes());
    out.extend_from_slice(b":{");
    let mut skip_magic = incomplete;
    // `ZEND_HASH_FOREACH_KEY_VAL_IND(ht, ...)` (`Z/ext/standard/var.c:991`),
    // and `ZEND_HASH_FOREACH_KEY_VAL` over what `__serialize()` answered
    // (`Z/ext/standard/var.c:1160`), which hold nothing: a row a
    // `__serialize` rewrites in place is written as it now is, and a
    // table the program gives up mid-walk ends the walk, where php reads
    // it freed, and the brace is closed after the rows written, whatever
    // the count said.
    walk::foreach(req, table, Hold::None, |req, row| {
        let key = row.key();
        if skip_magic && is_magic(key) {
            skip_magic = false;
            return Ok(ControlFlow::Continue(()));
        }
        match key {
            Key::Int(n) => write_long(out, *n),
            Key::Str(s) => write_string(out, s.as_bytes()),
        }
        // `Z_ISREF_P(data) && Z_REFCOUNT_P(data) == 1`.
        let data = match row.value().as_php_ref() {
            Some(cell) if row.value().refcount() == 1 => cell.get(),
            _ => *row.value(),
        };
        if let (Some(inner), Some(outer_array)) = (data.as_array(), outer.as_array())
            && inner.is_same(outer_array)
        {
            hash.add(req, outer, in_rcn_array);
            out.extend_from_slice(b"N;");
            return Ok(ControlFlow::Continue(()));
        }
        write(req, out, &data, hash, in_rcn_array, false)?;
        Ok(ControlFlow::<()>::Continue(()))
    })?;
    out.push(b'}');
    Ok(())
}

fn is_magic(key: &Key) -> bool {
    matches!(key, Key::Str(s) if s.as_bytes() == MAGIC_MEMBER)
}

/// An object whose class has `__serialize`, held across the call and
/// the rows as `ZVAL_OBJ_COPY(&obj, Z_OBJ_P(struc))`
/// (`Z/ext/standard/var.c:1148`) holds it, so a count the method reads
/// of it is php's.
fn serialize_method(
    req: &mut Request,
    out: &mut Vec<u8>,
    object: PhpObject,
    name: &[u8],
    hash: &mut Serializer,
) -> Result<(), Thrown> {
    let held = Owner::take(req, object);
    let written = serialized_data(req, out, object, name, hash);
    held.give_up(req);
    written
}

/// [`serialize_method`]'s body: the array `__serialize` answers written
/// as the object's data, `php_var_serialize_nested_data` over it, and
/// php's `TypeError` for anything else.
fn serialized_data(
    req: &mut Request,
    out: &mut Vec<u8>,
    object: PhpObject,
    name: &[u8],
    hash: &mut Serializer,
) -> Result<(), Thrown> {
    let data = locked(req, |req| call_method(req, &object, b"__serialize", &[]))?;
    let Some(array) = data.as_array() else {
        data.give_up(req);
        let shown = String::from_utf8_lossy(name).into_owned();
        return Err(type_error(
            req,
            &format!("{shown}::__serialize() must return an array"),
        ));
    };
    write_class_name(req, out, object);
    // The serializer owns what `__serialize` answered, and the
    // program holds it too where its refcount says so, which php's
    // `in_rcn_array` carries into the rows.
    let shared = data.refcount() > 1;
    let written = nested(req, out, &object.as_value(), array, hash, shared, false);
    data.give_up(req);
    written
}

/// The `IS_OBJECT` arm.
fn object_value(
    req: &mut Request,
    out: &mut Vec<u8>,
    object: PhpObject,
    hash: &mut Serializer,
) -> Result<(), Thrown> {
    if hash.hash.incomplete_classes_only
        && class_name(req, object) == INCOMPLETE_CLASS
        && incomplete_raw(req, out, object)
    {
        return Ok(());
    }
    if let Some(shown) = not_serializable(req, object.class()) {
        let ce = Throwables::of(req)?;
        return Err(throw_exception_ex(
            req,
            ce.exception,
            &format!("Serialization of '{shown}' is not allowed"),
        ));
    }
    let name = class_name(req, object);
    if is_enum(req, object) {
        let case = object
            .prop(req, b"name")
            .and_then(|v| v.deref().as_str_bytes().map(<[u8]>::to_vec))
            .unwrap_or_default();
        out.extend_from_slice(b"E:");
        out.extend_from_slice(Decimal::size(name.len() + 1 + case.len()).as_bytes());
        out.extend_from_slice(b":\"");
        out.extend_from_slice(&name);
        out.push(b':');
        out.extend_from_slice(&case);
        out.extend_from_slice(b"\";");
        return Ok(());
    }
    if has_method(req, object.class(), b"__serialize") {
        return serialize_method(req, out, object, &name, hash);
    }
    if is_serializable(req, object.class()) {
        return custom(req, out, object, &name, hash);
    }
    if name != INCOMPLETE_CLASS && has_method(req, object.class(), b"__sleep") {
        // `ZVAL_OBJ_COPY(&tmp, Z_OBJ_P(struc))` (`Z/ext/standard/var.c:1222`):
        // the object held across `__sleep` and its rows.
        let held = Owner::take(req, object);
        let written = sleep_serialized(req, out, object, &name, hash);
        held.give_up(req);
        return written;
    }
    let incomplete = write_class_name(req, out, object);
    let table = object_rows(req, object)?;
    let written = nested(
        req,
        out,
        &object.as_value(),
        *table,
        hash,
        false,
        incomplete,
    );
    table.give_up(req);
    if written.is_err() {
        hash.fail(object);
    }
    written
}

/// `php_var_serialize_incomplete_raw`: an incomplete object's kept `C:`
/// payload or `E:` case written as the record it was read from, under
/// the class name it holds; `false`, for the ordinary path, where it
/// holds neither or no class name.
fn incomplete_raw(req: &mut Request, out: &mut Vec<u8>, object: PhpObject) -> bool {
    // `Z_TYPE_P(member) == IS_STRING` over the row as it stands, a
    // reference not followed.
    let string = |req: &mut Request, name: &[u8]| {
        object
            .prop(req, name)
            .and_then(|v| v.as_str_bytes().map(<[u8]>::to_vec))
    };
    if let Some(payload) = string(req, MAGIC_MEMBER_SERIALIZED) {
        let Some(class) = string(req, MAGIC_MEMBER) else {
            return false;
        };
        out.extend_from_slice(b"C:");
        out.extend_from_slice(Decimal::size(class.len()).as_bytes());
        out.extend_from_slice(b":\"");
        out.extend_from_slice(&class);
        out.extend_from_slice(b"\":");
        out.extend_from_slice(Decimal::size(payload.len()).as_bytes());
        out.extend_from_slice(b":{");
        out.extend_from_slice(&payload);
        out.push(b'}');
        return true;
    }
    if let Some(case) = string(req, MAGIC_MEMBER_ENUM_CASE) {
        let Some(class) = string(req, MAGIC_MEMBER) else {
            return false;
        };
        out.extend_from_slice(b"E:");
        out.extend_from_slice(Decimal::size(class.len() + 1 + case.len()).as_bytes());
        out.extend_from_slice(b":\"");
        out.extend_from_slice(&class);
        out.push(b':');
        out.extend_from_slice(&case);
        out.extend_from_slice(b"\";");
        return true;
    }
    false
}

/// The rows `php_var_serialize_intern` writes for an object its class
/// serializes no way of its own, copied into a table of their own, one
/// owner each: its slots read where they stand where php's walk rebuilds
/// no table (`Z/ext/standard/var.c:1155`), and otherwise the table
/// `zend_get_properties_for(struc, ZEND_PROP_PURPOSE_SERIALIZE)`
/// answers, held while it is copied.
///
/// ```php
/// #[AllowDynamicProperties] class K { public $d = 1; public $p = [2]; }
/// $k = new K; $m = memory_get_usage(); $s = serialize($k); echo memory_get_usage() - $m, "\n";
/// $k->e = 3; echo serialize($k), "\n";
/// ```
/// ```expect
/// 80
/// O:1:"K":3:{s:1:"d";i:1;s:1:"p";a:1:{i:0;i:2;}s:1:"e";i:3;}
/// ```
fn object_rows(req: &mut Request, object: PhpObject) -> Result<Owner<PhpArray>, Thrown> {
    let (table, held) = if let Some(slots) = object.slots_without_table(req)? {
        (slots, None)
    } else {
        let held = object.properties_for(req, PropertyPurpose::Serialize)?;
        (held.table(), Some(held))
    };
    let rows: Vec<(Key, Value)> = walk::rows(req, table).collect();
    let copied = copied_rows(req, rows);
    let released = held.map_or(Ok(()), |held| held.release(req));
    match (copied, released) {
        (Ok(copied), Ok(())) => Ok(copied),
        (Ok(copied), Err(thrown)) => {
            copied.give_up(req);
            Err(thrown)
        }
        (Err(thrown), _) => Err(thrown),
    }
}

/// `rows` laid down in a table of their own, one owner of each key and
/// value taken.
fn copied_rows(req: &mut Request, rows: Vec<(Key, Value)>) -> Result<Owner<PhpArray>, Thrown> {
    let mut table = PhpArray::new(req)?;
    for (key, value) in rows {
        if let Err(thrown) = table.set(req, Owner::take(req, key), Owner::take(req, value)) {
            table.give_up(req);
            return Err(thrown);
        }
    }
    Ok(table)
}

/// An object whose class has `__sleep`, held by its caller: the names
/// the method answers read off the object, `php_var_serialize_class`
/// over them, and php's warning and `N;` where it answers no array.
fn sleep_serialized(
    req: &mut Request,
    out: &mut Vec<u8>,
    object: PhpObject,
    name: &[u8],
    hash: &mut Serializer,
) -> Result<(), Thrown> {
    let names = locked(req, |req| call_method(req, &object, b"__sleep", &[]))?;
    let Some(list) = names.as_array() else {
        names.give_up(req);
        let shown = String::from_utf8_lossy(name).into_owned();
        docref(
            req,
            Level::Warning,
            &format!(
                "{shown}::__sleep() should return an array only containing the names of instance-variables to serialize"
            ),
        );
        out.extend_from_slice(b"N;");
        return Ok(());
    };
    let table = sleep_props(req, object, name, list);
    names.give_up(req);
    let table = table?;
    write_class_name(req, out, object);
    let written = nested(req, out, &object.as_value(), *table, hash, false, false);
    table.give_up(req);
    written
}

/// `php_var_serialize_try_add_sleep_prop`'s `IS_UNDEF` arm: the name,
/// mangled as the table would hold it, is a declared typed instance
/// property of the object's class, which the table leaves out while
/// nothing has written it.
///
/// ```php
/// class T { public int $x; protected int $y; private int $z; public $w;
///     function __sleep() { return ['x', 'y', 'z', 'w', 'nope']; } }
/// echo serialize(new T), "\n";
/// ```
/// ```expect
/// Warning: serialize(): "nope" returned as member variable from __sleep() but does not exist in %s on line 4
/// O:1:"T":1:{s:1:"w";N;}
/// ```
fn uninitialized_typed(req: &mut Request, object: PhpObject, candidate: &[u8]) -> bool {
    let (scope, name) = match candidate.strip_prefix(b"\0") {
        Some(rest) => match rest.iter().position(|b| *b == 0) {
            Some(end) => (Some(&rest[..end]), &rest[end + 1..]),
            None => return false,
        },
        None => (None, candidate),
    };
    let mut at = Some(object.class());
    for _ in 0..64 {
        let Some(here) = at else { return false };
        let Some(decl) = req.symbols().class_decl(here) else {
            return false;
        };
        let declared = decl.properties.iter().any(|p| {
            p.is_instance
                && p.ty.is_some()
                && *p.name.0 == *name
                && match (scope, p.visibility) {
                    (None, Visibility::Public) | (Some(b"*"), Visibility::Protected) => true,
                    (Some(class), Visibility::Private) => *decl.name.0 == *class,
                    _ => false,
                }
        });
        if declared {
            return true;
        }
        at = req.symbols().class_parent(here);
    }
    false
}

/// The `ce->serialize` arm, `zend_user_serialize`: the object's
/// `serialize()` called outside `BG(serialize_lock)`, so what it
/// serializes shares this var hash, and its string written as `C:len:"class":len:{string}`.
/// `null` fails the object, which is written `N;` here and wherever it is
/// seen again; anything else fails it and throws php's `Exception`.
///
/// ```php
/// class S implements Serializable {
///     public $d = 1;
///     function serialize() { return serialize([$this->d, $GLOBALS['x']]); }
///     function unserialize($s) { [$this->d] = unserialize($s); }
/// }
/// class N implements Serializable { function serialize() { return null; } function unserialize($s) {} }
/// class Bad implements Serializable { function serialize() { return 1; } function unserialize($s) {} }
/// $x = new stdClass;
/// echo serialize([$x, new S]), "\n", serialize([$n = new N, $n]), "\n";
/// try { serialize(new Bad); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Deprecated: S implements the Serializable interface, which is deprecated. Implement __serialize() and __unserialize() instead (or in addition, if support for old PHP versions is necessary) in %s on line 2
///
/// Deprecated: N implements the Serializable interface, which is deprecated. Implement __serialize() and __unserialize() instead (or in addition, if support for old PHP versions is necessary) in %s on line 7
///
/// Deprecated: Bad implements the Serializable interface, which is deprecated. Implement __serialize() and __unserialize() instead (or in addition, if support for old PHP versions is necessary) in %s on line 8
/// a:2:{i:0;O:8:"stdClass":0:{}i:1;C:1:"S":22:{a:2:{i:0;i:1;i:1;r:2;}}}
/// a:2:{i:0;N;i:1;N;}
/// Bad::serialize() must return a string or NULL
/// ```
fn custom(
    req: &mut Request,
    out: &mut Vec<u8>,
    object: PhpObject,
    name: &[u8],
    hash: &mut Serializer,
) -> Result<(), Thrown> {
    let answered = hash.out(req, |req| call_method(req, &object, b"serialize", &[]));
    let answered = match answered {
        Ok(answered) => answered,
        Err(thrown) => {
            hash.fail(object);
            return Err(thrown);
        }
    };
    if let Some(payload) = answered.as_str_bytes().map(<[u8]>::to_vec) {
        answered.give_up(req);
        out.extend_from_slice(b"C:");
        out.extend_from_slice(Decimal::size(name.len()).as_bytes());
        out.extend_from_slice(b":\"");
        out.extend_from_slice(name);
        out.extend_from_slice(b"\":");
        out.extend_from_slice(Decimal::size(payload.len()).as_bytes());
        out.extend_from_slice(b":{");
        out.extend_from_slice(&payload);
        out.push(b'}');
        return Ok(());
    }
    let null = matches!(answered.as_ref(), ValueRef::Null);
    answered.give_up(req);
    hash.fail(object);
    out.extend_from_slice(b"N;");
    if null {
        return Ok(());
    }
    let shown = String::from_utf8_lossy(name).into_owned();
    let ce = Throwables::of(req)?;
    Err(throw_exception_ex(
        req,
        ce.exception,
        &format!("{shown}::serialize() must return a string or NULL"),
    ))
}

/// `php_var_serialize_get_sleep_props`: each name `__sleep()` returned,
/// looked for as it is, then mangled private, then mangled protected,
/// among the properties php serializes; php's warnings for a name that
/// is no string, is not there, or comes twice.
///
/// php finds each name in the table `zend_get_properties_for` handed it
/// before the names are walked, held until `zend_release_properties`
/// (`zend_hash_find`). A warning can reach the program's handler, and a
/// name's conversion its `__toString()`, either of which can write the
/// object's properties: a declared row is read in its slot as it stands
/// when its name is looked up, and a dynamic one in the table held,
/// which the write separates, so no row is named through a view of a
/// value the write freed.
///
/// ```php
/// set_error_handler(function () { global $o; $o->b = 'new'; return true; });
/// class T { public $a = 1; public $b = 'old'; function __sleep() { return [1, 'b']; } }
/// $o = new T;
/// echo serialize($o), "\n";
/// ```
/// ```expect
/// O:1:"T":1:{s:1:"b";s:3:"new";}
/// ```
fn sleep_props(
    req: &mut Request,
    object: PhpObject,
    class: &[u8],
    list: PhpArray,
) -> Result<Owner<PhpArray>, Thrown> {
    // `zend_get_properties_for` before the names are walked, which
    // initializes a lazy object first, held until the last is.
    let held = object.properties_for(req, PropertyPurpose::Serialize)?;
    let shown_class = String::from_utf8_lossy(class).into_owned();
    let mut out = match PhpArray::new(req) {
        Ok(out) => out,
        Err(thrown) => {
            return Err(match held.release(req) {
                Ok(()) => thrown,
                Err(earlier) => earlier.chain(req, thrown),
            });
        }
    };
    // `ZEND_HASH_FOREACH_VAL_IND(sleep_retval, name_val)`
    // (`Z/ext/standard/var.c:919`), which holds nothing: the caller owns
    // what `__sleep()` answered.
    let walked = walk::foreach(req, list, Hold::None, |req, row| {
        // A name the list holds by reference is what the reference holds.
        let name = row.value().deref();
        if name.as_php_string().is_none() {
            docref(
                req,
                Level::Warning,
                &format!(
                    "{shown_class}::__sleep() should return an array only containing the names of instance-variables to serialize"
                ),
            );
        }
        let text = with_string(req, &name, <[u8]>::to_vec)?;
        let mut private = vec![0];
        private.extend_from_slice(class);
        private.push(0);
        private.extend_from_slice(&text);
        let mut protected = b"\0*\0".to_vec();
        protected.extend_from_slice(&text);
        let candidates = [text.clone(), private, protected];
        let found = candidates.iter().find_map(|candidate| {
            walk::rows(req, held.table()).find_map(|(key, value)| match key {
                Key::Str(prop) if prop.as_bytes() == candidate.as_slice() => Some((prop, value)),
                _ => None,
            })
        });
        let shown = String::from_utf8_lossy(&text).into_owned();
        // A typed property nothing has written yet is left out quietly.
        if found.is_none()
            && candidates
                .iter()
                .any(|candidate| uninitialized_typed(req, object, candidate))
        {
            return Ok(ControlFlow::Continue(()));
        }
        let Some((prop, value)) = found else {
            docref(
                req,
                Level::Warning,
                &format!(
                    "\"{shown}\" returned as member variable from __sleep() but does not exist"
                ),
            );
            return Ok(ControlFlow::Continue(()));
        };
        let key = Key::Str(prop);
        if out.contains(&key) {
            docref(
                req,
                Level::Warning,
                &format!("\"{shown}\" is returned from __sleep() multiple times"),
            );
            return Ok(ControlFlow::Continue(()));
        }
        let value = Owner::take(req, value);
        let key = Owner::take(req, key);
        out.set(req, key, value)?;
        Ok(ControlFlow::<()>::Continue(()))
    });
    let released = held.release(req);
    match walked.and(released) {
        Ok(()) => Ok(out),
        Err(thrown) => {
            out.give_up(req);
            Err(thrown)
        }
    }
}

/// The `d:` arm's text: `zend_gcvt` at `serialize_precision` as the
/// entry stands, with no `precision ? precision : 1` of its own. Out of
/// line, so the buffer the text is laid in stands in this leaf's frame
/// and not in every nesting level of [`write`], as php's `buf` stands in
/// `smart_str_append_double`'s and not in `php_var_serialize_intern`'s.
#[inline(never)]
fn write_double(out: &mut Vec<u8>, f: f64, precision: i32) {
    out.extend_from_slice(gcvt(f, precision, Exponent::Upper).as_bytes());
}
