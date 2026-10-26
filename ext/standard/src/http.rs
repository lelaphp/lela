//! `Z/ext/standard/http.c`: `http_build_query` over
//! `php_url_encode_hash_ex`, and the two natives over the headers the
//! http wrapper's last request read, `BG(last_http_headers)`, which
//! `crate::http_fopen_wrapper` keeps. `request_parse_body` reads its
//! `$options` as `cache_request_parse_body_options` does and the
//! request's content type as `sapi_read_post_data` does.
//!
//! The four, in the stub's order.
//!
//! ```php
//! foreach (['http_build_query', 'http_get_last_response_headers', 'http_clear_last_response_headers'] as $f) echo new ReflectionFunction($f);
//! ```
//! ```expect
//! Function [ <internal:standard> function http_build_query ] {
//!
//!   - Parameters [4] {
//!     Parameter #0 [ <required> object|array $data ]
//!     Parameter #1 [ <optional> string $numeric_prefix = "" ]
//!     Parameter #2 [ <optional> ?string $arg_separator = null ]
//!     Parameter #3 [ <optional> int $encoding_type = PHP_QUERY_RFC1738 ]
//!   }
//!   - Return [ string ]
//! }
//! Function [ <internal:standard> function http_get_last_response_headers ] {
//!
//!   - Parameters [0] {
//!   }
//!   - Return [ ?array ]
//! }
//! Function [ <internal:standard> function http_clear_last_response_headers ] {
//!
//!   - Parameters [0] {
//!   }
//!   - Return [ void ]
//! }
//! ```
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use std::ops::ControlFlow;

use php_ext::abi::{Raw, Union};
use php_ext::attr::php_function;
use php_ext::coerce::{value_name, with_string};
use php_ext::decl::{ClassKind, EnumBacking};
use php_ext::raising::{Thrown, arg_type_error, throw_error, value_error, warning};
use php_ext::request::{BodyLimits, Ini, Request};
use php_ext::values::walk;
use php_ext::values::walk::Hold;
use php_ext::values::{
    ClassId, Key, Owner, PhpArray, PhpObject, PhpString, SmartStr, Value, ValueRef,
};

use crate::string::Decimal;
use crate::throwables::Throwables;
use crate::url::{PHP_QUERY_RFC3986, url_encode};

/// The limits `$options` may name, `REQUEST_PARSE_BODY_OPTION_*`, each
/// standing in for the ini entry of its name while the body is parsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Limit {
    MaxFileUploads,
    MaxInputVars,
    MaxMultipartBodyParts,
    PostMaxSize,
    UploadMaxFilesize,
}

impl Limit {
    /// `CHECK_OPTION`'s `zend_string_equals_literal_ci`.
    fn named(key: &[u8]) -> Option<Limit> {
        [
            (&b"max_file_uploads"[..], Limit::MaxFileUploads),
            (b"max_input_vars", Limit::MaxInputVars),
            (b"max_multipart_body_parts", Limit::MaxMultipartBodyParts),
            (b"post_max_size", Limit::PostMaxSize),
            (b"upload_max_filesize", Limit::UploadMaxFilesize),
        ]
        .into_iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(key))
        .map(|(_, limit)| limit)
    }
}

