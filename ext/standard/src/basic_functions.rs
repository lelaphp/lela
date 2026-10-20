//! `Z/ext/standard/basic_functions.c`'s natives: `constant`, `sleep`,
//! the `call_user_func` and `forward_static_call` pairs, the environment
//! and ini natives, `getenv`, `putenv`, `time_nanosleep`, the include
//! path's pair,
//! `ip2long` and `long2ip`, and `parse_ini_string` and `parse_ini_file`
//! over the INI grammar of [`crate::ini`], and the stored diagnostic's
//! pair, `error_get_last` and `error_clear_last`.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use std::ops::ControlFlow;
use std::os::unix::ffi::OsStrExt;

use php_ext::abi::{Callable, NamedVariadic, OrFalse, Path, Ref, True, Union, Untyped, Variadic};
use php_ext::attr::{php_const, php_function};
use php_ext::call::{call, call_array, call_named, fetch_constant};
use php_ext::coerce::{c_str, numeric_string, with_string};
use php_ext::raising::{Level, Thrown, arg_value_error, docref, throw_error, warning};
use php_ext::request::{OwnedStream as _, Request, ScannerFile, Streams as _};
use php_ext::syntax::Scanning;
use php_ext::values::walk::{self, Hold};
use php_ext::values::{Emalloc, Key, Owner, PhpArray, PhpString, Value, ValueRef};

use constants::{CONNECTION_ABORTED, INI_PERDIR, INI_SYSTEM, INI_USER};
pub(crate) use constants::{INI_SCANNER_NORMAL, INI_SCANNER_RAW, INI_SCANNER_TYPED};

/// `INI_SCANNER_NORMAL`, `INI_SCANNER_RAW` and `INI_SCANNER_TYPED`.
///
/// ```php
/// var_dump(INI_SCANNER_NORMAL, INI_SCANNER_RAW, INI_SCANNER_TYPED);
/// ```
/// ```expect
/// int(0)
/// int(1)
/// int(2)
/// ```
#[php_const]
pub(crate) mod constants {
    pub(crate) const CONNECTION_ABORTED: i64 = 1;
    pub(crate) const CONNECTION_NORMAL: i64 = 0;
    pub(crate) const CONNECTION_TIMEOUT: i64 = 2;
    pub(crate) const INI_USER: i64 = 1;
    pub(crate) const INI_PERDIR: i64 = 2;
    pub(crate) const INI_SYSTEM: i64 = 4;
    pub(crate) const INI_ALL: i64 = INI_USER | INI_PERDIR | INI_SYSTEM;
    /// `ZEND_INI_SCANNER_NORMAL`, `_RAW` and `_TYPED`.
    pub(crate) const INI_SCANNER_NORMAL: i64 = 0;
    pub(crate) const INI_SCANNER_RAW: i64 = 1;
    pub(crate) const INI_SCANNER_TYPED: i64 = 2;
}

/// The four bytes of a dotted quad, as the host's `inet_pton(AF_INET)`
/// reads one: the BSD `inet_pton4`, four decimal parts of at most 255
/// each, a leading zero allowed.
fn inet_pton4(text: &[u8]) -> Option<[u8; 4]> {
    let mut out = [0u8; 4];
    let mut at = 0;
    let mut octets = 0;
    let mut saw_digit = false;
    for &c in text {
        if c.is_ascii_digit() {
            let next = u32::from(out[at]) * 10 + u32::from(c - b'0');
            out[at] = u8::try_from(next).ok()?;
            if !saw_digit {
                octets += 1;
                if octets > 4 {
                    return None;
                }
                saw_digit = true;
            }
        } else if c == b'.' && saw_digit {
            if octets == 4 {
                return None;
            }
            at += 1;
            saw_digit = false;
        } else {
            return None;
        }
    }
    (octets == 4).then_some(out)
}

/// `ip2long(string $ip): int|false`: the dotted quad as a number, or
/// `false` for text the host's `inet_pton` refuses.
///
/// ```php
/// var_dump(ip2long("1.2.3.4"), ip2long("255.255.255.255"), ip2long("01.2.3.4"), ip2long("1.2.3"), ip2long("256.1.1.1"), ip2long(""), ip2long(" 1.2.3.4"), ip2long("1.2.3.4."));
/// try { ip2long("1.2.3.4\0"); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// int(16909060)
/// int(4294967295)
/// int(16909060)
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(false)
/// ip2long(): Argument #1 ($ip) must not contain any null bytes
/// ```
#[php_function]
fn ip2long(ip: Path) -> OrFalse<i64> {
    let Path(ip) = ip;
    match inet_pton4(ip.as_bytes()) {
        Some(bytes) if !ip.is_empty() => OrFalse::Value(i64::from(u32::from_be_bytes(bytes))),
        _ => OrFalse::False,
    }
}

/// `long2ip(int $ip): string`: the low 32 bits in dotted form.
///
/// ```php
/// var_dump(long2ip(16909060), long2ip(-1), long2ip(0), long2ip(4294967296 + 5), long2ip("167772161"));
/// ```
/// ```expect
/// string(7) "1.2.3.4"
/// string(15) "255.255.255.255"
/// string(7) "0.0.0.0"
/// string(7) "0.0.0.5"
/// string(8) "10.0.0.1"
/// ```
#[php_function]
fn long2ip(req: &mut Request, ip: i64) -> Result<Owner<PhpString>, Thrown> {
    let [a, b, c, d] = u32::try_from(ip & 0xFFFF_FFFF).unwrap_or(0).to_be_bytes();
    PhpString::new(req, format!("{a}.{b}.{c}.{d}").as_bytes())
}

/// `getenv(?string $name = null, bool $local_only = false):
/// array|string|false`: the variable, `false` where it is not set, and
/// with no name every variable whose name holds no space, dot or
/// bracket, as `import_environment_variable` keeps them.
///
/// ```php
/// putenv("DAX_T=on"); putenv("DAX_U=");
/// var_dump(getenv("DAX_T"), getenv("DAX_U"), getenv("NOT_SET_HERE"), getenv("DAX_T", true));
/// $all = getenv();
/// var_dump(is_array($all), $all["DAX_T"], $all["DAX_U"]);
/// ```
/// ```expect
/// string(2) "on"
/// string(0) ""
/// bool(false)
/// string(2) "on"
/// bool(true)
/// string(2) "on"
/// string(0) ""
/// ```
///
/// A name asks the host's `sapi_getenv` first, the request's own
/// variables, and the process's environment after; `$local_only` skips
/// the host and reads the process's environment alone
/// (`Z/ext/standard/basic_functions.c:724`). Under a host whose request
/// variables are not the process's environment the two forms part:
///
/// ```php
/// putenv("DAX_T=on");
/// var_dump(getenv("HTTP_HOST"), getenv("HTTP_HOST", true), getenv("REQUEST_METHOD", true), getenv("DAX_T", true));
/// ```
/// ```request makala
/// --GET--
/// q=1
/// ```
/// ```expect capture
/// 200
/// x-powered-by: PHP/8.5.9
/// content-type: text/html; charset=UTF-8
///
/// string(9) "localhost"
/// bool(false)
/// bool(false)
/// string(2) "on"
/// ```
///
/// A named value is `zend_string_init`'s, an allocation however short:
///
/// ```php
/// putenv("DAX_V=a"); putenv("DAX_W=");
/// function pk($f) { $f(); $s = memory_get_usage(); memory_reset_peak_usage(); $f(); echo memory_get_peak_usage() - $s, "\n"; }
/// pk(function () { $a = getenv("DAX_V"); $b = getenv("DAX_W", true); });
/// ```
/// ```expect
/// 64
/// ```
///
/// The declarations Reflection is asked for most.
///
/// ```php
/// echo new ReflectionFunction('getenv'), new ReflectionFunction('parse_ini_file');
/// ```
/// ```expect
/// Function [ <internal:standard> function getenv ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <optional> ?string $name = null ]
///     Parameter #1 [ <optional> bool $local_only = false ]
///   }
///   - Return [ array|string|false ]
/// }
/// Function [ <internal:standard> function parse_ini_file ] {
///
///   - Parameters [3] {
///     Parameter #0 [ <required> string $filename ]
///     Parameter #1 [ <optional> bool $process_sections = false ]
///     Parameter #2 [ <optional> int $scanner_mode = INI_SCANNER_NORMAL ]
///   }
///   - Return [ array|false ]
/// }
/// ```
#[php_function]
fn getenv(
    req: &mut Request,
    #[php(default = null)] name: Option<PhpString>,
    #[php(default = false)] local_only: bool,
) -> Result<Union<Owner<PhpArray>, OrFalse<Owner<PhpString>>>, Thrown> {
    let Some(name) = name else {
        let pairs = req.loaded_env();
        let mut array = PhpArray::new(req)?;
        for (name, value) in pairs {
            if name.is_empty() || name.iter().any(|c| matches!(c, b' ' | b'.' | b'[')) {
                continue;
            }
            let key = PhpString::new(req, &name)?;
            let key = Key::from_string(req, key);
            let value = PhpString::new(req, &value)?.into_value();
            array.set(req, key, value)?;
        }
        return Ok(Union::A(array));
    };
    let name = c_str(name.as_bytes());
    let value = if local_only {
        (req.env_pairs().iter())
            .find(|(n, _)| n.as_slice() == name)
            .map(|(_, value)| value.clone())
    } else {
        req.env(name).map(<[u8]>::to_vec)
    };
    match value {
        // `RETVAL_STRING` and `php_getenv`'s `zend_string_init`: an
        // allocation whatever the length, never the interned string.
        Some(value) => {
            let value = PhpString::filled(req, value.len(), |bytes| bytes.copy_from_slice(&value))?;
            Ok(Union::B(OrFalse::Value(value)))
        }
        None => Ok(Union::B(OrFalse::False)),
    }
}

/// `PHP_MINFO_FUNCTION(basic)` (`Z/ext/standard/basic_functions.c:490`):
/// `dl`'s and `mail`'s rows, then `assert`'s `DISPLAY_INI_ENTRIES()`,
/// every entry `basic` registers, the booleans through
/// `zend_ini_boolean_displayer_cb`. `browscap` and
/// `unserialize_callback_func` are main's in php, so they are not among
/// them. `dl()` here is php's without `HAVE_LIBDL`, whose row
/// says `unavailable`.
///
/// The witness starts after the `dl` row, where the pinned build, which
/// has `HAVE_LIBDL`, says `enabled`:
///
/// ```php
/// @ini_set('assert.warning', '0');
/// $e = new ReflectionExtension('standard'); ob_start(); $e->info(); echo strstr(ob_get_clean(), 'Path to sendmail');
/// ```
/// ```expect
/// Path to sendmail => /usr/sbin/sendmail -t -i
///
/// Directive => Local Value => Master Value
/// unserialize_max_depth => 4096 => 4096
/// user_agent => no value => no value
/// from => no value => no value
/// default_socket_timeout => 60 => 60
/// auto_detect_line_endings => Off => Off
/// assert.active => On => On
/// assert.bail => Off => Off
/// assert.warning => Off => On
/// assert.callback => no value => no value
/// assert.exception => On => On
/// session.trans_sid_tags => a=href,area=href,frame=src,form= => a=href,area=href,frame=src,form=
/// session.trans_sid_hosts => no value => no value
/// url_rewriter.tags => form= => form=
/// url_rewriter.hosts => no value => no value
/// ```
pub(crate) fn minfo(req: &mut Request) {
    let sendmail =
        String::from_utf8_lossy(req.ini_get(b"sendmail_path").unwrap_or_default()).into_owned();
    req.info_table(&[
        &["Dynamic Library Support", "unavailable"],
        &["Path to sendmail", &sendmail],
    ]);
    let Some(me) = req.extension_named(b"standard") else {
        return;
    };
    let booleans: [&[u8]; 5] = [
        b"auto_detect_line_endings",
        b"assert.active",
        b"assert.bail",
        b"assert.warning",
        b"assert.exception",
    ];
    let rows: Vec<[String; 3]> = req
        .extension_ini(me)
        .into_iter()
        .map(|entry| {
            let name: &[u8] = &entry.decl.name.0;
            let boolean = booleans.contains(&name);
            let shown = |value: &[u8]| match boolean {
                true if Request::ini_parse_bool(value) => String::from("On"),
                true => String::from("Off"),
                false => String::from_utf8_lossy(value).into_owned(),
            };
            [
                String::from_utf8_lossy(name).into_owned(),
                shown(entry.value.as_deref().unwrap_or_default()),
                shown(entry.master.as_deref().unwrap_or_default()),
            ]
        })
        .collect();
    let rows: Vec<[&str; 3]> = rows
        .iter()
        .map(|[name, local, master]| [name.as_str(), local.as_str(), master.as_str()])
        .collect();
    req.info_ini_table(&rows);
}

