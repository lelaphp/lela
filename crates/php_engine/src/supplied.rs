//! What extensions supply the engine beyond their declarations: the
//! slots php's `MINIT` writes, a saved function pointer, a list it
//! appends to, a field of the host's table or a row under a key. Each
//! extension's builder takes its own into [`Supplies`]; the engine
//! resolves the bundle's once, as it is built, into one [`Resolved`]
//! table that every read site reads, each kind combined by php's rule
//! for it:
//!
//! - [`Chain`]: php's saved pointer, which each supplier overwrites and
//!   keeps as the one beneath (`zend_interrupt_function`,
//!   `zend_compile_file`, `zend_resolve_path`, `zend_post_startup_cb`,
//!   `php_rfc1867_callback`);
//! - [`Listed`]: a list each supplier appends to (`PG(tick_functions)`);
//! - [`Last`]: a slot the last supplier writes (`sapi_module`'s
//!   `treat_data`, `input_filter` and `log_message`, the multibyte
//!   functions, the CLI's shell callbacks);
//! - [`Keyed`]: rows under a key, each key's in supplier order (a
//!   native's interception, `pdo_driver_hash`).
//!
//! Suppliers are taken in the order php installs them ([`Phase`]):
//! every extension's at its `MINIT`, in the order the extensions start,
//! the host's own extension last among them, as php starts a host's
//! `additional_module` after the bundle; then every Zend extension's,
//! in that same order. [`Resolved::resolve`] is handed the start order
//! with each supplier's extension and phase beside it, so the start
//! order has one source, the caller's, and a kind added later changes
//! no read site.

use std::any::{Any, TypeId};
use std::sync::Arc;

use php_unit::decl::Name;

use crate::api::abi::{
    AdmissionHook, CachedHook, CompileFileHook, EncodingsHook, Entry, Extension, FatalLoggedHook,
    InputFilterHook, InterruptHook, PostStartupHook, ResolvePathHook, Shell, TicksHook,
    TreatDataHook, UploadHook,
};
use crate::api::request::{ExtensionId, Request};

/// A value one extension hands the extension that reads every value of
/// its type ([`ExtensionBuilder::register`](crate::api::abi::ExtensionBuilder::register)).
pub(crate) type Registered = Arc<dyn Any + Send + Sync>;

/// When php installs what an extension supplies, the phases in the
/// order `php_module_startup` runs them (`Z/main/main.c:2353-2356`):
/// every extension's `MINIT` in `zend_startup_modules`, then every Zend
/// extension's startup in `zend_startup_extensions`.
///
/// A Zend extension's phase is the extension's, every kind it installs
/// together, as opcache, the bundle's one Zend extension, installs
/// nothing at its `MINIT`, which runs among the other extensions' and
/// only registers its `zend_extension` (`start_accel_extension`,
/// `Z/ext/opcache/ZendAccelerator.c:3168-3171`;
/// `Z/ext/opcache/zend_accelerator_module.c:448-453`). It installs its
/// `zend_post_startup_cb` link at `accel_startup` (`:3220-3221`) and the
/// rest, `zend_compile_file`, `zend_resolve_path` and its three
/// interceptions, at `accel_post_startup` (`:3403-3434`): two moments,
/// both after every other extension's `MINIT`, which differ only to a
/// Zend extension started after opcache, and the bundle has none. What
/// an extension registers into another's table is registered at its
/// `MINIT` by definition, so [`Resolved::resolve`] keeps those rows in
/// start order whatever the phase.
///
/// Declared in the order php runs the phases, which the sort reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Phase {
    /// An extension's `MINIT`.
    Minit,
    /// A Zend extension's startup, and its `zend_post_startup_cb` after.
    ZendExtension,
}

impl Phase {
    /// The phase `extension` installs what it supplies in.
    pub(crate) fn of(extension: &Extension) -> Phase {
        if extension.zend.is_some() {
            Phase::ZendExtension
        } else {
            Phase::Minit
        }
    }
}

