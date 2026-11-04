//! A request's `JIT_G` ([`tier::setting`]), which only the handler
//! of `opcache.jit` writes ([`UnitCache::tier_setting`]): each request
//! starts from what the handler left as the engine's start served the
//! host's value, as php's `OnUpdateJit` writes `JIT_G` at startup, kept
//! by a tier that starts nothing; a request's own change is its own,
//! and the next request starts from the start's again.
//!
//! [`UnitCache::tier_setting`]: crate::api::request::UnitCache::tier_setting

use crate::api::abi::ExtensionBuilder;
use crate::api::decl::IniDecl;
use crate::api::request::Request;
use crate::engine::Engine;
use crate::tier;

/// `JIT_G(on)` as the request's end found it.
#[derive(Debug, Default, Clone, Copy)]
struct Seen(bool);

/// An engine whose extension declares `opcache.jit`, its handler turning
/// `JIT_G(on)` on for `tracing` and off otherwise, started with `start`.
fn engine(start: &str) -> Engine {
    let declared = IniDecl {
        name: "opcache.jit".as_bytes(),
        default: Some(b"disable"),
        user: true,
        per_dir: false,
        system: true,
    };
    Engine::builder()
        .ini("opcache.jit", start)
        .extensions(&[ExtensionBuilder::default()
            .ini(vec![declared])
            .on_modify(b"opcache.jit", |req, value| {
                let jit = req.code_mut().unit_cache().tier_setting();
                jit.on = value == Some(b"tracing".as_slice());
                Ok(true)
            })
            .request_state::<Seen>()
            .rshutdown(|req: &mut Request| {
                let on = tier::setting(req).on;
                req.state::<Seen>().0 = on;
            })
            .build("tier_setting")])
        .build()
}

#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_request_starts_from_the_starts_jit_g_and_keeps_its_own_change() {
    let dir = std::env::temp_dir();
    let on = |engine: &Engine, source: &str| {
        let (_, seen) = tier::serve_kept::<Seen>(engine, &dir, b"main.php", source);
        seen.unwrap_or_default().0
    };
    let started_on = engine("tracing");
    assert!(on(&started_on, "<?php"), "the start's value");
    assert!(
        !on(&started_on, "<?php ini_set('opcache.jit', 'disable');"),
        "the request's own change"
    );
    assert!(on(&started_on, "<?php"), "the next request starts again");
    let started_off = engine("disable");
    assert!(!on(&started_off, "<?php"), "the start's value");
    assert!(
        on(&started_off, "<?php ini_set('opcache.jit', 'tracing');"),
        "the request's own change"
    );
}

/// A pool's `php_admin_value[opcache.jit]`, laid over the started engine
/// as php-fpm's child lays it (`fpm_php_zend_ini_alter_master`): the
/// handler's `JIT_G` is every later request's start, as the child's
/// process-wide `JIT_G` is.
#[cfg_attr(miri, ignore = "serves PHP through an engine, which Miri does not run")]
#[test]
fn a_pools_value_laid_over_the_start_is_every_requests_jit_g() {
    let dir = std::env::temp_dir();
    let on = |engine: &Engine| {
        let (_, seen) = tier::serve_kept::<Seen>(engine, &dir, b"main.php", "<?php");
        seen.unwrap_or_default().0
    };
    let mut engine = engine("disable");
    assert!(!on(&engine), "the start's value");
    let laid = engine.alter_master_ini(vec![(
        b"opcache.jit".to_vec(),
        b"tracing".to_vec(),
        crate::engine::IniOrigin::Admin,
    )]);
    assert!(laid.is_ok(), "the engine stands alone");
    assert!(on(&engine), "the pool's value");
    assert!(on(&engine), "and the next request's");
}