/// `request_parse_body(?array $options = null): array`, php's
/// `PHP_FUNCTION(request_parse_body)`: `$options` held to the five
/// limits it may name, each an integer or a quantity string read as an
/// ini entry reads one (its warning raised), then the request's content
/// type, and the body parsed by the handler registered for it into the
/// `[$_POST, $_FILES]` pair. A request with no content type, the CLI's,
/// and one of a type no handler is registered for are
/// `RequestParseBodyException`s.
///
/// ```php
/// foreach ([['foo' => 1], [42 => 1], ['' => 1], ['max_input_vars' => []], ['UPLOAD_MAX_FILESIZE' => '1GB', 'post_max_size' => 5], null] as $options) {
///     try { request_parse_body($options); } catch (Throwable $e) { echo get_class($e), ': ', $e->getMessage(), "\n"; }
/// }
/// ```
/// ```expect
/// ValueError: Invalid key "foo" in $options argument
/// ValueError: Invalid integer key in $options argument
/// ValueError: Invalid empty string key in $options argument
/// ValueError: Invalid array value in $options argument
///
/// Warning: Invalid quantity "1GB": unknown multiplier "B", interpreting as "1" for backwards compatibility in %s on line 3
/// RequestParseBodyException: Request does not provide a content type
/// RequestParseBodyException: Request does not provide a content type
/// ```
#[php_function]
fn request_parse_body(
    req: &mut Request,
    #[php(default = null)] options: Option<PhpArray>,
) -> Result<Owner<PhpArray>, Thrown> {
    let mut limits = BodyLimits::default();
    if let Some(options) = options {
        for (limit, quantity) in body_limits(req, options)? {
            let slot = match limit {
                Limit::MaxFileUploads => &mut limits.max_file_uploads,
                Limit::MaxInputVars => &mut limits.max_input_vars,
                Limit::MaxMultipartBodyParts => &mut limits.max_multipart_body_parts,
                Limit::PostMaxSize => &mut limits.post_max_size,
                Limit::UploadMaxFilesize => &mut limits.upload_max_filesize,
            };
            *slot = Some(quantity);
        }
    }
    let (post, files) = req.variables_mut().parse_body(&limits)?;
    let mut pair = PhpArray::new(req)?;
    pair.append(req, post.into_value())?;
    pair.append(req, files.into_value())?;
    Ok(pair)
}

/// `cache_request_parse_body_options` (`Z/ext/standard/http.c:279`):
/// every key a string naming one of the five limits, every value an
/// integer or a quantity string, each refusal php's `ValueError`.
fn body_limits(req: &mut Request, options: PhpArray) -> Result<Vec<(Limit, i64)>, Thrown> {
    let mut limits = Vec::new();
    // `ZEND_HASH_FOREACH_STR_KEY_VAL(options, key, value)`
    // (`Z/ext/standard/http.c:283`), no owner of the table taken: the
    // argument holds it while a quantity's warning reaches the program.
    walk::foreach(req, options, Hold::None, |req, row| -> walk::Step {
        let key = row.key().clone();
        let value = row.value().deref();
        let key = match key {
            Key::Int(_) => {
                return Err(value_error(req, "Invalid integer key in $options argument"));
            }
            Key::Str(key) => key,
        };
        if key.is_empty() {
            return Err(value_error(
                req,
                "Invalid empty string key in $options argument",
            ));
        }
        let Some(limit) = Limit::named(key.as_bytes()) else {
            let text = format!(
                "Invalid key \"{}\" in $options argument",
                String::from_utf8_lossy(key.as_bytes())
            );
            return Err(value_error(req, &text));
        };
        let value = value.deref();
        let quantity = match value.as_ref() {
            ValueRef::Str(text) => {
                let (quantity, said) = Ini::parse_quantity(text.as_bytes());
                if let Some(said) = said {
                    warning(req, &said);
                }
                quantity
            }
            ValueRef::Int(n) => n,
            _ => {
                let text = [
                    b"Invalid ".as_slice(),
                    &value_name(req, &value),
                    b" value in $options argument",
                ]
                .concat();
                return Err(value_error(req, &text));
            }
        };
        limits.push((limit, quantity));
        Ok(ControlFlow::Continue(()))
    })?;
    Ok(limits)
}

/// Whether a value is an enum case, and its class's backing.
fn enum_backing(req: &Request, object: PhpObject) -> Option<EnumBacking> {
    req.symbols()
        .class_decl(object.class())
        .filter(|decl| decl.kind == ClassKind::Enum)
        .map(|decl| decl.backing)
}

/// One pass of `php_url_encode_hash_ex`'s walk.
struct Query<'a> {
    /// `formstr`, the `smart_str` the walk appends to.
    out: SmartStr,
    separator: &'a [u8],
    raw: bool,
}

