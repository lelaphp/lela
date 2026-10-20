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
//!   `zend_compile_file`, `zend_compile_string`, `zend_resolve_path`,
//!   `zend_error_cb`, `gc_collect_cycles`, `zend_throw_exception_hook`,
//!   `zend_post_startup_cb`, `php_rfc1867_callback`,
//!   `zend_user_opcode_handlers`), every one entered from the top with
//!   each link handed the ones beneath ([`chain`]);
//! - [`Listed`]: a list each supplier appends to (`PG(tick_functions)`,
//!   `zend_observers_fcall_list`, the observer's three notice lists and
//!   its three fiber lists, the statement handlers of `zend_extensions`);
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
//! in that same order. [`Suppliers`] holds the start order with each
//! supplier's extension and phase beside it, so the start order has one
//! source, the caller's, and a kind added later changes no read site;
//! a start that refuses resolves it again with what php had installed
//! by then ([`Suppliers::refused_at`]).

use std::any::{Any, TypeId};
use std::sync::Arc;

use php_unit::PublishedUnit;
use php_unit::decl::Name;

use crate::api::abi::{
    AdmissionHook, CachedHook, CompileFileHook, CompileStringHook, EncodingsHook, Entry, ErrorHook,
    Extension, FatalLoggedHook, FiberHandler, FiberSwitchHandler, GcCollectHook, InputFilterHook,
    InterruptHook, ObserverInit, OptimizerPass, PostStartupHook, ResolvePathHook, Shell,
    ThrownHook, TicksHook, TreatDataHook, UploadHook,
};
use crate::api::request::{ExtensionId, Request};
use crate::layer::StreamHooks;
use crate::observer::{ClassLinked, ErrorObserver, FunctionDeclared, OpcodeObserver};
use crate::statement::StatementHandler;

pub(crate) mod chain;

pub use chain::{Beneath, ChainKind};

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
enum Phase {
    /// An extension's `MINIT`.
    Minit,
    /// A Zend extension's startup, and its `zend_post_startup_cb` after.
    ZendExtension,
}

