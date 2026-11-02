//! The `gettext` extension: `php-src/ext/gettext` as one crate under
//! `ext/` (ADR-0004), behind the `gettext` feature as php builds it
//! `--with-gettext`, over the keg's libintl, the library the oracle
//! links, bound in [`ffi`] alone (design §7.2, revision 72). No state of
//! its own (ADR-0005): the domain table is libintl's. The phpt subset is
//! `php-src/ext/gettext/tests`, through `xtask phpt --subset`.
//!
//! `gettext.c` carried over: the ten functions of `gettext.stub.php`,
//! `_` the alias of `gettext`, with the `PHP_GETTEXT_*` length checks
//! and their `ValueError`s. A lookup reads the locale the oracle's
//! libintl reads, which on macOS is the environment's and not
//! `setlocale()`'s: the request's `LC_ALL`, the category's own variable
//! or `LANG`, as gnulib's `gl_locale_name_environ` reads them, and where
//! the request's environment names none, the process's default
//! ([`locale`]).
#![deny(unsafe_code)]
// seams.toml rule lints.no-panic: Internal invariant failures are InternalError fatals with a message, not panics (design §10).
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::todo,
    clippy::unimplemented
)]
// The `expect` fence is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

mod ffi;
mod throwables;

use std::ffi::{CStr, CString, c_int};

use ffi::Domain;
use php_ext::abi::{Extension, ExtensionBuilder, OrFalse, Path};
use php_ext::attr::php_function;
use php_ext::decl::Name;
use php_ext::raising::{Thrown, arg_value_error, argument_error};
use php_ext::request::Request;
use php_ext::values::{Owner, PhpString};
use throwables::Throwables;

/// `PHP_GETTEXT_MAX_DOMAIN_LENGTH`.
const MAX_DOMAIN_LENGTH: usize = 1024;
/// `PHP_GETTEXT_MAX_MSGID_LENGTH`.
const MAX_MSGID_LENGTH: usize = 4096;

/// The extension value (design §7.1): the natives in
/// `gettext.stub.php`'s order, `_` beside `gettext` as
/// `ZEND_RAW_FENAME` lists it, and `PHP_MINFO`.
///
/// ```php
/// var_dump(extension_loaded('gettext'), function_exists('_'));
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// ```
pub fn extension() -> Extension {
    ExtensionBuilder::default()
        .functions(&[textdomain::DECLARED, gettext::DECLARED])
        .function_reshaped(&gettext::DECLARED, |underscore| {
            underscore.name = Name::from("_");
        })
        .functions(&[
            dgettext::DECLARED,
            dcgettext::DECLARED,
            bindtextdomain::DECLARED,
            ngettext::DECLARED,
            dngettext::DECLARED,
            dcngettext::DECLARED,
            bind_textdomain_codeset::DECLARED,
        ])
        .engine_state(Throwables::start)
        .info(info)
        .build("gettext")
}

/// `PHP_MINFO_FUNCTION(php_gettext)`.
///
/// ```php
/// (new ReflectionExtension('gettext'))->info();
/// ```
/// ```expect
///
/// gettext
///
/// GetText Support => enabled
/// ```
fn info(req: &mut Request) {
    req.extensions_mut()
        .info_table(&[&["GetText Support", "enabled"]]);
}

/// `PHP_GETTEXT_DOMAIN_LENGTH_CHECK`: a domain longer than libintl's
/// limit, or empty, refused as the argument at `position`.
fn domain_check(req: &mut Request, position: u32, domain: &[u8]) -> Result<CString, Thrown> {
    if domain.len() > MAX_DOMAIN_LENGTH {
        let ce = Throwables::of(req)?;
        return Err(argument_error(req, ce.value_error, position, "is too long"));
    }
    if domain.is_empty() {
        return Err(arg_value_error(req, position, "not be empty"));
    }
    Ok(ffi::c_string(domain))
}

/// `PHP_GETTEXT_LENGTH_CHECK`: a message id longer than the limit
/// refused as the argument at `position`.
fn length_check(req: &mut Request, position: u32, msgid: &[u8]) -> Result<CString, Thrown> {
    if msgid.len() > MAX_MSGID_LENGTH {
        let ce = Throwables::of(req)?;
        return Err(argument_error(req, ce.value_error, position, "is too long"));
    }
    Ok(ffi::c_string(msgid))
}

/// `PHP_DCGETTEXT_CATEGORY_CHECK`, then the category as the C `int`
/// php's `zend_long` narrows to at the call.
fn category_check(req: &mut Request, position: u32, category: i64) -> Result<c_int, Thrown> {
    if category == i64::from(ffi::LC_ALL) {
        let ce = Throwables::of(req)?;
        return Err(argument_error(
            req,
            ce.value_error,
            position,
            "cannot be LC_ALL",
        ));
    }
    #[expect(
        clippy::cast_possible_truncation,
        reason = "php hands its zend_long to the int parameter, which keeps the low bits"
    )]
    Ok(category as c_int)
}