impl Query<'_> {
    /// `php_url_encode_scalar`: `key=value`, the separator before any
    /// but the first. A call of its own, so what it takes is off the
    /// stack of a nest walked level by level (#3159).
    #[inline(never)]
    fn scalar(
        &mut self,
        req: &mut Request,
        value: &Value,
        key: &KeyText,
        num_prefix: &[u8],
        key_prefix: Option<&[u8]>,
    ) -> Result<(), Thrown> {
        let mut value = *value;
        // An enum case: its backing value, or php's `ValueError`.
        if let ValueRef::Object(object) = value.as_ref()
            && let Some(backing) = enum_backing(req, object)
        {
            if backing == EnumBacking::Pure {
                let name = req
                    .symbols()
                    .class_decl(object.class())
                    .map(|decl| String::from_utf8_lossy(&decl.name.0).into_owned())
                    .unwrap_or_default();
                return Err(value_error(
                    req,
                    &format!("Unbacked enum {name} cannot be converted to a string"),
                ));
            }
            value = object.prop(req, b"value").unwrap_or_default();
        }
        let out = &mut self.out;
        if !out.as_bytes().is_empty() {
            out.append(req, self.separator)?;
        }
        if let Some(prefix) = key_prefix {
            out.append(req, prefix)?;
        }
        let mut encoded = Vec::new();
        match key {
            KeyText::Name(name) => {
                url_encode(name, self.raw, &mut encoded);
                out.append(req, &encoded)?;
            }
            KeyText::Index(index) => {
                out.append(req, num_prefix)?;
                out.append(req, Decimal::of(*index).as_bytes())?;
            }
        }
        if key_prefix.is_some() {
            out.append(req, b"%5D")?;
        }
        out.append(req, b"=")?;
        match value.deref().as_ref() {
            ValueRef::Int(n) => out.append(req, Decimal::of(n).as_bytes()),
            ValueRef::Bool(b) => out.append(req, if b { b"1" } else { b"0" }),
            _ => {
                encoded.clear();
                with_string(req, &value, |s| url_encode(s, self.raw, &mut encoded))?;
                out.append(req, &encoded)
            }
        }
    }

    /// `php_url_encode_hash_ex` over `rows`, an array's entries or an
    /// object's visible properties, under the numeric and the key
    /// prefix. Each level opens with `php_url_check_stack_limit`
    /// (`Z/ext/standard/http.c:86-114`): where the machine stack has
    /// come down to `zend.max_allowed_stack_size`, php's `Error`
    /// `Maximum call stack size reached.` and nothing more of the level.
    ///
    /// Under the small-stack regime a nest 50,000 deep reaches the ini's
    /// 256K in both engines:
    ///
    /// ```php
    /// $q = []; $c = &$q; for ($i = 0; $i < 50000; $i++) { $c['a'] = []; $c = &$c['a']; } unset($c);
    /// try { var_dump(strlen(http_build_query($q))); } catch (Error $e) { echo $e->getMessage(), "\n"; }
    /// ```
    /// ```expect regime=small-stack
    /// Maximum call stack size reached.
    /// ```
    ///
    /// The guard is php's `GC_TRY_PROTECT_RECURSION(ht)` over the table
    /// being walked while a nested one is (`Z/ext/standard/http.c:213`),
    /// and the test at the top of each level is of the table it is
    /// handed: an object's property table, so an object whose rows lead
    /// back to it stops there, the outer walk's own rows still written.
    ///
    /// ```php
    /// class S { public $o; function __toString(): string { $this->o->b = 'replaced'; $this->o->c = 'zz'; return "s"; } }
    /// $o = new stdClass;
    /// $s = new S; $s->o = $o;
    /// $o->a = $s;
    /// $o->b = str_repeat('x', 40);
    /// $o->c = str_repeat('y', 40);
    /// echo http_build_query($o), "\n";
    /// $a = ['a' => $s, 'b' => str_repeat('x', 40)];
    /// $s2 = new class { public $arr; function __toString(): string { global $a; $a['b'] = 'new'; return 't'; } };
    /// $a['a'] = $s2;
    /// echo http_build_query($a), "\n";
    /// ```
    /// ```expect
    /// b=xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx&c=yyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyyy
    /// b=xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx
    /// ```
    fn hash(
        &mut self,
        req: &mut Request,
        table: walk::Table,
        object: Option<PhpObject>,
        num_prefix: &[u8],
        key_prefix: Option<&[u8]>,
    ) -> Result<(), Thrown> {
        // `GC_IS_RECURSIVE(ht)`: a table already being walked is
        // skipped where it meets itself again, before the stack is asked.
        if table.array().is_recursive() {
            return Ok(());
        }
        if req.limits().stack_overflowed() {
            let ce = Throwables::of(req)?;
            return Err(throw_error(
                req,
                ce.error,
                "Maximum call stack size reached.",
            ));
        }
        let scope = req.frames().scope();
        // The table is walked with no owner of it taken: what holds it
        // keeps it while code of the program runs under the walk, and
        // where that code frees it, the walk ends there. A declared slot
        // nothing holds is stepped over.
        let walked = walk::foreach_guarded(req, table, Hold::None, |req, row| -> walk::Step<()> {
            let key = match row.key() {
                Key::Int(n) => KeyText::Index(*n),
                Key::Str(name) => {
                    let name = name.as_bytes();
                    match object {
                        Some(object) if name.first() == Some(&0) => {
                            match visible_name(req, object, scope, name) {
                                Some(name) => KeyText::Name(name),
                                None => return Ok(ControlFlow::Continue(())),
                            }
                        }
                        _ => KeyText::Name(name.to_vec()),
                    }
                }
            };
            self.row(req, &key, *row.value(), num_prefix, key_prefix)?;
            Ok(ControlFlow::Continue(()))
        })?;
        match walked {
            // A walk inside the table already: nothing, as the test above.
            walk::Guarded::Walked(_) | walk::Guarded::Recursive => Ok(()),
        }
    }

    /// One row of [`Query::hash`]'s walk: a nested array or object
    /// walked under the row's prefix, nothing for `null` or a resource,
    /// and a scalar written.
    fn row(
        &mut self,
        req: &mut Request,
        key: &KeyText,
        value: Value,
        num_prefix: &[u8],
        key_prefix: Option<&[u8]>,
    ) -> Result<(), Thrown> {
        let value = value.deref();
        let nested = match value.as_ref() {
            ValueRef::Array(array) => Some((walk::Table::Array(array), None)),
            ValueRef::Object(object) if enum_backing(req, object).is_none() => {
                // `HASH_OF(zdata)`, the object's `Z_OBJPROP`.
                Some((object.property_table(req)?, Some(object)))
            }
            _ => None,
        };
        if let Some((table, object)) = nested {
            let prefix = self.nested_prefix(key, num_prefix, key_prefix);
            return self.hash(req, table, object, b"", Some(&prefix));
        }
        if matches!(value.as_ref(), ValueRef::Null | ValueRef::Resource(_)) {
            return Ok(());
        }
        self.scalar(req, &value, key, num_prefix, key_prefix)?;
        Ok(())
    }

    /// The key prefix a nested array or object under `key` is walked
    /// with: `key%5B` at the top, `outer key%5D%5B` below it, an integer
    /// key behind the numeric prefix. A call of its own, so what building
    /// it takes is off the stack of a nest walked level by level (#3159).
    #[inline(never)]
    fn nested_prefix(
        &self,
        key: &KeyText,
        num_prefix: &[u8],
        key_prefix: Option<&[u8]>,
    ) -> Vec<u8> {
        let mut prefix: Vec<u8> = Vec::new();
        match key {
            KeyText::Name(name) => {
                let mut encoded = Vec::new();
                url_encode(name, self.raw, &mut encoded);
                if let Some(outer) = key_prefix {
                    prefix.extend_from_slice(outer);
                    prefix.extend_from_slice(&encoded);
                    prefix.extend_from_slice(b"%5D%5B");
                } else {
                    prefix.extend_from_slice(&encoded);
                    prefix.extend_from_slice(b"%5B");
                }
            }
            KeyText::Index(index) => {
                let digits = Decimal::of(*index);
                if let Some(outer) = key_prefix {
                    prefix.extend_from_slice(outer);
                    prefix.extend_from_slice(num_prefix);
                    prefix.extend_from_slice(digits.as_bytes());
                    prefix.extend_from_slice(b"%5D%5B");
                } else {
                    prefix.extend_from_slice(num_prefix);
                    prefix.extend_from_slice(digits.as_bytes());
                    prefix.extend_from_slice(b"%5B");
                }
            }
        }
        prefix
    }
}

