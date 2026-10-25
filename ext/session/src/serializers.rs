//! The three serializers of `session.c`, `php_serialize`, `php` and
//! `php_binary`, each over ext/standard's `php_var_serialize` and
//! `php_var_unserialize` with one var hash for the whole session, so a
//! value that is referenced from two session variables is written, and
//! read back, once.

use std::ops::ControlFlow;

use php_ext::raising::{Level, Thrown, docref};
use php_ext::request::Request;
use php_ext::values::walk::{self, GiveUp, Hold};
use php_ext::values::{Key, Owner, PhpArray, PhpString, ValueRef};
use php_ext_standard::{Serializer, Unserializer};

use crate::{Format, State, absorb, bind_vars, session_array, session_array_mut, shown};

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
///
/// Each variable is read from the session's array as it stands when the
/// walk reaches its key, so one a `__sleep` unset is not written:
///
/// ```php
/// ob_start(); session_start(["save_path" => __DIR__, "use_cookies" => 0, "cache_limiter" => ""]);
/// class S { function __sleep() { unset($_SESSION['c']); return []; } }
/// $_SESSION['a'] = 1; $_SESSION['b'] = new S; $_SESSION['c'] = 3;
/// var_dump(session_encode()); session_destroy(); ob_end_flush();
/// ```
/// ```expect
/// string(20) "a|i:1;b|O:1:"S":0:{}"
/// ```
pub(super) fn encode(req: &mut Request, format: Format) -> Result<Option<Vec<u8>>, Thrown> {
    let Some(array) = session_array(req) else {
        return Ok(None);
    };
    let mut hash = Serializer::new(req);
    // `php_session_serialize_init`.
    hash.set_incomplete_classes_only(req.state::<State>().in_rfc1867_update);
    let encoded = encode_through(req, format, array, &mut hash);
    hash.finish(req);
    encoded
}