/// `putenv(string $assignment): bool`: `NAME=value` sets the variable
/// and `NAME` alone unsets it.
///
/// ```php
/// var_dump(putenv("DAX_A=one"), getenv("DAX_A"), putenv("DAX_A=two=2"), getenv("DAX_A"), putenv("DAX_A"), getenv("DAX_A"), putenv("DAX_B="), getenv("DAX_B"));
/// try { putenv("=x"); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { putenv(""); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// string(3) "one"
/// bool(true)
/// string(5) "two=2"
/// bool(true)
/// bool(false)
/// bool(true)
/// string(0) ""
/// putenv(): Argument #1 ($assignment) must have a valid syntax
/// putenv(): Argument #1 ($assignment) must have a valid syntax
/// ```
#[php_function]
fn putenv(req: &mut Request, assignment: PhpString) -> Result<bool, Thrown> {
    let setting = assignment;
    let bytes = setting.as_bytes();
    if bytes.first().is_none_or(|c| *c == b'=') {
        return Err(arg_value_error(req, 1, "have a valid syntax"));
    }
    let text = c_str(bytes).to_vec();
    match text.iter().position(|c| *c == b'=') {
        Some(at) => req.set_env(&text[..at], Some(&text[at + 1..])),
        None => req.set_env(&text, None),
    };
    Ok(true)
}

/// One option `getopt` found: its name and each value it was given.
type Found = (Vec<u8>, Vec<Option<Vec<u8>>>);

/// `getopt(string $short_options, array $long_options = [], &$rest_index
/// = null): array|false`, php's `PHP_FUNCTION(getopt)`: the options
/// `php_getopt` finds in the host's `argv` ([`crate::getopt`]), each
/// under its letter or name, an integer key where the name is one
/// without a leading zero, as its value or `false`, and as a list where
/// it was given more than once; `$rest_index` is where the first
/// argument after them sits. A request with no `argv`, a web host's,
/// answers `false`. Under the witness the `argv` is the file served alone.
///
/// ```php
/// var_dump(getopt("ab:c::", ["long:", "flag"], $rest), $rest);
/// try { getopt("a", null); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// array(0) {
/// }
/// int(1)
/// getopt(): Argument #2 ($long_options) must be of type array, null given
/// ```
///
/// The body holds what php's does for its length, the argv copy and the
/// option table with each long option's name, past which the answer is
/// counted; each one measured past `getopt("z")`, which holds the same
/// argv.
///
/// ```php
/// function pk($f) { $f(); $s = memory_get_usage(); memory_reset_peak_usage(); $f(); return memory_get_peak_usage() - $s; }
/// $z = pk(fn() => getopt("z"));
/// echo pk(fn() => getopt("a:b::c", ["dd:", "e::"])) - $z, ' ', pk(fn() => getopt("abcdefghij")) - $z, ' ', pk(fn() => getopt("", array_fill(0, 9, "long-option-name"))) - $z, "\n";
/// ```
/// ```expect
/// 96 160 720
/// ```
#[php_function]
fn getopt(
    req: &mut Request,
    short_options: PhpString,
    #[php(default = [])] long_options: PhpArray,
    #[php(default = null)] rest_index: Ref<Untyped<Value>>,
) -> Result<OrFalse<Owner<PhpArray>>, Thrown> {
    let spec = short_options;
    rest_index.set(req, Untyped(Value::int(1)))?;
    // `$_SERVER['argv']` as `php_build_argv` wrote it, each argument a C
    // string; the one place `getopt` reads the host's `argv`.
    let argv: Vec<Vec<u8>> = req.argv().iter().map(|a| c_str(a).to_vec()).collect();
    if argv.is_empty() {
        return Ok(OrFalse::False);
    }
    // What php's body allocates beside its answer, all of it given back as
    // it returns (`free_longopts`, `efree(orig_opts)`, `free_argv`).
    let mut scratch = GetoptScratch::default();
    let answer = getopt_in(req, spec, long_options, &rest_index, &argv, &mut scratch);
    scratch.free(req);
    answer
}

/// The blocks `getopt` holds for its length, as php's: the argv copy,
/// `safe_emalloc(sizeof(char *), argc + 1, 0)` and an `estrdup` of each
/// argument, and the option table, `parse_opts`' `safe_emalloc` grown by
/// `safe_erealloc` for the long options, with an `estrdup` of each long
/// option's name.
#[derive(Default)]
struct GetoptScratch {
    blocks: Vec<Emalloc>,
    opts: Option<Emalloc>,
}

/// `sizeof(opt_struct)` on the oracle's LP64.
const OPT_STRUCT: usize = 16;

impl GetoptScratch {
    fn hold(&mut self, req: &mut Request, len: usize) -> Result<(), Thrown> {
        self.blocks.push(Emalloc::new(req, len)?);
        Ok(())
    }

    fn free(self, req: &mut Request) {
        if let Some(opts) = self.opts {
            opts.free(req);
        }
        for block in self.blocks.into_iter().rev() {
            block.free(req);
        }
    }
}

/// `getopt` past the read of `argv`, its scratch held in `scratch`.
fn getopt_in(
    req: &mut Request,
    spec: PhpString,
    long_options: PhpArray,
    rest_index: &Ref<Untyped<Value>>,
    argv: &[Vec<u8>],
    scratch: &mut GetoptScratch,
) -> Result<OrFalse<Owner<PhpArray>>, Thrown> {
    use php_ext::syntax::{Getopt, Opt, Taken};
    scratch.hold(req, size_of::<usize>() * (argv.len() + 1))?;
    for arg in argv {
        scratch.hold(req, arg.len() + 1)?;
    }
    // `parse_opts`: a row for each letter or digit of the string up to
    // its NUL, then the table grown to every long option and the end row.
    let short = c_str(spec.as_bytes());
    let len = short.iter().filter(|b| b.is_ascii_alphanumeric()).count();
    scratch.opts = Some(Emalloc::new(req, OPT_STRUCT * len)?);
    let count = long_options.len();
    if let Some(opts) = scratch.opts.take() {
        scratch.opts = Some(opts.resize(req, OPT_STRUCT * (len + count + 1))?);
    }
    // `ZEND_HASH_FOREACH_VAL(Z_ARRVAL_P(p_longopts), entry)`
    // (`Z/ext/standard/basic_functions.c:1012`), no owner of the table
    // taken: the argument holds it while `__toString` runs.
    let mut longs = Vec::new();
    walk::foreach(req, long_options, Hold::None, |req, row| -> walk::Step {
        let (option, len) = with_string(req, row.value(), |text| {
            (crate::getopt::long_option(c_str(text)), c_str(text).len())
        })?;
        scratch.hold(req, len + 1)?;
        longs.push(option);
        Ok(ControlFlow::Continue(()))
    })?;
    let mut opts: Vec<Opt<'_>> = crate::getopt::short_options(short);
    opts.extend(longs.iter().map(|(name, need)| Opt {
        code: 0,
        need: *need,
        name: Some(name),
    }));
    // `php_getopt(argc, argv, opts, &php_optarg, &php_optind, 0, 1)`,
    // php's unknown switches passed over.
    let mut walk = Getopt::new(&opts, 1);
    // Each option found, by name in the order first found, with each
    // value it was given.
    let mut found: Vec<Found> = Vec::new();
    loop {
        match walk.next(argv) {
            Ok(Taken::End) => break,
            Err(_) => {}
            Ok(Taken::Switch { code, row, value }) => {
                // A long option's code is `0`, and its name is its row's.
                let name = match code {
                    0 => opts[row].name.unwrap_or_default().to_vec(),
                    letter => vec![letter],
                };
                match found.iter_mut().find(|(n, _)| *n == name) {
                    Some((_, values)) => values.push(value),
                    None => found.push((name, vec![value])),
                }
            }
        }
    }
    let optind = walk.optind;
    let mut result = PhpArray::new(req)?;
    for (name, values) in found {
        let index = if name.len() > 1 && name[0] == b'0' {
            None
        } else {
            numeric_string(&name).and_then(|v| v.as_int())
        };
        let mut items = Vec::new();
        for value in values {
            // `ZVAL_STRING`, which allocates whatever the length.
            items.push(match value {
                Some(bytes) => {
                    PhpString::filled(req, bytes.len(), |out| out.copy_from_slice(&bytes))?
                        .into_value()
                }
                None => Owner::bool(false),
            });
        }
        let value = if items.len() == 1 {
            items.remove(0)
        } else {
            let mut list = PhpArray::new(req)?;
            for item in items {
                list.append(req, item)?;
            }
            list.into_value()
        };
        match index {
            Some(n) => result.set(req, Owner::index(n), value)?,
            None => result.set_str(req, &name, value)?,
        }
    }
    rest_index.set(
        req,
        Untyped(Value::int(i64::try_from(optind).unwrap_or(i64::MAX))),
    )?;
    Ok(OrFalse::Value(result))
}

/// `time_nanosleep(int $seconds, int $nanoseconds): array|bool`: `true`
/// after the sleep; a count of nanoseconds past 999 999 999 is the
/// `EINVAL` php turns into its `ValueError`.
///
/// ```php
/// var_dump(time_nanosleep(0, 1000));
/// foreach ([[-1, 0], [0, -1], [0, 1000000000]] as $a) {
///     try { time_nanosleep($a[0], $a[1]); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// }
/// ```
/// ```expect
/// bool(true)
/// time_nanosleep(): Argument #1 ($seconds) must be greater than or equal to 0
/// time_nanosleep(): Argument #2 ($nanoseconds) must be greater than or equal to 0
/// Nanoseconds was not in the range 0 to 999 999 999 or seconds was negative
/// ```
#[php_function]
fn time_nanosleep(
    req: &mut Request,
    seconds: i64,
    nanoseconds: i64,
) -> Result<Union<PhpArray, bool>, Thrown> {
    let Ok(seconds) = u64::try_from(seconds) else {
        return Err(arg_value_error(req, 1, "be greater than or equal to 0"));
    };
    let Ok(nanoseconds) = u32::try_from(nanoseconds) else {
        return Err(arg_value_error(req, 2, "be greater than or equal to 0"));
    };
    if nanoseconds > 999_999_999 {
        return Err(throw_error(
            req,
            "ValueError",
            "Nanoseconds was not in the range 0 to 999 999 999 or seconds was negative",
        ));
    }
    req.blocking(|| std::thread::sleep(std::time::Duration::new(seconds, nanoseconds)));
    Ok(Union::B(true))
}

/// `set_include_path(string $include_path): string|false`: the old
/// path, the new one in place; `false` for an empty one, which
/// `OnUpdateStringUnempty` refuses.
///
/// ```php
/// set_include_path("/one:/two");
/// var_dump(get_include_path(), set_include_path("/three"), get_include_path(), set_include_path(""), get_include_path(), ini_get("include_path"));
/// ```
/// ```expect
/// string(9) "/one:/two"
/// string(9) "/one:/two"
/// string(6) "/three"
/// bool(false)
/// string(6) "/three"
/// string(6) "/three"
/// ```
///
/// The entry is one more owner of the string it is handed
/// (`Z/ext/standard/basic_functions.c:2068`), so a variable's string
/// outlives the variable, and the one it held before is released:
///
/// ```php
/// ini_set('from', 'x');
/// $v = str_repeat('a', 40); $m = memory_get_usage(); set_include_path($v); unset($v); echo memory_get_usage() - $m, "\n";
/// $m = memory_get_usage(); set_include_path(str_repeat('b', 2)); echo memory_get_usage() - $m, "\n";
/// ```
/// ```expect
/// 0
/// -40
/// ```
#[php_function]
fn set_include_path(
    req: &mut Request,
    include_path: Path,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let Path(path) = include_path;
    let Some(old) = req.ini_get(b"include_path").map(<[u8]>::to_vec) else {
        return Ok(OrFalse::False);
    };
    if path.is_empty() || req.ini_set_string(b"include_path", path)?.is_none() {
        return Ok(OrFalse::False);
    }
    PhpString::new(req, &old).map(OrFalse::Value)
}

/// `get_include_path(): string|false`: the `include_path` entry.
///
/// ```php
/// ini_set("include_path", "/a");
/// var_dump(get_include_path());
/// try { get_include_path(1); } catch (ArgumentCountError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(2) "/a"
/// get_include_path() expects exactly 0 arguments, 1 given
/// ```
#[php_function]
fn get_include_path(req: &mut Request) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    match req.ini_get(b"include_path").map(<[u8]>::to_vec) {
        Some(path) => PhpString::new(req, &path).map(OrFalse::Value),
        None => Ok(OrFalse::False),
    }
}

