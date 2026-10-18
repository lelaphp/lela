//! The engine finishing its startup (design §6.3): php's
//! `zend_post_startup_cb`, which the engine asks each extension once, on
//! the first request's host, since it is built before any host names
//! itself; and the request `opcache.preload` serves its file in,
//! `accel_finish_startup_preload`'s, before the first request is served.
//! What the preload declared is published into the unit cache, and every
//! request after starts from it (principle 3, ADR-0005); nothing of the
//! preload runs again.

use std::sync::Arc;

use crate::api::abi::{Beneath, PostStartup, PreloadThread, chain};
use crate::api::request::{Phase, Request};
use crate::engine::{Engine, Lent, OutputSink, RequestDescription, refused_program, serve_unit};
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
    /// where it started; and `sink`, handed back.
    pub fn start_up(
        &self,
        env: Vec<(Vec<u8>, Vec<u8>)>,
        cwd: std::path::PathBuf,
        sink: Box<dyn OutputSink + Send>,
    ) -> (Option<i32>, Box<dyn OutputSink + Send>) {
        let host = RequestDescription {
            env,
            cwd,
            sapi_name: self.shared.host.to_owned(),
            ..RequestDescription::default()
        };
        let mut req = Request::lent(
            Arc::clone(&self.shared),
            RequestDescription {
                sapi_name: host.sapi_name.clone(),
                ..RequestDescription::default()
            },
            sink,
        );
        let failed = self.show_startup_raised(&mut req);
        let sink = req.release_sink();
        if failed {
            return (Some(1), sink);
        }
        match self.started(&host, sink) {
            (Started::Serving, sink) => (None, sink),
            (Started::Refused(status), sink) => (Some(status), sink),
        }
    }

    /// How the engine's startup finished, which the first request's host
    /// finishes: the `zend_post_startup_cb` chain (`php_module_startup`)
    /// entered from the top ([`crate::api::abi::chain::PostStartup`]),
    /// whose answer serves, refuses or names a file to preload. What the
    /// extensions wrote goes to
    /// `sink` before anything the first request writes, and `sink` is
    /// handed back. One thread finishes it, and a thread that asks while
    /// it does waits for its answer.
    pub(crate) fn started(&self, first: &RequestDescription, sink: Lent) -> (Started, Lent) {
        if let Some(started) = self.shared.started.get() {
            return (*started, sink);
        }
        let _finishing =
            (self.shared.starting.lock()).unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(started) = self.shared.started.get() {
            return (*started, sink);
        }
        let (started, sink) = self.finish_startup(first, sink);
        self.shared.started.set(started).ok();
        (started, sink)
    }

    fn finish_startup(&self, first: &RequestDescription, sink: Lent) -> (Started, Lent) {
        let links = self.shared.supplied.post_startup.links();
        if links.is_empty() {
            return (Started::Serving, sink);
        }
        // `accel_finish_startup_preload`'s request: the host's, with no
        // program, no arguments and no server variables of its own
        // (`sapi_module.register_server_variables` is `NULL` for it).
        let description = || RequestDescription {
            env: first.env.clone(),
            cwd: first.cwd.clone(),
            sapi_name: first.sapi_name.clone(),
            ..RequestDescription::default()
        };
        let mut req = Request::lent(Arc::clone(&self.shared), description(), sink);
        let answer = Beneath::<chain::PostStartup>::top(links, ()).serve(&mut req);
        match answer {
            PostStartup::Serve => (Started::Serving, req.release_sink()),
            PostStartup::Refuse { status } => (Started::Refused(status), req.release_sink()),
            PostStartup::Preload {
                script,
                thread: None,
            } => self.preload(req, &script, false),
            PostStartup::Preload {
                script,
                thread: Some(thread),
            } => self.preload_on(thread, description(), req.release_sink(), script),
        }
    }

    /// `accel_finish_startup`'s parent and its child
    /// (`Z/ext/opcache/ZendAccelerator.c:5079-5106`): the preload served
    /// in a request of its own on a thread of its own, with the stack
    /// this thread has, as a forked child has its parent's, once that
    /// thread took what `thread` takes, and this thread waiting for it
    /// as the parent `waitpid`s. Where the take fails the file is never
    /// served and the startup fails with status 1, as the child's
    /// `exit(1)` fails it; where the system refuses the thread it fails
    /// as php's refused `fork()` does, a fatal of the engine's own with
    /// the status `ACCEL_LOG_FATAL` ends the process with.
    fn preload_on(
        &self,
        thread: PreloadThread,
        description: RequestDescription,
        sink: Lent,
        script: Vec<u8>,
    ) -> (Started, Lent) {
        let stack = php_heap::call_stack_bounds()
            .map_or_else(php_heap::call_stack_default_size, |stack| stack.max_size);
        let engine = self.clone();
        let ran = crate::fork::run_joined(
            "php-preload",
            stack,
            (description, sink),
            move |(description, sink)| {
                let mut req = Request::lent(Arc::clone(&engine.shared), description, sink);
                if thread.take(&mut req) {
                    engine.preload(req, &script, true)
                } else {
                    (Started::Refused(1), req.release_sink())
                }
            },
        );
        match ran {
            Ok(Ok(finished)) => finished,
            Ok(Err(panicked)) => std::panic::resume_unwind(panicked),
            Err((refused, (description, sink))) => {
                let mut req = Request::lent(Arc::clone(&self.shared), description, sink);
                req.note_startup_error(
                    crate::api::raising::Level::CoreError,
                    format!("Preloading failed to start its thread: {refused}").as_bytes(),
                );
                (Started::Refused(-2), req.release_sink())
            }
        }
    }

    /// `accel_preload` over the file at `script`: the request started
    /// as php starts it, the file compiled as a `require` and served,
    /// and, where it neither failed nor exited, the program's values
    /// given up, its classes linked by `preload_link` and the tables it
    /// left published for every request, each file it compiled told as
    /// cached; then the request shut down. A file that fails fails the
    /// startup, and the process ends with status 1, as php's does.
    /// `in_child` is php's `ZEND_COMPILE_PRELOAD_IN_CHILD`: the request
    /// is served on the thread that took `opcache.preload_user`'s
    /// credentials.
    fn preload(&self, mut req: Request, script: &[u8], in_child: bool) -> (Started, Lent) {
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
        req.begin_preload(in_child);
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
        let (_, _, sink) = self.shutdown_request(&mut req);
        if failed {
            (Started::Refused(1), sink)
        } else {
            (Started::Serving, sink)
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::sync::Mutex;
    use std::thread::ThreadId;

    use php_unit::decl::{IniDecl, Name};

    use crate::api::abi::{
        Admission, Extension, ExtensionBuilder, Lookup, PostStartup, PreloadThread, Validation,
    };
    use crate::engine::Engine;
    use crate::test::Capture;

    /// Where each step stood: the thread a take ran on, and the thread
    /// each diagnostic was raised on, with whether its request was the
    /// preload's and the child's, each under the preload file its engine
    /// names.
    static SEEN: Mutex<Vec<(String, ThreadId, bool, bool)>> = Mutex::new(Vec::new());

    fn seen(req: &crate::api::request::Request, what: &str) {
        let script = String::from_utf8_lossy(req.ini_get(b"t.preload").unwrap_or_default());
        let mut seen = SEEN
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        seen.push((
            format!("{what} {script}"),
            std::thread::current().id(),
            req.preloading(),
            req.preloading_as_user(),
        ));
    }

    /// An extension that preloads the file its `t.preload` names on a
    /// thread of its own whose take answers `t.take`, noting where it
    /// took it and where each diagnostic was raised.
    fn preloading_on_a_thread(tag: &str) -> Extension {
        let entry = |name: &'static [u8]| IniDecl {
            name: Name(name.into()),
            default: Some(b""[..].into()),
            user: false,
            per_dir: false,
            system: true,
        };
        ExtensionBuilder::default()
            .ini(vec![entry(b"t.preload"), entry(b"t.take")])
            .admission(|_, _| Admission::Held {
                validation: Validation::Never,
                lookup: Lookup::Resolved,
                protection: 0,
                file_cache: None,
            })
            .error(|req, beneath| {
                seen(req, "raised");
                beneath.serve(req);
            })
            .post_startup(|req, _| PostStartup::Preload {
                script: req.ini_get(b"t.preload").unwrap_or_default().to_vec(),
                thread: Some(PreloadThread::new(|req| {
                    seen(req, "take");
                    req.write_stderr(b"taken\n");
                    req.ini_get(b"t.take") == Some(&b"1"[..])
                })),
            })
            .build(tag)
    }

    fn served(dir: &Path, take: &str, preload: &str, program: &str) -> Vec<crate::test::Served> {
        std::fs::create_dir_all(dir).ok();
        let script = dir.join("preload.inc");
        std::fs::write(&script, preload).ok();
        std::fs::write(dir.join("after.inc"), "<?php echo \"after\\n\";").ok();
        let engine = Engine::cli_builder()
            .extensions(&[preloading_on_a_thread(&dir.to_string_lossy())])
            .ini("t.preload", script.as_os_str().as_encoded_bytes())
            .ini("t.take", take)
            .build();
        let program = program.replace("DIR", &dir.to_string_lossy());
        let first = engine.serve_capture(&program, Capture::default());
        let second = engine.serve_capture(&program, Capture::default());
        std::fs::remove_dir_all(dir).ok();
        vec![first, second]
    }

    /// The take runs on a thread of its own before the preload's file is
    /// compiled there, in the request php's child serves it in; the
    /// requests after are served on the host's thread, outside it.
    #[test]
    fn a_preload_that_takes_its_thread_is_served_on_it_and_no_request_after_is() {
        let dir =
            std::env::temp_dir().join(format!("engine-preload-thread-{}", std::process::id()));
        let preload = std::env::temp_dir().canonicalize().unwrap_or_default();
        let preload = preload
            .join(dir.file_name().unwrap_or_default())
            .join("preload.inc");
        let preload = preload.display();
        let served = served(
            &dir,
            "1",
            "<?php echo \"preloading\\n\", $u; function on_thread() { return 'f'; }",
            "<?php echo on_thread(), \"\\n\", $v; include 'DIR/after.inc';",
        );
        let here = std::thread::current().id();
        let seen: Vec<_> = SEEN
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .iter()
            .filter(|(what, ..)| what.contains(&*dir.to_string_lossy()))
            .map(|(what, thread, preload, child)| {
                (
                    what.split(' ').next().unwrap_or("").to_owned(),
                    *thread == here,
                    *preload,
                    *child,
                )
            })
            .collect();
        assert_eq!(
            seen,
            [
                ("take".to_owned(), false, false, false),
                ("raised".to_owned(), false, true, true),
                ("raised".to_owned(), true, false, false),
                ("raised".to_owned(), true, false, false),
            ],
            "the take and the preload on a thread of their own, as the child; each request after on the host's"
        );
        let after = "f\n\nWarning: Undefined variable $v in Standard input code on line 1\nafter\n";
        assert_eq!(
            served
                .iter()
                .map(|s| String::from_utf8_lossy(&s.stdout).into_owned())
                .collect::<Vec<_>>(),
            [
                format!(
                    "preloading\n\nWarning: Undefined variable $u in {preload} on line 1\n{after}"
                ),
                after.to_owned()
            ]
        );
        assert_eq!(
            (served[0].stderr.as_slice(), served[0].exit_code),
            (&b"taken\n"[..], 0)
        );
    }

    /// A take that fails ends the startup as the child's `exit(1)` does:
    /// the file is never served, and every request ends with status 1;
    /// a file that exits on the thread fails it the same way.
    #[test]
    fn a_failed_take_or_an_exit_on_the_thread_fails_the_startup_with_status_one() {
        let base =
            std::env::temp_dir().join(format!("engine-preload-refused-{}", std::process::id()));
        let refused = served(
            &base.join("take"),
            "0",
            "<?php echo \"preloading\\n\";",
            "<?php echo 1;",
        );
        let exited = served(
            &base.join("exit"),
            "1",
            "<?php echo \"preloading\\n\"; exit(0);",
            "<?php echo 1;",
        );
        std::fs::remove_dir_all(&base).ok();
        assert_eq!(
            (
                refused[0].stdout.as_slice(),
                refused[0].stderr.as_slice(),
                refused[0].exit_code
            ),
            (&b""[..], &b"taken\n"[..], 1)
        );
        assert_eq!(
            (refused[1].stdout.as_slice(), refused[1].exit_code),
            (&b""[..], 1)
        );
        assert_eq!(
            (exited[0].stdout.as_slice(), exited[0].exit_code),
            (&b"preloading\n"[..], 1)
        );
        assert_eq!(exited[1].exit_code, 1);
    }

    /// A preload file the thread may not open is php's warning with the
    /// open's own error, as `php_stream_open_for_zend` renders it, then
    /// the `require`'s fatal: a directory no one but root may enter
    /// refuses any other user with "Permission denied".
    #[test]
    fn a_preload_file_the_thread_may_not_open_names_why() {
        let dir = std::env::temp_dir().join(format!("engine-preload-shut-{}", std::process::id()));
        let shut = dir.join("shut");
        std::fs::create_dir_all(&shut).ok();
        std::fs::write(shut.join("preload.inc"), "<?php echo 1;").ok();
        std::fs::set_permissions(&shut, std::os::unix::fs::PermissionsExt::from_mode(0o000)).ok();
        let engine = Engine::cli_builder()
            .extensions(&[preloading_on_a_thread(&dir.to_string_lossy())])
            .ini(
                "t.preload",
                shut.join("preload.inc").as_os_str().as_encoded_bytes(),
            )
            .ini("t.take", "1")
            .build();
        let served = engine.serve_capture("<?php echo 1;", Capture::default());
        std::fs::set_permissions(&shut, std::os::unix::fs::PermissionsExt::from_mode(0o755)).ok();
        std::fs::remove_dir_all(&dir).ok();
        let root = php_process::process::euid() == 0;
        let stdout = String::from_utf8_lossy(&served.stdout);
        assert!(
            root || stdout.starts_with("\nWarning: PHP Startup: Failed to open stream: Permission denied in Unknown on line 0\n"),
            "{stdout}"
        );
        assert_eq!(served.exit_code, i32::from(!root));
    }
}
