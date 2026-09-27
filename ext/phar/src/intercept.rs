//! `func_interceptors.c`: `Phar::interceptFileFuncs()`.

use php_ext::request::Request;

/// `phar_intercept_functions`: the request marked, as php marks it
/// (`PHAR_G(intercepted)`), for the file functions to read a relative
/// path inside the archive being served.
pub(crate) fn intercept(req: &mut Request) {
    req.state::<crate::State>().intercepted = true;
}