/// `error_get_last(): ?array`, php's `PHP_FUNCTION(error_get_last)`
/// (`Z/ext/standard/basic_functions.c:1420`): the last diagnostic the
/// built-in handler stored, as `type`, `message`, `file` and `line`, and
/// `null` while none is. The handler stores what `error_reporting` and
/// `@` hide, and nothing a user handler served; a handler that answers
/// `false` falls through to it and the diagnostic is stored.
///
/// ```php
/// var_dump(error_get_last()); @trigger_error("x", E_USER_WARNING); var_dump(error_get_last()); var_dump(error_clear_last(), error_get_last());
/// set_error_handler(fn () => true); trigger_error("y"); var_dump(error_get_last());
/// set_error_handler(fn () => false); trigger_error("z"); var_dump(error_get_last()["message"]);
/// ```
/// ```expect
/// NULL
/// array(4) {
///   ["type"]=>
///   int(512)
///   ["message"]=>
///   string(1) "x"
///   ["file"]=>
///   string(%d) "%s"
///   ["line"]=>
///   int(2)
/// }
/// NULL
/// NULL
/// NULL
///
/// Notice: z in %s on line 4
/// string(1) "z"
/// ```
///
/// A fatal raised under `fatal_error_backtraces` carries its backtrace
/// as `trace`, `EG(last_fatal_error_backtrace)`:
///
/// ```php
/// ini_set('fatal_error_backtraces', '1');
/// register_shutdown_function(function () { $e = error_get_last(); var_dump($e["type"], array_column($e["trace"], "function")); });
/// function f() { eval("class C {} class C {}"); }
/// f();
/// ```
/// ```expect exit=255
/// Fatal error: Cannot redeclare class C (previously declared in %s(4) : eval()'d code:1) in %s(4) : eval()'d code on line 1
/// Stack trace:
/// #0 %s(4): eval()
/// #1 %s(5): f()
/// #2 {main}
/// int(64)
/// array(2) {
///   [0]=>
///   string(4) "eval"
///   [1]=>
///   string(1) "f"
/// }
/// ```
///
/// The message is kept on the request's heap until the next diagnostic
/// replaces it or `error_clear_last()` gives it back: `trigger_error()`'s
/// own string with one more owner, and a new string of the engine's
/// text.
///
/// ```php
/// @trigger_error('warm', E_USER_NOTICE);
/// $m = memory_get_usage(); @trigger_error(str_repeat('x', 100), E_USER_NOTICE); echo memory_get_usage() - $m, "\n";
/// $m = memory_get_usage(); @trigger_error('y', E_USER_NOTICE); echo memory_get_usage() - $m, "\n";
/// $m = memory_get_usage(); $x = @$undefined; echo memory_get_usage() - $m, "\n";
/// $m = memory_get_usage(); error_clear_last(); echo memory_get_usage() - $m, "\n";
/// ```
/// ```expect
/// 160
/// -160
/// 56
/// -56
/// ```
#[php_function]
fn error_get_last(req: &mut Request) -> Result<Option<Owner<PhpArray>>, Thrown> {
    req.diagnostics_mut().last_error()
}

/// `error_clear_last(): void`, php's `PHP_FUNCTION(error_clear_last)`
/// (`Z/ext/standard/basic_functions.c:1449`): the stored diagnostic
/// dropped, so `error_get_last()` answers `null` until the next.
///
/// ```php
/// @trigger_error("x"); error_clear_last(); var_dump(error_get_last()); error_clear_last(1);
/// ```
/// ```expect exit=255
/// NULL
///
/// Fatal error: Uncaught ArgumentCountError: error_clear_last() expects exactly 0 arguments, 1 given in %s:2
/// Stack trace:
/// #0 %s(2): error_clear_last(1)
/// #1 {main}
///   thrown in %s on line 2
/// ```
#[php_function]
fn error_clear_last(req: &mut Request) {
    req.diagnostics_mut().clear_last_error();
}

/// `zend_parse_ini_string`/`zend_parse_ini_file` and the callback's
/// array, `into`, which the native `array_init`ed before the parse and
/// which is the answer: `false` after the warning for a `$scanner_mode`
/// that is none of the three or for a syntax error, which `ini_error`
/// writes with the file's name, `Unknown` for a string, and the line the
/// scanner was on, the array destroyed.
fn parse_ini(
    req: &mut Request,
    into: Owner<PhpArray>,
    bytes: &[u8],
    file: &[u8],
    sections: bool,
    mode: i64,
) -> Result<OrFalse<Owner<PhpArray>>, Thrown> {
    let Some(scanner) = crate::ini::scanner(mode) else {
        warning(req, "Invalid scanner mode");
        into.give_up(req);
        return Ok(OrFalse::False);
    };
    let mut callback = crate::ini::Callback::new(*into, sections);
    let parsed =
        php_ext::syntax::parse_ini(req, bytes, scanner, |req, event| callback.event(req, event))
            .map(|parsed| match parsed {
                Ok(()) => true,
                Err((message, line)) => {
                    let file = String::from_utf8_lossy(file);
                    warning(req, &format!("{message} in {file} on line {line}\n"));
                    false
                }
            });
    if matches!(parsed, Ok(true)) {
        return Ok(OrFalse::Value(into));
    }
    into.give_up(req);
    parsed.map(|_| OrFalse::False)
}

/// The string's entries, the string read to its first NUL, as
/// `zend_ini_prepare_string_for_scanning` takes its `strlen`:
/// `parse_ini_string(string $ini_string, bool $process_sections = false, int $scanner_mode = INI_SCANNER_NORMAL): array|false`.
///
/// ```php
/// putenv("DAX_T=t");
/// var_dump(parse_ini_string("a = 1\nb = on\nc = \"q \\\"x\\\" \${DAX_T}\"\nd[] = 1\nd[] = 2\nd[k] = v\n5 = five\n; note\n[sec]\ne = E_ALL & ~E_NOTICE\nf = null"));
/// ```
/// ```expect
/// array(7) {
///   ["a"]=>
///   string(1) "1"
///   ["b"]=>
///   string(1) "1"
///   ["c"]=>
///   string(7) "q "x" t"
///   ["d"]=>
///   array(3) {
///     [0]=>
///     string(1) "1"
///     [1]=>
///     string(1) "2"
///     ["k"]=>
///     string(1) "v"
///   }
///   [5]=>
///   string(4) "five"
///   ["e"]=>
///   string(5) "30711"
///   ["f"]=>
///   string(0) ""
/// }
/// ```
///
/// Sections nest their entries, and the typed scanner answers booleans,
/// null and the numbers that were written as numbers.
///
/// ```php
/// var_dump(parse_ini_string("top = 1\n[one]\nx = yes\ny = 1.5\nz = 0x1A\n[two]\nw = -3\nv = \"7\"\nu = 99999999999999999999", true, INI_SCANNER_TYPED));
/// ```
/// ```expect
/// array(3) {
///   ["top"]=>
///   int(1)
///   ["one"]=>
///   array(3) {
///     ["x"]=>
///     bool(true)
///     ["y"]=>
///     float(1.5)
///     ["z"]=>
///     string(4) "0x1A"
///   }
///   ["two"]=>
///   array(3) {
///     ["w"]=>
///     int(-3)
///     ["v"]=>
///     string(1) "7"
///     ["u"]=>
///     string(20) "99999999999999999999"
///   }
/// }
/// ```
///
/// The raw scanner leaves values as written, quotes around the whole
/// value taken off and a comment cut at its first `;`.
///
/// ```php
/// var_dump(parse_ini_string("a = \"x;y\" ; c\nb = on ; c\nc = \${X} | 1\n[s ~ t]\nd = 'q'", true, INI_SCANNER_RAW));
/// ```
/// ```expect
/// array(4) {
///   ["a"]=>
///   string(3) "x;y"
///   ["b"]=>
///   string(2) "on"
///   ["c"]=>
///   string(8) "${X} | 1"
///   ["s ~ t"]=>
///   array(1) {
///     ["d"]=>
///     string(3) "'q'"
///   }
/// }
/// ```
///
/// A syntax error is a warning and `false`; so is a `$scanner_mode`
/// that is none of the three.
///
/// ```php
/// var_dump(parse_ini_string("a = (1"), parse_ini_string("x\ny = 1\nz = 1 |"), parse_ini_string("a = \${x"), parse_ini_string("[a"), parse_ini_string("a=\"b"), parse_ini_string("a[x]"), parse_ini_string("a", false, 7));
/// ```
/// ```expect
/// Warning: syntax error, unexpected END_OF_LINE, expecting '^' or '|' or '&' or ')' in Unknown on line 1
///  in %s on line 2
///
/// Warning: syntax error, unexpected END_OF_LINE in Unknown on line 3
///  in %s on line 2
///
/// Warning: syntax error, unexpected end of file, expecting TC_FALLBACK or '}' in Unknown on line 1
///  in %s on line 2
///
/// Warning: syntax error, unexpected end of file, expecting ']' in Unknown on line 1
///  in %s on line 2
///
/// Warning: syntax error, unexpected end of file, expecting TC_DOLLAR_CURLY or TC_QUOTED_STRING or '"' in Unknown on line 1
///  in %s on line 2
///
/// Warning: syntax error, unexpected end of file, expecting '=' in Unknown on line 1
///  in %s on line 2
///
/// Warning: Invalid scanner mode in %s on line 2
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(false)
/// bool(false)
/// ```
#[php_function]
fn parse_ini_string(
    req: &mut Request,
    ini_string: PhpString,
    #[php(default = false)] process_sections: bool,
    #[php(default = INI_SCANNER_NORMAL)] scanner_mode: i64,
) -> Result<OrFalse<Owner<PhpArray>>, Thrown> {
    let (text, sections, mode) = (ini_string, process_sections, scanner_mode);
    // "Setup string": a copy with `ZEND_MMAP_AHEAD` zero bytes past it,
    // held while it is parsed.
    let len = text.as_bytes().len();
    let mut string = Emalloc::new(req, len.saturating_add(ZEND_MMAP_AHEAD))?;
    string.bytes_mut()[..len].copy_from_slice(text.as_bytes());
    let into = match PhpArray::new(req) {
        Ok(into) => into,
        Err(thrown) => {
            string.free(req);
            return Err(thrown);
        }
    };
    let parsed = parse_ini(
        req,
        into,
        c_str(&string.bytes()[..len]),
        b"Unknown",
        sections,
        mode,
    );
    string.free(req);
    parsed
}

/// `ZEND_MMAP_AHEAD`: the zero bytes php keeps past a string it hands the
/// scanner.
const ZEND_MMAP_AHEAD: usize = 32;