/// The locale libintl reads for `category` in php's build: the first of
/// `LC_ALL`, the category's own variable and `LANG` the request's
/// environment holds non-empty, as `gl_locale_name_environ` reads the
/// process's, a `LANG` of `UTF-8` passed over on macOS as gnulib passes
/// over the value Terminal sets; none where it names none.
fn locale(req: &Request, category: c_int) -> Option<CString> {
    let env = |name: &[u8]| {
        req.environment()
            .env_pairs()
            .iter()
            .find(|(n, v)| n.as_slice() == name && !v.is_empty())
            .map(|(_, v)| v.as_slice())
    };
    let named = env(b"LC_ALL").or_else(|| ffi::variable(category).and_then(env));
    let lang = env(b"LANG").filter(|lang| !cfg!(target_vendor = "apple") || *lang != b"UTF-8");
    named.or(lang).map(ffi::c_string)
}

/// `ffi::lookup` under the locale [`locale`] names for the category.
fn lookup(req: &Request, domain: Domain<'_>, msgid: &CStr) -> Option<Vec<u8>> {
    let category = match domain {
        Domain::Category(_, category) => category,
        _ => ffi::LC_MESSAGES,
    };
    let name = locale(req, category);
    ffi::under(category, name.as_deref(), || ffi::lookup(domain, msgid))
}

/// `ffi::plural` under the locale [`locale`] names for the category.
fn form_of(req: &Request, domain: Domain<'_>, one: &CStr, many: &CStr, count: i64) -> Vec<u8> {
    let category = match domain {
        Domain::Category(_, category) => category,
        _ => ffi::LC_MESSAGES,
    };
    let name = locale(req, category);
    let n = count.cast_unsigned();
    ffi::under(category, name.as_deref(), || {
        ffi::plural(domain, one, many, n)
    })
}

/// The translation `found`, or the message id itself, whole, where
/// libintl found none (`RETURN_STR_COPY(msgid)`).
fn translated(
    req: &mut Request,
    found: Option<Vec<u8>>,
    msgid: PhpString,
) -> Result<Owner<PhpString>, Thrown> {
    match found {
        Some(text) => PhpString::new(req, &text),
        None => Ok(Owner::take(req, msgid)),
    }
}

/// `textdomain(?string $domain = null): string`: the current domain,
/// after setting it where one is given.
///
/// ```php
/// var_dump(textdomain('witness'), textdomain(null), textdomain());
/// foreach (['', '0', str_repeat('x', 1025)] as $d) {
///     try { textdomain($d); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// }
/// ```
/// ```expect
/// string(7) "witness"
/// string(7) "witness"
/// string(7) "witness"
/// textdomain(): Argument #1 ($domain) must not be empty
/// textdomain(): Argument #1 ($domain) cannot be zero
/// textdomain(): Argument #1 ($domain) is too long
/// ```
#[php_function]
fn textdomain(
    req: &mut Request,
    #[php(default = null)] domain: Option<PhpString>,
) -> Result<Owner<PhpString>, Thrown> {
    let name = match &domain {
        Some(domain) => {
            let name = domain_check(req, 1, domain.as_bytes())?;
            if domain.as_bytes() == b"0" {
                let ce = Throwables::of(req)?;
                return Err(argument_error(req, ce.value_error, 1, "cannot be zero"));
            }
            Some(name)
        }
        None => None,
    };
    let current = ffi::text_domain(name.as_deref()).unwrap_or_default();
    PhpString::new(req, &current)
}