/// One extension's supplied behaviour as its builder took it, each
/// slot what the builder method of its name was handed.
#[derive(Clone, Default)]
pub(crate) struct Supplies {
    pub(crate) interrupt: Option<InterruptHook>,
    pub(crate) compile_file: Option<CompileFileHook>,
    pub(crate) resolve_path: Option<ResolvePathHook>,
    pub(crate) post_startup: Option<PostStartupHook>,
    pub(crate) upload: Option<UploadHook>,
    pub(crate) ticks: Option<TicksHook>,
    pub(crate) cached: Option<CachedHook>,
    pub(crate) program_ended: Option<fn(&mut Request)>,
    pub(crate) admission: Option<AdmissionHook>,
    pub(crate) fatal_logged: Option<FatalLoggedHook>,
    pub(crate) shell: Option<Shell>,
    pub(crate) input_filter: Option<InputFilterHook>,
    pub(crate) treat_data: Option<TreatDataHook>,
    pub(crate) encodings: Option<EncodingsHook>,
    pub(crate) intercepts: Vec<(Name, Entry)>,
    pub(crate) registered: Vec<(TypeId, Registered)>,
}

/// The bundle's supplied behaviour, resolved once as the engine is
/// built: each kind as php's rule for it combines every supplier.
#[derive(Default)]
pub(crate) struct Resolved {
    /// `zend_interrupt_function`, served from the top, every link, as
    /// pcntl's dispatches and then calls the one it saved
    /// (`Z/ext/pcntl/pcntl.c:1922-1927`).
    pub(crate) interrupt: Chain<InterruptHook>,
    /// `zend_compile_file`, from the top until a link answers, as
    /// phar's falls through to `phar_orig_compile_file` for a file it
    /// does not serve (`Z/ext/phar/phar.c:3248-3256`).
    pub(crate) compile_file: Chain<CompileFileHook>,
    /// `zend_resolve_path`, from the top until a link answers, as
    /// phar's falls through to `phar_save_resolve_path`
    /// (`Z/ext/phar/phar.c:3239-3246`).
    pub(crate) resolve_path: Chain<ResolvePathHook>,
    /// `zend_post_startup_cb`, from the bottom until a link does not
    /// serve, as opcache's calls the one it saved before its own work
    /// and fails where it failed (`Z/ext/opcache/ZendAccelerator.c:3234-3241`);
    /// ffi's serves its own first (`Z/ext/ffi/ffi.c:5461-5476`), work
    /// that cannot fail, so one walk is both links'.
    pub(crate) post_startup: Chain<PostStartupHook>,
    /// `php_rfc1867_callback`, from the bottom, every link, as session's
    /// calls the one it saved first and keeps its answer
    /// (`Z/ext/session/session.c:3174-3176`).
    pub(crate) upload: Chain<UploadHook>,
    /// `PG(tick_functions)`, as `php_add_tick_function` appends and
    /// `php_run_ticks` walks it (`Z/main/php_ticks.c:48-52`, `:66-69`).
    pub(crate) ticks: Listed<TicksHook>,
    /// Told of each file the unit cache holds.
    pub(crate) cached: Listed<CachedHook>,
    /// Served once the primary program has ended.
    pub(crate) program_ended: Listed<fn(&mut Request)>,
    /// The unit cache's one question, which opcache's
    /// `persistent_compile_file` answers over whatever it saved, so the
    /// last supplier's alone is asked.
    pub(crate) admission: Last<AdmissionHook>,
    /// `sapi_module.log_message`, the host's slot.
    pub(crate) fatal_logged: Last<FatalLoggedHook>,
    /// `cli_shell_callbacks`, as `readline_cli.c`'s `MINIT` writes the
    /// table; empty under a host that carries none.
    pub(crate) shell: Last<Shell>,
    /// `sapi_module.input_filter`, as `sapi_register_input_filter`
    /// overwrites it (`Z/main/SAPI.c:1009`).
    pub(crate) input_filter: Last<InputFilterHook>,
    /// `sapi_module.treat_data`, as `sapi_register_treat_data`
    /// overwrites it (`Z/main/SAPI.c:1000`).
    pub(crate) treat_data: Last<TreatDataHook>,
    /// `zend_multibyte_functions`, as `zend_multibyte_set_functions`
    /// overwrites them.
    pub(crate) encodings: Last<EncodingsHook>,
    /// Each native intercepted, by its name, each name's entries
    /// stacked later over earlier, as opcache saves its handler at
    /// `accel_post_startup` over phar's of its `MINIT`.
    pub(crate) intercepts: Keyed<Name, Entry>,
    /// What each `MINIT` registered into another extension's table, by
    /// the value's type.
    pub(crate) registered: Keyed<TypeId, Registered>,
}