/// The file's entries, the file opened on the include path as
/// `zend_stream_open` opens it, then read as `parse_ini_string` reads a
/// string, a syntax error naming the file:
/// `parse_ini_file(string $filename, bool $process_sections = false, int $scanner_mode = INI_SCANNER_NORMAL): array|false`.
///
/// ```php
/// $f = fopen("t.ini", "w"); fwrite($f, "[a]\nx = 1\ny = \"two\"\n[b]\nz[] = 3\n"); fclose($f);
/// var_dump(parse_ini_file("t.ini", true));
/// $f = fopen("u.ini", "w"); fwrite($f, "k = v\n\nbad = (\n"); fclose($f);
/// var_dump(parse_ini_file("u.ini"), parse_ini_file("missing.ini"));
/// try { parse_ini_file(""); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// array(2) {
///   ["a"]=>
///   array(2) {
///     ["x"]=>
///     string(1) "1"
///     ["y"]=>
///     string(3) "two"
///   }
///   ["b"]=>
///   array(1) {
///     ["z"]=>
///     array(1) {
///       [0]=>
///       string(1) "3"
///     }
///   }
/// }
///
/// Warning: syntax error, unexpected END_OF_LINE in u.ini on line 4
///  in %s on line 5
///
/// Warning: parse_ini_file(missing.ini): Failed to open stream: No such file or directory in %s on line 5
/// bool(false)
/// bool(false)
/// parse_ini_file(): Argument #1 ($filename) must not be empty
/// ```
///
/// The file is held whole while it is parsed, in the buffer
/// `zend_stream_fixup` `emalloc`s at its size and `ZEND_MMAP_AHEAD`, with
/// the stream open and its opened path held until `zend_destroy_file_handle`
/// after the parse, and no read buffer beside it; every name
/// and value is a string of the request's. A path of `tempnam`'s fixed
/// length keeps the opened path's size the same under both runs:
///
/// ```php
/// $p = tempnam(sys_get_temp_dir(), 'ini'); file_put_contents($p, str_repeat("; a comment filling a line\n", 16384) . "k = v\n");
/// $a = parse_ini_file($p); unset($a);
/// memory_reset_peak_usage(); $at = memory_get_usage(); $a = parse_ini_file($p);
/// echo memory_get_peak_usage() - $at - filesize($p), ' ', memory_get_usage() - $at, "\n";
/// file_put_contents($p, "k = v\n"); $a = parse_ini_file($p); unset($a);
/// memory_reset_peak_usage(); $at = memory_get_usage(); $a = parse_ini_file($p);
/// echo memory_get_peak_usage() - $at, ' ', memory_get_usage() - $at, "\n";
/// var_dump($a); unlink($p);
/// ```
/// ```expect
/// 5186 472
/// 1136 440
/// array(1) {
///   ["k"]=>
///   string(1) "v"
/// }
/// ```
#[php_function]
fn parse_ini_file(
    req: &mut Request,
    filename: Path,
    #[php(default = false)] process_sections: bool,
    #[php(default = INI_SCANNER_NORMAL)] scanner_mode: i64,
) -> Result<OrFalse<Owner<PhpArray>>, Thrown> {
    let (Path(filename), sections, mode) = (filename, process_sections, scanner_mode);
    if filename.is_empty() {
        return Err(arg_value_error(req, 1, "not be empty"));
    }
    let into = PhpArray::new(req)?;
    let name = filename.as_bytes().to_vec();
    let mut fh = match ScannerFile::open(req, &name) {
        Ok(Some(fh)) => fh,
        Ok(None) => {
            into.give_up(req);
            return Ok(OrFalse::False);
        }
        Err(thrown) => {
            into.give_up(req);
            return Err(thrown);
        }
    };
    let parsed = match fh.load(req) {
        Ok(Some(bytes)) => parse_ini(req, into, bytes, &name, sections, mode),
        Ok(None) => {
            into.give_up(req);
            Ok(OrFalse::False)
        }
        Err(thrown) => {
            into.give_up(req);
            Err(thrown)
        }
    };
    fh.destroy(req);
    parsed
}

/// `getservbyname(string $service, string $protocol): int|false`,
/// `PHP_FUNCTION(getservbyname)` (`Z/ext/standard/basic_functions.c:2030`):
/// the `port` `/etc/services` gives the `service` under the protocol,
/// `getservbyname(3)`'s, or `false`.
///
/// ```php
/// var_dump(getservbyname('http', 'tcp'), getservbyname('domain', 'udp'), getservbyname('nope', 'tcp'), getservbyname('http', ''));
/// ```
/// ```expect
/// int(80)
/// int(53)
/// bool(false)
/// bool(false)
/// ```
#[php_function]
fn getservbyname(service: Path, protocol: Path) -> OrFalse<i64> {
    let (Path(name), Path(proto)) = (service, protocol);
    crate::ffi::service_port(name.as_bytes(), proto.as_bytes())
        .map_or(OrFalse::False, |port| OrFalse::Value(i64::from(port)))
}

/// `getservbyport(int $port, string $protocol): string|false`,
/// `PHP_FUNCTION(getservbyport)` (`Z/ext/standard/basic_functions.c:2069`):
/// the `service` name `getservbyport(3)` gives the `port`, cut to sixteen
/// bits as php's `(unsigned short)` cast cuts it, or `false`.
///
/// ```php
/// var_dump(getservbyport(80, 'tcp'), getservbyport(65536 + 22, 'tcp'), getservbyport(80, 'nope'));
/// ```
/// ```expect
/// string(4) "http"
/// string(3) "ssh"
/// bool(false)
/// ```
#[php_function]
fn getservbyport(
    req: &mut Request,
    port: i64,
    protocol: Path,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let Path(proto) = protocol;
    let port = u16::from_le_bytes([port.to_le_bytes()[0], port.to_le_bytes()[1]]);
    match crate::ffi::service_name(port, proto.as_bytes()) {
        Some(name) => PhpString::new(req, &name).map(OrFalse::Value),
        None => Ok(OrFalse::False),
    }
}

/// `getprotobyname(string $protocol): int|false`,
/// `PHP_FUNCTION(getprotobyname)` (`Z/ext/standard/basic_functions.c:2098`):
/// `/etc/protocols`' number for the name, or `false`.
///
/// ```php
/// var_dump(getprotobyname('tcp'), getprotobyname('icmp'), getprotobyname('nope'));
/// ```
/// ```expect
/// int(6)
/// int(1)
/// bool(false)
/// ```
#[php_function]
fn getprotobyname(protocol: Path) -> OrFalse<i64> {
    let Path(name) = protocol;
    crate::ffi::protocol_number(name.as_bytes())
        .map_or(OrFalse::False, |n| OrFalse::Value(i64::from(n)))
}

/// `getprotobynumber(int $protocol): string|false`,
/// `PHP_FUNCTION(getprotobynumber)` (`Z/ext/standard/basic_functions.c:2119`):
/// the name for the number, cut to an `int`, or `false`.
///
/// ```php
/// var_dump(getprotobynumber(17), getprotobynumber(6), getprotobynumber(99999));
/// ```
/// ```expect
/// string(3) "udp"
/// string(3) "tcp"
/// bool(false)
/// ```
#[php_function]
fn getprotobynumber(req: &mut Request, protocol: i64) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let number = protocol;
    // `(int)proto`: the low 32 bits.
    #[allow(clippy::cast_possible_truncation)]
    let number = number as i32;
    match crate::ffi::protocol_name(number) {
        Some(name) => PhpString::new(req, &name).map(OrFalse::Value),
        None => Ok(OrFalse::False),
    }
}

/// `flush(): void`, php's `PHP_FUNCTION(flush)`: `sapi_flush`, the host
/// told it may push what it has, which the handler stack does not see.
///
/// ```php
/// ob_start(); echo "kept"; var_dump(flush()); $kept = ob_get_clean(); echo strlen($kept), "\n";
/// ```
/// ```expect
/// 9
/// ```
#[php_function]
fn flush(req: &mut Request) {
    req.flush();
}

/// `usleep(int $microseconds): void`: the thread asleep that long; a
/// count below zero is php's `ValueError`.
///
/// ```php
/// usleep(10); var_dump(usleep(0));
/// try { usleep(-1); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// NULL
/// usleep(): Argument #1 ($microseconds) must be greater than or equal to 0
/// ```
#[php_function]
fn usleep(req: &mut Request, microseconds: i64) -> Result<(), Thrown> {
    let micros = microseconds;
    let Ok(micros) = u64::try_from(micros) else {
        return Err(arg_value_error(req, 1, "be greater than or equal to 0"));
    };
    // `usleep((unsigned int)num)`: the count as C's cast leaves it.
    let micros = micros & u64::from(u32::MAX);
    req.blocking(|| std::thread::sleep(std::time::Duration::from_micros(micros)));
    Ok(())
}

/// `time_sleep_until(float $timestamp): bool`, php's
/// `PHP_FUNCTION(time_sleep_until)`: asleep until the wall clock reads
/// the timestamp, in whole nanoseconds; a time already past is the
/// warning and `false`, and one outside `0..=u64::MAX / 10^9` seconds is
/// php's `ValueError`, read before the clock is.
///
/// ```php
/// var_dump(time_sleep_until(microtime(true) + 0.01), time_sleep_until(1));
/// try { time_sleep_until(-1); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Warning: time_sleep_until(): Argument #1 ($timestamp) must be greater than or equal to the current time in %s on line 2
/// bool(true)
/// bool(false)
/// time_sleep_until(): Argument #1 ($timestamp) must be between 0 and 18446744073
/// ```
#[php_function]
fn time_sleep_until(req: &mut Request, timestamp: f64) -> Result<bool, Thrown> {
    const NS_PER_SEC: u64 = 1_000_000_000;
    let target = timestamp;
    let Ok(now) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) else {
        return Ok(false);
    };
    let top = u64::MAX / NS_PER_SEC;
    #[allow(clippy::cast_precision_loss)] // php's `(double)(UINT64_MAX / ns_per_sec)`
    let top_secs = top as f64;
    if !(target >= 0.0 && target <= top_secs) {
        return Err(arg_value_error(req, 1, &format!("be between 0 and {top}")));
    }
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    // `(uint64_t)` of a checked double
    let target_ns = (target * 1e9) as u64;
    // `gettimeofday`: the clock to the microsecond.
    let current_ns = now.as_secs() * NS_PER_SEC + u64::from(now.subsec_micros()) * 1000;
    if target_ns < current_ns {
        docref(
            req,
            Level::Warning,
            "Argument #1 ($timestamp) must be greater than or equal to the current time",
        );
        return Ok(false);
    }
    req.blocking(|| std::thread::sleep(std::time::Duration::from_nanos(target_ns - current_ns)));
    Ok(true)
}

/// `get_current_user(): string`, `php_get_current_user`
/// (`Z/main/main.c:1450`): the login name of the owner of the file the
/// request serves, which `sapi_get_stat` stats; the empty string where
/// there is no such file, `php -r` and standard input, or no name for
/// its owner. The name is the host's, so the witness pins its shape.
///
/// ```php
/// $u = get_current_user(); var_dump(is_string($u), $u !== "", $u === get_current_user());
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// bool(true)
/// ```
#[php_function]
fn get_current_user(req: &mut Request) -> Result<Owner<PhpString>, Thrown> {
    let owner = php_get_current_user(req);
    PhpString::new(req, &owner)
}

/// `php_get_current_user` (`Z/main/main.c:1550`): the login name of the
/// owner of the file the request serves, empty where there is none,
/// which `php_sdl.c` names its cache files by.
pub fn php_get_current_user(req: &mut Request) -> Vec<u8> {
    use std::os::unix::fs::MetadataExt;
    req.script_name()
        .map(<[u8]>::to_vec)
        .and_then(|script| req.realpath(&script))
        .and_then(|path| std::fs::metadata(path).ok())
        .and_then(|meta| crate::ffi::user_name(meta.uid()))
        .unwrap_or_default()
}

/// `get_cfg_var(string $option): string|array|false`: the row of
/// `configuration_hash` under the name, what the host's ini text set,
/// with the array `name[]=` lines build ([`Request::cfg_entry`]); `false`
/// where the text set nothing, a default included.
///
/// ```php
/// var_dump(get_cfg_var('date.timezone'), get_cfg_var('display_errors'), get_cfg_var('precision'), get_cfg_var('nope'));
/// ```
/// ```expect
/// string(3) "UTC"
/// string(1) "1"
/// string(2) "14"
/// bool(false)
/// ```
#[php_function]
fn get_cfg_var(
    req: &mut Request,
    option: PhpString,
) -> Result<Union<Owner<PhpString>, OrFalse<Owner<PhpArray>>>, Thrown> {
    let Some(entry) = req.cfg_entry(option.as_bytes())? else {
        return Ok(Union::B(OrFalse::False));
    };
    let answer = match entry.as_ref() {
        ValueRef::Str(value) => Union::A(Owner::take(req, *value)),
        ValueRef::Array(entries) => Union::B(OrFalse::Value(Owner::take(req, *entries))),
        _ => Union::B(OrFalse::False),
    };
    entry.give_up(req);
    Ok(answer)
}

/// Where `error_log` sends a message, its `$message_type` decoded once
/// (`_php_error_log_ex`, `Z/ext/standard/basic_functions.c:1378`).
enum LogTo<'a> {
    /// `0` and any number not below: `php_log_err_with_severity`, the
    /// host's log.
    Log,
    /// `1`: `php_mail` to the destination, with the headers.
    Mail(&'a [u8], Option<&'a [u8]>),
    /// `2`: the removed TCP/IP option.
    Tcp,
    /// `3`: appended to the file the destination names.
    File(&'a [u8]),
    /// `4`: `sapi_module.log_message`, the host's error stream.
    Sapi,
}