impl Phase {
    /// The phase `extension` installs what it supplies in.
    fn of(extension: &Extension) -> Phase {
        if extension.entry.zend.is_some() {
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
    pub(crate) compile_string: Option<CompileStringHook>,
    pub(crate) error: Option<ErrorHook>,
    pub(crate) gc_collect: Option<GcCollectHook>,
    pub(crate) thrown: Option<ThrownHook>,
    pub(crate) resolve_path: Option<ResolvePathHook>,
    pub(crate) stream_hooks: Option<StreamHooks>,
    pub(crate) post_startup: Option<PostStartupHook>,
    pub(crate) upload: Option<UploadHook>,
    pub(crate) ticks: Option<TicksHook>,
    pub(crate) cached: Option<CachedHook>,
    pub(crate) optimizer_passes: Vec<OptimizerPass>,
    pub(crate) program_ended: Option<fn(&mut Request)>,
    pub(crate) admission: Option<AdmissionHook>,
    pub(crate) fatal_logged: Option<FatalLoggedHook>,
    pub(crate) shell: Option<Shell>,
    pub(crate) input_filter: Option<InputFilterHook>,
    pub(crate) treat_data: Option<TreatDataHook>,
    pub(crate) encodings: Option<EncodingsHook>,
    pub(crate) intercepts: Vec<(Name, Entry)>,
    pub(crate) registered: Vec<(TypeId, Registered)>,
    pub(crate) observers: Vec<ObserverInit>,
    pub(crate) error_observers: Vec<ErrorObserver>,
    pub(crate) functions_declared: Vec<FunctionDeclared>,
    pub(crate) classes_linked: Vec<ClassLinked>,
    pub(crate) fiber_init: Vec<FiberHandler>,
    pub(crate) fiber_switch: Vec<FiberSwitchHandler>,
    pub(crate) fiber_destroy: Vec<FiberHandler>,
    pub(crate) opcodes: Option<OpcodeObserver>,
    /// The Zend extension row's `statement_handler`, which
    /// [`ExtensionBuilder::build`](crate::api::abi::ExtensionBuilder::build)
    /// takes from it.
    pub(crate) statement_handler: Option<StatementHandler>,
}

/// Every extension's supplies as its builder took them, in start order,
/// each beside the extension it is of and the phase it installs in: what
/// [`Resolved::resolve`] is handed, kept until the engine has started, so
/// a start that refuses leaves installed only what php's would have.
pub(crate) struct Suppliers(Vec<(ExtensionId, Phase, Supplies)>);

impl Suppliers {
    /// The suppliers of `extensions`, each taken out of its extension.
    pub(crate) fn take<'a>(
        extensions: impl IntoIterator<Item = (ExtensionId, &'a mut Extension)>,
    ) -> Suppliers {
        Suppliers(
            extensions
                .into_iter()
                .map(|(by, e)| (by, Phase::of(e), std::mem::take(&mut e.entry.supplies)))
                .collect(),
        )
    }

    /// The CLI's `cli_shell_callbacks` left out: a host that carries no
    /// such table has no slot for readline's `MINIT` to write.
    pub(crate) fn without_shell(mut self) -> Suppliers {
        for (_, _, own) in &mut self.0 {
            own.shell = None;
        }
        self
    }

    /// Every supplier's, as an engine that starts installs them.
    pub(crate) fn resolved(&self) -> Resolved {
        Resolved::resolve(self.0.iter().cloned())
    }

    /// What an engine whose start `refused` refused has installed: each
    /// link an extension's `MINIT` installs, from the extensions started
    /// before it alone, since `zend_startup_module_ex` ends the startup
    /// there and no later `MINIT` and no Zend extension's startup runs;
    /// and the refused one's none, its start having failed. The host's
    /// own slots stand whatever started: `sapi_module.log_message` is the
    /// host's table and `zend_stream_open_function` main's, both set
    /// before any extension starts.
    pub(crate) fn refused_at(self, refused: ExtensionId) -> Resolved {
        Resolved::resolve(self.0.into_iter().map(|(by, phase, own)| {
            if phase == Phase::Minit && by.0 < refused.0 {
                return (by, phase, own);
            }
            let host = Supplies {
                fatal_logged: own.fatal_logged,
                stream_hooks: own.stream_hooks,
                ..Supplies::default()
            };
            (by, phase, host)
        }))
    }
}

/// The bundle's supplied behaviour, resolved once as the engine is
/// built: each kind as php's rule for it combines every supplier.
#[derive(Default)]
pub(crate) struct Resolved {
    /// `zend_interrupt_function` ([`chain::Interrupt`]): pcntl's link
    /// dispatches and then calls the one it saved
    /// (`Z/ext/pcntl/pcntl.c:1922-1927`).
    pub(crate) interrupt: Chain<InterruptHook>,
    /// `zend_compile_file` ([`chain::CompileFile`]): phar's link falls
    /// through to `phar_orig_compile_file` for a file it does not serve
    /// and hands it an archive's stub (`Z/ext/phar/phar.c:3248-3310`),
    /// and phpdbg's keeps what was compiled (`phpdbg_compile_file`).
    pub(crate) compile_file: Chain<CompileFileHook>,
    /// `zend_compile_string` ([`chain::CompileString`]): the tracers' and
    /// the profilers' links do their work on both sides of the one they
    /// saved, as
    /// dd-trace's `zai_interceptor_compile_string` notes the tables'
    /// ends, compiles and resolves what the string declared.
    pub(crate) compile_string: Chain<CompileStringHook>,
    /// `zend_error_cb` ([`chain::Error`]): most links serve before the
    /// one they saved, as New Relic's
    /// `nr_php_error_cb` records and then calls it, but soap's
    /// `soap_error_handler` serves a client's fatal itself, throwing its
    /// fault, and wraps a server's in its own response
    /// (`Z/ext/soap/soap.c:1885-1966`).
    pub(crate) error: Chain<ErrorHook>,
    /// The `gc_collect_cycles` pointer ([`chain::GcCollect`]): a link
    /// measures the collection it serves, as xdebug's `xdebug_gc_collect_cycles` reads the status,
    /// the time and the memory before the one it saved and records the
    /// collection after it.
    pub(crate) gc_collect: Chain<GcCollectHook>,
    /// `zend_throw_exception_hook` ([`chain::Throw`]): each link serves
    /// before the one it saved, as dd-trace's
    /// `zai_interceptor_exception_hook` and the Datadog profiler's
    /// `exception_profiling_throw_exception_hook` do; xdebug's installs
    /// over whatever stood and saves none.
    pub(crate) thrown: Chain<ThrownHook>,
    /// `zend_resolve_path` ([`chain::ResolvePath`]): phar's link falls
    /// through to `phar_save_resolve_path` for a path it does not find
    /// (`Z/ext/phar/phar.c:3239-3246`).
    pub(crate) resolve_path: Chain<ResolvePathHook>,
    /// `zend_stream_open_function` and the `zend_resolve_path` beneath
    /// every override, which `php_module_startup` points at main's
    /// streams once (`Z/main/main.c`); the stream layer's, handed by the
    /// extension that registers the plain files wrapper.
    pub(crate) stream_hooks: Last<StreamHooks>,
    /// `zend_post_startup_cb` ([`chain::PostStartup`]): opcache's link
    /// calls the one it saved before its own work and fails where it
    /// failed (`Z/ext/opcache/ZendAccelerator.c:3234-3241`); ffi's serves
    /// its own first (`Z/ext/ffi/ffi.c:5461-5476`).
    pub(crate) post_startup: Chain<PostStartupHook>,
    /// `php_rfc1867_callback` ([`chain::Upload`]): session's link calls
    /// the one it saved first and keeps its answer
    /// (`Z/ext/session/session.c:3174-3176`).
    pub(crate) upload: Chain<UploadHook>,
    /// `PG(tick_functions)`, as `php_add_tick_function` appends and
    /// `php_run_ticks` walks it (`Z/main/php_ticks.c:48-52`, `:66-69`).
    pub(crate) ticks: Listed<TicksHook>,
    /// Told of each file the unit cache holds.
    pub(crate) cached: Listed<CachedHook>,
    /// `zend_optimizer_registered_passes`, as
    /// `zend_optimizer_register_pass` appends to it and
    /// `zend_optimizer_call_registered_passes` walks it
    /// (`Z/Zend/Optimizer/zend_optimizer.c:1589-1597`, `:1762-1775`).
    pub(crate) optimizer_passes: Listed<OptimizerPass>,
    /// `zend_observers_fcall_list`, as `zend_observer_fcall_register`
    /// (`Z/Zend/zend_observer.c:44`)
    /// appends to it at each `MINIT` and each Zend extension's startup,
    /// and `zend_observer_fcall_install` asks every one of a function in
    /// that order (design §4.3 seam 9, [`crate::observer`]).
    pub(crate) observers: Listed<ObserverInit>,
    /// `zend_observer_error_callbacks`, as `zend_observer_error_register`
    /// (`Z/Zend/zend_observer.c:367`) appends to it, every one told of
    /// each diagnostic in that order ([`crate::observer::error`]).
    pub(crate) error_observers: Listed<ErrorObserver>,
    /// `zend_observer_function_declared_callbacks`, as
    /// `zend_observer_function_declared_register` (`:331`) appends to it
    /// ([`crate::observer::function_declared`]).
    pub(crate) functions_declared: Listed<FunctionDeclared>,
    /// `zend_observer_class_linked_callbacks`, as
    /// `zend_observer_class_linked_register` (`:349`) appends to it
    /// ([`crate::observer::class_linked`]).
    pub(crate) classes_linked: Listed<ClassLinked>,
    /// `zend_observer_fiber_init`, `zend_observer_fiber_switch` and
    /// `zend_observer_fiber_destroy` (`Z/Zend/zend_observer.c:33-35`),
    /// each walked in the order its handlers were registered
    /// ([`crate::observer::fiber_switched`]).
    pub(crate) fiber_init: Listed<FiberHandler>,
    pub(crate) fiber_switch: Listed<FiberSwitchHandler>,
    pub(crate) fiber_destroy: Listed<FiberHandler>,
    /// `zend_user_opcode_handlers`, one handler per opcode, which an
    /// extension that sets one over another's saves and calls after its
    /// own, as xdebug's `xdebug_set_opcode_handler` does
    /// ([`chain::Opcode`]): each link declaring at engine start the
    /// opcodes it observes, which the engine's start makes the chain's
    /// links ([`crate::observer::ObservedOpcodes`]).
    pub(crate) opcodes: Chain<OpcodeObserver>,
    /// `zend_extensions` as `ZEND_EXT_STMT` walks it, each one's
    /// `statement_handler` in the order they were registered
    /// (`zend_register_extension`, `Z/Zend/zend_extensions.c:150`), which a
    /// Zend extension's phase keeps ([`crate::statement`]).
    pub(crate) statement_handlers: Listed<StatementHandler>,
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
    fn resolve(suppliers: impl IntoIterator<Item = (ExtensionId, Phase, Supplies)>) -> Resolved {
        let mut table = Resolved::default();
        let mut suppliers: Vec<_> = suppliers.into_iter().collect();
        for (by, _, own) in &mut suppliers {
            table
                .registered
                .install(*by, std::mem::take(&mut own.registered));
        }
        suppliers.sort_by_key(|&(_, phase, _)| phase);
        for (by, _, own) in suppliers {
            table.interrupt.install(by, own.interrupt);
            table.compile_file.install(by, own.compile_file);
            table.compile_string.install(by, own.compile_string);
            table.error.install(by, own.error);
            table.gc_collect.install(by, own.gc_collect);
            table.thrown.install(by, own.thrown);
            table.resolve_path.install(by, own.resolve_path);
            table.stream_hooks.install(by, own.stream_hooks);
            table.post_startup.install(by, own.post_startup);
            table.upload.install(by, own.upload);
            table.ticks.install(by, own.ticks);
            table.cached.install(by, own.cached);
            table.optimizer_passes.install_all(by, own.optimizer_passes);
            table.observers.install_all(by, own.observers);
            table.error_observers.install_all(by, own.error_observers);
            table
                .functions_declared
                .install_all(by, own.functions_declared);
            table.classes_linked.install_all(by, own.classes_linked);
            table.fiber_init.install_all(by, own.fiber_init);
            table.fiber_switch.install_all(by, own.fiber_switch);
            table.fiber_destroy.install_all(by, own.fiber_destroy);
            table.opcodes.install(by, own.opcodes);
            table.statement_handlers.install(by, own.statement_handler);
            table.program_ended.install(by, own.program_ended);
            table.admission.install(by, own.admission);
            table.fatal_logged.install(by, own.fatal_logged);
            table.shell.install(by, own.shell);
            table.input_filter.install(by, own.input_filter);
            table.treat_data.install(by, own.treat_data);
            table.encodings.install(by, own.encodings);
            table.intercepts.install(by, own.intercepts);
        }
        table
    }
}

/// php's saved pointer: each supplier installs over the one before and
/// keeps it as the one beneath. The chain is entered from the top, each
/// link handed the ones beneath ([`Beneath`]), so each link chooses
/// where its work stands against them, as each of php's chooses where
/// it calls the pointer it saved.
pub(crate) struct Chain<H>(Vec<(ExtensionId, H)>);

impl<H: Copy> Chain<H> {
    fn install(&mut self, by: ExtensionId, hook: Option<H>) {
        self.0.extend(hook.map(|hook| (by, hook)));
    }

    /// Every link beside the extension that supplied it, the first
    /// supplier's first: what [`Beneath::top`] enters from the last, each
    /// link served as its extension.
    pub(crate) fn links(&self) -> &[(ExtensionId, H)] {
        &self.0
    }
}

/// What a compile answered, php's `zend_op_array *`: the unit, or the
/// compiler's refusal, which the engine raises where the string or the
/// file was to compile. A link of `zend_compile_string` or
/// `zend_compile_file` answers what the one beneath answered, and reads
/// what it compiled to.
pub struct Compiled(pub(crate) Result<PublishedUnit, crate::unit_cache::CompileRefused>);

impl Compiled {
    /// The main op array of what compiled, `op_array` as php's
    /// `zend_compile_file` returns it; none for a refusal.
    #[must_use]
    pub fn op_array(&self) -> Option<crate::api::request::OpArrayView<'_>> {
        let unit = self.0.as_ref().ok()?;
        Some(crate::api::request::OpArrayView::of(
            unit,
            crate::exec::code_of(unit),
        ))
    }
}

/// Whether `php_error_cb` bails out of a level it cannot recover from,
/// php's `E_DONT_BAIL` bit on the type it is handed: set by
/// `zend_exception_error` on the uncaught throwable it renders
/// (`Z/Zend/zend_exceptions.c:951`, `:999`), under which the status is
/// written and the store is not marked destructed
/// (`Z/main/main.c:1528`).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Bail {
    Out,
    DontBail,
}

