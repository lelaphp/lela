//! `Z/ext/standard/http.c`: `http_build_query` over
//! `php_url_encode_hash_ex`, and the two natives over the headers the
//! http wrapper's last response left. This engine's streams have no
//! http wrapper yet, so no request ever leaves headers and the first
//! answers `null` as php does before one; `request_parse_body` reads
//! the request body, which the host face does not hand an extension,
//! and waits on it.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use php_ext::coerce::{to_string, value_name};
use php_ext::decl::{ClassKind, EnumBacking, FunctionDecl};
use php_ext::raising::{Thrown, arg_type_error, throw};
use php_ext::request::Request;
use php_ext::values::{Key, PhpArray, PhpObject, Value, ValueRef};

use crate::argument_count_between;
use crate::output::Entry;
use crate::stub::stub;
use crate::url::{PHP_QUERY_RFC3986, url_encode};
use crate::zpp::{nullable_string, optional_long, optional_string, string_value};

/// The three, in the stub's order.
///
/// ```php
/// foreach (['http_build_query', 'http_get_last_response_headers', 'http_clear_last_response_headers'] as $f) echo new ReflectionFunction($f);
/// ```
/// ```expect
/// Function [ <internal:standard> function http_build_query ] {
///
///   - Parameters [4] {
///     Parameter #0 [ <required> object|array $data ]
///     Parameter #1 [ <optional> string $numeric_prefix = "" ]
///     Parameter #2 [ <optional> ?string $arg_separator = null ]
///     Parameter #3 [ <optional> int $encoding_type = PHP_QUERY_RFC1738 ]
///   }
///   - Return [ string ]
/// }
/// Function [ <internal:standard> function http_get_last_response_headers ] {
///
///   - Parameters [0] {
///   }
///   - Return [ ?array ]
/// }
/// Function [ <internal:standard> function http_clear_last_response_headers ] {
///
///   - Parameters [0] {
///   }
///   - Return [ void ]
/// }
/// ```
pub(crate) fn natives() -> Vec<(FunctionDecl, Entry)> {
    vec![
        (
            stub(
                "http_build_query(object|array $data, string $numeric_prefix = \"\", ?string $arg_separator = null, int $encoding_type = PHP_QUERY_RFC1738): string",
            ),
            http_build_query as Entry,
        ),
        (
            stub("http_get_last_response_headers(): ?array"),
            http_get_last_response_headers,
        ),
        (
            stub("http_clear_last_response_headers(): void"),
            http_clear_last_response_headers,
        ),
    ]
}

/// Whether a value is an enum case, and its class's backing.
fn enum_backing(req: &Request, object: PhpObject) -> Option<EnumBacking> {
    req.class_decl(object.class())
        .filter(|decl| decl.kind == ClassKind::Enum)
        .map(|decl| decl.backing)
}

/// One pass of `php_url_encode_hash_ex`'s walk.
struct Query<'a> {
    out: Vec<u8>,
    separator: &'a [u8],
    raw: bool,
}

impl Query<'_> {
    /// `php_url_encode_scalar`: `key=value`, the separator before any
    /// but the first.
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
            && let Some(backing) = enum_backing(req, *object)
        {
            if backing == EnumBacking::Pure {
                let name = req
                    .class_decl(object.class())
                    .map(|decl| String::from_utf8_lossy(&decl.name.0).into_owned())
                    .unwrap_or_default();
                return Err(throw(
                    req,
                    "ValueError",
                    &format!("Unbacked enum {name} cannot be converted to a string"),
                ));
            }
            value = object.prop(req, b"value").unwrap_or_default();
        }
        if !self.out.is_empty() {
            self.out.extend_from_slice(self.separator);
        }
        if let Some(prefix) = key_prefix {
            self.out.extend_from_slice(prefix);
        }
        match key {
            KeyText::Name(name) => url_encode(name, self.raw, &mut self.out),
            KeyText::Index(index) => {
                self.out.extend_from_slice(num_prefix);
                self.out.extend_from_slice(index.to_string().as_bytes());
            }
        }
        if key_prefix.is_some() {
            self.out.extend_from_slice(b"%5D");
        }
        self.out.push(b'=');
        match value.deref().as_ref() {
            ValueRef::Int(n) => self.out.extend_from_slice(n.to_string().as_bytes()),
            ValueRef::Bool(b) => self.out.push(if b { b'1' } else { b'0' }),
            _ => {
                let s = to_string(req, &value.deref())?;
                url_encode(s.as_bytes(), self.raw, &mut self.out);
            }
        }
        Ok(())
    }

    /// `php_url_encode_hash_ex` over `rows`, an array's entries or an
    /// object's visible properties, under the numeric and the key
    /// prefix.
    fn hash(
        &mut self,
        req: &mut Request,
        rows: Vec<(KeyText, Value)>,
        num_prefix: &[u8],
        key_prefix: Option<&[u8]>,
    ) -> Result<(), Thrown> {
        for (key, value) in rows {
            let value = value.deref();
            let nested = match value.as_ref() {
                ValueRef::Array(array) => Some(Nested::Array(*array)),
                ValueRef::Object(object) if enum_backing(req, *object).is_none() => {
                    Some(Nested::Object(*object))
                }
                _ => None,
            };
            if let Some(nested) = nested {
                let mut prefix: Vec<u8> = Vec::new();
                match &key {
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
                        let digits = index.to_string();
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
                // `GC_IS_RECURSIVE`: an array already being walked is
                // skipped where it meets itself again.
                match nested {
                    Nested::Array(array) => {
                        if array.is_recursive() {
                            continue;
                        }
                        array.protect();
                        let rows = array_rows(array);
                        let walked = self.hash(req, rows, b"", Some(&prefix));
                        array.unprotect();
                        walked?;
                    }
                    Nested::Object(object) => {
                        if object.is_recursive() {
                            continue;
                        }
                        object.protect();
                        let rows = object_rows(req, object);
                        let walked = match rows {
                            Ok(rows) => self.hash(req, rows, b"", Some(&prefix)),
                            Err(thrown) => Err(thrown),
                        };
                        object.unprotect();
                        walked?;
                    }
                }
                continue;
            }
            if matches!(value.as_ref(), ValueRef::Null | ValueRef::Resource(_)) {
                continue;
            }
            self.scalar(req, &value, &key, num_prefix, key_prefix)?;
        }
        Ok(())
    }
}