/// `error_log(string $message, int $message_type = 0, ?string
/// $destination = null, ?string $additional_headers = null): bool`:
/// the message to the host's log, to a mail, appended to a file, or to
/// the host's error stream, and `true` where it went.
///
/// ```php
/// var_dump(error_log("to the log", 4), error_log("x", 3, "/nonexistent/dir/f"));
/// $f = tempnam(sys_get_temp_dir(), "el"); var_dump(error_log("one\n", 3, $f), error_log("two", 3, $f), file_get_contents($f)); unlink($f);
/// try { error_log("x", 2); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Warning: error_log(/nonexistent/dir/f): Failed to open stream: No such file or directory in %s on line 2
/// bool(true)
/// bool(false)
/// bool(true)
/// bool(true)
/// string(7) "one
/// two"
/// TCP/IP option is not available for error logging
/// ```
/// ```stderr
/// to the log
/// ```
///
/// The log is the file `error_log` names where it names one, each entry
/// stamped `[d-M-Y H:i:s e]` in the request's zone, `date.timezone` as
/// the ini names it (`php_log_err_with_severity`).
///
/// ```php
/// $f = sys_get_temp_dir() . "/error-log-" . getmypid();
/// ini_set("date.timezone", "ASIA/Tokyo"); ini_set("error_log", $f);
/// var_dump(error_log("to the file"));
/// readfile($f); unlink($f);
/// ```
/// ```expect
/// bool(true)
/// [%d-%s-%d %d:%d:%d ASIA/Tokyo] to the file
/// ```
#[php_function]
fn error_log(
    req: &mut Request,
    message: PhpString,
    #[php(default = 0)] message_type: i64,
    #[php(default = null)] destination: Option<Path>,
    #[php(default = null)] additional_headers: Option<PhpString>,
) -> Result<bool, Thrown> {
    let (kind, headers) = (message_type, additional_headers);
    let destination = destination.map(|Path(path)| path);
    let destination_bytes = destination.as_ref().map(PhpString::as_bytes);
    let to = match i32::try_from(kind).unwrap_or(0) {
        1 => LogTo::Mail(
            destination_bytes.unwrap_or_default(),
            headers.as_ref().map(PhpString::as_bytes),
        ),
        2 => LogTo::Tcp,
        3 => LogTo::File(destination_bytes.unwrap_or_default()),
        4 => LogTo::Sapi,
        _ => LogTo::Log,
    };
    let message = message.as_bytes();
    let sent = match to {
        LogTo::Log => {
            req.diagnostics_mut().error_log(message);
            true
        }
        LogTo::Mail(to, headers) => {
            crate::mail::php_mail(req, to, b"PHP error_log message", message, headers, None)?
        }
        LogTo::Tcp => {
            return Err(throw_error(
                req,
                "ValueError",
                "TCP/IP option is not available for error logging",
            ));
        }
        LogTo::File(path) => match req.open_stream(path, b"a", false)? {
            Some(stream) => {
                let written = stream.write(req, message);
                stream.free(req);
                written == Some(message.len())
            }
            None => false,
        },
        LogTo::Sapi => {
            let mut line = message.to_vec();
            line.push(b'\n');
            req.write_stderr(&line);
            true
        }
    };
    Ok(sent)
}

/// `ini_get_all(?string $extension = null, bool $details = true):
/// array|false`, php's `PHP_FUNCTION(ini_get_all)`: `EG(ini_directives)`
/// sorted by `zend_ini_sort_entries`, a case-blind comparison of the
/// names, for the rest of the request ([`Request::ini_sort_entries`]),
/// before the extension is looked up, each entry as its current value
/// or, with `$details`, as its `global_value`, `local_value` and
/// `access`; with an extension named
/// as `module_registry` keys it, lower-case, its own entries alone,
/// `core` being the one name every entry is walked for. A name no
/// extension of the bundle has is the warning and `false`. An entry with
/// no value, php's `NULL`, is `null`, and its `global_value` once the
/// request changed it is the value it was changed to, as php reads
/// `orig_value` and then `value`:
///
/// ```php
/// ini_set('from', 'me');
/// var_dump(ini_get_all(null, false)['user_agent'], ini_get_all()['user_agent'], ini_get_all()['from']);
/// ```
/// ```expect
/// NULL
/// array(3) {
///   ["global_value"]=>
///   NULL
///   ["local_value"]=>
///   NULL
///   ["access"]=>
///   int(7)
/// }
/// array(3) {
///   ["global_value"]=>
///   string(2) "me"
///   ["local_value"]=>
///   string(2) "me"
///   ["access"]=>
///   int(7)
/// }
/// ```
///
/// ```php
/// var_dump(ini_get_all("reflection"), ini_get_all("pcre", false));
/// var_dump(ini_get_all(""), ini_get_all("PCRE"), count(ini_get_all("core")) === count(ini_get_all()));
/// ```
/// ```expect
/// array(0) {
/// }
/// array(3) {
///   ["pcre.backtrack_limit"]=>
///   string(7) "1000000"
///   ["pcre.jit"]=>
///   string(1) "1"
///   ["pcre.recursion_limit"]=>
///   string(6) "100000"
/// }
///
/// Warning: ini_get_all(): Extension "" cannot be found in %s on line 3
///
/// Warning: ini_get_all(): Extension "PCRE" cannot be found in %s on line 3
/// bool(false)
/// bool(false)
/// bool(true)
/// ```
#[php_function]
fn ini_get_all(
    req: &mut Request,
    #[php(default = null)] extension: Option<PhpString>,
    #[php(default = true)] details: bool,
) -> Result<OrFalse<Owner<PhpArray>>, Thrown> {
    req.ini_sort_entries();
    let mut entries = req.ini_entries();
    if let Some(name) = extension {
        let name = name.as_bytes();
        let registered = name.iter().all(|c| !c.is_ascii_uppercase());
        let module = registered.then(|| req.extension_named(name)).flatten();
        if name == b"core" {
        } else if let Some(module) = module {
            entries.retain(|entry| entry.extension == module);
        } else {
            let text = format!(
                "Extension \"{}\" cannot be found",
                String::from_utf8_lossy(name)
            );
            docref(req, Level::Warning, &text);
            return Ok(OrFalse::False);
        }
    }
    let mut all = PhpArray::new(req)?;
    for entry in entries {
        let value = if details {
            let access = (i64::from(entry.decl.user) * INI_USER)
                | (i64::from(entry.decl.per_dir) * INI_PERDIR)
                | (i64::from(entry.decl.system) * INI_SYSTEM);
            // `orig_value`, else `value`: an entry changed from no value
            // shows the value it was changed to.
            let global = entry.master.as_deref().or(entry.value.as_deref());
            let mut option = PhpArray::new(req)?;
            for (name, value) in [
                (&b"global_value"[..], ini_value(req, global)?),
                (b"local_value", ini_value(req, entry.value.as_deref())?),
                (b"access", Owner::int(access)),
            ] {
                option.set_str(req, name, value)?;
            }
            option.into_value()
        } else {
            ini_value(req, entry.value.as_deref())?
        };
        all.set_symtable_str(req, &entry.decl.name.0, value)?;
    }
    Ok(OrFalse::Value(all))
}

/// An entry's value as `ini_get_all()` answers it: the string, or `null`
/// where the entry has none (php's `NULL`).
fn ini_value(req: &mut Request, value: Option<&[u8]>) -> Result<Owner<Value>, Thrown> {
    Ok(match value {
        Some(value) => PhpString::new(req, value)?.into_value(),
        None => Owner::null(),
    })
}

/// `ini_restore(string $option): void`, `zend_restore_ini_entry` at
/// runtime ([`Request::ini_restore`]): an entry the request changed set
/// back to the value it started with, through the entry's own
/// `on_modify`, and no longer changed; an entry it did not change, or
/// one `PHP_INI_USER` may not touch, is left as it is.
///
/// ```php
/// var_dump(set_include_path("var"), ini_restore("include_path"), get_include_path() === set_include_path("x"));
/// ini_set("precision", "5"); ini_restore("precision"); echo 1/3, "\n";
/// var_dump(ini_restore("nope"), ini_restore("enable_dl"));
/// ```
/// ```expect
/// string(%d) "%s"
/// NULL
/// bool(true)
/// 0.33333333333333
/// NULL
/// NULL
/// ```
#[php_function]
fn ini_restore(req: &mut Request, option: PhpString) -> Result<(), Thrown> {
    req.ini_restore(option.as_bytes())
}

/// `ini_parse_quantity(string $shorthand): int`: the quantity as an ini
/// entry reads one, `zend_ini_parse_quantity`
/// ([`Request::ini_parse_quantity`]), with the sentence it says of a
/// shape it interprets raised as a warning.
///
/// ```php
/// var_dump(ini_parse_quantity('128M'), ini_parse_quantity('0x0b'), ini_parse_quantity('-0xB'), ini_parse_quantity('0b+0'));
/// ```
/// ```expect
/// Warning: Invalid quantity "0b+0": no digits after base prefix, interpreting as "0" for backwards compatibility in %s on line 2
/// int(134217728)
/// int(11)
/// int(-11)
/// int(0)
/// ```
#[php_function]
fn ini_parse_quantity(req: &mut Request, shorthand: PhpString) -> i64 {
    let (value, said) = Request::ini_parse_quantity(shorthand.as_bytes());
    if let Some(said) = said {
        warning(req, &said);
    }
    value
}

/// `connection_aborted(): int`: `PG(connection_status)`'s aborted bit,
/// which a host's sink sets by refusing a write
/// (`php_handle_aborted_connection`); `CONNECTION_NORMAL` until then.
///
/// ```php
/// var_dump(connection_aborted(), connection_status(), CONNECTION_NORMAL, CONNECTION_ABORTED, CONNECTION_TIMEOUT, INI_USER, INI_PERDIR, INI_SYSTEM, INI_ALL);
/// ```
/// ```expect
/// int(0)
/// int(0)
/// int(0)
/// int(1)
/// int(2)
/// int(1)
/// int(2)
/// int(4)
/// int(7)
/// ```
#[php_function]
fn connection_aborted(req: &mut Request) -> i64 {
    req.connection_status() & CONNECTION_ABORTED
}

/// `connection_status(): int`: `PG(connection_status)`, see
/// [`connection_aborted`].
///
/// ```php
/// var_dump(connection_status() === CONNECTION_NORMAL);
/// ```
/// ```expect
/// bool(true)
/// ```
#[php_function]
fn connection_status(req: &mut Request) -> i64 {
    req.connection_status()
}

/// `ignore_user_abort(?bool $enable = null): int`: `PG(ignore_user_abort)`
/// as it stood, and, for a boolean given, the `ignore_user_abort` entry
/// set to `1` or `0` through `zend_alter_ini_entry_chars`.
///
/// ```php
/// var_dump(ignore_user_abort(), ignore_user_abort(true), ignore_user_abort(null), ini_get("ignore_user_abort"), ignore_user_abort(false));
/// ```
/// ```expect
/// int(0)
/// int(0)
/// int(1)
/// string(1) "1"
/// int(1)
/// ```
#[php_function]
fn ignore_user_abort(
    req: &mut Request,
    #[php(default = null)] enable: Option<bool>,
) -> Result<i64, Thrown> {
    let old = req
        .ini_get(b"ignore_user_abort")
        .is_some_and(Request::ini_parse_bool);
    if let Some(enable) = enable {
        req.ini_set(b"ignore_user_abort", if enable { b"1" } else { b"0" })?;
    }
    Ok(i64::from(old))
}

/// `register_tick_function(callable $callback, mixed ...$args): bool`:
/// the callback and its arguments kept for `ZEND_TICKS` to serve.
///
/// ```php
/// function tick($x) { echo "tick $x\n"; }
/// var_dump(register_tick_function("tick", "a"), unregister_tick_function("tick"), unregister_tick_function("tick"));
/// try { register_tick_function("nope"); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// NULL
/// NULL
/// register_tick_function(): Argument #1 ($callback) must be a valid callback, function "nope" not found or invalid function name
/// ```
///
/// Under a `declare(ticks)` scope each statement after it serves the
/// callback with the arguments kept:
///
/// ```php
/// declare(ticks=1);
/// register_tick_function(function (stdClass $o, array $a) { echo get_class($o), " ", count($a), "\n"; }, new stdClass, [1, 2]);
/// echo "x\n";
/// ```
/// ```expect
/// stdClass 2
/// x
/// stdClass 2
/// ```
#[php_function]
fn register_tick_function(req: &mut Request, callback: Callable, args: Variadic<Value>) -> bool {
    req.register_tick_function(callback, args.0)
}

