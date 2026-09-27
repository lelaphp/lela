//! The path natives of `string.c`: `basename` over `php_basename`,
//! `dirname` over `zend_dirname` (`Z/Zend/zend_compile.c:2176`), and
//! `pathinfo` over both. The separator is `/` alone, php's on a host
//! that is not Windows, and the locale is ASCII-compatible, so
//! `php_basename` takes its byte loop.

use php_ext::abi::Union;
use php_ext::attr::php_function;
use php_ext::raising::{Thrown, arg_value_error};
use php_ext::request::Request;
use php_ext::values::{Key, PhpArray, PhpString, Value};

use super::constants::{
    PATHINFO_ALL, PATHINFO_BASENAME, PATHINFO_DIRNAME, PATHINFO_EXTENSION, PATHINFO_FILENAME,
};

/// `php_basename(s, len, suffix, suffix_len)`: the last component,
/// trailing slashes dropped, and the suffix taken off when it ends the
/// component and is shorter than it.
fn php_basename<'a>(s: &'a [u8], suffix: &[u8]) -> &'a [u8] {
    let Some(end) = s.iter().rposition(|c| *c != b'/') else {
        return &[];
    };
    let end = end + 1;
    let start = s[..end]
        .iter()
        .rposition(|c| *c == b'/')
        .map_or(0, |at| at + 1);
    let name = &s[start..end];
    if !suffix.is_empty() && suffix.len() < name.len() && name.ends_with(suffix) {
        return &name[..name.len() - suffix.len()];
    }
    name
}

/// `zend_dirname(path, len)`: the length the path is cut to, and
/// whether the cut path is the one byte php writes over it, `/` or
/// `.`. `dirname` is one reader and `symlink` another, which cuts the
/// expanded link path to its directory.
///
/// ```php
/// var_dump(dirname('/a/b/'), dirname('a'), dirname('/'), dirname(''), dirname('//x//y//'));
/// ```
/// ```expect
/// string(2) "/a"
/// string(1) "."
/// string(1) "/"
/// string(0) ""
/// string(3) "//x"
/// ```
pub(crate) fn zend_dirname(path: &[u8]) -> Vec<u8> {
    if path.is_empty() {
        return Vec::new();
    }
    let mut end = path.len();
    while end > 0 && path[end - 1] == b'/' {
        end -= 1;
    }
    if end == 0 {
        return b"/".to_vec();
    }
    while end > 0 && path[end - 1] != b'/' {
        end -= 1;
    }
    if end == 0 {
        return b".".to_vec();
    }
    while end > 0 && path[end - 1] == b'/' {
        end -= 1;
    }
    if end == 0 {
        return b"/".to_vec();
    }
    path[..end].to_vec()
}

/// `basename(string $path, string $suffix = ""): string`.
///
/// ```php
/// var_dump(basename("/etc/sudoers.d"), basename("/etc/sudoers.d", ".d"), basename("/etc/"), basename("."), basename("/"), basename(""), basename("a.d", "a.d"), basename("//a//b//"), basename("dir\\file"));
/// ```
/// ```expect
/// string(9) "sudoers.d"
/// string(7) "sudoers"
/// string(3) "etc"
/// string(1) "."
/// string(0) ""
/// string(0) ""
/// string(3) "a.d"
/// string(1) "b"
/// string(8) "dir\file"
/// ```
#[php_function]
pub(super) fn basename(
    req: &mut Request,
    path: PhpString,
    #[php(default = "")] suffix: PhpString,
) -> Result<PhpString, Thrown> {
    let suffix: &[u8] = suffix.as_bytes();
    let name = php_basename(path.as_bytes(), suffix).to_vec();
    PhpString::new(req, &name)
}