/// A key as `php_url_encode_hash_ex` writes it: a name, unmangled for
/// a property, or an integer index.
enum KeyText {
    Name(Vec<u8>),
    Index(i64),
}

/// What a nested value is walked as.
enum Nested {
    Array(PhpArray),
    Object(PhpObject),
}

/// An array's rows.
fn array_rows(array: PhpArray) -> Vec<(KeyText, Value)> {
    array
        .snapshot()
        .into_iter()
        .map(|(key, value)| match key {
            Key::Int(n) => (KeyText::Index(n), value),
            Key::Str(s) => (KeyText::Name(s.as_bytes().to_vec()), value),
        })
        .collect()
}

/// An object's properties the calling scope may read,
/// `zend_check_property_access` over the mangled names: a public or
/// dynamic one always, a protected one from a scope the object is an
/// instance of, a private one from its declaring class; each unmangled.
fn object_rows(req: &mut Request, object: PhpObject) -> Result<Vec<(KeyText, Value)>, Thrown> {
    let scope = req.scope();
    let mut rows = Vec::new();
    for (key, value) in object.entries(req)? {
        let name = match key {
            Key::Int(n) => {
                rows.push((KeyText::Index(n), value));
                continue;
            }
            Key::Str(s) => s.as_bytes().to_vec(),
        };
        if name.first() != Some(&0) {
            rows.push((KeyText::Name(name), value));
            continue;
        }
        let mut parts = name[1..].splitn(2, |c| *c == 0);
        let class = parts.next().unwrap_or(&[]).to_vec();
        let prop = parts.next().unwrap_or(&[]).to_vec();
        let visible = match scope {
            None => false,
            Some(scope) if class == b"*" => object.instance_of(req, scope),
            Some(scope) => req
                .class_decl(scope)
                .is_some_and(|decl| decl.name.0.eq_ignore_ascii_case(&class)),
        };
        if visible {
            rows.push((KeyText::Name(prop), value));
        }
    }
    Ok(rows)
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
fn http_build_query(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 1, 4)?;
    let data = req.argument(1).unwrap_or_default().deref();
    let prefix = optional_string(req, 2)?;
    let separator = nullable_string(req, 3)?;
    let encoding = optional_long(req, 4, 1)?;
    let rows = match data.as_ref() {
        ValueRef::Array(array) => array_rows(*array),
        ValueRef::Object(object) => {
            if enum_backing(req, *object).is_some() {
                let name = value_name(req, &data);
                return Err(arg_type_error(
                    req,
                    1,
                    &format!("not be an enum, {name} given"),
                ));
            }
            object_rows(req, *object)?
        }
        _ => {
            let name = value_name(req, &data);
            return Err(arg_type_error(
                req,
                1,
                // `Z_PARAM_ARRAY_OR_OBJECT` names `array` alone.
                &format!("be of type array, {name} given"),
            ));
        }
    };
    let separator: Vec<u8> = match separator {
        Some(s) => s.as_bytes().to_vec(),
        None => req
            .ini_get(b"arg_separator.output")
            .filter(|v| !v.is_empty())
            .map_or_else(|| b"&".to_vec(), <[u8]>::to_vec),
    };
    let mut query = Query {
        out: Vec::new(),
        separator: &separator,
        raw: encoding == PHP_QUERY_RFC3986,
    };
    let num_prefix = prefix.as_ref().map_or(&b""[..], |p| p.as_bytes()).to_vec();
    query.hash(req, rows, &num_prefix, None)?;
    string_value(req, &query.out)
}

/// `http_get_last_response_headers(): ?array`: the headers the http
/// wrapper's last response left, none here.
///
/// ```php
/// var_dump(http_get_last_response_headers());
/// ```
/// ```expect
/// NULL
/// ```
fn http_get_last_response_headers(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 0, 0)?;
    Ok(Value::null())
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
fn http_clear_last_response_headers(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 0, 0)?;
    Ok(Value::null())
}
