//! `Z/ext/standard/password.c`: `password_hash`, `password_verify`,
//! `password_needs_rehash`, `password_get_info` and `password_algos`
//! over php's three algorithms in the order `PHP_MINIT_FUNCTION(password)`
//! registers them: `2y`, bcrypt through `crypt`'s `php_crypt`, and
//! `argon2i` and `argon2id` through libargon2, which the oracle is built
//! with and `argon2` ports. A salt is `php_password_make_salt`'s: random
//! bytes from the host's `arc4random_buf(3)`, base64 with `+` as `.`.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use php_ext::abi::Union;
use php_ext::attr::{php_const, php_function};
use php_ext::coerce::to_long;
use php_ext::raising::{Level, Thrown, arg_value_error, docref, throw_error, value_error};
use php_ext::request::Request;
use php_ext::values::{Owner, PhpArray, PhpString, Value};

use constants::PASSWORD_BCRYPT_DEFAULT_COST as BCRYPT_COST;

mod argon2;

/// One of the registered algorithms, `php_password_algo`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Algo {
    Bcrypt,
    Argon2(argon2::Variant),
}

impl Algo {
    /// `algo->name`.
    fn name(self) -> &'static str {
        match self {
            Algo::Bcrypt => "bcrypt",
            Algo::Argon2(variant) => variant.name(),
        }
    }

    /// `php_password_algo_find`: the algorithm registered under `ident`.
    fn find(ident: &[u8]) -> Option<Algo> {
        match ident {
            b"2y" => Some(Algo::Bcrypt),
            b"argon2i" => Some(Algo::Argon2(argon2::Variant::I)),
            b"argon2id" => Some(Algo::Argon2(argon2::Variant::Id)),
            _ => None,
        }
    }

    /// `algo->valid`, which only bcrypt has: sixty bytes beginning `$2y`.
    fn valid(self, hash: &[u8]) -> bool {
        match self {
            Algo::Bcrypt => hash.len() == 60 && hash.starts_with(b"$2y"),
            Algo::Argon2(_) => true,
        }
    }
}

/// The registered names, `php_password_algos`, in their order.
const IDENTS: [&str; 3] = ["2y", "argon2i", "argon2id"];

/// `php_password_algo_extract_ident` (`Z/ext/standard/password.c:510`):
/// what stands between the first byte and the next `$`, read as the C
/// string the hash is; `None` for a hash under three bytes or with no
/// second `$`.
fn extract_ident(hash: &[u8]) -> Option<&[u8]> {
    if hash.len() < 3 {
        return None;
    }
    let rest = hash.get(1..)?;
    let rest = php_ext::coerce::c_str(rest);
    let end = rest.iter().position(|c| *c == b'$')?;
    rest.get(..end)
}

/// `php_password_algo_identify_ex` (`Z/ext/standard/password.c:527`):
/// the algorithm the hash names, where it is registered and takes the
/// hash, else `default`.
fn identify(hash: &[u8], default: Option<Algo>) -> Option<Algo> {
    let Some(ident) = extract_ident(hash) else {
        return default;
    };
    match Algo::find(ident) {
        Some(algo) if algo.valid(hash) => Some(algo),
        _ => default,
    }
}

/// The constants of `password.stub.php`, in its order.
///
/// ```php
/// var_dump(PASSWORD_DEFAULT, PASSWORD_BCRYPT, PASSWORD_BCRYPT_DEFAULT_COST, PASSWORD_ARGON2I, PASSWORD_ARGON2ID, PASSWORD_ARGON2_PROVIDER, PASSWORD_ARGON2_DEFAULT_MEMORY_COST, PASSWORD_ARGON2_DEFAULT_TIME_COST, PASSWORD_ARGON2_DEFAULT_THREADS);
/// ```
/// ```expect
/// string(2) "2y"
/// string(2) "2y"
/// int(12)
/// string(7) "argon2i"
/// string(8) "argon2id"
/// string(8) "standard"
/// int(65536)
/// int(4)
/// int(1)
/// ```
#[php_const]
pub(crate) mod constants {
    use super::argon2;