/// A key as `php_url_encode_hash_ex` writes it: a name, unmangled for
/// a property, or an integer index.
enum KeyText {
    Name(Vec<u8>),
    Index(i64),
}

/// A mangled property name of `object`'s table as `zend_check_property_access`
/// lets the calling scope see it, unmangled
/// (`zend_unmangle_property_name_ex`): a protected one from a scope the
/// object is an instance of, a private one from its declaring class;
/// `None` for one the scope may not see. A call of its own, so the name
/// it builds is off the stack of a nest walked level by level (#3159).
#[inline(never)]
fn visible_name(
    req: &mut Request,
    object: PhpObject,
    scope: Option<ClassId>,
    name: &[u8],
) -> Option<Vec<u8>> {
    let mut parts = name.get(1..)?.splitn(2, |c| *c == 0);
    let class = parts.next().unwrap_or(&[]);
    let prop = parts.next().unwrap_or(&[]).to_vec();
    let visible = match scope {
        None => false,
        Some(scope) if class == b"*" => object.instance_of(req, scope),
        Some(scope) => req
            .symbols()
            .class_decl(scope)
            .is_some_and(|decl| decl.name.0.eq_ignore_ascii_case(class)),
    };
    visible.then_some(prop)
}

/// `http_build_query(array|object $data, ...): string`, the stub's
/// four parameters: the data as a form-encoded query, a
/// nested array or object as `key[inner]` in `%5B` and `%5D`, the
/// numeric prefix before a top-level integer key, `null` and resources
/// left out, and `arg_separator.output` between the pairs unless the
/// call names one. An enum case is its backing value, and an unbacked
/// one php's `ValueError`; this compiler reads no enum declaration yet,
/// so no witness holds that arm.
///
/// ```php
/// var_dump(http_build_query(["a" => 1, "b" => "x y&z", "c" => [1, 2, "k" => ["d" => true]], "e" => null, "f" => false, 3 => 1.5]), http_build_query([1, 2, "x" => [3]], "n_"), http_build_query(["a" => "b c", "d" => "~"], "", "&amp;", PHP_QUERY_RFC3986), http_build_query([]), http_build_query(["é" => "é"]));
/// class P { public $pub = 1; protected $pro = 2; private $pri = 3; public $arr = ["x" => 4]; function q() { return http_build_query($this); } }
/// var_dump(http_build_query(new P), (new P)->q(), http_build_query(["o" => new P]));
/// try { http_build_query("x"); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(63) "a=1&b=x+y%26z&c%5B0%5D=1&c%5B1%5D=2&c%5Bk%5D%5Bd%5D=1&f=0&3=1.5"
/// string(22) "n_0=1&n_1=2&x%5B0%5D=3"
/// string(15) "a=b%20c&amp;d=~"
/// string(0) ""
/// string(13) "%C3%A9=%C3%A9"
/// string(18) "pub=1&arr%5Bx%5D=4"
/// string(30) "pub=1&pro=2&pri=3&arr%5Bx%5D=4"
/// string(32) "o%5Bpub%5D=1&o%5Barr%5D%5Bx%5D=4"
/// http_build_query(): Argument #1 ($data) must be of type array, string given
/// ```
///
/// The query is built in a `smart_str` and returned by
/// `smart_str_extract`, so the string kept is trimmed to its length at
/// `zend_string_finit`'s size, and the empty query is the interned empty
/// string:
///
/// ```php
/// class K { public $d = 1; }
/// foreach ([[], ['a' => 1], new K, ['abc' => 'def', 'g' => [1, 2]], array_fill(0, 40, 'xyz'), array_fill(0, 400, 'xyz')] as $q) {
///     $m = memory_get_usage(); $s = http_build_query($q); echo strlen($s), ' ', memory_get_usage() - $m, "\n"; unset($s);
/// }
/// ```
/// ```expect
/// 0 32
/// 3 40
/// 3 416
/// 29 64
/// 269 320
/// 3089 4096
/// ```
#[php_function]
fn http_build_query(
    req: &mut Request,
    data: Raw<Union<PhpObject, PhpArray>>,
    #[php(default = "")] numeric_prefix: PhpString,
    #[php(default = null)] arg_separator: Option<PhpString>,
    #[php(default = PHP_QUERY_RFC1738)] encoding_type: i64,
) -> Result<Owner<PhpString>, Thrown> {
    let data = data.value();
    let (table, object) = match data.as_ref() {
        ValueRef::Array(array) => (walk::Table::Array(array), None),
        ValueRef::Object(object) => {
            if enum_backing(req, object).is_some() {
                let name = value_name(req, &data);
                return Err(arg_type_error(
                    req,
                    1,
                    &[b"not be an enum, ".as_slice(), &name, b" given"].concat(),
                ));
            }
            (object.property_table(req)?, Some(object))
        }
        _ => {
            let name = value_name(req, &data);
            return Err(arg_type_error(
                req,
                1,
                // `Z_PARAM_ARRAY_OR_OBJECT` names `array` alone.
                &[b"be of type array, ".as_slice(), &name, b" given"].concat(),
            ));
        }
    };
    let separator: Vec<u8> = match arg_separator {
        Some(s) => s.as_bytes().to_vec(),
        None => req
            .ini()
            .get(b"arg_separator.output")
            .filter(|v| !v.is_empty())
            .map_or_else(|| b"&".to_vec(), <[u8]>::to_vec),
    };
    let mut query = Query {
        out: SmartStr::default(),
        separator: &separator,
        raw: encoding_type == PHP_QUERY_RFC3986,
    };
    let num_prefix = numeric_prefix.as_bytes().to_vec();
    // `smart_str_free(&formstr)` before `RETURN_THROWS()`, and
    // `RETURN_STR(smart_str_extract(&formstr))` (`Z/ext/standard/http.c:248`):
    // the walk's buffer trimmed to the query, `zend_string_finit`'s size
    // in php-8.5.12.
    if let Err(thrown) = query.hash(req, table, object, &num_prefix, None) {
        query.out.free(req);
        return Err(thrown);
    }
    query.out.extract(req)
}

/// `http_get_last_response_headers(): ?array`
/// (`Z/ext/standard/http.c:375`): the headers the http wrapper's last
/// request read, `BG(last_http_headers)`, and `null` before one read
/// any; the headers themselves are witnessed beside the wrapper.
///
/// ```php
/// var_dump(http_get_last_response_headers());
/// ```
/// ```expect
/// NULL
/// ```
#[php_function]
fn http_get_last_response_headers(req: &mut Request) -> Option<PhpArray> {
    crate::http_fopen_wrapper::last_headers(req)
}

/// `http_clear_last_response_headers(): void`: the headers forgotten.
///
/// ```php
/// var_dump(http_clear_last_response_headers(), http_get_last_response_headers());
/// ```
/// ```expect
/// NULL
/// NULL
/// ```
#[php_function]
fn http_clear_last_response_headers(req: &mut Request) {
    crate::http_fopen_wrapper::clear_last_headers(req);
}
