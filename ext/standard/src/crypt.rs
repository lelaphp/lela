//! `Z/ext/standard/crypt.c`: `crypt()` and the `php_crypt` it calls (`Z/ext/standard/crypt.c:70`), with
//! php's own hash implementations, the ones a `PHP_USE_PHP_CRYPT_R` build
//! uses: `FreeSec` DES, MD5 `$1$`, bcrypt `$2?$`, SHA-256 `$5$` and SHA-512
//! `$6$`. `password_hash()` and `password_verify()` for bcrypt
//! (`Z/ext/standard/password.c:209`, `Z/ext/standard/password.c:156`) call
//! [`php_crypt`] directly with `quiet` set and need nothing else from here.
//!
//! php hands every algorithm C strings: a password or a salt ends at its
//! first NUL byte, and no algorithm reads past that NUL, so both are cut
//! there once, in [`php_crypt`].
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use php_ext::decl::{Attribute, ConstDecl, FunctionDecl, Name};
use php_ext::raising::Thrown;
use php_ext::request::Request;
use php_ext::values::{PhpString, Value};

use crate::output::Entry;
use crate::stub::stub;
use crate::{argument_count_between, constant, string_argument};

mod blowfish;
mod freesec;
mod md5_crypt;
mod sha_crypt;

/// `PHP_MAX_SALT_LEN`, `Z/ext/standard/php_crypt.h:28`; also the constant
/// `CRYPT_SALT_LENGTH`.
const PHP_MAX_SALT_LEN: usize = 123;

/// `IS_VALID_SALT_CHARACTER`, `Z/ext/standard/crypt.c:48`: a byte of the
/// traditional DES salt alphabet `./0-9A-Za-z`.
fn is_valid_salt_character(c: u8) -> bool {
    (b'.'..=b'9').contains(&c) || c.is_ascii_uppercase() || c.is_ascii_lowercase()
}

/// The bytes of `s` before its first NUL: what C's `strlen` sees.
fn c_str(s: &[u8]) -> &[u8] {
    s.iter()
        .position(|&c| c == 0)
        .and_then(|n| s.get(..n))
        .unwrap_or(s)
}

/// `php_crypt`, `Z/ext/standard/crypt.c:70`, as a `PHP_USE_PHP_CRYPT_R` build
/// compiles it. The hash is chosen by the salt's prefix: `$1$` MD5, `$6$`
/// SHA-512, `$5$` SHA-256, `$2` + any byte + `$` bcrypt, and `_` or two
/// DES salt characters DES. `None` is php's NULL, the only failure answer
/// `php_crypt` gives: a salt starting `*0` or `*1`, an unknown prefix, or a
/// salt the chosen algorithm refuses. The `*0` and `*1` strings are the
/// native's to produce (see [`crypt`]). `quiet` is unused, as in php.
pub(super) fn php_crypt(password: &[u8], salt: &[u8], quiet: bool) -> Option<Vec<u8>> {
    // php 8.5 no longer reads `quiet` on any path.
    let _ = quiet;
    let password = c_str(password);
    let salt = c_str(salt);
    let at = |i: usize| salt.get(i).copied().unwrap_or(0);

    if at(0) == b'*' && (at(1) == b'0' || at(1) == b'1') {
        return None;
    }

    if at(0) == b'$' && at(1) == b'1' && at(2) == b'$' {
        Some(md5_crypt::php_md5_crypt_r(password, salt))
    } else if at(0) == b'$' && at(1) == b'6' && at(2) == b'$' {
        sha_crypt::php_sha512_crypt_r(password, salt)
    } else if at(0) == b'$' && at(1) == b'5' && at(2) == b'$' {
        sha_crypt::php_sha256_crypt_r(password, salt)
    } else if at(0) == b'$' && at(1) == b'2' && at(2) != 0 && at(3) == b'$' {
        blowfish::php_crypt_blowfish_rn(password, salt)
    } else if at(0) == b'_' || (is_valid_salt_character(at(0)) && is_valid_salt_character(at(1))) {
        // DES fallback. php also answers NULL for a salt starting `*0`
        // here, which cannot reach this branch.
        freesec::crypt_extended_r(password, salt)
    } else {
        // Unknown hash type.
        None
    }
}

/// The body of `PHP_FUNCTION(crypt)`, `Z/ext/standard/crypt.c:201`, once its
/// two strings are parsed: the salt is cut to [`PHP_MAX_SALT_LEN`] bytes
/// (`Z/ext/standard/crypt.c:212-221`), and a NULL from [`php_crypt`] becomes
/// `*1` when the salt starts `*0` and `*0` otherwise
/// (`Z/ext/standard/crypt.c:223-229`), so the answer always differs from the
/// salt. It raises nothing.
fn crypt_string(string: &[u8], salt: &[u8]) -> Vec<u8> {
    let salt = salt.get(..PHP_MAX_SALT_LEN).unwrap_or(salt);
    match php_crypt(string, salt, false) {
        Some(hash) => hash,
        None if salt.starts_with(b"*0") => b"*1".to_vec(),
        None => b"*0".to_vec(),
    }
}

