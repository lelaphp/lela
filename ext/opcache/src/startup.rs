//! `accel_post_startup` (`ZendAccelerator.c` at the tag), which the
//! engine asks as it finishes starting up: a configuration the cache
//! cannot start under is `ACCEL_LOG_FATAL`, which ends the process
//! before anything is served whatever the log's verbosity.

use php_ext::abi::PostStartup;
use php_ext::request::Request;

use crate::ini::{Log, accel_log, atoi, flag, long, value};
use crate::{Startup, verdict};

/// `sizeof(zend_accel_shared_globals)` on the oracle's build, what
/// `zend_accel_init_shm` asks the segment for beside the interned
/// strings buffer.
const SHARED_GLOBALS: i64 = 216;

/// The engine finishing its startup, once, on the first request's host.
pub(crate) fn post_startup(req: &mut Request) -> PostStartup {
    if verdict(req) != Startup::Ok {
        return PostStartup::Serve;
    }
    if let Some(refusal) = refused(req) {
        accel_log(req, Log::Fatal, &refusal);
        return PostStartup::Refuse { status: -2 };
    }
    let file_cache_only = flag(req, "opcache.file_cache_only");
    if file_cache_only && flag(req, "opcache.enable_file_override") {
        accel_log(
            req,
            Log::Warning,
            "file_override_enabled has no effect when file_cache_only is set",
        );
    }
    PostStartup::Serve
}

/// `accel_post_startup`'s refusals, in the order it meets them: the
/// shared segment's interned strings buffer, which
/// `zend_accel_init_shm` cannot allocate where the buffer is as large as
/// `opcache.memory_consumption`, and the file cache's directory.
fn refused(req: &mut Request) -> Option<String> {
    let file_cache = value(req, "opcache.file_cache").to_vec();
    if !flag(req, "opcache.file_cache_only") {
        let buffer = long(req, "opcache.interned_strings_buffer");
        let memory = atoi(value(req, "opcache.memory_consumption"));
        if buffer > 0 && buffer >= memory {
            let tried = buffer
                .saturating_mul(1 << 20)
                .saturating_add(SHARED_GLOBALS);
            return Some(format!(
                "Insufficient shared memory for interned strings buffer! (tried to allocate {tried} bytes)"
            ));
        }
    } else if file_cache.is_empty() {
        return Some(String::from(
            "opcache.file_cache_only is set without a proper setting of opcache.file_cache",
        ));
    }
    if flag(req, "opcache.file_cache_read_only") {
        accel_log(req, Log::Info, "opcache.file_cache is in read-only mode");
        if file_cache.is_empty() {
            return Some(String::from(
                "opcache.file_cache_read_only is set without a proper setting of opcache.file_cache",
            ));
        }
    }
    let accessible = || {
        std::path::Path::new(String::from_utf8_lossy(&file_cache).as_ref())
            .metadata()
            .is_ok_and(|meta| meta.is_dir())
    };
    if !file_cache.is_empty() && (!file_cache.starts_with(b"/") || !accessible()) {
        return Some(String::from(
            "opcache.file_cache must be a full path of an accessible directory",
        ));
    }
    None
}