/// [`encode`]'s body through one var hash, `PHP_VAR_SERIALIZE_INIT` to
/// `PHP_VAR_SERIALIZE_DESTROY` being the caller's.
fn encode_through(
    req: &mut Request,
    format: Format,
    array: PhpArray,
    hash: &mut Serializer,
) -> Result<Option<Vec<u8>>, Thrown> {
    let mut buf = Vec::new();
    if format == Format::PhpSerialize {
        hash.write(req, &mut buf, &array.to_value())?;
        return Ok(Some(buf));
    }
    let mut wrote = false;
    // `PS_ENCODE_LOOP` (`Z/ext/session/php_session.h:297`): `ZVAL_COPY` of
    // the session's array "to protect against user interference" and
    // `ZEND_HASH_FOREACH_KEY` over the copy, a numeric key skipped with
    // php's warning, and each variable read again from the session's
    // array as it now stands (`php_get_session_var`), so one a `__sleep`
    // unset is left out. The copy is given up as `zval_ptr_dtor(&_zv)`.
    let held = Hold::Owner(GiveUp::Dtor);
    let refused = walk::foreach(req, array, held, |req, row| -> walk::Step {
        let name = match row.key() {
            Key::Str(name) => name.as_bytes().to_vec(),
            Key::Int(n) => {
                docref(req, Level::Warning, &format!("Skipping numeric key {n}"));
                return Ok(ControlFlow::Continue(()));
            }
        };
        let Some(value) = session_array(req).and_then(|now| now.get(row.key())) else {
            return Ok(ControlFlow::Continue(()));
        };
        if format == Format::PhpBinary {
            let Ok(len) = u8::try_from(name.len()) else {
                return Ok(ControlFlow::Continue(()));
            };
            if len > 127 {
                return Ok(ControlFlow::Continue(()));
            }
            buf.push(len);
            buf.extend_from_slice(&name);
        } else {
            buf.extend_from_slice(&name);
            if name.contains(&b'|') {
                docref(
                    req,
                    Level::Warning,
                    &format!(
                        "Failed to write session data. Data contains invalid key \"{}\"",
                        shown(&name)
                    ),
                );
                return Ok(ControlFlow::Break(()));
            }
            buf.push(b'|');
        }
        wrote = true;
        hash.write(req, &mut buf, &value)?;
        Ok(ControlFlow::Continue(()))
    })?;
    if refused.is_some() {
        return Ok(None);
    }
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
///
/// `php_serialize` binds whatever the data holds as `$_SESSION`, and the
/// empty array for `N;` or data it cannot read:
///
/// ```php
/// ini_set("session.serialize_handler", "php_serialize"); ob_start(); session_start(["save_path" => __DIR__, "use_cookies" => 0, "cache_limiter" => ""]);
/// var_dump(session_decode("O:8:\"stdClass\":0:{}"), $_SESSION, session_decode("i:1;"), $_SESSION, session_decode("N;"), $_SESSION); ob_end_flush();
/// ```
/// ```expect
/// bool(true)
/// object(stdClass)#1 (0) {
/// }
/// bool(true)
/// int(1)
/// bool(true)
/// array(0) {
/// }
/// ```
///
/// A delayed `__wakeup()` that throws after a good `php_serialize` read
/// leaves the value bound and its throwable pending, with no warning and
/// the session standing:
///
/// ```php
/// class A { function __wakeup() { echo "wakeup A\n"; throw new Exception("wake"); } }
/// ini_set("session.serialize_handler", "php_serialize");
/// ob_start(); session_start(["save_path" => sys_get_temp_dir(), "use_cookies" => 0, "cache_limiter" => ""]);
/// try { var_dump(session_decode('a:2:{s:1:"x";i:1;s:1:"a";O:1:"A":0:{}}')); } catch (Exception $e) { echo $e->getMessage(), "\n"; }
/// var_dump($_SESSION);
/// session_destroy(); ob_end_flush();
/// ```
/// ```expect
/// wakeup A
/// wake
/// array(2) {
///   ["x"]=>
///   int(1)
///   ["a"]=>
///   object(A)#1 (0) {
///   }
/// }
/// ```
pub(super) fn decode(req: &mut Request, format: Format, data: &[u8]) -> Result<bool, Thrown> {
    let data = data.to_vec();
    let incomplete_only = req.state::<State>().in_rfc1867_update;
    if format == Format::PhpSerialize {
        let mut reader = Unserializer::new(req, &data)?;
        reader.set_incomplete_classes_only(incomplete_only);
        // The value is the var hash's until it is destroyed; the session
        // keeps an owner of its own, beside the read's outcome.
        let (held, read) = match reader.value(req) {
            Ok(read) => (read.map(|value| Owner::take(req, value)), Ok(())),
            Err(thrown) => (None, Err(thrown)),
        };
        // `PHP_VAR_UNSERIALIZE_DESTROY`, then the value bound whatever a
        // delayed call threw: php's decode answers by the read alone, so
        // a `__wakeup` that throws after a good read leaves its throwable
        // pending over the bound session, and only a read that failed,
        // on a throw or not, fails the decode.
        if let Err(thrown) = reader.finish(req, read)
            && let Err(fatal) = absorb(req, Err::<(), _>(thrown))
        {
            if let Some(held) = held {
                held.give_up(req);
            }
            return Err(fatal);
        }
        let ok = held.is_some();
        // A value read is bound whatever it is, and nothing read is the
        // empty array.
        let vars = match held {
            Some(value) if !matches!(value.as_ref(), ValueRef::Null) => value,
            held => {
                if let Some(null) = held {
                    null.give_up(req);
                }
                PhpArray::new(req)?.into_value()
            }
        };
        bind_vars(req, vars)?;
        return Ok(ok || data.is_empty());
    }
    let Some(array) = session_array_mut(req)? else {
        return Ok(true);
    };
    let mut reader = Unserializer::new(req, &data)?;
    // `php_session_unserialize_init`.
    reader.set_incomplete_classes_only(incomplete_only);
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
        match reader.element(req, array, name.into_key()) {
            Ok(true) => p = reader.offset(),
            Ok(false) => {
                ok = false;
                break;
            }
            Err(thrown) => return reader.finish(req, Err(thrown)),
        }
    }
    reader.finish(req, Ok(ok))
}