    pub(crate) const PASSWORD_DEFAULT: &str = "2y";
    pub(crate) const PASSWORD_BCRYPT: &str = "2y";
    /// `PHP_PASSWORD_BCRYPT_COST` (`Z/ext/standard/php_password.h:24`).
    pub(crate) const PASSWORD_BCRYPT_DEFAULT_COST: i64 = 12;
    pub(crate) const PASSWORD_ARGON2I: &str = "argon2i";
    pub(crate) const PASSWORD_ARGON2ID: &str = "argon2id";
    pub(crate) const PASSWORD_ARGON2_PROVIDER: &str = "standard";
    pub(crate) const PASSWORD_ARGON2_DEFAULT_MEMORY_COST: i64 = argon2::DEFAULT_MEMORY_COST;
    pub(crate) const PASSWORD_ARGON2_DEFAULT_TIME_COST: i64 = argon2::DEFAULT_TIME_COST;
    pub(crate) const PASSWORD_ARGON2_DEFAULT_THREADS: i64 = argon2::DEFAULT_THREADS;
}

/// `php_password_algo_find_zval` (`Z/ext/standard/password.c:475`) over
/// what `Z_PARAM_STR_OR_LONG_OR_NULL` held: `null` and `0` the default,
/// bcrypt; `1` bcrypt, `2` and `3` the two argon2; a string the
/// algorithm registered under it; `None` for anything else.
fn algo_of(algo: Option<Union<PhpString, i64>>) -> Option<Algo> {
    match algo {
        None | Some(Union::B(0 | 1)) => Some(Algo::Bcrypt),
        Some(Union::A(ident)) => Algo::find(ident.as_bytes()),
        Some(Union::B(2)) => Some(Algo::Argon2(argon2::Variant::I)),
        Some(Union::B(3)) => Some(Algo::Argon2(argon2::Variant::Id)),
        Some(Union::B(_)) => None,
    }
}

/// `zend_hash_str_find(options, name)` then `zval_get_long`.
fn option_long(
    req: &mut Request,
    options: Option<PhpArray>,
    name: &[u8],
) -> Result<Option<i64>, Thrown> {
    let Some(options) = options else {
        return Ok(None);
    };
    match options.find(name) {
        Some(value) => Ok(Some(to_long(req, &value)?)),
        None => Ok(None),
    }
}

/// `php_password_get_salt` then `php_password_make_salt(length)`
/// (`Z/ext/standard/password.c:77`, `:106`): php's warning for a `salt`
/// option, then `length * 3 / 4 + 1` random bytes in base64, `+` as `.`,
/// cut to `length`.
fn make_salt(
    req: &mut Request,
    options: Option<PhpArray>,
    length: usize,
) -> Result<Vec<u8>, Thrown> {
    if let Some(options) = options
        && options.find(b"salt").is_some()
    {
        docref(
            req,
            Level::Warning,
            "The \"salt\" option has been ignored, since providing a custom salt is no longer supported",
        );
    }
    let mut buffer = vec![0u8; length * 3 / 4 + 1];
    if !crate::ffi::random_bytes(&mut buffer) {
        return Err(value_error(req, "Unable to generate salt"));
    }
    let encoded = crate::base64::encode(&buffer);
    let salt = encoded.get(..length).unwrap_or_default();
    if salt.len() < length || salt.contains(&b'=') {
        return Err(value_error(req, "Generated salt too short"));
    }
    Ok(salt
        .iter()
        .map(|c| if *c == b'+' { b'.' } else { *c })
        .collect())
}

