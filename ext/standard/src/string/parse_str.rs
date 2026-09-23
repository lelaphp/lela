//! `parse_str` over `php_default_treat_data(PARSE_STRING, ...)` and
//! `php_register_variable_ex` (`Z/main/php_variables.c:108,462`): the
//! query split on `arg_separator.input` as `php_strtok_r` splits it,
//! each name and value `php_url_decode`d, and each name mangled and
//! walked into nested arrays by its brackets, as `$_GET` is built.

use php_ext::raising::{Level, Thrown, docref};
use php_ext::request::Request;
use php_ext::values::{Key, PhpArray, PhpString, Value, ValueRef};

use crate::url::php_url_decode;
use crate::zpp::string_value;
use crate::{argument_count_between, string_argument};

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
pub(super) fn parse_str(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 2, 2)?;
    let string = string_argument(req, 1)?;
    let target = req.argument(2).unwrap_or_default();
    let ValueRef::Reference(result) = target.as_ref() else {
        return Ok(Value::null());
    };
    let result = *result;
    let array = PhpArray::new(req)?;
    treat_data(req, string.as_bytes(), array)?;
    result.set(req, Value::array(array));
    Ok(Value::null())
}

/// `php_default_treat_data` for `PARSE_STRING`.
fn treat_data(req: &mut Request, query: &[u8], array: PhpArray) -> Result<(), Thrown> {
    let query = query.split(|c| *c == 0).next().unwrap_or(&[]);
    let separator: Vec<u8> = req
        .ini_get(b"arg_separator.input")
        .map_or_else(|| b"&".to_vec(), <[u8]>::to_vec);
    let max_input_vars = req
        .ini_get(b"max_input_vars")
        .and_then(|v| std::str::from_utf8(v).ok()?.trim().parse::<i64>().ok())
        .unwrap_or(1000);
    let mut count = 0i64;
    for var in query
        .split(|c| separator.contains(c))
        .filter(|piece| !piece.is_empty())
    {
        count += 1;
        if count > max_input_vars {
            docref(
                req,
                Level::Warning,
                &format!(
                    "Input variables exceeded {max_input_vars}. To increase the limit change max_input_vars in php.ini."
                ),
            );
            break;
        }
        let (name, value) = match var.iter().position(|c| *c == b'=') {
            Some(eq) => (&var[..eq], php_url_decode(&var[eq + 1..])),
            None => (var, Vec::new()),
        };
        let name = php_url_decode(name);
        let value = string_value(req, &value)?;
        register_variable(req, &name, value, array)?;
    }
    Ok(())
}

/// `php_is_forbidden_variable_name`: a name the mangling turned into a
/// `__Host-` or `__Secure-` cookie prefix it did not have.
fn forbidden(mangled: &[u8], original: &[u8]) -> bool {
    [&b"__Host-"[..], b"__Secure-"]
        .iter()
        .any(|prefix| mangled.starts_with(prefix) && !original.starts_with(prefix))
}

/// The slot one step of the walk writes into: the next index, or a
/// symtable key.
fn slot_key(req: &mut Request, index: &[u8]) -> Result<Key, Thrown> {
    Ok(Key::from_string(PhpString::new(req, index)?))
}

