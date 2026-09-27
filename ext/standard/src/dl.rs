//! `Z/ext/standard/dl.c`: `dl()`, php's loading of an extension into a
//! running request. This engine links its extensions at build and has
//! no loader for a C library built against php's extension API, so `dl()`
//! refuses as php's does where loading is off, where the name is too
//! long and where it names a path, and otherwise says what a php built
//! without `HAVE_LIBDL` says (`Z/ext/standard/dl.c:261`).
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use php_ext::attr::php_function;
use php_ext::raising::{Level, docref};
use php_ext::request::Request;
use php_ext::values::PhpString;

/// `dl(string $extension_filename): bool`, `PHP_FUNCTION(dl)` and
/// `php_load_extension`'s checks before it opens anything: `enable_dl`
/// off is php's warning, as is a name of `MAXPATHLEN` bytes or more and
/// a name with a `/` in it, since what `dl()` loads is a file name under
/// `extension_dir`; each answers `false`. A name that passes is php's
/// `php_dl` without `HAVE_LIBDL`, its warning, and `false`.
///
/// ```php
/// var_dump(dl("/path/to/module"), dl(str_repeat("a", 2000)));
/// ```
/// ```expect
/// Warning: dl(): Temporary module name should contain only filename in %s on line 2
///
/// Warning: dl(): Filename exceeds the maximum allowed length of 1024 characters in %s on line 2
/// bool(false)
/// bool(false)
/// ```
///
/// ```php
/// echo new ReflectionFunction("dl");
/// ```
/// ```expect
/// Function [ <internal:standard> function dl ] {
///
///   - Parameters [1] {
///     Parameter #0 [ <required> string $extension_filename ]
///   }
///   - Return [ bool ]
/// }
/// ```
#[php_function]
fn dl(req: &mut Request, extension_filename: PhpString) -> bool {
    let filename = extension_filename.as_bytes();
    if !req
        .ini_get(b"enable_dl")
        .is_some_and(Request::ini_parse_bool)
    {
        docref(
            req,
            Level::Warning,
            "Dynamically loaded extensions aren't enabled",
        );
        return false;
    }
    // `MAXPATHLEN`, the host's `PATH_MAX`.
    let max = usize::try_from(libc::PATH_MAX).unwrap_or(usize::MAX);
    if filename.len() >= max {
        let text = format!("Filename exceeds the maximum allowed length of {max} characters");
        docref(req, Level::Warning, &text);
        return false;
    }
    let text = if filename.contains(&b'/') {
        String::from("Temporary module name should contain only filename")
    } else {
        format!(
            "Cannot dynamically load {} - dynamic modules are not supported",
            String::from_utf8_lossy(filename)
        )
    };
    docref(req, Level::Warning, &text);
    false
}