/// `password_hash(string $password, string|int|null $algo, array $options = []): string`,
/// `PHP_FUNCTION(password_hash)` (`Z/ext/standard/password.c:617`): the
/// algorithm, or php's `ValueError` for one that is none; for bcrypt
/// (`php_password_bcrypt_hash`) a password with a NUL refused, the
/// `cost` option from 4 to 31, and `$2y$` with the cost and 22 salt
/// characters handed to `php_crypt`; for argon2
/// (`php_password_argon2_hash`) `memory_cost`, `time_cost` and `threads`
/// checked in that order, 16 salt characters, a 32-byte tag and
/// libargon2's encoded form, or its error as a `ValueError`.
///
/// ```php
/// $h = password_hash('secret', PASSWORD_BCRYPT, ['cost' => 4]);
/// var_dump(strlen($h), substr($h, 0, 7), password_verify('secret', $h), password_verify('Secret', $h));
/// $a = password_hash('secret', PASSWORD_ARGON2ID, ['memory_cost' => 1024, 'time_cost' => 1]);
/// var_dump(substr($a, 0, 30), strlen($a), password_verify('secret', $a), password_verify('x', $a));
/// $i = password_hash('pw', 2, ['memory_cost' => 64, 'time_cost' => 2, 'threads' => 2, 'salt' => 'x']);
/// var_dump(substr($i, 0, 27), password_verify('pw', $i));
/// foreach ([['x', 'nope', []], ['x', 1, ['cost' => 3]], ["a\0b", null, []], ['x', 3, ['memory_cost' => 7]], ['x', 3, ['time_cost' => 0]], ['x', 3, ['threads' => 0]], ['x', 3, ['memory_cost' => 16, 'threads' => 4]]] as $call) {
///     try { password_hash($call[0], $call[1], $call[2]); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// }
/// ```
/// ```expect
/// int(60)
/// string(7) "$2y$04$"
/// bool(true)
/// bool(false)
/// string(30) "$argon2id$v=19$m=1024,t=1,p=1$"
/// int(96)
/// bool(true)
/// bool(false)
///
/// Warning: password_hash(): The "salt" option has been ignored, since providing a custom salt is no longer supported in %s on line 6
/// string(27) "$argon2i$v=19$m=64,t=2,p=2$"
/// bool(true)
/// password_hash(): Argument #2 ($algo) must be a valid password hashing algorithm
/// Invalid bcrypt cost parameter specified: 3
/// Bcrypt password must not contain null character
/// Memory cost is outside of allowed memory range
/// Time cost is outside of allowed time range
/// Invalid number of threads
/// Memory cost is too small
/// ```
#[php_function]
fn password_hash(
    req: &mut Request,
    #[php(sensitive)] password: PhpString,
    algo: Option<Union<PhpString, i64>>,
    #[php(default = [])] options: PhpArray,
) -> Result<Owner<PhpString>, Thrown> {
    let options = Some(options);
    let Some(algo) = algo_of(algo) else {
        return Err(arg_value_error(
            req,
            2,
            "be a valid password hashing algorithm",
        ));
    };
    let password = password.as_bytes();
    let digest = match algo {
        Algo::Bcrypt => {
            if password.contains(&0) {
                return Err(value_error(
                    req,
                    "Bcrypt password must not contain null character",
                ));
            }
            let cost = option_long(req, options, b"cost")?.unwrap_or(BCRYPT_COST);
            if !(4..=31).contains(&cost) {
                let text = format!("Invalid bcrypt cost parameter specified: {cost}");
                return Err(value_error(req, &text));
            }
            let salt = make_salt(req, options, 22)?;
            let mut setting = format!("$2y${cost:02}$").into_bytes();
            setting.extend_from_slice(&salt);
            crate::crypt::php_crypt(password, &setting, true).filter(|hash| hash.len() >= 13)
        }
        Algo::Argon2(variant) => {
            let memory =
                option_long(req, options, b"memory_cost")?.unwrap_or(argon2::DEFAULT_MEMORY_COST);
            let Some(memory) = argon2::memory_cost_in_range(memory) else {
                return Err(value_error(req, argon2::MEMORY_COST_RANGE));
            };
            let time =
                option_long(req, options, b"time_cost")?.unwrap_or(argon2::DEFAULT_TIME_COST);
            let Some(time) = argon2::time_cost_in_range(time) else {
                return Err(value_error(req, argon2::TIME_COST_RANGE));
            };
            let threads = option_long(req, options, b"threads")?.unwrap_or(argon2::DEFAULT_THREADS);
            let Some(threads) = argon2::threads_in_range(threads) else {
                return Err(value_error(req, argon2::THREADS_RANGE));
            };
            let salt = make_salt(req, options, argon2::SALT_LEN)?;
            match argon2::password_hash(variant, password, &salt, memory, time, threads) {
                Ok(encoded) => Some(encoded.into_bytes()),
                Err(error) => return Err(value_error(req, error.message())),
            }
        }
    };
    let Some(digest) = digest else {
        return Err(throw_error(
            req,
            "Error",
            "Password hashing failed for unknown reason",
        ));
    };
    PhpString::new(req, &digest)
}