/// The constants `basic_functions.stub.php` declares for `crypt.c`:
/// the salt's greatest length and one for each algorithm, every one
/// built in.
///
/// ```php
/// var_dump(CRYPT_SALT_LENGTH, CRYPT_STD_DES, CRYPT_EXT_DES, CRYPT_MD5, CRYPT_BLOWFISH, CRYPT_SHA256, CRYPT_SHA512);
/// ```
/// ```expect
/// int(123)
/// int(1)
/// int(1)
/// int(1)
/// int(1)
/// int(1)
/// int(1)
/// ```
pub(crate) fn constants() -> Vec<ConstDecl> {
    vec![
        constant(
            b"CRYPT_SALT_LENGTH",
            i64::try_from(PHP_MAX_SALT_LEN).unwrap_or(0),
        ),
        constant(b"CRYPT_STD_DES", 1),
        constant(b"CRYPT_EXT_DES", 1),
        constant(b"CRYPT_MD5", 1),
        constant(b"CRYPT_BLOWFISH", 1),
        constant(b"CRYPT_SHA256", 1),
        constant(b"CRYPT_SHA512", 1),
    ]
}

/// The one native, `$string` under `#[\SensitiveParameter]` as the
/// stub marks it.
///
/// ```php
/// echo new ReflectionFunction('crypt');
/// ```
/// ```expect
/// Function [ <internal:standard> function crypt ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <required> string $string ]
///     Parameter #1 [ <required> string $salt ]
///   }
///   - Return [ string ]
/// }
/// ```
pub(crate) fn natives() -> Vec<(FunctionDecl, Entry)> {
    let mut decl = stub("crypt(string $string, string $salt): string");
    if let Some(string) = decl.parameters.first_mut() {
        string.attributes.push(Attribute {
            name: Name(b"SensitiveParameter".as_slice().into()),
            arguments: Vec::new(),
        });
    }
    vec![(decl, crypt as Entry)]
}

/// `crypt(string $string, string $salt): string`, `PHP_FUNCTION(crypt)`
/// (`Z/ext/standard/crypt.c:201`): the salt's prefix picks the
/// algorithm, each php's own, byte for byte: two salt characters are
/// DES, `_` and eight more extended DES, `$1$` MD5, `$2a$`, `$2b$`,
/// `$2x$` and `$2y$` bcrypt, `$5$` SHA-256 and `$6$` SHA-512 with their
/// `rounds=`; a salt none of them takes is `*0`, or `*1` where the salt
/// is `*0` itself.
///
/// ```php
/// foreach (['rl', '_J9..rasm', '$1$rasmusle$', '$2a$07$rasmuslerd............', '$2y$04$usesomesillystringforsalt$', '$5$rounds=5000$usesomesillystri$', '$6$rounds=1000$saltsaltsaltsalt$', '$6$saltstring', '*0', '*1', '$3$x', '$2y$03$toolowcostsaltsaltsalts', ''] as $salt) echo crypt('rasmuslerdorf', $salt), "\n";
/// echo crypt("\xff\xa3" . str_repeat('x', 80), '$2x$05$abcdefghijklmnopqrstuu'), "\n", crypt("\xff\xa3", '$2a$05$abcdefghijklmnopqrstuu'), "\n";
/// try { crypt('x'); } catch (ArgumentCountError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// rl.3StKT.4T8M
/// _J9..rasmBYk8r9AiWNc
/// $1$rasmusle$rISCgZzpwk3UhDidwXvin0
/// $2a$07$rasmuslerd............nIdrcHdxcUxWomQX9j6kvERCFjTg7Ra
/// $2y$04$usesomesillystringforeSZ3saLXGnQtLrpxgqydzBYMhY/4iDpG
/// $5$rounds=5000$usesomesillystri$KqJWpanXZHKq2BOB43TSaYhEWsQ1Lr5QNyPCDH/Tp.6
/// $6$rounds=1000$saltsaltsaltsalt$MuRWjkVQhEQVAJz5vyxtYYUrbLCVDHPqHE.8HVVAB5Axdleliv4RvBbuw6C0tQgaAvjmfVLSx3gHWh9w8IaU2.
/// $6$saltstring$jRRe28CuYa4mk/iQwLNsE.29duEG6zP5eGsoozlYu/Qi9y6utgXWQQxeFF48eVCDq9b1ysyTDbTk7pLr4P6Kd.
/// *1
/// *0
/// *0
/// *0
/// *0
/// $2x$05$abcdefghijklmnopqrstuuS7Qccef93gmaeS4PV./q1sB6LXy/vHO
/// $2a$05$abcdefghijklmnopqrstuuS4MQfWavlhc6PZtLZxy3d/FQNIPTs2e
/// crypt() expects exactly 2 arguments, 1 given
/// ```
fn crypt(req: &mut Request) -> Result<Value, Thrown> {
    argument_count_between(req, 2, 2)?;
    let string = string_argument(req, 1)?;
    let salt = string_argument(req, 2)?;
    let hash = crypt_string(string.as_bytes(), salt.as_bytes());
    Ok(Value::string(PhpString::new(req, &hash)?))
}