/// `gettext(string $message): string`, and `_`: the translation in the
/// current domain under the locale the environment names for
/// `LC_MESSAGES`, `setlocale()`'s aside, or the message.
///
/// ```php
/// $dir = sys_get_temp_dir() . '/gettext-witness-' . bin2hex(random_bytes(8));
/// mkdir("$dir/en_US.UTF-8/LC_MESSAGES", 0777, true);
/// $t = ['' => "Content-Type: text/plain; charset=UTF-8\nPlural-Forms: nplurals=2; plural=(n != 1);\n",
///       "Basic test" => "Perustesti", "item\0items" => "Stück\0Stücke"];
/// $n = count($t); $ids = $strs = $data = ''; $at = 28 + 16 * $n;
/// foreach ($t as $k => $v) { $ids .= pack('VV', strlen($k), $at + strlen($data)); $data .= "$k\0"; }
/// foreach ($t as $k => $v) { $strs .= pack('VV', strlen($v), $at + strlen($data)); $data .= "$v\0"; }
/// file_put_contents("$dir/en_US.UTF-8/LC_MESSAGES/witness.mo", pack('V7', 0x950412de, 0, $n, 28, 28 + 8 * $n, 0, 0) . $ids . $strs . $data);
/// putenv('LC_ALL=C'); putenv('LANG=C');
/// bindtextdomain('witness', $dir);
/// textdomain('witness');
/// setlocale(LC_ALL, 'en_US.UTF-8');
/// var_dump(gettext('Basic test'), ngettext('item', 'items', 2));
/// putenv('LC_ALL'); putenv('LC_MESSAGES=en_US.UTF-8');
/// var_dump(dcgettext('witness', 'Basic test', LC_CTYPE));
/// var_dump(gettext('Basic test'), _('Basic test'), gettext("Basic test\0tail"), gettext('none'));
/// var_dump(ngettext('item', 'items', 1), ngettext('item', 'items', 2), dngettext('witness', 'item', 'items', -1));
/// var_dump(dcgettext('witness', 'Basic test', LC_MESSAGES));
/// unlink("$dir/en_US.UTF-8/LC_MESSAGES/witness.mo");
/// rmdir("$dir/en_US.UTF-8/LC_MESSAGES"); rmdir("$dir/en_US.UTF-8"); rmdir($dir);
/// try { gettext(str_repeat('x', 4097)); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect locale=en_US.UTF-8
/// string(10) "Basic test"
/// string(5) "items"
/// string(10) "Basic test"
/// string(10) "Perustesti"
/// string(10) "Perustesti"
/// string(10) "Perustesti"
/// string(4) "none"
/// string(6) "Stück"
/// string(7) "Stücke"
/// string(7) "Stücke"
/// string(10) "Perustesti"
/// gettext(): Argument #1 ($message) is too long
/// ```
#[php_function]
fn gettext(req: &mut Request, message: PhpString) -> Result<Owner<PhpString>, Thrown> {
    let msgid = length_check(req, 1, message.as_bytes())?;
    let found = lookup(req, Domain::Current, &msgid);
    translated(req, found, message)
}

/// `dgettext(string $domain, string $message): string`: the translation
/// in `$domain`, or the message.
///
/// ```php
/// var_dump(bin2hex(dgettext('nowhere', "a\0b")));
/// try { dgettext('', 'x'); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { dgettext('d', str_repeat('x', 4097)); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(6) "610062"
/// dgettext(): Argument #1 ($domain) must not be empty
/// dgettext(): Argument #2 ($message) is too long
/// ```
#[php_function]
fn dgettext(
    req: &mut Request,
    domain: PhpString,
    message: PhpString,
) -> Result<Owner<PhpString>, Thrown> {
    let name = domain_check(req, 1, domain.as_bytes())?;
    let msgid = length_check(req, 2, message.as_bytes())?;
    let found = lookup(req, Domain::Named(&name), &msgid);
    translated(req, found, message)
}

/// `dcgettext(string $domain, string $message, int $category): string`:
/// the translation in `$domain` under `$category`, or the message.
///
/// ```php
/// var_dump(dcgettext('nowhere', 'item', LC_MESSAGES));
/// try { dcgettext('nowhere', 'item', LC_ALL); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(4) "item"
/// dcgettext(): Argument #3 ($category) cannot be LC_ALL
/// ```
#[php_function]
fn dcgettext(
    req: &mut Request,
    domain: PhpString,
    message: PhpString,
    category: i64,
) -> Result<Owner<PhpString>, Thrown> {
    let name = domain_check(req, 1, domain.as_bytes())?;
    let msgid = length_check(req, 2, message.as_bytes())?;
    let category = category_check(req, 3, category)?;
    let found = lookup(req, Domain::Category(&name, category), &msgid);
    translated(req, found, message)
}

/// `bindtextdomain(string $domain, ?string $directory = null):
/// string|false`: the directory `$domain` is bound to, after binding it
/// to `$directory` resolved, or to the working directory where that is
/// empty or `"0"`; `false` where the directory does not resolve.
///
/// ```php
/// var_dump(bindtextdomain('witness-unbound') !== false);
/// var_dump(bindtextdomain('witness-bind', '') === getcwd());
/// var_dump(bindtextdomain('witness-bind', '/nonexistent/dir'));
/// var_dump(bindtextdomain('witness-bind', sys_get_temp_dir()) === realpath(sys_get_temp_dir()));
/// try { bindtextdomain("a\0b"); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { bindtextdomain(''); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// bool(false)
/// bool(true)
/// bindtextdomain(): Argument #1 ($domain) must not contain any null bytes
/// bindtextdomain(): Argument #1 ($domain) must not be empty
/// ```
#[php_function]
fn bindtextdomain(
    req: &mut Request,
    domain: Path,
    #[php(default = null)] directory: Option<Path>,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let name = domain_check(req, 1, domain.0.as_bytes())?;
    let dir = match directory {
        None => None,
        Some(Path(dir)) if !dir.as_bytes().is_empty() && dir.as_bytes() != b"0" => {
            let Some(real) = req.environment_mut().realpath(dir.as_bytes()) else {
                return Ok(OrFalse::False);
            };
            Some(real)
        }
        Some(_) => Some(req.environment().cwd()),
    };
    let dir = dir.map(|dir| ffi::c_string(dir.as_os_str().as_encoded_bytes()));
    match ffi::bind(&name, dir.as_deref()) {
        Some(bound) => PhpString::new(req, &bound).map(OrFalse::Value),
        None => Ok(OrFalse::False),
    }
}