/// `password_verify(string $password, string $hash): bool`,
/// `PHP_FUNCTION(password_verify)` (`Z/ext/standard/password.c:599`):
/// the hash's algorithm, bcrypt where it names none registered or one
/// that refuses it, so any `crypt` hash verifies through `php_crypt`;
/// bcrypt compares the whole hash `php_crypt` gives back, and argon2 is
/// `argon2_verify`.
///
/// ```php
/// var_dump(password_verify('rasmuslerdorf', '$2y$07$BCryptRequires22Chrcte/VlQH0piJtjXl.0t1XkA8pw9dMXTpOq'), password_verify('rasmuslerdorf', '$1$rasmusle$rISCgZzpwk3UhDidwXvin0'), password_verify('rasmuslerdorf', 'rl.3StKT.4T8M'), password_verify('password', '$argon2i$v=19$m=1024,t=2,p=1$ei9wbXNWd2o0eDI2U2FZdA$xdXldTiK2wptHI7Oj5YIRKYIcX5TPV1EbQsU3e6Jz3s'), password_verify('password', '$argon2id$v=19$m=256,t=1,p=2$MmpIYlJUelBoUWxzTlRGcw$Qy4xk30Bir0uOn6VXrWzi6+90g0Z6hmn4HaD8uU1HjU'), password_verify('x', ''), password_verify('x', '$2y$04$short'));
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(true)
/// bool(false)
/// bool(false)
/// ```
#[php_function]
fn password_verify(#[php(sensitive)] password: PhpString, hash: PhpString) -> bool {
    let (password, hash) = (password.as_bytes(), hash.as_bytes());
    match identify(hash, Some(Algo::Bcrypt)) {
        Some(Algo::Argon2(variant)) => argon2::verify(hash, password, variant),
        _ => crate::crypt::php_crypt(password, hash, true)
            .is_some_and(|out| hash.len() >= 13 && out == hash),
    }
}

