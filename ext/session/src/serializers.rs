//! The three serializers of `session.c`, `php_serialize`, `php` and
//! `php_binary`, each over ext/standard's `php_var_serialize` and
//! `php_var_unserialize` with one var hash for the whole session, so a
//! value that is referenced from two session variables is written, and
//! read back, once.

use php_ext::raising::{Level, Thrown, docref};
use php_ext::request::Request;
use php_ext::values::{Key, PhpArray, PhpString, Value};
use php_ext_standard::{Serializer, Unserializer};

use crate::{Format, bind_array, session_array, session_array_mut, shown};

/// `PS_ENCODE_LOOP`: each variable under a string key, a numeric one
/// skipped with php's warning, over a copy of the array so the loop
/// holds against what a `__sleep` does to it.
fn variables(req: &mut Request, array: PhpArray) -> Vec<(PhpString, Value)> {
    let mut out = Vec::new();
    for (key, value) in array.snapshot_refs() {
        match key {
            Key::Str(name) => out.push((name, value)),
            Key::Int(n) => docref(req, Level::Warning, &format!("Skipping numeric key {n}")),
        }
    }
    out
}

/// `PS_SERIALIZER_ENCODE_FUNC`: the session's variables as the format
/// writes them, `None` where `php` meets a name with `|` in it, and
/// where `php` or `php_binary` wrote nothing, as php's `smart_str` stays
/// `NULL`.
///
/// ```php
/// ob_start(); session_start(["save_path" => __DIR__, "use_cookies" => 0, "cache_limiter" => ""]);
/// session_decode('a|i:1;b|a:1:{i:0;s:1:"x";}');
/// var_dump(session_encode());
/// session_write_close(); ini_set("session.serialize_handler", "php_serialize"); session_start();
/// var_dump(session_encode());
/// session_write_close(); ini_set("session.serialize_handler", "php_binary"); session_start();
/// var_dump(bin2hex(session_encode()));
/// session_decode("");
/// var_dump(session_encode());
/// session_write_close(); ob_end_flush();
/// ```
/// ```expect
/// string(26) "a|i:1;b|a:1:{i:0;s:1:"x";}"
///
/// Warning: session_start(): Failed to decode session object. Session has been destroyed in %s on line 5
/// string(6) "a:0:{}"
/// string(0) ""
/// bool(false)
/// ```
pub(super) fn encode(req: &mut Request, format: Format) -> Result<Option<Vec<u8>>, Thrown> {
    let Some(array) = session_array(req) else {
        return Ok(None);
    };
    let mut buf = Vec::new();
    let mut hash = Serializer::default();
    if format == Format::PhpSerialize {
        hash.write(req, &mut buf, &array.to_value())?;
        return Ok(Some(buf));
    }
    let copy = array.to_value();
    req.addref(&copy);
    let entries = variables(req, array);
    let mut wrote = false;
    for (name, value) in entries {
        let name = name.as_bytes().to_vec();
        if format == Format::PhpBinary {
            let Ok(len) = u8::try_from(name.len()) else {
                continue;
            };
            if len > 127 {
                continue;
            }
            buf.push(len);
            buf.extend_from_slice(&name);
        } else {
            buf.extend_from_slice(&name);
            if name.contains(&b'|') {
                req.release(copy);
                docref(
                    req,
                    Level::Warning,
                    &format!(
                        "Failed to write session data. Data contains invalid key \"{}\"",
                        shown(&name)
                    ),
                );
                return Ok(None);
            }
            buf.push(b'|');
        }
        wrote = true;
        if let Err(thrown) = hash.write(req, &mut buf, &value) {
            req.release(copy);
            return Err(thrown);
        }
    }
    req.release(copy);
    Ok(wrote.then_some(buf))
}

/// `PS_SERIALIZER_DECODE_FUNC`: the data read into the session's array,
/// `false` for data the format refuses, what was read before it kept.
///
/// ```php
/// ob_start(); session_start(["save_path" => __DIR__, "use_cookies" => 0, "cache_limiter" => ""]);
/// var_dump(session_decode('x|O:8:"stdClass":1:{s:1:"p";i:1;}y|r:1;z|R:1;'), session_encode());
/// var_dump(session_decode('bad'), session_status(), session_encode());
/// ob_end_flush();
/// ```
/// ```expect
/// bool(true)
/// string(45) "x|O:8:"stdClass":1:{s:1:"p";i:1;}y|r:1;z|R:1;"
///
/// Warning: session_decode(): Failed to decode session object. Session has been destroyed in %s on line 4
/// bool(false)
/// int(1)
/// bool(false)
/// ```
pub(super) fn decode(req: &mut Request, format: Format, data: &[u8]) -> Result<bool, Thrown> {
    let data = data.to_vec();
    if format == Format::PhpSerialize {
        let mut reader = Unserializer::new(req, &data);
        let read = reader.value(req);
        let finished = reader.finish(req);
        let read = read?;
        finished?;
        let ok = read.is_some();
        let array = match read {
            Some(value) if value.as_array().is_some() => value.as_array().copied(),
            Some(value) => {
                req.release(value);
                None
            }
            None => None,
        };
        let array = match array {
            Some(array) => array,
            None => PhpArray::new(req)?,
        };
        bind_array(req, array)?;
        return Ok(ok || data.is_empty());
    }
    let Some(array) = session_array_mut(req)? else {
        return Ok(true);
    };
    let mut reader = Unserializer::new(req, &data);
    let mut ok = true;
    let mut p = 0;
    while p < data.len() {
        let (name, at) = if format == Format::PhpBinary {
            let len = usize::from(data[p] & 0x7f);
            if p + len >= data.len() {
                ok = false;
                break;
            }
            (&data[p + 1..=p + len], p + len + 1)
        } else {
            let Some(q) = data[p..].iter().position(|&b| b == b'|') else {
                ok = false;
                break;
            };
            (&data[p..p + q], p + q + 1)
        };
        let name = PhpString::new(req, name)?;
        reader.set_offset(at);
        match reader.element(req, array, Key::Str(name)) {
            Ok(true) => p = reader.offset(),
            Ok(false) => {
                ok = false;
                break;
            }
            Err(thrown) => {
                let _ = reader.finish(req);
                return Err(thrown);
            }
        }
    }
    reader.finish(req)?;
    Ok(ok)
}