/// `ngettext(string $singular, string $plural, int $count): string`: the
/// form for `$count` in the current domain, or of the two given, as
/// English chooses.
///
/// ```php
/// putenv('LC_ALL=C');
/// var_dump(ngettext('item', 'items', 1), ngettext('item', 'items', 0), ngettext("a\0b", 'c', 1));
/// try { ngettext('item', str_repeat('x', 4097), 1); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(4) "item"
/// string(5) "items"
/// string(1) "a"
/// ngettext(): Argument #2 ($plural) is too long
/// ```
#[php_function]
fn ngettext(
    req: &mut Request,
    singular: PhpString,
    plural: PhpString,
    count: i64,
) -> Result<Owner<PhpString>, Thrown> {
    let one = length_check(req, 1, singular.as_bytes())?;
    let many = length_check(req, 2, plural.as_bytes())?;
    let form = form_of(req, Domain::Current, &one, &many, count);
    PhpString::new(req, &form)
}

/// `dngettext(string $domain, string $singular, string $plural, int
/// $count): string`: `ngettext` in `$domain`.
///
/// ```php
/// var_dump(dngettext('nowhere', 'item', 'items', 3));
/// try { dngettext(str_repeat('d', 1025), 'item', 'items', 3); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(5) "items"
/// dngettext(): Argument #1 ($domain) is too long
/// ```
#[php_function]
fn dngettext(
    req: &mut Request,
    domain: PhpString,
    singular: PhpString,
    plural: PhpString,
    count: i64,
) -> Result<Owner<PhpString>, Thrown> {
    let name = domain_check(req, 1, domain.as_bytes())?;
    let one = length_check(req, 2, singular.as_bytes())?;
    let many = length_check(req, 3, plural.as_bytes())?;
    let form = form_of(req, Domain::Named(&name), &one, &many, count);
    PhpString::new(req, &form)
}

/// `dcngettext(string $domain, string $singular, string $plural, int
/// $count, int $category): string`: `ngettext` in `$domain` under
/// `$category`.
///
/// ```php
/// var_dump(dcngettext('nowhere', 'item', 'items', 1, LC_MESSAGES));
/// try { dcngettext('nowhere', 'item', 'items', 1, LC_ALL); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(4) "item"
/// dcngettext(): Argument #5 ($category) cannot be LC_ALL
/// ```
#[php_function]
fn dcngettext(
    req: &mut Request,
    domain: PhpString,
    singular: PhpString,
    plural: PhpString,
    count: i64,
    category: i64,
) -> Result<Owner<PhpString>, Thrown> {
    let name = domain_check(req, 1, domain.as_bytes())?;
    let one = length_check(req, 2, singular.as_bytes())?;
    let many = length_check(req, 3, plural.as_bytes())?;
    let category = category_check(req, 5, category)?;
    let domain = Domain::Category(&name, category);
    let form = form_of(req, domain, &one, &many, count);
    PhpString::new(req, &form)
}

/// `bind_textdomain_codeset(string $domain, ?string $codeset = null):
/// string|false`: the codeset `$domain`'s translations are converted
/// to, after setting it where one is given; `false` where none is set.
///
/// ```php
/// var_dump(bind_textdomain_codeset('witness-codeset'), bind_textdomain_codeset('witness-codeset', 'UTF-8'), bind_textdomain_codeset('witness-codeset', null));
/// try { bind_textdomain_codeset(''); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// bool(false)
/// string(5) "UTF-8"
/// string(5) "UTF-8"
/// bind_textdomain_codeset(): Argument #1 ($domain) must not be empty
/// ```
#[php_function]
fn bind_textdomain_codeset(
    req: &mut Request,
    domain: PhpString,
    #[php(default = null)] codeset: Option<PhpString>,
) -> Result<OrFalse<Owner<PhpString>>, Thrown> {
    let name = domain_check(req, 1, domain.as_bytes())?;
    let codeset = codeset.map(|codeset| ffi::c_string(codeset.as_bytes()));
    match ffi::bind_codeset(&name, codeset.as_deref()) {
        Some(bound) => PhpString::new(req, &bound).map(OrFalse::Value),
        None => Ok(OrFalse::False),
    }
}