impl Resolved {
    /// The bundle's supplied behaviour from each supplier's own, handed
    /// in the order the extensions start, each beside the extension it
    /// is of and the phase it installs in: installed phase by phase,
    /// each phase's in start order ([`Phase`]), and what each `MINIT`
    /// registered in start order alone.
    pub(crate) fn resolve(
        suppliers: impl IntoIterator<Item = (ExtensionId, Phase, Supplies)>,
    ) -> Resolved {
        let mut table = Resolved::default();
        let mut suppliers: Vec<_> = suppliers.into_iter().collect();
        for (_, _, own) in &mut suppliers {
            table
                .registered
                .install(std::mem::take(&mut own.registered));
        }
        suppliers.sort_by_key(|&(_, phase, _)| phase);
        for (by, _, own) in suppliers {
            table.interrupt.install(own.interrupt);
            table.compile_file.install(own.compile_file);
            table.resolve_path.install(own.resolve_path);
            table.post_startup.install(own.post_startup);
            table.upload.install(own.upload);
            table.ticks.install(own.ticks);
            table.cached.install(own.cached);
            table.program_ended.install(own.program_ended);
            table.admission.install(by, own.admission);
            table.fatal_logged.install(by, own.fatal_logged);
            table.shell.install(by, own.shell);
            table.input_filter.install(by, own.input_filter);
            table.treat_data.install(by, own.treat_data);
            table.encodings.install(by, own.encodings);
            table.intercepts.install(own.intercepts);
        }
        table
    }
}

/// php's saved pointer: each supplier installs over the one before and
/// keeps it as the one beneath. Which end a walk starts from is what
/// php's links do with the one beneath: a link that serves before
/// calling it is walked from the top, one that calls it first from the
/// bottom.
pub(crate) struct Chain<H>(Vec<H>);

impl<H: Copy> Chain<H> {
    fn install(&mut self, hook: Option<H>) {
        self.0.extend(hook);
    }

    /// The links as php enters the chain: the last supplier's first.
    pub(crate) fn top_down(&self) -> impl Iterator<Item = H> + '_ {
        self.0.iter().rev().copied()
    }

    /// The links as their work is done where each calls the one beneath
    /// before its own: the first supplier's first.
    pub(crate) fn bottom_up(&self) -> impl Iterator<Item = H> + '_ {
        self.0.iter().copied()
    }
}

/// A list each supplier appends to, served in supplier order.
pub(crate) struct Listed<H>(Vec<H>);

impl<H: Copy> Listed<H> {
    fn install(&mut self, hook: Option<H>) {
        self.0.extend(hook);
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = H> + '_ {
        self.0.iter().copied()
    }
}

/// A slot each supplier overwrites: the last one's, with the extension
/// that wrote it.
pub(crate) struct Last<H>(Option<(ExtensionId, H)>);

impl<H: Copy> Last<H> {
    fn install(&mut self, by: ExtensionId, hook: Option<H>) {
        if let Some(hook) = hook {
            self.0 = Some((by, hook));
        }
    }

    pub(crate) fn get(&self) -> Option<H> {
        self.0.map(|(_, hook)| hook)
    }

    /// The extension whose supplier stands in the slot.
    pub(crate) fn supplier(&self) -> Option<ExtensionId> {
        self.0.map(|(by, _)| by)
    }
}

/// Rows under a key, every supplier's in supplier order.
pub(crate) struct Keyed<K, H>(Vec<(K, H)>);