/// A list each supplier appends to, served in supplier order, each hook
/// beside the extension that supplied it, as which it is served.
pub(crate) struct Listed<H>(Vec<(ExtensionId, H)>);

impl<H: Copy> Listed<H> {
    fn install(&mut self, by: ExtensionId, hook: Option<H>) {
        self.0.extend(hook.map(|hook| (by, hook)));
    }

    /// Each of a supplier's, for a kind it may supply more than once.
    fn install_all(&mut self, by: ExtensionId, hooks: Vec<H>) {
        self.0.extend(hooks.into_iter().map(|hook| (by, hook)));
    }

    /// Each hook beside its extension, in supplier order.
    pub(crate) fn iter(&self) -> impl Iterator<Item = (ExtensionId, H)> + '_ {
        self.0.iter().copied()
    }

    /// Whether no supplier appended one, php's `zend_observer_*_observed`
    /// word for a notice's list.
    pub(crate) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// How many the list holds, as a `u32`.
    pub(crate) fn count(&self) -> u32 {
        u32::try_from(self.0.len()).unwrap_or(u32::MAX)
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

    /// The hook beside the extension that supplied it, as which it is
    /// served.
    pub(crate) fn entry(&self) -> Option<(ExtensionId, H)> {
        self.0
    }
}

/// Rows under a key, every supplier's in supplier order, each beside the
/// extension that supplied it.
pub(crate) struct Keyed<K, H>(Vec<(ExtensionId, K, H)>);