/// `password_get_info(string $hash): array`,
/// `PHP_FUNCTION(password_get_info)` (`Z/ext/standard/password.c:540`):
/// `algo`, the ident, `algoName` and the costs read back out of the
/// hash, bcrypt's `cost` and argon2's three; `null`, `unknown` and no
/// options for a hash no registered algorithm takes.
///
/// ```php
/// var_dump(password_get_info('$2y$07$BCryptRequires22Chrcte/VlQH0piJtjXl.0t1XkA8pw9dMXTpOq'), password_get_info('$argon2id$v=19$m=1024,t=2,p=3$c2FsdHNhbHQ$aGFzaA'), password_get_info('$argon2i$v=19$m=77$x'), password_get_info('$1$rasmusle$rISCgZzpwk3UhDidwXvin0'));
/// ```
/// ```expect
/// array(3) {
///   ["algo"]=>
///   string(2) "2y"
///   ["algoName"]=>
///   string(6) "bcrypt"
///   ["options"]=>
///   array(1) {
///     ["cost"]=>
///     int(7)
///   }
/// }
/// array(3) {
///   ["algo"]=>
///   string(8) "argon2id"
///   ["algoName"]=>
///   string(8) "argon2id"
///   ["options"]=>
///   array(3) {
///     ["memory_cost"]=>
///     int(1024)
///     ["time_cost"]=>
///     int(2)
///     ["threads"]=>
///     int(3)
///   }
/// }
/// array(3) {
///   ["algo"]=>
///   string(7) "argon2i"
///   ["algoName"]=>
///   string(7) "argon2i"
///   ["options"]=>
///   array(3) {
///     ["memory_cost"]=>
///     int(77)
///     ["time_cost"]=>
///     int(4)
///     ["threads"]=>
///     int(1)
///   }
/// }
/// array(3) {
///   ["algo"]=>
///   NULL
///   ["algoName"]=>
///   string(7) "unknown"
///   ["options"]=>
///   array(0) {
///   }
/// }
/// ```
///
/// The declarations as Reflection renders them.
///
/// ```php
/// foreach (['password_get_info', 'password_hash', 'password_needs_rehash', 'password_verify', 'password_algos'] as $f) echo new ReflectionFunction($f);
/// ```
/// ```expect
/// Function [ <internal:standard> function password_get_info ] {
///
///   - Parameters [1] {
///     Parameter #0 [ <required> string $hash ]
///   }
///   - Return [ array ]
/// }
/// Function [ <internal:standard> function password_hash ] {
///
///   - Parameters [3] {
///     Parameter #0 [ <required> string $password ]
///     Parameter #1 [ <required> string|int|null $algo ]
///     Parameter #2 [ <optional> array $options = [] ]
///   }
///   - Return [ string ]
/// }
/// Function [ <internal:standard> function password_needs_rehash ] {
///
///   - Parameters [3] {
///     Parameter #0 [ <required> string $hash ]
///     Parameter #1 [ <required> string|int|null $algo ]
///     Parameter #2 [ <optional> array $options = [] ]
///   }
///   - Return [ bool ]
/// }
/// Function [ <internal:standard> function password_verify ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <required> string $password ]
///     Parameter #1 [ <required> string $hash ]
///   }
///   - Return [ bool ]
/// }
/// Function [ <internal:standard> function password_algos ] {
///
///   - Parameters [0] {
///   }
///   - Return [ array ]
/// }
/// ```
#[php_function]
fn password_get_info(req: &mut Request, hash: PhpString) -> Result<Owner<PhpArray>, Thrown> {
    let hash = hash.as_bytes();
    let mut array = PhpArray::new(req)?;
    let mut options = PhpArray::new(req)?;
    let ident = extract_ident(hash);
    let algo = ident.and_then(Algo::find).filter(|algo| algo.valid(hash));
    let set = |req: &mut Request, array: &mut Owner<PhpArray>, name: &str, value: Owner<Value>| {
        array.set_str(req, name.as_bytes(), value)
    };
    if let (Some(algo), Some(ident)) = (algo, ident) {
        {
            let ident = PhpString::new(req, ident)?.into_value();
            set(req, &mut array, "algo", ident)?;
            let name = PhpString::new(req, algo.name().as_bytes())?.into_value();
            set(req, &mut array, "algoName", name)?;
            match algo {
                Algo::Bcrypt => {
                    let cost = argon2::sscanf_long(hash.get(4..).unwrap_or_default())
                        .unwrap_or(BCRYPT_COST);
                    set(req, &mut options, "cost", Owner::int(cost))?;
                }
                Algo::Argon2(_) => {
                    let costs = argon2::get_info(hash);
                    set(
                        req,
                        &mut options,
                        "memory_cost",
                        Owner::int(costs.memory_cost),
                    )?;
                    set(req, &mut options, "time_cost", Owner::int(costs.time_cost))?;
                    set(req, &mut options, "threads", Owner::int(costs.threads))?;
                }
            }
        }
    } else {
        set(req, &mut array, "algo", Owner::null())?;
        let unknown = PhpString::new(req, b"unknown")?.into_value();
        set(req, &mut array, "algoName", unknown)?;
    }
    set(req, &mut array, "options", options.into_value())?;
    Ok(array)
}