/// `unregister_tick_function(callable $callback): void`: the first
/// entry kept for the same callee, scope and object dropped.
///
/// ```php
/// function tock() {}
/// register_tick_function("tock");
/// var_dump(unregister_tick_function("tock"));
/// try { unregister_tick_function("nope"); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// NULL
/// unregister_tick_function(): Argument #1 ($callback) must be a valid callback, function "nope" not found or invalid function name
/// ```
///
/// The entry a `ZEND_TICKS` round is serving is php's `Error` to
/// unregister from inside it:
///
/// ```php
/// declare(ticks=1);
/// function a() {
///     try { unregister_tick_function('a'); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// }
/// register_tick_function('a');
/// unregister_tick_function('a');
/// echo "gone\n";
/// ```
/// ```expect
/// Registered tick function cannot be unregistered while it is being executed
/// gone
/// ```
#[php_function]
fn unregister_tick_function(req: &mut Request, callback: Callable) -> Result<(), Thrown> {
    req.unregister_tick_function(&callback).map(drop)
}

/// `is_uploaded_file(string $filename): bool`: whether the request's
/// upload table holds the path.
///
/// ```php
/// var_dump(is_uploaded_file(__FILE__), move_uploaded_file(__FILE__, "/tmp/x"));
/// try { is_uploaded_file("a\0b"); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(false)
/// bool(false)
/// is_uploaded_file(): Argument #1 ($filename) must not contain any null bytes
/// ```
#[php_function]
fn is_uploaded_file(req: &mut Request, filename: Path) -> bool {
    req.is_uploaded_file(filename.0.as_bytes())
}

/// `move_uploaded_file(string $from, string $to): bool`: an upload the
/// request holds renamed to its new path, or copied there and unlinked
/// where the rename fails, and taken out of the table; `false` for a
/// path that was no upload, and php's warning for one that could not
/// be moved.
///
/// ```php
/// var_dump(move_uploaded_file(__FILE__, sys_get_temp_dir() . "/moved"));
/// try { move_uploaded_file(__FILE__, "a\0b"); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(false)
/// move_uploaded_file(): Argument #2 ($to) must not contain any null bytes
/// ```
#[php_function]
fn move_uploaded_file(req: &mut Request, from: Path, to: Path) -> bool {
    let (Path(from), Path(to)) = (from, to);
    if !req.is_uploaded_file(from.as_bytes()) {
        return false;
    }
    let Ok(target) = req.check_open_basedir(to.as_bytes(), true) else {
        return false;
    };
    let Some(source) = req.take_uploaded_file(from.as_bytes()) else {
        return false;
    };
    let moved = std::fs::rename(&source, &target).is_ok()
        || (std::fs::copy(&source, &target).is_ok() && std::fs::remove_file(&source).is_ok());
    if !moved {
        let text = format!(
            "Unable to move \"{}\" to \"{}\"",
            String::from_utf8_lossy(from.as_bytes()),
            String::from_utf8_lossy(to.as_bytes())
        );
        docref(req, Level::Warning, &text);
    }
    moved
}

/// `sys_getloadavg(): array|false`: the host's one-, five- and
/// fifteen-minute load averages, `getloadavg(3)`; `false` where it has
/// none. The numbers are the host's, so the witness pins the shape.
///
/// ```php
/// $l = sys_getloadavg(); var_dump(count($l), is_float($l[0]) && is_float($l[1]) && is_float($l[2]), array_keys($l));
/// ```
/// ```expect
/// int(3)
/// bool(true)
/// array(3) {
///   [0]=>
///   int(0)
///   [1]=>
///   int(1)
///   [2]=>
///   int(2)
/// }
/// ```
#[php_function]
fn sys_getloadavg(req: &mut Request) -> Result<OrFalse<Owner<PhpArray>>, Thrown> {
    let Some(load) = crate::ffi::loadavg() else {
        return Ok(OrFalse::False);
    };
    let mut array = PhpArray::new(req)?;
    for average in load {
        array.append(req, Owner::float(average))?;
    }
    Ok(OrFalse::Value(array))
}

/// `ftok(string $filename, string $project_id): int`, `Z/ext/standard/ftok.c`:
/// the System V IPC key the host makes of the path and the one
/// character; `-1` after php's warning where the host refuses.
///
/// ```php
/// var_dump(ftok(__FILE__, "a") > 0, ftok("/nonexistent/f", "a"));
/// try { ftok("", "a"); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { ftok(__FILE__, "ab"); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Warning: ftok(): ftok() failed - No such file or directory in %s on line 2
/// bool(true)
/// int(-1)
/// ftok(): Argument #1 ($filename) must not be empty
/// ftok(): Argument #2 ($project_id) must be a single character
/// ```
#[php_function]
fn ftok(req: &mut Request, filename: Path, project_id: PhpString) -> Result<i64, Thrown> {
    let (Path(path), project) = (filename, project_id);
    if path.is_empty() {
        return Err(arg_value_error(req, 1, "not be empty"));
    }
    let &[project] = project.as_bytes() else {
        return Err(arg_value_error(req, 2, "be a single character"));
    };
    let Ok(full) = req.check_open_basedir(path.as_bytes(), true) else {
        return Ok(-1);
    };
    match crate::ffi::ftok(full.as_os_str().as_bytes(), project) {
        Ok(key) => Ok(key),
        Err(errno) => {
            let error = std::io::Error::from_raw_os_error(errno);
            let text = format!("ftok() failed - {}", crate::filestat::strerror(&error));
            docref(req, Level::Warning, &text);
            Ok(-1)
        }
    }
}

/// `inet_ntop(string $ip): string|false`: four bytes as a dotted quad
/// and sixteen as the host's IPv6 text; any other length, `false`.
///
/// ```php
/// var_dump(inet_ntop("\x7f\0\0\1"), inet_ntop(str_repeat("\0", 15) . "\1"), inet_ntop("\0\0\0\0\0\0\0\0\0\0\xff\xff\x01\x02\x03\x04"), inet_ntop("abc"), inet_ntop(""));
/// ```
/// ```expect
/// string(9) "127.0.0.1"
/// string(3) "::1"
/// string(14) "::ffff:1.2.3.4"
/// bool(false)
/// bool(false)
/// ```
#[php_function]
fn inet_ntop(req: &mut Request, ip: PhpString) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let address = ip;
    match crate::ffi::inet_text(address.as_bytes()) {
        Some(text) => PhpString::new(req, &text).map(OrFalse::Value),
        None => Ok(OrFalse::False),
    }
}

/// `inet_pton(string $ip): string|false`: text with a `:` read as IPv6
/// into sixteen bytes, text with a `.` as IPv4 into four, by the host's
/// `inet_pton`; anything else, and what the host refuses, `false`.
///
/// ```php
/// var_dump(inet_pton("127.0.0.1") === "\x7f\0\0\1", bin2hex(inet_pton("::1")), inet_pton("1.2.3"), inet_pton("abra"), inet_pton(""));
/// try { inet_pton("\0"); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// string(32) "00000000000000000000000000000001"
/// bool(false)
/// bool(false)
/// bool(false)
/// inet_pton(): Argument #1 ($ip) must not contain any null bytes
/// ```
#[php_function]
fn inet_pton(req: &mut Request, ip: Path) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let Path(address) = ip;
    let text = address.as_bytes();
    let six = text.contains(&b':');
    if !six && !text.contains(&b'.') {
        return Ok(OrFalse::False);
    }
    match crate::ffi::inet_bytes(text, six) {
        Some(bytes) => PhpString::new(req, &bytes).map(OrFalse::Value),
        None => Ok(OrFalse::False),
    }
}

/// `header_register_callback(callable $callback): bool`,
/// `Z/main/SAPI.c`: the callback `sapi_send_headers` serves once, just
/// before the header block goes out, replacing one registered before.
///
/// ```php
/// header_register_callback(function () { echo "first\n"; });
/// var_dump(header_register_callback(function () { echo "headers\n"; }));
/// echo "body\n";
/// ```
/// ```expect
/// headers
/// bool(true)
/// body
/// ```
#[php_function]
fn header_register_callback(req: &mut Request, callback: Callable) -> bool {
    req.set_header_callback(callback);
    true
}

/// The source of a file as `open_file_for_scanning` reads it through
/// `zend_stream_open`, the include path searched and the stream's
/// warning raised where the file could not be opened: `None` then.
fn scanned_file(req: &mut Request, filename: &[u8]) -> Result<Option<Vec<u8>>, Thrown> {
    let Some(stream) = req.open_stream(filename, b"rb", true)? else {
        return Ok(None);
    };
    let source = match stream.copy_to_mem(req, None) {
        Ok(source) => source,
        Err(thrown) => {
            stream.give_up(req);
            return Err(thrown);
        }
    };
    stream.free(req);
    let bytes = source.as_bytes().to_vec();
    source.give_up(req);
    Ok(Some(bytes))
}