impl<K: PartialEq, H> Keyed<K, H> {
    fn install(&mut self, by: ExtensionId, rows: Vec<(K, H)>) {
        self.0.extend(rows.into_iter().map(|(k, h)| (by, k, h)));
    }

    /// Each row beside its extension.
    pub(crate) fn iter(&self) -> impl Iterator<Item = &(ExtensionId, K, H)> {
        self.0.iter()
    }

    /// The rows under `key`, in supplier order, each beside its
    /// extension.
    pub(crate) fn under(&self, key: K) -> impl Iterator<Item = (ExtensionId, &H)> {
        self.0
            .iter()
            .filter(move |(_, k, _)| *k == key)
            .map(|(by, _, h)| (*by, h))
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
                .interrupt(|req, beneath| {
                    note("earlier");
                    beneath.serve(req)
                })
                .build("earlier"),
            ExtensionBuilder::default()
                .interrupt(|req, beneath| {
                    note("later");
                    beneath.serve(req)
                })
                .build("later"),
        ];
        let engine = Engine::cli_builder().extensions(&extensions).build();
        let mut req = Request::new(Arc::clone(&engine.shared), RequestDescription::default());
        req.limits().interrupt().raise();
        assert_eq!(crate::interrupt::check(&mut req), Ok(false));
        assert_eq!(served(&LOG), ["later", "earlier"]);
    }

    /// A link's work stands where it serves the one beneath, before it
    /// or after it, and a link that does not serve it has answered for
    /// every link under it, as php leaves each link to call the pointer
    /// it saved where it will: `php_rfc1867_callback` here, the later
    /// link serving the earlier first as session's does
    /// (`Z/ext/session/session.c:3174-3176`), then one that answers
    /// php's `FAILURE` alone.
    #[test]
    fn a_link_chooses_where_the_one_beneath_is_served() {
        static LOG: Served = Mutex::new(Vec::new());
        fn note(name: &'static str) {
            if let Ok(mut log) = LOG.lock() {
                log.push(name);
            }
        }
        let earlier = ExtensionBuilder::default()
            .upload(|_, _| {
                note("earlier");
                Ok(true)
            })
            .build("earlier");
        let after = ExtensionBuilder::default()
            .upload(|req, beneath| {
                let went_on = beneath.serve(req)?;
                note("after");
                Ok(went_on)
            })
            .build("after");
        let alone = ExtensionBuilder::default()
            .upload(|_, _| {
                note("alone");
                Ok(false)
            })
            .build("alone");
        let start = crate::api::request::Upload::Start { content_length: 0 };
        let mut answers = Vec::new();
        for extensions in [[earlier.clone(), after], [earlier, alone]] {
            let engine = Engine::cli_builder().extensions(&extensions).build();
            let mut req = Request::new(Arc::clone(&engine.shared), RequestDescription::default());
            answers.push(req.upload_event(&start).ok());
        }
        assert_eq!(answers, [Some(true), Some(false)]);
        assert_eq!(served(&LOG), ["earlier", "after", "alone"]);
    }

    /// A chained kind whose link answers or passes, `zend_resolve_path`
    /// as phar's `phar_resolve_path` falls through to the one it saved
    /// (`Z/ext/phar/phar.c:3239-3246`): the later extension answers
    /// what it takes, and what it leaves goes to the one beneath.
    #[test]
    fn a_chained_kind_falls_through_to_the_one_beneath() {
        let extensions = [
            ExtensionBuilder::default()
                .resolve_path(|_, _| Ok(b"/beneath".to_vec()))
                .build("earlier"),
            ExtensionBuilder::default()
                .resolve_path(|req, beneath| match beneath.path() {
                    b"a" => Ok(b"/above".to_vec()),
                    _ => beneath.serve(req),
                })
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
                    req.response_mut().write(b"earlier ");
                    Ok(())
                })
                .build("earlier"),
            ExtensionBuilder::default()
                .ticks(|req| {
                    req.response_mut().write(b"later ");
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
        builder.build().serve_into(
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
            supplier.and_then(|id| req.extensions().name(id)),
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
                statement_handler: None,
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
            req.response_mut().write(b"opcache ");
            crate::api::call::passthrough(req, None)
        };
        let phar: crate::api::abi::Entry = |req| {
            req.response_mut().write(b"phar ");
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
    /// saves ffi's (`Z/ext/opcache/ZendAccelerator.c:3220-3221`), so an
    /// extension's link is beneath it and asked first: a refusal there ends
    /// startup and the Zend extension's preload is never asked for,
    /// though it stands first in the bundle. ffi's own failure is its
    /// engine start's refusal, which php never takes past `MINIT`.
    #[test]
    fn a_minit_s_post_startup_link_is_asked_before_a_zend_extension_s() {
        let extensions = [
            zend(
                ExtensionBuilder::default().post_startup(|req, beneath| match beneath.serve(req) {
                    crate::api::abi::PostStartup::Serve => {
                        req.response_mut().write_stderr(b"preloading\n");
                        crate::api::abi::PostStartup::Preload {
                            script: b"/nowhere".to_vec(),
                            thread: None,
                        }
                    }
                    refused => refused,
                }),
                "opcache",
            ),
            ExtensionBuilder::default()
                .post_startup(|req, _| {
                    req.response_mut().write_stderr(b"refused\n");
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

    /// What `extensions` print serving `source` under the CLI.
    fn printed(extensions: &[crate::api::abi::Extension], source: &str) -> String {
        let served = Engine::cli_builder()
            .extensions(extensions)
            .build()
            .serve_capture(source, crate::test::Capture::default());
        String::from_utf8_lossy(&served.stdout).into_owned()
    }

    /// `zend_error_cb`, each link handed the ones beneath and php's own
    /// `php_error_cb` beneath them all: the later extension's link is
    /// served first, and one that serves the diagnostic itself, as
    /// soap's serves a client's fatal (`Z/ext/soap/soap.c:1885-1905`),
    /// leaves the earlier link and php's display unserved.
    #[test]
    fn an_error_link_serves_the_ones_beneath_where_it_will() {
        use crate::api::abi::{Beneath, chain};
        use crate::api::raising::Level;
        use crate::tests::test_ext::{Event::Ext, Phase, Recording, TestExt, record};
        fn notices_kept(req: &mut Request, beneath: Beneath<'_, chain::Error>) {
            let message = String::from_utf8_lossy(beneath.message()).into_owned();
            record(2, Phase::Error(message));
            if beneath.level() != Level::UserNotice {
                beneath.serve(req);
            }
        }
        let recording = Recording::start();
        let extensions = [
            TestExt::<1>::extension(),
            TestExt::<2>::builder()
                .error(notices_kept)
                .build("test_ext_2"),
        ];
        let out = printed(
            &extensions,
            "<?php echo $u; trigger_error('kept'); echo 'end'; throw new Exception('x');",
        );
        let shown = "\nWarning: Undefined variable $u in Standard input code on line 1\nend";
        assert!(out.starts_with(&format!("{shown}\nFatal error: Uncaught Exception: x")));
        let warning = || Phase::Error(String::from("Undefined variable $u"));
        let mut handed = recording.events().only(&[warning()]).0;
        let uncaught = handed.split_off(3);
        assert_eq!(
            handed,
            [
                Ext(2, warning()),
                Ext(1, warning()),
                Ext(2, Phase::Error("kept".into()))
            ]
        );
        // `zend_exception_error`'s uncaught throwable, past the user
        // handler, through the same links.
        let uncaught: Vec<_> = uncaught
            .iter()
            .map(|e| match e {
                Ext(n, Phase::Error(m)) => (*n, m.starts_with("Uncaught Exception: x")),
                _ => (0, false),
            })
            .collect();
        assert_eq!(uncaught, [(2, true), (1, true)]);
    }

    /// The `gc_collect_cycles` pointer, each link handed the ones
    /// beneath: the later extension's link is served first and its work
    /// stands around the earlier's, as xdebug's
    /// `xdebug_gc_collect_cycles` measures around the one it saved; a
    /// full root buffer's collection is served through it as
    /// `gc_collect_cycles()`'s is (`Z/Zend/zend_gc.c:702`).
    #[test]
    fn a_collection_link_stands_around_the_ones_beneath() {
        use crate::tests::test_ext::{Event::Ext, Phase, Recording, TestExt};
        let recording = Recording::start();
        let extensions = [TestExt::<3>::extension(), TestExt::<4>::extension()];
        let around = [Phase::Collect, Phase::Collected(0)];
        let out = printed(
            &extensions,
            "<?php $a = new stdClass; $a->a = $a; unset($a); echo gc_collect_cycles();",
        );
        assert_eq!(out, "1");
        assert_eq!(
            recording.events().only(&around).0,
            [
                Ext(4, Phase::Collect),
                Ext(3, Phase::Collect),
                Ext(3, Phase::Collected(1)),
                Ext(4, Phase::Collected(1)),
            ]
        );
        recording.mark("full");
        let out = printed(
            &extensions,
            "<?php for ($i = 0; $i < 10002; $i++) { $o = new stdClass; $o->o = $o; } $s = gc_status(); echo $s['runs'], ' ', $s['collected'];",
        );
        recording.mark("served");
        assert_eq!(out, "1 10000");
        assert_eq!(
            recording.events().between("full", "served").only(&around).0,
            [
                Ext(4, Phase::Collect),
                Ext(3, Phase::Collect),
                Ext(3, Phase::Collected(10000)),
                Ext(4, Phase::Collected(10000)),
            ]
        );
    }

    /// `zend_compile_string`, each link handed the ones beneath with
    /// the source it compiles: the later extension's link is served
    /// first and may hand the earlier a source of its own, as php's
    /// links call the pointer they saved with a string; what a link
    /// throws is raised where the `eval` stands.
    #[test]
    fn a_string_compile_link_hands_the_ones_beneath_its_source() {
        use crate::api::abi::{Beneath, Compiled, chain};
        use crate::api::raising::Thrown;
        use crate::tests::test_ext::{Event::Ext, Phase, Recording, TestExt, record};
        fn rewritten(
            req: &mut Request,
            beneath: Beneath<'_, chain::CompileString>,
        ) -> Result<Compiled, Thrown> {
            let source = String::from_utf8_lossy(beneath.source()).into_owned();
            record(6, Phase::CompileString(source));
            match beneath.source() {
                b"refused;" => Err(crate::api::raising::throw(req, "Exception", "refused")),
                _ => beneath.with_source(b"echo 'b';").serve(req),
            }
        }
        let recording = Recording::start();
        let extensions = [
            TestExt::<5>::extension(),
            TestExt::<6>::builder()
                .compile_string(rewritten)
                .build("test_ext_6"),
        ];
        let out = printed(
            &extensions,
            "<?php eval(\"echo 'a';\"); try { eval('refused;'); } catch (Exception $e) { echo ' ', $e->getMessage(); }",
        );
        assert_eq!(out, "b refused");
        let string = |s: &str| Phase::CompileString(s.into());
        assert_eq!(
            recording.events().only(&[string("")]).0,
            [
                Ext(6, string("echo 'a';")),
                Ext(5, string("echo 'b';")),
                Ext(6, string("refused;")),
            ]
        );
    }

    /// `zend_throw_exception_hook`, every link told from the top: each
    /// throwable thrown once, by `throw`, by a native and by
    /// `Generator::throw()`, where it is thrown and before the unwind
    /// serves the `finally` it leaves through; and a throwable a fiber
    /// carries across a switch told again where it lands, as
    /// `zend_fiber_delegate_transfer_result` throws it again
    /// (`Z/Zend/zend_fibers.c:641-646`).
    #[test]
    fn a_thrown_link_is_told_of_every_throw_before_the_unwind() {
        use crate::api::abi::{Beneath, chain};
        use crate::tests::test_ext::{Event::Ext, Phase, Recording, TestExt, message_of, record};
        const TYPE: &str = "strlen(): Argument #1 ($string) must be of type string, array given";
        fn shown(req: &mut Request, beneath: Beneath<'_, chain::Throw>) {
            let message = message_of(req, beneath.throwable());
            req.response_mut().write(format!("[{message}]").as_bytes());
            record(8, Phase::Thrown(message));
            beneath.serve(req);
        }
        let recording = Recording::start();
        let extensions = [
            TestExt::<7>::extension(),
            TestExt::<8>::builder().thrown(shown).build("test_ext_8"),
        ];
        let out = printed(
            &extensions,
            concat!(
                "<?php function f() { try { throw new Exception('u'); } finally { echo 'finally '; } }\n",
                "try { f(); } catch (Exception $e) { echo 'caught '; }\n",
                "try { strlen([]); } catch (TypeError $e) { echo 'caught '; }\n",
                "function g() { try { yield 1; } catch (Exception $e) { echo 'in g '; } }\n",
                "$g = g(); $g->current(); $g->throw(new Exception('g'));\n",
                "$f = new Fiber(function () { Fiber::suspend(); });\n",
                "$f->start(); try { $f->throw(new Exception('f')); } catch (Exception $e) { echo 'out'; }",
            ),
        );
        assert_eq!(
            out,
            "[u]finally caught [strlen(): Argument #1 ($string) must be of type string, array given]caught [g]in g [f][f]out"
        );
        let told = |n, m: &str| Ext(n, Phase::Thrown(m.into()));
        assert_eq!(
            recording.events().only(&[Phase::Thrown(String::new())]).0,
            [
                told(8, "u"),
                told(7, "u"),
                told(8, TYPE),
                told(7, TYPE),
                told(8, "g"),
                told(7, "g"),
                told(8, "f"),
                told(7, "f"),
                told(8, "f"),
                told(7, "f"),
            ]
        );
    }
}