/// `password_needs_rehash(string $hash, string|int|null $algo, array $options = []): bool`,
/// `PHP_FUNCTION(password_needs_rehash)` (`Z/ext/standard/password.c:571`):
/// `false` for an algorithm that is none; `true` where the hash is
/// another algorithm's or none's; else bcrypt's cost, or argon2's three
/// costs, against the options, php's defaults where one is absent.
///
/// ```php
/// $b = '$2y$07$BCryptRequires22Chrcte/VlQH0piJtjXl.0t1XkA8pw9dMXTpOq';
/// $a = '$argon2id$v=19$m=65536,t=4,p=1$c2FsdHNhbHQ$aGFzaA';
/// var_dump(password_needs_rehash($b, PASSWORD_BCRYPT), password_needs_rehash($b, PASSWORD_BCRYPT, ['cost' => 7]), password_needs_rehash($b, 'nope'), password_needs_rehash($b, PASSWORD_ARGON2ID), password_needs_rehash($a, PASSWORD_ARGON2ID), password_needs_rehash($a, PASSWORD_ARGON2ID, ['threads' => 2]), password_needs_rehash('x', null));
/// ```
/// ```expect
/// bool(true)
/// bool(false)
/// bool(false)
/// bool(true)
/// bool(false)
/// bool(true)
/// bool(true)
/// ```
#[php_function]
fn password_needs_rehash(
    req: &mut Request,
    hash: PhpString,
    algo: Option<Union<PhpString, i64>>,
    #[php(default = [])] options: PhpArray,
) -> Result<bool, Thrown> {
    let options = Some(options);
    let Some(new_algo) = algo_of(algo) else {
        return Ok(false);
    };
    let hash = hash.as_bytes();
    let old_algo = identify(hash, None);
    if old_algo != Some(new_algo) {
        return Ok(true);
    }
    let rehash = match new_algo {
        Algo::Bcrypt => {
            let old = argon2::sscanf_long(hash.get(4..).unwrap_or_default()).unwrap_or(BCRYPT_COST);
            let new = option_long(req, options, b"cost")?.unwrap_or(BCRYPT_COST);
            old != new
        }
        Algo::Argon2(_) => {
            let memory =
                option_long(req, options, b"memory_cost")?.unwrap_or(argon2::DEFAULT_MEMORY_COST);
            let time =
                option_long(req, options, b"time_cost")?.unwrap_or(argon2::DEFAULT_TIME_COST);
            let threads = option_long(req, options, b"threads")?.unwrap_or(argon2::DEFAULT_THREADS);
            argon2::needs_rehash(hash, memory, time, threads)
        }
    };
    Ok(rehash)
}

/// `password_algos(): array`, `PHP_FUNCTION(password_algos)`
/// (`Z/ext/standard/password.c:651`): the registered idents.
///
/// ```php
/// var_dump(password_algos());
/// ```
/// ```expect
/// array(3) {
///   [0]=>
///   string(2) "2y"
///   [1]=>
///   string(7) "argon2i"
///   [2]=>
///   string(8) "argon2id"
/// }
/// ```
#[php_function]
fn password_algos(req: &mut Request) -> Result<Owner<PhpArray>, Thrown> {
    let mut array = PhpArray::new(req)?;
    for ident in IDENTS {
        let ident = PhpString::new(req, ident.as_bytes())?.into_value();
        array.append(req, ident)?;
    }
    Ok(array)
}
