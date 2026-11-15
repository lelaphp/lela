//! `accel_post_startup` and `accel_finish_startup` (`ZendAccelerator.c`
//! at the tag), which the engine asks as it finishes starting up: a
//! configuration the cache cannot start under is `ACCEL_LOG_FATAL`,
//! which ends the process before anything is served whatever the log's
//! verbosity, and a started cache with `opcache.preload` set names the
//! file the unit cache preloads.

#[cfg(any(target_os = "linux", target_os = "macos"))]
use php_ext::abi::PreloadThread;
use php_ext::abi::{Beneath, PostStartup, chain};
use php_ext::coerce::atoi;
use php_ext::process;
use php_ext::request::{CacheBudget, Request};

use crate::ini::{Log, accel_log, flag, long, value};
use crate::preload_user::Account;
use crate::{Startup, cli, consumption, max_cached_keys, verdict};

/// `sizeof(zend_accel_shared_globals)` on the oracle's build, what
/// `zend_accel_init_shm` asks the segment for beside the interned
/// strings buffer.
const SHARED_GLOBALS: i64 = 216;

/// The engine finishing its startup, once, on the first request's host:
/// `accel_post_startup`, which serves the link it saved first and fails
/// where it failed (`Z/ext/opcache/ZendAccelerator.c:3234-3241`), then
/// does its own work.
pub(crate) fn post_startup(
    req: &mut Request,
    beneath: Beneath<'_, chain::PostStartup>,
) -> PostStartup {
    match beneath.serve(req) {
        PostStartup::Serve => {}
        answered @ (PostStartup::Preload { .. } | PostStartup::Refuse { .. }) => return answered,
    }
    if verdict(req) != Startup::Ok {
        return PostStartup::Serve;
    }
    if let Some(refusal) = refused(req) {
        accel_log(req, Log::Fatal, &refusal);
        return PostStartup::Refuse { status: -2 };
    }
    // `zend_accel_init_shm`: the segment's size and the hash's, and what
    // must be wasted of it for a cache that runs out to restart.
    let budget = CacheBudget {
        memory: u64::try_from(consumption(req)).unwrap_or(0),
        max_wasted: u32::try_from(atoi(value(req, "opcache.max_wasted_percentage"))).unwrap_or(5),
        keys: u64::try_from(max_cached_keys(req)).unwrap_or(u64::MAX),
        root: root_hash(req),
        tier: crate::ini::jit_starts(req),
    };
    req.code_mut().unit_cache().bound(budget);
    let file_cache_only = flag(req, "opcache.file_cache_only");
    if file_cache_only && flag(req, "opcache.enable_file_override") {
        accel_log(
            req,
            Log::Warning,
            "file_override_enabled has no effect when file_cache_only is set",
        );
    }
    preload(req, file_cache_only)
}

/// `ZCG(root_hash)` as `accel_activate` takes it
/// (`Z/ext/opcache/ZendAccelerator.c:2676-2696`): the inode of `/` where
/// `opcache.validate_root` is on and `/` stats, and zero otherwise.
fn root_hash(req: &Request) -> u64 {
    if !flag(req, "opcache.validate_root") {
        return 0;
    }
    std::fs::metadata("/").map_or(0, |root| std::os::unix::fs::MetadataExt::ino(&root))
}

/// `accel_post_startup`'s refusals, in the order it meets them: the
/// shared segment's interned strings buffer, which
/// `zend_accel_init_shm` cannot allocate where the buffer is as large as
/// `opcache.memory_consumption`, and the file cache's directory.
fn refused(req: &mut Request) -> Option<String> {
    let file_cache = value(req, "opcache.file_cache").to_vec();
    if !flag(req, "opcache.file_cache_only") {
        let buffer = long(req, "opcache.interned_strings_buffer");
        let memory = i64::from(atoi(value(req, "opcache.memory_consumption")));
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

/// `accel_finish_startup`: the file `opcache.preload` names, where the
/// cache is not the file cache alone, checked against
/// `opcache.preload_user` as `accel_finish_startup_preload_subprocess`
/// checks it (`Z/ext/opcache/ZendAccelerator.c:4979-5045`). Under uid 0
/// with a `preload_user` naming another user, php forks and preloads in
/// a child that took that user's ids; this engine's process serves
/// requests on threads, which share its ids, so the preload is served on
/// a thread of its own that takes the user's ids for itself alone
/// first ([`Account::take`]), and the process keeps its own (ruled
/// 2026-09-30).
fn preload(req: &mut Request, file_cache_only: bool) -> PostStartup {
    let script = value(req, "opcache.preload").to_vec();
    if script.is_empty() {
        return PostStartup::Serve;
    }
    if file_cache_only {
        accel_log(
            req,
            Log::Warning,
            "Preloading doesn't work in \"file_cache_only\" mode",
        );
        return PostStartup::Serve;
    }
    let here = PostStartup::Preload {
        script: script.clone(),
        thread: None,
    };
    let user = value(req, "opcache.preload_user").to_vec();
    let euid = process::euid();
    if euid != 0 {
        if !user.is_empty() {
            accel_log(
                req,
                Log::Warning,
                "\"opcache.preload_user\" is ignored because the current user is not \"root\"",
            );
        }
        return here;
    }
    if user.is_empty() {
        if cli(req) {
            return here;
        }
        accel_log(
            req,
            Log::Fatal,
            "\"opcache.preload\" requires \"opcache.preload_user\" when running under uid 0",
        );
        return PostStartup::Refuse { status: -2 };
    }
    let Some(account) = Account::named(&user) else {
        let user = String::from_utf8_lossy(&user);
        accel_log(
            req,
            Log::Fatal,
            &format!("Preloading failed to getpwnam(\"{user}\")"),
        );
        return PostStartup::Refuse { status: -2 };
    };
    if account.uid == euid {
        return here;
    }
    as_user(req, script, account)
}

/// The preload served on a thread that takes `account`'s ids first.
/// On Linux the process's dumpable setting, which the thread's change of
/// ids resets, is put back once the thread has ended, as php's master
/// keeps its own while its child changes ids ([`crate::ffi::dumpable`]).
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn as_user(_: &mut Request, script: Vec<u8>, account: Account) -> PostStartup {
    let thread = PreloadThread::new(move |req| account.take(req), unforked);
    #[cfg(target_os = "linux")]
    let thread = match crate::ffi::dumpable() {
        Some(flag) => thread.joined(move || crate::ffi::set_dumpable(flag)),
        None => thread,
    };
    PostStartup::Preload {
        script,
        thread: Some(thread),
    }
}

/// A preload thread the system refused: php's refused `fork()`, a fatal,
/// which ends the process with `ACCEL_LOG_FATAL`'s status.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn unforked(req: &mut Request) -> i32 {
    accel_log(req, Log::Fatal, "Preloading failed to fork()");
    -2
}

/// A platform where no thread can take another user's ids for itself
/// alone: the engine refuses to start rather than serve a file the
/// operator declared unprivileged as root, where php would preload as
/// the user (`trace:preload-user-no-thread-credentials`, as the ruling
/// of 2026-10-01 accepts it).
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn as_user(req: &mut Request, _: Vec<u8>, _: Account) -> PostStartup {
    accel_log(
        req,
        Log::Fatal,
        "Preloading as \"opcache.preload_user\" needs a thread that takes the user's ids for itself alone, which this platform has not",
    );
    PostStartup::Refuse { status: -2 }
}
