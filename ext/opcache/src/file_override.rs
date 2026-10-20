//! `zend_accel_override_file_functions` (`zend_accelerator_module.c` at
//! the tag): with `opcache.enable_file_override` on, `file_exists`,
//! `is_file` and `is_readable` answer `true` for a file the cache holds
//! without asking the filesystem, and hand every other call to the entry
//! they replaced, which is phar's where phar intercepts the native too.

use php_ext::abi::ExtensionBuilder;
use php_ext::call::passthrough;
use php_ext::raising::Thrown;
use php_ext::request::Request;
use php_ext::values::{Owner, Value, ValueRef};

use crate::ini::flag;
use crate::{Startup, state};

/// The three natives replaced, as the engine is built; whether a call
/// is answered from the cache is the startup's verdict and the
/// directive's, read as the call is served.
pub(crate) fn install(builder: ExtensionBuilder) -> ExtensionBuilder {
    builder
        .intercept(b"file_exists", in_cache)
        .intercept(b"is_file", in_cache)
        .intercept(b"is_readable", in_cache)
}

/// `accel_file_exists`, `accel_is_file` and `accel_is_readable`: a call
/// of one non-empty string names a file `filename_is_in_cache` finds,
/// and is `true`; anything else is the replaced entry's to answer.
fn in_cache(req: &mut Request) -> Result<Owner<Value>, Thrown> {
    let named = match req.frames().argument(1).map(|value| value.deref()) {
        Some(value) if req.frames().argument_count() == 1 => match value.as_ref() {
            ValueRef::Str(name) if !name.as_bytes().is_empty() => Some(name.as_bytes().to_vec()),
            _ => None,
        },
        _ => None,
    };
    if let Some(name) = named
        && overriding(req)
        && req.unit_cache().holds(&name, None)?
    {
        return Ok(Owner::bool(true));
    }
    passthrough(req, None)
}

/// Whether `accel_startup` installed the override: the cache started,
/// `opcache.enable_file_override` is on, and the cache is not the file
/// cache alone, which php refuses with a warning at startup.
fn overriding(req: &mut Request) -> bool {
    let (startup, _) = state(req);
    startup == Startup::Ok
        && flag(req, "opcache.enable_file_override")
        && !flag(req, "opcache.file_cache_only")
}
