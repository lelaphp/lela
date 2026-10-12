//! The engine finishing its startup (design §6.3): php's
//! `zend_post_startup_cb`, which the engine asks each extension once, on
//! the first request's host, since it is built before any host names
//! itself; and the request `opcache.preload` serves its file in,
//! `accel_finish_startup_preload`'s, before the first request is served.
//! What the preload declared is published into the unit cache, and every
//! request after starts from it (principle 3, ADR-0005); nothing of the
//! preload runs again.

use std::sync::Arc;

use crate::api::abi::{Beneath, PostStartup, chain};
use crate::api::request::{Phase, Request, Sink};
use crate::engine::{Engine, OutputSink, RequestDescription, refused_program, serve_unit};
use crate::unit_cache::PRELOAD_SCRIPT;

/// Whether the engine serves, decided once as it finishes starting up.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Started {
    Serving,
    /// php's startup failed: nothing is served, and the process ends
    /// with this status.
    Refused(i32),
}

impl Engine {
    /// `php_module_startup` finished, as every host's `main` runs it
    /// before it reads its line further (`php_sapi::start`): what the
    /// extensions' `MINIT`s raised, displayed to `sink` as `php_error_cb`
    /// displays a startup error and once, then `zend_post_startup_cb`'s
    /// chain on the host process's environment `env` and working
    /// directory `cwd`, the request `opcache.preload` serves among it
    /// (`accel_finish_startup_preload`). No extension's `rinit` runs. The
    /// status the process ends with where the startup failed, 1 where an
    /// `E_CORE_ERROR` bailed out of it, or the chain's refusal; `None`
    /// where it started.
    pub fn start_up(
        &self,
        env: Vec<(Vec<u8>, Vec<u8>)>,
        cwd: std::path::PathBuf,
        sink: &mut dyn OutputSink,
    ) -> Option<i32> {
        let host = RequestDescription {
            env,
            cwd,
            sapi_name: self.shared.host.to_owned(),
            ..RequestDescription::default()
        };
        let mut req = Request::new(
            Arc::clone(&self.shared),
            RequestDescription {
                sapi_name: host.sapi_name.clone(),
                ..RequestDescription::default()
            },
        );
        if let Some(held) = sink.held() {
            req.hold(held);
        }
        let failed = self.show_startup_raised(&mut req);
        hand_over(&mut req, sink);
        if failed {
            return Some(1);
        }
        match self.started(&host, sink) {
            Started::Serving => None,
            Started::Refused(status) => Some(status),
        }
    }

    /// How the engine's startup finished, which the first request's host
    /// finishes: the `zend_post_startup_cb` chain (`php_module_startup`)
    /// entered from the top ([`crate::api::abi::chain::PostStartup`]),
    /// whose answer serves, refuses or names a file to preload. What the
    /// extensions wrote goes to
    /// `sink` before anything the first request writes.
    pub(crate) fn started(&self, first: &RequestDescription, sink: &mut dyn OutputSink) -> Started {
        *self
            .shared
            .started
            .get_or_init(|| self.finish_startup(first, sink))
    }

    fn finish_startup(&self, first: &RequestDescription, sink: &mut dyn OutputSink) -> Started {
        let links = self.shared.supplied.post_startup.links();
        if links.is_empty() {
            return Started::Serving;
        }
        // `accel_finish_startup_preload`'s request: the host's, with no
        // program, no arguments and no server variables of its own
        // (`sapi_module.register_server_variables` is `NULL` for it).
        let description = RequestDescription {
            env: first.env.clone(),
            cwd: first.cwd.clone(),
            sapi_name: first.sapi_name.clone(),
            ..RequestDescription::default()
        };
        let mut req = Request::new(Arc::clone(&self.shared), description);
        if let Some(held) = sink.held() {
            req.hold(held);
        }
        let answer = Beneath::<chain::PostStartup>::top(links, ()).serve(&mut req);
        match answer {
            PostStartup::Serve => {
                hand_over(&mut req, sink);
                Started::Serving
            }
            PostStartup::Refuse { status } => {
                hand_over(&mut req, sink);
                Started::Refused(status)
            }
            PostStartup::Preload(script) => self.preload(req, &script, sink),
        }
    }

    /// `accel_preload` over the file at `script`: the request started
    /// as php starts it, the file compiled as a `require` and served,
    /// and, where it neither failed nor exited, the program's values
    /// given up, its classes linked by `preload_link` and the tables it
    /// left published for every request, each file it compiled told as
    /// cached; then the request shut down. A file that fails fails the
    /// startup, and the process ends with status 1, as php's does.
    fn preload(&self, mut req: Request, script: &[u8], sink: &mut dyn OutputSink) -> Started {
        req.hash_environment().ok();
        for extension in &self.shared.extensions {
            if let Some(rinit) = extension.rinit {
                rinit(&mut req).ok();
            }
        }
        // "don't send headers": `SG(headers_sent)` and
        // `SG(request_info).no_headers` set once the request started, so
        // the host is handed no head (`Z/ext/opcache/ZendAccelerator.c:4917`).
        req.output_mut().quiet();
        req.begin_preload();
        req.enter_phase(Phase::Running);
        // `zend_compile_file(ZEND_REQUIRE)`: the script opened as a
        // `require` opens it, so it is filed under the path it resolves
        // to, as every include of it after finds it.
        let compiled = match req.resolve_path(script) {
            Ok(resolved) => {
                req.note_primary_file(&resolved);
                req.cached_file(&resolved, None, crate::api::request::HandlePath::Opened)
            }
            Err(_) => None,
        };
        let failed = match compiled {
            Some(Ok(unit)) => !serve_unit(&mut req, &unit, self.shared.loops.entry),
            Some(Err(refused)) => {
                refused_program(&mut req, &refused, script);
                true
            }
            None => {
                req.preload_unopened(script);
                true
            }
        };
        Engine::shutdown_program(&mut req);
        if !failed {
            // `php_free_shutdown_functions` and
            // `zend_shutdown_executor_values`, before `preload_link`: no
            // handler of the program's hears what the link raises.
            req.free_shutdown_functions();
            req.free_user_handlers();
            req.destroy_symbol_table();
            req.destroy_static_vars();
            if let Some(preloaded) = req.publish_preload() {
                let scripts: Vec<Vec<u8>> = preloaded
                    .scripts
                    .iter()
                    .map(|held| held.path.clone())
                    .collect();
                // `preload_optimize`: `zend_optimize_script` over the
                // preload's own pseudo-file and then each saved script,
                // before any is stored.
                req.run_optimizer_passes(PRELOAD_SCRIPT);
                for script in &scripts {
                    req.run_optimizer_passes(script);
                }
                self.shared.units.publish_preload(preloaded);
                req.save_preload_state();
                // `preload_script_in_shared_memory`, for the preload's
                // own pseudo-file and then each saved script.
                req.tell_cached(PRELOAD_SCRIPT);
                for script in scripts {
                    req.tell_cached(&script);
                }
            }
        }
        // The preload's request reports to no host: what it observed
        // and the natives it entered end with it.
        drop(self.shutdown_request(&mut req, sink));
        if failed {
            Started::Refused(1)
        } else {
            Started::Serving
        }
    }
}

/// What a request that served no program wrote, handed to the host.
pub(crate) fn hand_over(req: &mut Request, sink: &mut dyn OutputSink) {
    match req.release_sink() {
        Sink::Held(mut held) => held.flush(),
        Sink::Kept(kept) => kept.hand_to(sink),
    }
}