/// `highlight_file(string $filename, bool $return = false): string|bool`,
/// php's `PHP_FUNCTION(highlight_file)`, and its alias `show_source`:
/// the file as `zend_highlight` writes it (`php_ext::syntax::highlight`),
/// to the body or, with `$return`, as the answer; a file that cannot
/// be opened is the stream's warning, php's `Failed opening '%s' for
/// highlighting` and `false`.
///
/// ```php
/// $f = tempnam(sys_get_temp_dir(), 'h'); file_put_contents($f, "<?php echo 'a'; ?>\n");
/// var_dump(highlight_file($f, true)); var_dump(show_source($f));
/// var_dump(highlight_file("nope.php"));
/// try { highlight_file("a\0b"); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// unlink($f);
/// ```
/// ```expect
/// string(252) "<pre><code style="color: #000000"><span style="color: #0000BB">&lt;?php </span><span style="color: #007700">echo </span><span style="color: #DD0000">'a'</span><span style="color: #007700">; </span><span style="color: #0000BB">?&gt;
/// </span></code></pre>"
/// <pre><code style="color: #000000"><span style="color: #0000BB">&lt;?php </span><span style="color: #007700">echo </span><span style="color: #DD0000">'a'</span><span style="color: #007700">; </span><span style="color: #0000BB">?&gt;
/// </span></code></pre>bool(true)
///
/// Warning: highlight_file(nope.php): Failed to open stream: No such file or directory in %s on line 4
///
/// Warning: highlight_file(): Failed opening 'nope.php' for highlighting in %s on line 4
/// bool(false)
/// highlight_file(): Argument #1 ($filename) must not contain any null bytes
/// ```
///
/// Under `zend.multibyte` the file is read as a compile reads it, its
/// encoding found and the source filter passed over it
/// ([`php_ext::syntax::Scanning::File`]), so a `Shift_JIS` file is
/// highlighted as the text it holds, `ソ`'s trail byte no backslash:
///
/// ```php
/// $f = tempnam(sys_get_temp_dir(), 'm'); file_put_contents($f, "<?php\necho \"\x83\x74\x83\x5c\"; // \x95\x5c\n");
/// $p = proc_open([PHP_BINARY, '-n', '-d', 'zend.multibyte=1', '-d', 'zend.script_encoding=Shift_JIS', '-d', 'internal_encoding=UTF-8', '-r', 'highlight_file($argv[1]);', $f], [1 => ['pipe', 'w'], 2 => ['pipe', 'w']], $pipes);
/// echo stream_get_contents($pipes[1]); proc_close($p); unlink($f);
/// ```
/// ```expect process
/// <pre><code style="color: #000000"><span style="color: #0000BB">&lt;?php
/// </span><span style="color: #007700">echo </span><span style="color: #DD0000">"フソ"</span><span style="color: #007700">; </span><span style="color: #FF8000">// 表
/// </span></code></pre>
/// ```
#[php_function(aliases(show_source))]
fn highlight_file(
    req: &mut Request,
    filename: Path,
    #[php(default = false)] r#return: bool,
) -> Result<Union<Owner<PhpString>, bool>, Thrown> {
    let (Path(filename), returned) = (filename, r#return);
    if req.check_open_basedir(filename.as_bytes(), true).is_err() {
        return Ok(Union::B(false));
    }
    let Some(source) = scanned_file(req, filename.as_bytes())? else {
        let text = format!(
            "Failed opening '{}' for highlighting",
            String::from_utf8_lossy(filename.as_bytes())
        );
        docref(req, Level::Warning, &text);
        return Ok(Union::B(false));
    };
    let html = php_ext::syntax::highlight(req, &source, Scanning::File)?;
    if returned {
        return captured(req, &html).map(Union::A);
    }
    req.write(&html);
    Ok(Union::B(true))
}

/// `php_strip_whitespace(string $filename): string`, php's
/// `PHP_FUNCTION(php_strip_whitespace)`: the file as `zend_strip` writes
/// it (`php_ext::syntax::strip`), comments dropped and whitespace one
/// space; a file that cannot be opened is the stream's warning and the
/// empty string.
///
/// ```php
/// $f = tempnam(sys_get_temp_dir(), 'w'); file_put_contents($f, "<?php\n/* c */ \$a  =  1; // d\n\$b = <<<E\n x\n E;\n\n\techo \$a;\n");
/// var_dump(php_strip_whitespace($f), php_strip_whitespace("nope.php")); unlink($f);
/// ```
/// ```expect
/// Warning: php_strip_whitespace(nope.php): Failed to open stream: No such file or directory in %s on line 3
/// string(41) "<?php
///  $a = 1; $b = <<<E
///  x
///  E;
/// echo $a; "
/// string(0) ""
/// ```
///
/// Under `zend.multibyte` the file is read as a compile reads it:
///
/// ```php
/// $f = tempnam(sys_get_temp_dir(), 'm'); file_put_contents($f, "<?php\necho \"\x83\x74\x83\x5c\"; // \x95\x5c\n");
/// $p = proc_open([PHP_BINARY, '-n', '-d', 'zend.multibyte=1', '-d', 'zend.script_encoding=Shift_JIS', '-d', 'internal_encoding=UTF-8', '-r', 'echo php_strip_whitespace($argv[1]);', $f], [1 => ['pipe', 'w'], 2 => ['pipe', 'w']], $pipes);
/// echo stream_get_contents($pipes[1]); proc_close($p); unlink($f);
/// ```
/// ```expect process
/// <?php
/// echo "フソ";
/// ```
#[php_function]
fn php_strip_whitespace(req: &mut Request, filename: Path) -> Result<Owner<PhpString>, Thrown> {
    let Path(filename) = filename;
    let Some(source) = scanned_file(req, filename.as_bytes())? else {
        return PhpString::new(req, b"");
    };
    let stripped = php_ext::syntax::strip(req, &source)?;
    captured(req, &stripped)
}

/// What a native prints into php's default handler and hands back,
/// `php_output_start_default`, `php_output_get_contents` and
/// `php_output_discard` ([`Request::captured`]), and php's `Error`
/// where the handler could not start.
fn captured(req: &mut Request, printed: &[u8]) -> Result<Owner<PhpString>, Thrown> {
    match req.captured(|req| req.write(printed))? {
        Some(captured) => Ok(captured),
        None => Err(throw_error(req, "Error", "Unable to start output handler")),
    }
}

/// `highlight_string(string $string, bool $return = false): string|true`,
/// php's `PHP_FUNCTION(highlight_string)`: the string as `zend_highlight`
/// writes it, scanned from `INITIAL` as a file is, to the body or,
/// with `$return`, as the answer.
///
/// ```php
/// var_dump(highlight_string("<?php 1;", true), highlight_string("x"));
/// ```
/// ```expect
/// <pre><code style="color: #000000">x</code></pre>string(130) "<pre><code style="color: #000000"><span style="color: #0000BB">&lt;?php 1</span><span style="color: #007700">;</span></code></pre>"
/// bool(true)
/// ```
///
/// Under `zend.multibyte` the string is read in the internal encoding,
/// `zend_prepare_string_for_scanning`'s, through the filters it needs,
/// and each token written through the token filter, back in the
/// internal encoding:
///
/// ```php
/// $f = tempnam(sys_get_temp_dir(), 'm'); file_put_contents($f, "<?php\necho \"\x83\x74\x83\x5c\"; // \x95\x5c\n");
/// $p = proc_open([PHP_BINARY, '-n', '-d', 'zend.multibyte=1', '-d', 'zend.script_encoding=Shift_JIS', '-d', 'internal_encoding=Shift_JIS', '-r', 'echo mb_convert_encoding(highlight_string(file_get_contents($argv[1]), true), "UTF-8", "Shift_JIS");', $f], [1 => ['pipe', 'w'], 2 => ['pipe', 'w']], $pipes);
/// echo stream_get_contents($pipes[1]); proc_close($p); unlink($f);
/// ```
/// ```expect process
/// <pre><code style="color: #000000"><span style="color: #0000BB">&lt;?php
/// </span><span style="color: #007700">echo </span><span style="color: #DD0000">"フソ"</span><span style="color: #007700">; </span><span style="color: #FF8000">// 表
/// </span></code></pre>
/// ```
#[php_function]
fn highlight_string(
    req: &mut Request,
    string: PhpString,
    #[php(default = false)] r#return: bool,
) -> Result<Union<Owner<PhpString>, True>, Thrown> {
    let returned = r#return;
    let html = php_ext::syntax::highlight(req, string.as_bytes(), Scanning::String)?;
    if returned {
        return captured(req, &html).map(Union::A);
    }
    req.write(&html);
    Ok(Union::B(True))
}

/// `call_user_func(callable $callback, mixed ...$args): mixed`, php's
/// `PHP_FUNCTION(call_user_func)` (`Z/ext/standard/basic_functions.c:1473`):
/// `Z_PARAM_FUNC` and then `Z_PARAM_VARIADIC_WITH_NAMED`, the
/// positional arguments and the extra named ones off this call's own
/// frame, handed to `zend_call_function` as `fci.params` and
/// `fci.named_params`, which is the engine's `call_named`. The callee's
/// return is this call's. php's compiler serves the spelling
/// `call_user_func(...)` as a dynamic call of the callback itself
/// (`zend_compile_func_cuf`) and this native is what the spelling
/// `$f('call_user_func')` and every other reaches; this compiler serves
/// every spelling through the native, and what differs is the frame a
/// trace prints for a raise at the boundary, so the witnesses that pin
/// a trace spell the call the way the oracle serves through the native
/// too.
///
/// ```php
/// function greet($who, $punct = '!') { return "hi $who$punct"; }
/// class W { static function s($x) { return "s:$x"; } function i($x) { return "i:$x"; } }
/// var_dump(call_user_func('strtoupper', 'abc'), call_user_func('greet', 'bob'), call_user_func(['W', 's'], 1), call_user_func([new W, 'i'], 2), call_user_func(fn($n) => $n * 2, 21), call_user_func('W::s', 3));
/// ```
/// ```expect
/// string(3) "ABC"
/// string(7) "hi bob!"
/// string(3) "s:1"
/// string(3) "i:2"
/// int(42)
/// string(3) "s:3"
/// ```
///
/// The declarations, as `basic_functions.stub.php` has them.
///
/// ```php
/// echo new ReflectionFunction('call_user_func'), new ReflectionFunction('call_user_func_array');
/// ```
/// ```expect
/// Function [ <internal:standard> function call_user_func ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <required> callable $callback ]
///     Parameter #1 [ <optional> mixed ...$args ]
///   }
///   - Return [ mixed ]
/// }
/// Function [ <internal:standard> function call_user_func_array ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <required> callable $callback ]
///     Parameter #1 [ <required> array $args ]
///   }
///   - Return [ mixed ]
/// }
/// ```
///
/// A callback that is none is `Z_PARAM_FUNC`'s `TypeError`, with the
/// sentence `zend_is_callable_ex` wrote for the miss; a call with no
/// callback at all is the arity's `ArgumentCountError`.
///
/// ```php
/// $f = 'call_user_func'; $f('nope');
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught TypeError: call_user_func(): Argument #1 ($callback) must be a valid callback, function "nope" not found or invalid function name in %s:2
/// Stack trace:
/// #0 %s(2): call_user_func('nope')
/// #1 {main}
///   thrown in %s on line 2
/// ```
///
/// ```php
/// call_user_func();
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ArgumentCountError: call_user_func() expects at least 1 argument, 0 given in %s:2
/// Stack trace:
/// #0 %s(2): call_user_func()
/// #1 {main}
///   thrown in %s on line 2
/// ```
///
/// The extra named arguments this call carries are the callee's named
/// ones: `call_user_func_array` places `length` on this native's frame,
/// since `call_user_func` declares no parameter of that name and
/// collects into a variadic, and this native hands it on to `str_pad`,
/// where it lands at the parameter of that name. A trace prints them
/// after the positional arguments, `name: value`, on the frame that
/// holds them.
///
/// ```php
/// $f = 'call_user_func_array';
/// var_dump($f('call_user_func', ['str_pad', 'abc', 'length' => 6, 'pad_string' => '-']));
/// $f('call_user_func', ['str_pad', 'abc', 'pad_type' => 0, 'pad_string' => ' ', 'length' => 'x']);
/// ```
/// ```expect exit=255
/// string(6) "abc---"
///
/// Fatal error: Uncaught TypeError: str_pad(): Argument #2 ($length) must be of type int, string given in %s:4
/// Stack trace:
/// #0 [internal function]: str_pad('abc', 'x', ' ', 0)
/// #1 [internal function]: call_user_func('str_pad', 'abc', pad_type: 0, pad_string: ' ', length: 'x')
/// #2 %s(4): call_user_func_array('call_user_func', Array)
/// #3 {main}
///   thrown in %s on line 4
/// ```
#[php_function]
fn call_user_func(
    req: &mut Request,
    callback: Callable,
    args: NamedVariadic<Value>,
) -> Result<Owner<Value>, Thrown> {
    call_named(req, &callback, &args.positional, args.named())
}

/// `call_user_func_array(callable $callback, array $args): mixed`, php's
/// `PHP_FUNCTION(call_user_func_array)`
/// (`Z/ext/standard/basic_functions.c:1497`): `Z_PARAM_FUNC` and
/// `Z_PARAM_ARRAY_HT`, then `zend_call_function` with the array as
/// `fci.named_params` and nothing in `fci.params`, which is the
/// engine's `call_array`: an integer-keyed element is the next
/// positional argument whatever its key, and a string-keyed one is
/// placed at the parameter of that name by `zend_handle_named_arg`,
/// the slots between left for the callee. php's compiler serves the
/// spelling `call_user_func_array(...)` as `SEND_ARRAY` on a dynamic
/// call (`zend_compile_func_cufa`), so a raise at the boundary prints
/// one frame fewer there than through this native; the witnesses that
/// pin a trace spell the call through a variable, which the oracle
/// serves through the native too.
///
/// ```php
/// function greet($who, $punct = '!') { return "hi $who$punct"; }
/// var_dump(call_user_func_array('str_pad', ['abc', 'length' => 6]), call_user_func_array('str_pad', [1 => 'abc', 5 => 6, 'pad_string' => '-', 'pad_type' => STR_PAD_LEFT]), call_user_func_array('greet', ['punct' => '?', 'who' => 'ann']), call_user_func_array('strtoupper', ['abc']));
/// ```
/// ```expect
/// string(6) "abc   "
/// string(6) "---abc"
/// string(7) "hi ann?"
/// string(3) "ABC"
/// ```
///
/// A name the callee declares no parameter for, and a name whose slot
/// a positional argument filled, are `zend_handle_named_arg`'s two
/// `Error`s, raised before the callee is entered, so the trace ends at
/// this native's frame; a positional element after a named one is
/// `zend_call_function`'s own, raised where the walk meets it, after
/// an earlier name's.
///
/// ```php
/// try { call_user_func_array('str_pad', ['abc', 'nope' => 6]); } catch (Error $e) { echo get_class($e), ': ', $e->getMessage(), "\n"; }
/// try { call_user_func_array('str_pad', ['abc', 6, 'length' => 7]); } catch (Error $e) { echo get_class($e), ': ', $e->getMessage(), "\n"; }
/// try { call_user_func_array('str_pad', ['abc', 'length' => 6, 7]); } catch (Error $e) { echo get_class($e), ': ', $e->getMessage(), "\n"; }
/// try { call_user_func_array('str_pad', ['x' => 1, 2]); } catch (Error $e) { echo get_class($e), ': ', $e->getMessage(), "\n"; }
/// function f($a) {} try { call_user_func_array('f', ['b' => 1]); } catch (Error $e) { echo get_class($e), ': ', $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Error: Unknown named parameter $nope
/// Error: Named parameter $length overwrites previous argument
/// Error: Cannot use positional argument after named argument
/// Error: Unknown named parameter $x
/// Error: Unknown named parameter $b
/// ```
///
/// ```php
/// $f = 'call_user_func_array'; $f('str_pad', ['abc', 'nope' => 6]);
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught Error: Unknown named parameter $nope in %s:2
/// Stack trace:
/// #0 %s(2): call_user_func_array('str_pad', Array)
/// #1 {main}
///   thrown in %s on line 2
/// ```
///
/// ```php
/// $f = 'call_user_func_array'; $f('str_pad', ['abc', 6, 'length' => 7]);
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught Error: Named parameter $length overwrites previous argument in %s:2
/// Stack trace:
/// #0 %s(2): call_user_func_array('str_pad', Array)
/// #1 {main}
///   thrown in %s on line 2
/// ```
///
/// A name a variadic callee does not declare rides on the callee's
/// frame as an extra named argument (`ZEND_CALL_HAS_EXTRA_NAMED_PARAMS`):
/// a user function's `RECV_VARIADIC` takes it into the variadic's array
/// under its name, after the positional extras, held to the declared
/// type under the number after the last positional argument; and a
/// trace prints it `name: value` after the positional arguments.
///
/// ```php
/// function v($a, int ...$rest) { var_dump($rest); }
/// call_user_func_array('v', [1, 2, 'x' => "3", 'y' => 4]);
/// call_user_func_array('v', ['y' => 4, 'a' => 1]);
/// function w(...$rest) { throw new Exception('x'); }
/// $f = 'call_user_func_array'; $f('w', [1, 'x' => 5, 'y' => 'str']);
/// ```
/// ```expect exit=255
/// array(3) {
///   [0]=>
///   int(2)
///   ["x"]=>
///   int(3)
///   ["y"]=>
///   int(4)
/// }
/// array(1) {
///   ["y"]=>
///   int(4)
/// }
///
/// Fatal error: Uncaught Exception: x in %s:5
/// Stack trace:
/// #0 [internal function]: w(1, x: 5, y: 'str')
/// #1 %s(6): call_user_func_array('w', Array)
/// #2 {main}
///   thrown in %s on line 5
/// ```
///
/// ```php
/// function v(int ...$rest) {}
/// $f = 'call_user_func_array';
/// try { $f('v', ['x' => 1, 'y' => 'a']); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// $f('v', [1, 'x' => 'a']);
/// ```
/// ```expect exit=255
/// v(): Argument #1 must be of type int, string given
///
/// Fatal error: Uncaught TypeError: v(): Argument #2 must be of type int, string given in %s:2
/// Stack trace:
/// #0 [internal function]: v(1, x: 'a')
/// #1 %s(5): call_user_func_array('v', Array)
/// #2 {main}
///   thrown in %s on line 2
/// ```
///
/// A variadic native meets them where its parameter parsing reads the
/// variadic, `Z_PARAM_VARIADIC`, and that is
/// `zend_unexpected_extra_named_error`'s `ArgumentCountError`, raised
/// inside the callee's frame after the checks before it: the arity's
/// own error comes first.
///
/// ```php
/// try { call_user_func_array('array_merge', ['a' => [1]]); } catch (Error $e) { echo get_class($e), ': ', $e->getMessage(), "\n"; }
/// try { call_user_func_array('sprintf', ['%d', 'a' => 1]); } catch (Error $e) { echo get_class($e), ': ', $e->getMessage(), "\n"; }
/// try { call_user_func_array('sprintf', ['a' => 1]); } catch (Error $e) { echo get_class($e), ': ', $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// ArgumentCountError: array_merge() does not accept unknown named parameters
/// ArgumentCountError: sprintf() does not accept unknown named parameters
/// ArgumentCountError: sprintf() expects at least 1 argument, 0 given
/// ```
///
/// ```php
/// $f = 'call_user_func_array'; $f('array_merge', [[1], 'a' => [1]]);
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ArgumentCountError: array_merge() does not accept unknown named parameters in %s:2
/// Stack trace:
/// #0 [internal function]: array_merge(Array, a: Array)
/// #1 %s(2): call_user_func_array('array_merge', Array)
/// #2 {main}
///   thrown in %s on line 2
/// ```
///
/// The two parameters' refusals.
///
/// ```php
/// $f = 'call_user_func_array'; $f('strlen', 'x');
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught TypeError: call_user_func_array(): Argument #2 ($args) must be of type array, string given in %s:2
/// Stack trace:
/// #0 %s(2): call_user_func_array('strlen', 'x')
/// #1 {main}
///   thrown in %s on line 2
/// ```
///
/// ```php
/// $f = 'call_user_func_array'; $f('strlen');
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ArgumentCountError: call_user_func_array() expects exactly 2 arguments, 1 given in %s:2
/// Stack trace:
/// #0 %s(2): call_user_func_array('strlen')
/// #1 {main}
///   thrown in %s on line 2
/// ```
#[php_function]
fn call_user_func_array(
    req: &mut Request,
    callback: Callable,
    args: PhpArray,
) -> Result<Owner<Value>, Thrown> {
    call_array(req, &callback, args)
}

/// `forward_static_call(callable $callback, mixed ...$args): mixed`,
/// php's `PHP_FUNCTION(forward_static_call)`
/// (`Z/ext/standard/basic_functions.c:1522`): `call_user_func` from
/// inside a class, the callee served with the caller's called scope as
/// its own where that scope is the callee's class or extends it
/// ([`Callable::with_called_scope`]). Outside a class scope it is
/// php's `Error`, raised before the callee is entered.
///
/// ```php
/// class A { const NAME = 'A'; public static function test() { echo static::NAME, ' ', implode(',', func_get_args()), "\n"; } }
/// class B extends A { const NAME = 'B'; public static function test() { forward_static_call(['A', 'test'], 'more', 'args'); forward_static_call('printf', "%s\n", 'fn'); } }
/// B::test();
/// function outside() { forward_static_call(['A', 'test']); }
/// try { outside(); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// B more,args
/// fn
/// Cannot call forward_static_call() when no class scope is active
/// ```
#[php_function]
fn forward_static_call(
    req: &mut Request,
    callback: Callable,
    args: Variadic<Value>,
) -> Result<Owner<Value>, Thrown> {
    let arguments = args.0;
    if req.scope().is_none() {
        return Err(php_ext::raising::throw_error(
            req,
            "Error",
            "Cannot call forward_static_call() when no class scope is active",
        ));
    }
    let callback = forwarded(req, &callback);
    call(req, &callback, &arguments)
}

/// `forward_static_call_array(callable $callback, array $args): mixed`,
/// php's `PHP_FUNCTION(forward_static_call_array)`: the array handed
/// over as `call_user_func_array` hands it, `fci.named_params`, with the
/// called scope forwarded as [`forward_static_call`] forwards it and no
/// class scope asked for.
///
/// ```php
/// class A { public static function who($a, $b = 'b') { echo static::class, " $a $b\n"; } }
/// class B extends A { public static function test() { forward_static_call_array(['A', 'who'], ['x', 'b' => 'y']); } }
/// B::test();
/// forward_static_call_array(['A', 'who'], ['z']);
/// try { forward_static_call_array(['A', 'who'], ['a' => 1, 2]); } catch (Error $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// B x y
/// A z b
/// Cannot use positional argument after named argument
/// ```
#[php_function]
fn forward_static_call_array(
    req: &mut Request,
    callback: Callable,
    args: PhpArray,
) -> Result<Owner<Value>, Thrown> {
    let array = args;
    let callback = forwarded(req, &callback);
    call_array(req, &callback, array)
}

/// The callable with the called scope of the native being served
/// forwarded to it, `zend_get_called_scope(execute_data)`.
fn forwarded(req: &Request, callback: &Callable) -> Callable {
    match req.called_scope() {
        Some(called) => callback.with_called_scope(req, called),
        None => callback.clone(),
    }
}

/// `constant(string $name): mixed`, php's `PHP_FUNCTION(constant)`
/// (`Z/ext/standard/basic_functions.c:501`): one `Z_PARAM_STR`, then
/// `zend_get_constant_ex` under `ZEND_FETCH_CLASS_EXCEPTION` with the
/// executed scope, which is the engine's `fetch_constant`, and its
/// answer one more owner (`ZVAL_COPY_OR_DUP`). Every arm of the name is
/// the engine's: a leading `\` dropped, a `::` read through the class
/// with `self`, `parent` and `static` against the scopes, a namespace
/// lowered, the table and then `true`, `false` and `null` in any case.
/// What `define()` wrote is what this reads back.
///
/// ```php
/// echo new ReflectionFunction('constant');
/// ```
/// ```expect
/// Function [ <internal:standard> function constant ] {
///
///   - Parameters [1] {
///     Parameter #0 [ <required> string $name ]
///   }
///   - Return [ mixed ]
/// }
/// ```
///
/// ```php
/// define("DEMO", 1); class E { const C = 3; } class A { const X = 1; private const P = 2; }
/// var_dump(constant("DEMO"), constant("\\PHP_EOL"), constant("E::C"), constant("a::X"), constant("\\A::X"), constant("TRUE"), constant("nUlL"), constant("E_ALL"), constant("\\E_STRICT"));
/// ```
/// ```expect
/// Deprecated: Constant E_STRICT is deprecated since 8.4, the error level was removed in %s on line 3
/// int(1)
/// string(1) "
/// "
/// int(3)
/// int(1)
/// int(1)
/// bool(true)
/// NULL
/// int(30719)
/// int(2048)
/// ```
///
/// The refusals, each `zend_get_constant_ex`'s or `zend_fetch_class`'s
/// `Error`, the class named as the string spelled it.
///
/// ```php
/// class A { const X = 1; private const P = 2; }
/// foreach (["a::Y", "A::P", "Nope::X", "self::X", "parent::X", "static::X", "foo\\BAR", "", "::X", "A::", "\\"] as $n) { try { constant($n); } catch (Error $e) { echo $e->getMessage(), "\n"; } }
/// ```
/// ```expect
/// Undefined constant a::Y
/// Cannot access private constant A::P
/// Class "Nope" not found
/// Cannot access "self" when no class scope is active
/// Cannot access "parent" when no class scope is active
/// Cannot access "static" when no class scope is active
/// Undefined constant "foo\BAR"
/// Undefined constant ""
/// Class "" not found
/// Undefined constant A::
/// Undefined constant ""
/// ```
///
/// `self`, `parent` and `static` are read against the scope of the
/// method that called this native, `zend_get_executed_scope` walking
/// past the native's own frame, so a private constant of the scope is
/// reachable and `static` is the called class.
///
/// ```php
/// class M { const K = 7; private const P = 8; function f() { return constant("self::P"); } static function g() { return constant("static::K"); } }
/// class N extends M { const K = 9; function p() { return [constant("parent::K"), constant("self::K"), constant("static::K")]; } }
/// var_dump((new M)->f(), N::g(), (new N)->p());
/// ```
/// ```expect
/// int(8)
/// int(9)
/// array(3) {
///   [0]=>
///   int(7)
///   [1]=>
///   int(9)
///   [2]=>
///   int(9)
/// }
/// ```
///
/// ```php
/// constant("NOPE");
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught Error: Undefined constant "NOPE" in %s:2
/// Stack trace:
/// #0 %s(2): constant('NOPE')
/// #1 {main}
///   thrown in %s on line 2
/// ```
///
/// ```php
/// var_dump(constant(null));
/// ```
/// ```expect exit=255
/// Deprecated: constant(): Passing null to parameter #1 ($name) of type string is deprecated in %s on line 2
///
/// Fatal error: Uncaught Error: Undefined constant "" in %s:2
/// Stack trace:
/// #0 %s(2): constant('')
/// #1 {main}
///   thrown in %s on line 2
/// ```
///
/// ```php
/// constant("A", "B");
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ArgumentCountError: constant() expects exactly 1 argument, 2 given in %s:2
/// Stack trace:
/// #0 %s(2): constant('A', 'B')
/// #1 {main}
///   thrown in %s on line 2
/// ```
#[php_function]
fn constant(req: &mut Request, name: PhpString) -> Result<Owner<Value>, Thrown> {
    fetch_constant(req, name.as_bytes())
}

/// `sleep(int $seconds): int`, php's `PHP_FUNCTION(sleep)`
/// (`Z/ext/standard/basic_functions.c:1122`): php's `ValueError` for a
/// negative count, then `php_sleep`, which is the CLI's `sleep(3)`, and
/// its answer, the seconds left unslept, zero when nothing woke it.
///
/// ```php
/// var_dump(sleep(0), sleep("0"), sleep(false));
/// ```
/// ```expect
/// int(0)
/// int(0)
/// int(0)
/// ```
///
/// ```php
/// sleep(-1);
/// ```
/// ```expect exit=255
/// Fatal error: Uncaught ValueError: sleep(): Argument #1 ($seconds) must be greater than or equal to 0 in %s:2
/// Stack trace:
/// #0 %s(2): sleep(-1)
/// #1 {main}
///   thrown in %s on line 2
/// ```
#[php_function]
fn sleep(req: &mut Request, seconds: i64) -> Result<i64, Thrown> {
    let Ok(seconds) = u64::try_from(seconds) else {
        return Err(arg_value_error(req, 1, "be greater than or equal to 0"));
    };
    req.blocking(|| std::thread::sleep(std::time::Duration::from_secs(seconds)));
    Ok(0)
}