/// `dirname(string $path, int $levels = 1): string`, `_zend_dirname`:
/// `zend_dirname` once, or `$levels` times while each cut shortens the
/// path, and the `ValueError` for a level below one.
///
/// ```php
/// var_dump(dirname("/etc/passwd"), dirname("/etc/"), dirname("."), dirname("/"), dirname(""), dirname("a"), dirname("//a//b//"), dirname("/usr/local/lib", 2), dirname("/usr/local/lib", 5), dirname("a/b/c", 10));
/// try { dirname("/a", 0); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(4) "/etc"
/// string(1) "/"
/// string(1) "."
/// string(1) "/"
/// string(0) ""
/// string(1) "."
/// string(3) "//a"
/// string(4) "/usr"
/// string(1) "/"
/// string(1) "."
/// dirname(): Argument #2 ($levels) must be greater than or equal to 1
/// ```
#[php_function]
pub(super) fn dirname(
    req: &mut Request,
    path: PhpString,
    #[php(default = 1)] levels: i64,
) -> Result<PhpString, Thrown> {
    if levels < 1 {
        return Err(arg_value_error(req, 2, "be greater than or equal to 1"));
    }
    let mut out = zend_dirname(path.as_bytes());
    let mut left = levels - 1;
    let mut before = path.len();
    while left > 0 && out.len() < before {
        before = out.len();
        out = zend_dirname(&out);
        left -= 1;
    }
    PhpString::new(req, &out)
}

/// `pathinfo(string $path, int $flags = PATHINFO_ALL): array|string`:
/// `dirname` (left out where it is empty), `basename`, `extension`
/// (left out without a `.`) and `filename`, all four as an array, or
/// the first the flags select as a string, the empty string where that
/// one is left out.
///
/// ```php
/// var_dump(pathinfo("/www/htdocs/inc/lib.inc.php"), pathinfo("/a/b", PATHINFO_EXTENSION), pathinfo("a.b", PATHINFO_FILENAME), pathinfo("", PATHINFO_DIRNAME), pathinfo(".x"), pathinfo("x", PATHINFO_DIRNAME | PATHINFO_BASENAME), pathinfo("/a/b.c", 0));
/// ```
/// ```expect
/// array(4) {
///   ["dirname"]=>
///   string(15) "/www/htdocs/inc"
///   ["basename"]=>
///   string(11) "lib.inc.php"
///   ["extension"]=>
///   string(3) "php"
///   ["filename"]=>
///   string(7) "lib.inc"
/// }
/// string(0) ""
/// string(1) "a"
/// string(0) ""
/// array(4) {
///   ["dirname"]=>
///   string(1) "."
///   ["basename"]=>
///   string(2) ".x"
///   ["extension"]=>
///   string(1) "x"
///   ["filename"]=>
///   string(0) ""
/// }
/// string(1) "."
/// string(0) ""
/// ```
#[php_function]
pub(super) fn pathinfo(
    req: &mut Request,
    path: PhpString,
    #[php(default = PATHINFO_ALL)] flags: i64,
) -> Result<Union<PhpArray, PhpString>, Thrown> {
    let opt = flags;
    let p = path.as_bytes();
    let mut parts: Vec<(&[u8], Vec<u8>)> = Vec::new();
    if opt & PATHINFO_DIRNAME != 0 {
        let dir = zend_dirname(p);
        if !dir.is_empty() {
            parts.push((b"dirname", dir));
        }
    }
    let base = php_basename(p, b"").to_vec();
    if opt & PATHINFO_BASENAME != 0 {
        parts.push((b"basename", base.clone()));
    }
    let dot = base.iter().rposition(|c| *c == b'.');
    if opt & PATHINFO_EXTENSION != 0
        && let Some(dot) = dot
    {
        parts.push((b"extension", base[dot + 1..].to_vec()));
    }
    if opt & PATHINFO_FILENAME != 0 {
        parts.push((b"filename", base[..dot.unwrap_or(base.len())].to_vec()));
    }
    if opt != PATHINFO_ALL {
        let first = parts.first().map(|(_, v)| v.clone()).unwrap_or_default();
        return PhpString::new(req, &first).map(Union::B);
    }
    let mut array = PhpArray::new(req)?;
    for (name, bytes) in parts {
        let key = Key::Str(PhpString::new(req, name)?);
        let value = Value::string(PhpString::new(req, &bytes)?);
        array.set(req, key, value)?;
    }
    Ok(Union::A(array))
}