impl<K: PartialEq, H> Keyed<K, H> {
    fn install(&mut self, rows: Vec<(K, H)>) {
        self.0.extend(rows);
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &(K, H)> {
        self.0.iter()
    }

    /// The rows under `key`, in supplier order.
    pub(crate) fn under(&self, key: K) -> impl Iterator<Item = &H> {
        self.0
            .iter()
            .filter(move |(k, _)| *k == key)
            .map(|(_, h)| h)
    }
}

impl<H> Default for Chain<H> {
    fn default() -> Self {
        Chain(Vec::new())
    }
}

impl<H> Default for Listed<H> {
    fn default() -> Self {
        Listed(Vec::new())
    }
}

impl<H> Default for Last<H> {
    fn default() -> Self {
        Last(None)
    }
}

impl<K, H> Default for Keyed<K, H> {
    fn default() -> Self {
        Keyed(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use crate::api::abi::{ExtensionBuilder, TreatDataHook};
    use crate::api::request::{Input, Request};
    use crate::{Engine, RequestDescription, WebRequest};

    /// What the hooks of one test were served, in the order served.
    type Served = Mutex<Vec<&'static str>>;

    fn served(log: &Served) -> Vec<&'static str> {
        log.lock().map(|log| log.clone()).unwrap_or_default()
    }

    /// A chained kind, `zend_interrupt_function`: the later extension's
    /// hook is the top of the chain and is served first, and the one
    /// beneath is served after it, as pcntl's `pcntl_interrupt_function`
    /// dispatches its signals and then calls `orig_interrupt_function`
    /// (`Z/ext/pcntl/pcntl.c:1922-1927`).
    #[test]
    fn a_chained_kind_serves_the_later_supplier_over_the_one_beneath() {
        static LOG: Served = Mutex::new(Vec::new());
        fn note(name: &'static str) {
            if let Ok(mut log) = LOG.lock() {
                log.push(name);
            }
        }
        let extensions = [
            ExtensionBuilder::default()
                .interrupt(|_| {
                    note("earlier");
                    Ok(())
                })
                .build("earlier"),
            ExtensionBuilder::default()
                .interrupt(|_| {
                    note("later");
                    Ok(())
                })
                .build("later"),
        ];
        let engine = Engine::cli_builder().extensions(&extensions).build();
        let mut req = Request::new(Arc::clone(&engine.shared), RequestDescription::default());
        req.interrupt().raise();
        assert_eq!(crate::interrupt::check(&mut req), Ok(false));
        assert_eq!(served(&LOG), ["later", "earlier"]);
    }

    /// A chained kind whose link answers or passes, `zend_resolve_path`
    /// as phar's `phar_resolve_path` falls through to the one it saved
    /// (`Z/ext/phar/phar.c:3239-3246`): the later extension answers
    /// what it takes, and what it leaves goes to the one beneath.
    #[test]
    fn a_chained_kind_falls_through_to_the_one_beneath() {
        let extensions = [
            ExtensionBuilder::default()
                .resolve_path(|_, _| Ok(Some(b"/beneath".to_vec())))
                .build("earlier"),
            ExtensionBuilder::default()
                .resolve_path(|_, written| Ok((written == b"a").then(|| b"/above".to_vec())))
                .build("later"),
        ];
        let engine = Engine::cli_builder().extensions(&extensions).build();
        let mut req = Request::new(Arc::clone(&engine.shared), RequestDescription::default());
        assert_eq!(req.resolve_path(b"a").ok(), Some(b"/above".to_vec()));
        assert_eq!(req.resolve_path(b"z").ok(), Some(b"/beneath".to_vec()));
    }

    /// A listed kind, `PG(tick_functions)` as `php_add_tick_function`
    /// appends to it and `php_run_ticks` walks it
    /// (`Z/main/php_ticks.c:48-69`): every supplier's hook, in the order the
    /// extensions started.
    #[test]
    fn a_listed_kind_serves_every_supplier_in_start_order() {
        let extensions = [
            ExtensionBuilder::default()
                .ticks(|req| {
                    req.write(b"earlier ");
                    Ok(())
                })
                .build("earlier"),
            ExtensionBuilder::default()
                .ticks(|req| {
                    req.write(b"later ");
                    Ok(())
                })
                .build("later"),
        ];
        let served = Engine::cli_builder()
            .extensions(&extensions)
            .build()
            .serve_capture(
                "<?php declare(ticks=1); echo '| ';",
                crate::test::Capture::default(),
            );
        assert_eq!(
            String::from_utf8_lossy(&served.stdout),
            "earlier later | earlier later "
        );
    }

    /// The treater that upper-cases the query string, and the one that
    /// reverses it.
    const UPPER: TreatDataHook = |_, input, source| {
        Ok((input == Input::Get).then(|| vec![(b"q".to_vec(), source.to_ascii_uppercase())]))
    };
    const REVERSE: TreatDataHook = |_, input, source| {
        Ok((input == Input::Get)
            .then(|| vec![(b"q".to_vec(), source.iter().rev().copied().collect())]))
    };

    /// `$_GET['q']` as an engine of `extensions`, with `host` as the
    /// host's own extension, registers it from the query `ab`.
    fn treated(
        extensions: &[crate::api::abi::Extension],
        host: Option<crate::api::abi::Extension>,
    ) -> String {
        let mut builder = Engine::cli_builder().extensions(extensions);
        if let Some(host) = host {
            builder = builder.additional_extension(host);
        }
        let mut sink = crate::test::Capture::default();
        builder.build().serve(
            RequestDescription {
                web: Some(WebRequest {
                    query: Some(b"ab".to_vec()),
                    ..WebRequest::default()
                }),
                sapi_name: String::from("cgi-fcgi"),
                script_name: Some(b"witness.php".to_vec()),
                source: Some(b"<?php echo $_GET['q'];".to_vec()),
                ..RequestDescription::default()
            },
            &mut sink,
        );
        String::from_utf8_lossy(&sink.stdout).into_owned()
    }

    /// A kind replaced by the last, `sapi_module.treat_data` as
    /// `sapi_register_treat_data` overwrites it (`Z/main/SAPI.c:1000`):
    /// the later extension's treater is the one served, and the earlier
    /// is never asked.
    #[test]
    fn a_replaced_kind_serves_the_last_supplier_alone() {
        let extensions = [
            ExtensionBuilder::default()
                .treat_data(UPPER)
                .build("earlier"),
            ExtensionBuilder::default()
                .treat_data(REVERSE)
                .build("later"),
        ];
        assert_eq!(treated(&extensions, None), "ba");
    }

    /// A replaced slot names the extension that wrote it by its
    /// `module_number`, as `multibyte_provider` answers mbstring's for
    /// phpinfo's `Zend Multibyte Support` row.
    #[test]
    fn a_replaced_kind_names_its_supplier_at_its_module_number() {
        let extensions = [
            ExtensionBuilder::default()
                .treat_data(UPPER)
                .build("earlier"),
            ExtensionBuilder::default()
                .treat_data(REVERSE)
                .build("later"),
            ExtensionBuilder::default().build("after"),
        ];
        let engine = Engine::cli_builder().extensions(&extensions).build();
        let req = Request::new(Arc::clone(&engine.shared), RequestDescription::default());
        let supplier = engine.shared.supplied.treat_data.supplier();
        assert_eq!(
            supplier.and_then(|id| req.extension_name(id)),
            Some("later")
        );
    }

    /// The host's own extension starts after the bundle, as php starts
    /// its `additional_module` last, so what it supplies replaces what a
    /// bundle extension supplied of the same kind.
    #[test]
    fn the_host_s_extension_resolves_last() {
        let bundle = [ExtensionBuilder::default().treat_data(UPPER).build("zzz")];
        let host = ExtensionBuilder::default().treat_data(REVERSE).build("aaa");
        assert_eq!(treated(&bundle, Some(host)), "ba");
    }

    /// A keyed kind, what `MINIT` registers into another extension's
    /// table, as `php_pdo_register_driver` adds to `pdo_driver_hash`:
    /// each key holds every supplier's values in start order, and one
    /// key's values are not another's.
    #[test]
    fn a_keyed_kind_holds_each_key_s_suppliers_in_start_order() {
        #[derive(Debug, PartialEq)]
        struct Driver(&'static str);
        #[derive(Debug, PartialEq)]
        struct Other(u8);
        let extensions = [
            ExtensionBuilder::default()
                .register(Driver("earlier"))
                .register(Other(1))
                .build("earlier"),
            ExtensionBuilder::default()
                .register(Driver("later"))
                .build("later"),
        ];
        let engine = Engine::cli_builder().extensions(&extensions).build();
        let req = Request::new(Arc::clone(&engine.shared), RequestDescription::default());
        let drivers: Vec<_> = req.registered::<Driver>().collect();
        assert_eq!(drivers, [&Driver("earlier"), &Driver("later")]);
        let others: Vec<_> = req.registered::<Other>().collect();
        assert_eq!(others, [&Other(1)]);
    }

    /// `builder` built as a Zend extension too, opcache's shape.
    fn zend(builder: ExtensionBuilder, name: &str) -> crate::api::abi::Extension {
        builder
            .zend_extension(crate::api::abi::ZendExtension {
                name: name.to_owned(),
                version: String::new(),
                author: String::new(),
                url: String::new(),
                copyright: String::new(),
            })
            .build(name)
    }

    /// A Zend extension's interception stands over another extension's
    /// whatever their bundle order, as opcache's `file_exists`,
    /// installed at `accel_post_startup`
    /// (`Z/ext/opcache/ZendAccelerator.c:3434`), is saved over phar's,
    /// installed at phar's `MINIT`: the Zend extension serves first and
    /// passes through to the other's.
    #[test]
    fn a_zend_extension_s_interception_stands_over_a_minit_s_in_either_order() {
        let opcache: crate::api::abi::Entry = |req| {
            req.write(b"opcache ");
            crate::api::call::passthrough(req, None)
        };
        let phar: crate::api::abi::Entry = |req| {
            req.write(b"phar ");
            crate::api::call::passthrough(req, None)
        };
        let serve = |extensions: &[crate::api::abi::Extension]| {
            let served = Engine::cli_builder()
                .extensions(extensions)
                .build()
                .serve_capture(
                    "<?php $f = 'strlen'; echo $f('abcd');",
                    crate::test::Capture::default(),
                );
            String::from_utf8_lossy(&served.stdout).into_owned()
        };
        let mut extensions = [
            zend(
                ExtensionBuilder::default().intercept(b"strlen", opcache),
                "opcache",
            ),
            ExtensionBuilder::default()
                .intercept(b"strlen", phar)
                .build("phar"),
        ];
        assert_eq!(serve(&extensions), "opcache phar 4");
        extensions.reverse();
        assert_eq!(serve(&extensions), "opcache phar 4");
    }

    /// What a Zend extension registers is its `MINIT`'s, which runs
    /// among the other extensions', so its rows keep their start order
    /// though what it installs moves after every other extension's.
    #[test]
    fn a_zend_extension_s_registrations_keep_their_start_order() {
        #[derive(Debug, PartialEq)]
        struct Driver(&'static str);
        let extensions = [
            zend(ExtensionBuilder::default().register(Driver("zend")), "zend"),
            ExtensionBuilder::default()
                .register(Driver("minit"))
                .build("minit"),
        ];
        let engine = Engine::cli_builder().extensions(&extensions).build();
        let req = Request::new(Arc::clone(&engine.shared), RequestDescription::default());
        let drivers: Vec<_> = req.registered::<Driver>().collect();
        assert_eq!(drivers, [&Driver("zend"), &Driver("minit")]);
    }

    /// A Zend extension installs its `zend_post_startup_cb` link after
    /// every other extension's `MINIT`, as opcache's `accel_startup`
    /// saves ffi's (`Z/ext/opcache/ZendAccelerator.c:3220-3221`), so
    /// ffi's link is beneath it and asked first: its refusal ends
    /// startup and the Zend extension's preload is never asked for,
    /// though it stands first in the bundle (#1797).
    #[test]
    fn a_minit_s_post_startup_link_is_asked_before_a_zend_extension_s() {
        let extensions = [
            zend(
                ExtensionBuilder::default().post_startup(|req| {
                    req.write_stderr(b"preloading\n");
                    crate::api::abi::PostStartup::Preload(b"/nowhere".to_vec())
                }),
                "opcache",
            ),
            ExtensionBuilder::default()
                .post_startup(|req| {
                    req.write_stderr(b"refused\n");
                    crate::api::abi::PostStartup::Refuse { status: -2 }
                })
                .build("ffi"),
        ];
        let served = Engine::cli_builder()
            .extensions(&extensions)
            .build()
            .serve_capture("<?php echo 1;", crate::test::Capture::default());
        assert_eq!(
            (
                served.stdout.as_slice(),
                served.stderr.as_slice(),
                served.exit_code
            ),
            (&b""[..], &b"refused\n"[..], -2)
        );
    }
}