/// `php_register_variable_ex(var_name, val, track_vars_array)`.
fn register_variable(
    req: &mut Request,
    var_name: &[u8],
    value: Value,
    track: PhpArray,
) -> Result<(), Thrown> {
    // The name is a C string: it ends at its first NUL.
    let var_name = var_name.split(|c| *c == 0).next().unwrap_or(&[]);
    let skip = var_name.iter().take_while(|c| **c == b' ').count();
    let original = &var_name[skip..];
    let mut var = original.to_vec();
    let mut bracket = None;
    for (i, c) in var.iter_mut().enumerate() {
        if *c == b' ' || *c == b'.' {
            *c = b'_';
        } else if *c == b'[' {
            bracket = Some(i);
            break;
        }
    }
    let var_len = bracket.unwrap_or(var.len());
    if var_len == 0 {
        req.release(value);
        return Ok(());
    }
    let max_nesting = req
        .ini_get(b"max_input_nesting_level")
        .and_then(|v| std::str::from_utf8(v).ok()?.trim().parse::<i64>().ok())
        .unwrap_or(64);
    // The walk: `index` is the key the next write lands at, `None`
    // for `[]`; `path` the containers entered so far.
    let mut index: Option<Vec<u8>> = Some(var[..var_len].to_vec());
    let Some(mut ip) = bracket else {
        return plain(req, track, index.as_deref(), original, value);
    };
    let mut path: Vec<Key> = Vec::new();
    let mut nest = 0i64;
    loop {
        nest += 1;
        if nest > max_nesting {
            let top = slot_key(req, &var[..var_len])?;
            let mut top_level = track;
            top_level.unset(req, &top)?;
            req.release(value);
            return Ok(());
        }
        ip += 1;
        let index_s = ip;
        if var
            .get(ip)
            .is_some_and(|c| matches!(c, b' ' | b'\t' | b'\n' | 0x0b | 0x0c | b'\r'))
        {
            ip += 1;
        }
        let new_index: Option<Vec<u8>> = if var.get(ip) == Some(&b']') {
            None
        } else if let Some(close) = var[ip.min(var.len())..].iter().position(|c| *c == b']') {
            ip += close;
            Some(var[index_s..ip].to_vec())
        } else {
            // Not an index: the `[` becomes `_`, and so does every ` `,
            // `.` and `[` after it. php writes over its buffer, so the
            // name grows only at the top level; a nested index ends at
            // the NUL its own `]` was overwritten with, and stays as it
            // was.
            let mut name = index.take();
            if path.is_empty() {
                let grown = name.get_or_insert_with(Vec::new);
                grown.push(b'_');
                for &c in &var[index_s..] {
                    grown.push(if matches!(c, b' ' | b'.' | b'[') {
                        b'_'
                    } else {
                        c
                    });
                }
            }
            return write_at(req, track, &path, name.as_deref(), original, value);
        };
        // Enter, or make, the container at `index`.
        let key = match &index {
            None => None,
            Some(index) => {
                if forbidden(index, original) {
                    req.release(value);
                    return Ok(());
                }
                Some(slot_key(req, index)?)
            }
        };
        let Some(key) = enter(req, track, &path, key)? else {
            req.release(value);
            return Ok(());
        };
        path.push(key);
        index = new_index;
        ip += 1;
        if var.get(ip) != Some(&b'[') {
            return write_at(req, track, &path, index.as_deref(), original, value);
        }
    }
}

/// The array at `path` inside `track`. Every container the walk
/// makes has the one owner that holds it, so a write through this
/// name of it lands in place, where php's `SEPARATE_ARRAY` would find
/// nothing to separate.
fn descend(track: PhpArray, path: &[Key]) -> PhpArray {
    let mut current = track;
    for key in path {
        match current.get(key).and_then(|v| v.as_array().copied()) {
            Some(next) => current = next,
            None => break,
        }
    }
    current
}

/// A key's second owner, for a write that gives one up.
fn owned(req: &mut Request, key: &Key) -> Key {
    match key {
        Key::Int(n) => Key::Int(*n),
        Key::Str(s) => {
            req.addref(&Value::string(*s));
            Key::Str(*s)
        }
    }
}

/// One step into the walk: the container at `key` (the next index for
/// `None`), an array made where it is none, and the key it now stands
/// under; `None` where the next index is taken.
fn enter(
    req: &mut Request,
    track: PhpArray,
    path: &[Key],
    key: Option<Key>,
) -> Result<Option<Key>, Thrown> {
    let mut parent = descend(track, path);
    let Some(key) = key else {
        let fresh = PhpArray::new(req)?;
        if !parent.try_append(req, Value::array(fresh))? {
            return Ok(None);
        }
        let last = parent.snapshot().last().map(|(k, _)| owned(req, k));
        return Ok(last);
    };
    if parent.get(&key).is_none_or(|v| v.as_array().is_none()) {
        let fresh = PhpArray::new(req)?;
        let slot = owned(req, &key);
        parent.set(req, slot, Value::array(fresh))?;
    }
    Ok(Some(key))
}

/// The `plain_var` write at the top level.
fn plain(
    req: &mut Request,
    track: PhpArray,
    index: Option<&[u8]>,
    original: &[u8],
    value: Value,
) -> Result<(), Thrown> {
    write_at(req, track, &[], index, original, value)
}

/// `plain_var:`: the value appended, or written at `index` as a
/// symtable key, in the container at `path`.
fn write_at(
    req: &mut Request,
    track: PhpArray,
    path: &[Key],
    index: Option<&[u8]>,
    original: &[u8],
    value: Value,
) -> Result<(), Thrown> {
    let mut container = descend(track, path);
    match index {
        None => {
            if !container.try_append(req, value)? {
                return Ok(());
            }
        }
        Some(index) => {
            if forbidden(index, original) {
                req.release(value);
                return Ok(());
            }
            let key = slot_key(req, index)?;
            container.set(req, key, value)?;
        }
    }
    Ok(())
}
