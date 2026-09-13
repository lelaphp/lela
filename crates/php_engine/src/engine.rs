//! `Engine` and `serve` (design §5.14): the process-wide, `Sync` engine
//! and the one request transaction every host crosses. It compiles the
//! source through `php_compile`, builds the request with every
//! extension's request state, serves `rinit`, hands the main op array's
//! entry frame to the tier-bound loop, serves `rshutdown` in reverse,
//! renders what php renders, and hands the bytes to the sink.
//!
//! The engine's own declarations go in first: the natives design §13
//! moves out of `ext/standard` and the throwable classes Zend declares,
//! so an engine with no extensions serves a program that uses Zend alone
//! (design §4.10).
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use std::any::Any;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, UNIX_EPOCH};

use php_compile::CompileError;
use php_heap::{Fatal, FrameRef, Intern, Strings};
use php_unit::Unit;
use php_unit::decl::{ClassDecl, ConstDecl, ConstExpr, FunctionDecl, Name};

use crate::api::abi::{Cache, Entry, Extension, StartupIni};
use crate::api::raising::Level;
use crate::api::request::{Ending, Request};
use crate::api::values::{PhpString, Value};
use crate::tier::{Nop, Tier, execute};

/// Everything a host knows about one request before it starts (design
/// §4.3 seam 4): one plain `Send` value, handed over once.
#[derive(Debug, Clone, Default)]
pub struct RequestDescription {
    pub argv: Vec<Vec<u8>>,
    pub env: Vec<(Vec<u8>, Vec<u8>)>,
    pub cwd: PathBuf,
    pub server: Vec<(Vec<u8>, Vec<u8>)>,
    pub headers: Vec<(Vec<u8>, Vec<u8>)>,
    pub body: Vec<u8>,
    pub sapi_name: String,
    pub is_web: bool,
    /// The served file's name, for `$_SERVER['SCRIPT_NAME']` and error text.
    pub script_name: Option<PhpString>,
    /// The source to serve, when the host has it in hand (`php -r`, the
    /// in-process tests); else the file is read by its name.
    pub source: Option<Vec<u8>>,
}

/// Where a request's bytes go (design §4.3 seam 4): the engine owns the
/// header list, the status and the commit moment; a sink only writes.
pub trait OutputSink {
    /// Body bytes.
    fn write(&mut self, bytes: &[u8]);
    /// The host may push what it has.
    fn flush(&mut self);
    /// The rendered header block, once, before the first body byte.
    fn send_headers(&mut self, block: &[u8]);
    /// One entry for the host's log, whole: what `php_log_err` was
    /// given, a diagnostic in its log form with its stack trace under
    /// it where it has one, or `error_log()`'s message as given, so it
    /// may carry newlines inside it and ends in none. The host ends it:
    /// php's CLI writes `"%s\n"` to stderr and a file log writes
    /// `[date] ` before it and `PHP_EOL` after.
    fn log(&mut self, entry: &[u8]);
}

/// What `serve` reports of one request.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Observables {
    pub exit_code: i32,
    pub headers_sent: bool,
    /// The fatal's rendered text, when the request ended in one.
    pub fatal: Option<String>,
}

/// The loop bound to the engine's tier, `execute::<T>` for one `T`.
type Loop = fn(&mut Request, FrameRef) -> Result<(), Fatal>;

/// What the unit cache holds for one file: the published unit, the
/// refusal the compiler gave for it, or nothing at all for a file that
/// could not be read when it was asked for.
type Cached = Option<Result<Arc<Unit>, CompileError>>;

/// What the unit cache is keyed by (design §6.3): the file's name and
/// what says it has not changed under the engine, php's realpath with
/// its `(mtime, size)`. A file edited in place is a key of its own, so
/// what was cached for the old bytes is never served for the new.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct UnitKey {
    path: Vec<u8>,
    /// What the system reports as the file's last write, php's
    /// `zend_stat_t.st_mtime`.
    modified: Option<Duration>,
    size: u64,
}

/// php's `opcache.max_accelerated_files` default: the units one engine
/// keeps before it drops half of them.
const MAX_ACCELERATED_FILES: usize = 10_000;

/// What every request shares: the bundle, the linked declarations, the
/// engine states, the ini defaults and the loop (design §5.14).
pub(crate) struct Shared {
    pub(crate) extensions: Vec<Extension>,
    pub(crate) functions: Vec<(FunctionDecl, Entry)>,
    pub(crate) classes: Vec<ClassDecl>,
    constants: Vec<ConstDecl>,
    pub(crate) engine_states: Vec<Box<dyn Any + Send + Sync>>,
    ini: Vec<(PhpString, PhpString)>,
    entry: Loop,
    /// The unit cache (design §6.3): what a file compiled to, held for
    /// every request that asks for the same file unchanged. A source the
    /// host handed over is not in it, because php caches neither `-r`
    /// nor `eval`. It stands above the table its units' constants are
    /// interned into, so a dropped engine drops its units first.
    units: Cache<UnitKey, Cached>,
    /// The engine's interned strings (design §5.2, §5.14): what a unit's
    /// constant strings enter as it is published, grown here alone and
    /// read by every request thread with nothing taken.
    strings: Mutex<Strings>,
}

impl Shared {
    /// One source compiled with its constant strings interned into the
    /// engine's table, which is what publication is (design §5.2, §6.3):
    /// opcache's persist walks an op array's constants and re-interns
    /// each into the shared table (`Z/ext/opcache/zend_persist.c:525-540`
    /// over `Z/ext/opcache/ZendAccelerator.c:515-575`), and compiling
    /// into that table reaches the same published unit with one
    /// allocation instead of two.
    fn publish(&self, source: &[u8], filename: &[u8]) -> Result<Arc<Unit>, CompileError> {
        let mut strings = match self.strings.lock() {
            Ok(strings) => strings,
            // The table is append-only and every string in it was
            // finished before it was published, so what a thread that
            // ended holding the lock left is consistent.
            Err(poisoned) => poisoned.into_inner(),
        };
        compile(source, filename, &mut *strings)
    }

    /// The unit for the file `path` names: compiled and published once,
    /// and shared by every request that asks for it afterwards (design
    /// §6.3). A file the engine cannot stat is no unit at all, which is
    /// the `None` `serve` serves nothing for.
    fn cached(&self, path: &[u8]) -> Cached {
        let name = String::from_utf8_lossy(path).into_owned();
        let described = std::fs::metadata(&name).ok()?;
        let key = UnitKey {
            path: path.to_vec(),
            modified: described
                .modified()
                .ok()
                .and_then(|at| at.duration_since(UNIX_EPOCH).ok()),
            size: described.len(),
        };
        let held = self.units.get_or_build(key, |_| {
            let source = std::fs::read(&name).ok()?;
            Some(self.publish(&source, path))
        });
        Cached::clone(&held)
    }

    pub(crate) fn ini_default(&self, name: &[u8]) -> Option<PhpString> {
        self.ini
            .iter()
            .find(|(n, _)| n.as_bytes() == name)
            .map(|(_, v)| v.clone())
    }

    pub(crate) fn ini_defaults(&self) -> Vec<(PhpString, PhpString)> {
        self.ini.clone()
    }

    pub(crate) fn constant(&self, name: &[u8]) -> Option<Value> {
        if let Some(decl) = self.constants.iter().find(|c| c.name.0.as_ref() == name) {
            return match &decl.value {
                ConstExpr::Null => Some(Value::null()),
                ConstExpr::Bool(b) => Some(Value::bool(*b)),
                ConstExpr::Long(n) => Some(Value::int(*n)),
                ConstExpr::Double(f) => Some(Value::float(*f)),
                ConstExpr::Str(s) => Some(Value::string(PhpString::from_bytes(s))),
                _ => None,
            };
        }
        match name {
            b"PHP_VERSION" => Some(Value::string(PhpString::from_bytes(
                crate::api::request::PHP_VERSION.as_bytes(),
            ))),
            b"PHP_EOL" => Some(Value::string(PhpString::from_bytes(b"\n"))),
            b"PHP_INT_MAX" => Some(Value::int(i64::MAX)),
            b"PHP_INT_MIN" => Some(Value::int(i64::MIN)),
            b"PHP_INT_SIZE" => Some(Value::int(8)),
            b"E_ALL" => Some(Value::int(32767)),
            _ => None,
        }
    }
}

/// The core ini entries and their defaults: php's `main.c` table and
/// the two `zend.` entries of `Zend/zend.c`'s a throwable's trace reads,
/// under the CLI's hardcoded overrides (`Z/sapi/cli/php_cli.c:116-121`),
/// which is what the oracle answers with no `php.ini`. `max_execution_time`
/// and `html_errors` are the two the CLI moves, from `30` and `1`.
/// `error_prepend_string` and `error_append_string` are `NULL` in php's
/// table and empty here, since an entry with no value at all is one
/// `ini_set()` refuses to set.
const CORE_INI: [(&str, &str); 15] = [
    ("precision", "14"),
    ("serialize_precision", "-1"),
    ("error_reporting", "32767"),
    ("display_errors", "1"),
    ("log_errors", "0"),
    ("html_errors", "0"),
    ("error_prepend_string", ""),
    ("error_append_string", ""),
    ("ignore_repeated_errors", "0"),
    ("ignore_repeated_source", "0"),
    ("memory_limit", "128M"),
    ("max_execution_time", "0"),
    ("default_charset", "UTF-8"),
    ("zend.exception_ignore_args", "0"),
    ("zend.exception_string_param_max_len", "15"),
];

/// Builds an [`Engine`]: the bundle, the tier and the startup ini.
pub struct EngineBuilder {
    extensions: Vec<Extension>,
    ini: Vec<(PhpString, PhpString)>,
    entry: Loop,
    tier_extension: Option<Extension>,
}

impl EngineBuilder {
    /// The extensions in `MINIT` order, the bundle's `enabled()`.
    #[must_use]
    pub fn extensions(mut self, extensions: &[Extension]) -> EngineBuilder {
        self.extensions.extend(extensions.iter().cloned());
        self
    }

    /// One ini entry as the host's `-d` or `php.ini` sets it.
    #[must_use]
    pub fn ini(mut self, name: &str, value: &str) -> EngineBuilder {
        self.ini.push((
            PhpString::from_bytes(name.as_bytes()),
            PhpString::from_bytes(value.as_bytes()),
        ));
        self
    }

    /// Bind the tier: `execute::<T>` becomes the loop, and the tier's
    /// state is declared under seam 3 (design §4.3 seam 8).
    #[must_use]
    pub fn tier<T: Tier>(mut self) -> EngineBuilder {
        self.entry = execute::<T>;
        self.tier_extension = T::extension();
        self
    }

    /// The engine: every engine state built from the startup ini, in
    /// bundle order (ADR-0005), the declarations linked.
    #[must_use]
    pub fn build(self) -> Engine {
        let mut ini: Vec<(PhpString, PhpString)> = CORE_INI
            .iter()
            .map(|(n, v)| {
                (
                    PhpString::from_bytes(n.as_bytes()),
                    PhpString::from_bytes(v.as_bytes()),
                )
            })
            .collect();
        let mut extensions = self.extensions;
        if let Some(tier) = self.tier_extension {
            extensions.push(tier);
        }
        for extension in &extensions {
            for entry in &extension.ini {
                ini.push((
                    PhpString::from_bytes(&entry.name.0),
                    PhpString::from_bytes(&entry.default),
                ));
            }
        }
        for (name, value) in self.ini {
            ini.retain(|(n, _)| *n != name);
            ini.push((name, value));
        }
        let startup = StartupIni {
            entries: ini.clone(),
        };
        let engine_states = extensions
            .iter()
            .filter_map(|e| e.engine_state.as_ref())
            .map(|build| build(&startup))
            .collect();
        let functions = crate::natives::natives()
            .into_iter()
            .chain(extensions.iter().flat_map(|e| e.functions.iter().cloned()))
            .collect();
        let classes = crate::classes::zend_classes()
            .into_iter()
            .chain(extensions.iter().flat_map(|e| e.classes.iter().cloned()))
            .collect();
        let constants = extensions
            .iter()
            .flat_map(|e| e.constants.iter().cloned())
            .collect();
        Engine {
            shared: Arc::new(Shared {
                extensions,
                functions,
                classes,
                constants,
                engine_states,
                ini,
                entry: self.entry,
                units: Cache::new(MAX_ACCELERATED_FILES),
                strings: Mutex::new(Strings::new()),
            }),
        }
    }
}

/// The process-wide engine (design §5.14): immutable once built, shared
/// by every request thread.
#[derive(Clone)]
pub struct Engine {
    pub(crate) shared: Arc<Shared>,
}

impl Engine {
    /// A builder with no extensions, `Nop` as the tier and the core ini.
    #[must_use]
    pub fn builder() -> EngineBuilder {
        EngineBuilder {
            extensions: Vec::new(),
            ini: Vec::new(),
            entry: execute::<Nop>,
            tier_extension: Nop::extension(),
        }
    }

    /// The compiled unit for `source`, as the cache holds it: its
    /// constant strings interned into this engine's table, so two units
    /// carrying one constant carry the one `zend_string` (design §5.2).
    #[must_use]
    pub fn unit(&self, source: &[u8], filename: &[u8]) -> Option<Arc<Unit>> {
        self.shared.publish(source, filename).ok()
    }

    /// The one request transaction (design §5.14): compile, seed, serve,
    /// commit, shut down, observe.
    ///
    /// An engine with no extensions serves what uses Zend alone (design
    /// §4.10): the loop over the main op array, `ECHO` through the
    /// string kernel, and the engine's own natives.
    ///
    /// ```php
    /// echo 1; var_dump(extension_loaded('Core'));
    /// ```
    /// ```expect
    /// 1bool(true)
    /// ```
    ///
    /// A source the compiler refuses ends the request the way php ends
    /// one: `E_PARSE` through `php_error_cb`, at the line the compiler
    /// stopped on, and php's status. What stands under the first `%s` is
    /// the compiler's own sentence, which names the construct it cannot
    /// read yet where php's names the token it did not want; the wording
    /// around it is php's, and the oracle is what holds it.
    ///
    /// ```php
    /// echo 1 +;
    /// ```
    /// ```expect exit=255
    /// Parse error: %s in %s on line 2
    /// ```
    pub fn serve(&self, description: RequestDescription, sink: &mut dyn OutputSink) -> Observables {
        let source = description.source.clone();
        let script = description
            .script_name
            .clone()
            .unwrap_or_else(|| PhpString::from_bytes(b"Standard input code"));
        let mut req = Request::new(Arc::clone(&self.shared), description);
        for extension in &self.shared.extensions {
            if let Some(rinit) = extension.rinit {
                let _ = rinit(&mut req);
            }
        }
        // Where the unit's constant strings live is where the unit
        // lives (design §5.2): a file's unit is published into the
        // engine's table and shared by every request that serves it,
        // and a source the host handed over is this request's alone,
        // interned into this request's own memory and gone with it, as
        // php holds what it compiles for `-r` and for `eval`.
        let compiled = match source {
            Some(source) => Some(compile(&source, script.as_bytes(), req.heap_mut())),
            None => self.shared.cached(script.as_bytes()),
        };
        match compiled {
            Some(Ok(unit)) => serve_unit(&mut req, &unit, self.shared.entry),
            // `php_error_cb` renders it where it renders every
            // diagnostic, and the request ends before a frame is pushed.
            Some(Err(refused)) => {
                req.note_error_at(Level::Parse, &refused.message, refused.line);
                req.end_in_parse();
            }
            None => {}
        }
        for extension in self.shared.extensions.iter().rev() {
            if let Some(rshutdown) = extension.rshutdown {
                rshutdown(&mut req);
            }
        }
        // `php_error_cb` rendered the fatal where it was raised, on
        // whichever of the two streams the ini entries name; what is left
        // for the host is the log form of it, whether or not `log_errors`
        // put that form on the log.
        let fatal = match req.ending() {
            Some(Ending::Fatal(logged)) => Some(logged.clone()),
            _ => None,
        };
        let exit_code = match req.ending() {
            Some(Ending::Fatal(_) | Ending::Parse) => 255,
            Some(Ending::Exit(code)) => *code,
            None => 0,
        };
        let headers = req.header_block();
        let body = req.take_body();
        for entry in req.take_log() {
            sink.log(&entry);
        }
        sink.send_headers(&headers);
        sink.write(&body);
        sink.flush();
        Observables {
            exit_code,
            headers_sent: true,
            fatal,
        }
    }
}

/// One source compiled, with what the compiler refused kept instead of
/// dropped: what `serve` needs to render php's parse error, and what
/// [`Engine::unit`] is the lossy face over. `strings` is the home the
/// unit's constant strings are interned into (design §5.2).
fn compile(
    source: &[u8],
    filename: &[u8],
    strings: &mut dyn Intern,
) -> Result<Arc<Unit>, CompileError> {
    let options = php_compile::Options {
        filename: Name(filename.into()),
        strict_types: false,
    };
    php_compile::compile_source(source, &options, strings).map(Arc::new)
}

/// The unit's main op array served on a fresh entry frame; the loop's
/// fatal, if any, ends the request.
fn serve_unit(req: &mut Request, unit: &Arc<Unit>, entry: Loop) {
    let frame = req.push_frame(Arc::clone(unit));
    if let Err(fatal) = entry(req, frame) {
        req.end_in(&fatal);
    }
    req.pop_frame();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::{Asked, Probe};
    use crate::tier::Site;

    /// Everything the request wrote, kept: the body as `Capture` keeps
    /// it behind the `test` feature, and the log as the calls the sink
    /// saw, one entry each.
    #[derive(Default)]
    struct Kept {
        body: Vec<u8>,
        log: Vec<Vec<u8>>,
    }

    impl Kept {
        /// The log as php's CLI writes it to stderr, `"%s\n"` per entry.
        fn logged(&self) -> String {
            let mut text = String::new();
            for entry in &self.log {
                text.push_str(&String::from_utf8_lossy(entry));
                text.push('\n');
            }
            text
        }
    }

    impl OutputSink for Kept {
        fn write(&mut self, bytes: &[u8]) {
            self.body.extend_from_slice(bytes);
        }
        fn flush(&mut self) {}
        fn send_headers(&mut self, _: &[u8]) {}
        fn log(&mut self, entry: &[u8]) {
            self.log.push(entry.to_vec());
        }
    }

    fn description(source: &str) -> RequestDescription {
        RequestDescription {
            sapi_name: String::from("cli"),
            script_name: Some(PhpString::from_bytes(b"witness.php")),
            source: Some(source.as_bytes().to_vec()),
            ..RequestDescription::default()
        }
    }

    fn served(engine: &Engine, source: &str) -> (String, Observables) {
        let mut sink = Kept::default();
        let observables = engine.serve(description(source), &mut sink);
        (
            String::from_utf8_lossy(&sink.body).into_owned(),
            observables,
        )
    }

    #[test]
    fn an_engine_with_no_extensions_serves_what_uses_zend_alone() {
        let engine = Engine::builder().build();
        let (body, observables) = served(&engine, "<?php echo 1;");
        assert_eq!(body, "1");
        assert_eq!(observables.exit_code, 0);
        assert_eq!(observables.fatal, None);
    }

    /// What the witness's `%s` stands over: the compiler's own sentence,
    /// which says which construct it could not read and where php's says
    /// which token it did not want. The frame around it is php's, and
    /// the witness beside `serve` is what holds that to the oracle.
    #[test]
    fn a_source_the_compiler_refuses_names_the_construct_it_could_not_read() {
        let engine = Engine::builder().build();
        let (body, observables) = served(&engine, "<?php\necho 1 +;");
        assert_eq!(
            body,
            "\nParse error: an expression this compiler cannot read yet in witness.php on line 2\n"
        );
        assert_eq!(observables.exit_code, 255);
        assert_eq!(
            observables.fatal, None,
            "a parse error is not the fatal `serve` renders a second time"
        );
    }

    /// A parse error is `error_reporting`'s and `log_errors`' to render,
    /// as every other diagnostic is; the status is php's either way.
    #[test]
    fn a_parse_error_is_logged_and_masked_the_way_php_masks_one() {
        let mut logged = Kept::default();
        let observables = Engine::builder()
            .ini("display_errors", "0")
            .ini("log_errors", "1")
            .build()
            .serve(description("<?php\necho 1 +;"), &mut logged);
        assert!(logged.body.is_empty());
        assert_eq!(
            logged.logged(),
            "PHP Parse error:  an expression this compiler cannot read yet in witness.php on line 2\n"
        );
        assert_eq!(observables.exit_code, 255);

        let mut masked = Kept::default();
        let observables = Engine::builder()
            .ini("error_reporting", "0")
            .build()
            .serve(description("<?php\necho 1 +;"), &mut masked);
        assert!(masked.body.is_empty(), "E_PARSE is masked out");
        assert_eq!(observables.exit_code, 255, "the status is php's regardless");
    }

    /// `php_error_cb`'s two renderings of one diagnostic, which the two
    /// ini entries decide, each naming the line the opline stands on.
    /// The oracle writes the same bytes for `<?php echo NAN;` at
    /// `display_errors=1, log_errors=0` and at the two swapped.
    #[test]
    fn a_diagnostic_is_displayed_or_logged_as_the_ini_entries_say() {
        let mut displayed = Kept::default();
        Engine::builder()
            .build()
            .serve(description("<?php echo NAN;"), &mut displayed);
        assert_eq!(
            String::from_utf8_lossy(&displayed.body),
            "\nWarning: unexpected NAN value was coerced to string in witness.php on line 1\nNAN"
        );
        assert!(
            displayed.log.is_empty(),
            "log_errors is off, so nothing is logged"
        );

        let mut logged = Kept::default();
        Engine::builder()
            .ini("display_errors", "0")
            .ini("log_errors", "1")
            .build()
            .serve(description("<?php echo NAN;"), &mut logged);
        assert_eq!(String::from_utf8_lossy(&logged.body), "NAN");
        assert_eq!(
            logged.logged(),
            "PHP Warning:  unexpected NAN value was coerced to string in witness.php on line 1\n"
        );
    }

    /// The warning php writes for `echo NAN;` on line 2 of the witness
    /// regime's file, which the display-branch tests below stand on.
    const NAN_WARNING: &str = "unexpected NAN value was coerced to string";

    /// `error_prepend_string` and `error_append_string` stand around
    /// php's plain display form and around nothing else: the log form
    /// carries neither. What the pinned oracle writes for
    /// `-d error_prepend_string='<PRE>' -d error_append_string='</PRE>'`
    /// over `<?php\necho NAN;` is
    /// `<PRE>\nWarning: … on line 2\n</PRE>NAN`.
    #[test]
    fn the_two_error_strings_stand_around_the_display_form() {
        let mut around = Kept::default();
        Engine::builder()
            .ini("error_prepend_string", "<PRE>")
            .ini("error_append_string", "</PRE>")
            .ini("log_errors", "1")
            .build()
            .serve(description("<?php\necho NAN;"), &mut around);
        assert_eq!(
            String::from_utf8_lossy(&around.body),
            format!("<PRE>\nWarning: {NAN_WARNING} in witness.php on line 2\n</PRE>NAN")
        );
        assert_eq!(
            around.logged(),
            format!("PHP Warning:  {NAN_WARNING} in witness.php on line 2\n"),
            "php's log form is the one rendering neither string reaches"
        );
    }

    /// `html_errors` is php's other display form, and the two strings
    /// stand around it as they stand around the plain one. What the
    /// pinned oracle writes for `-d html_errors=1` over
    /// `<?php\necho NAN;` is
    /// `<br />\n<b>Warning</b>:  … in <b>%s</b> on line <b>2</b><br />\n`.
    #[test]
    fn html_errors_is_phps_other_display_form() {
        let mut html = Kept::default();
        Engine::builder()
            .ini("html_errors", "1")
            .build()
            .serve(description("<?php\necho NAN;"), &mut html);
        assert_eq!(
            String::from_utf8_lossy(&html.body),
            format!(
                "<br />\n<b>Warning</b>:  {NAN_WARNING} in <b>witness.php</b> on line <b>2</b><br />\nNAN"
            )
        );

        let mut around = Kept::default();
        Engine::builder()
            .ini("html_errors", "1")
            .ini("error_prepend_string", "<PRE>")
            .ini("error_append_string", "</PRE>")
            .build()
            .serve(description("<?php\necho NAN;"), &mut around);
        assert_eq!(
            String::from_utf8_lossy(&around.body),
            format!(
                "<PRE><br />\n<b>Warning</b>:  {NAN_WARNING} in <b>witness.php</b> on line <b>2</b><br />\n</PRE>NAN"
            )
        );
    }

    /// `ignore_repeated_errors` drops a repeat of the last diagnostic,
    /// and `ignore_repeated_source` says whether a repeat at another
    /// line is one. What the pinned oracle writes for
    /// `<?php\necho NAN, NAN;\necho NAN;` is three warnings with both
    /// entries off, two with `ignore_repeated_errors=1` — the second on
    /// line 2 dropped, the one on line 3 kept — and one with
    /// `ignore_repeated_source=1` beside it.
    #[test]
    fn a_repeat_is_dropped_the_way_php_drops_one() {
        let source = "<?php\necho NAN, NAN;\necho NAN;";
        let warning =
            |line: u32| format!("\nWarning: {NAN_WARNING} in witness.php on line {line}\n");

        let mut every = Kept::default();
        Engine::builder()
            .build()
            .serve(description(source), &mut every);
        assert_eq!(
            String::from_utf8_lossy(&every.body),
            format!("{}NAN{}NAN{}NAN", warning(2), warning(2), warning(3))
        );

        let mut once_a_site = Kept::default();
        Engine::builder()
            .ini("ignore_repeated_errors", "1")
            .build()
            .serve(description(source), &mut once_a_site);
        assert_eq!(
            String::from_utf8_lossy(&once_a_site.body),
            format!("{}NANNAN{}NAN", warning(2), warning(3)),
            "the repeat on line 2 is dropped and the one on line 3 stands"
        );

        let mut once = Kept::default();
        Engine::builder()
            .ini("ignore_repeated_errors", "1")
            .ini("ignore_repeated_source", "1")
            .build()
            .serve(description(source), &mut once);
        assert_eq!(
            String::from_utf8_lossy(&once.body),
            format!("{}NANNANNAN", warning(2)),
            "the site no longer parts them, so one warning stands for all three"
        );
    }

    /// A dropped repeat is dropped whole, php's `if (display)` around
    /// the store as well as the two renderings: it is not logged, and
    /// the diagnostic it repeats stays the one `error_get_last()`
    /// answers.
    #[test]
    fn a_dropped_repeat_is_neither_logged_nor_stored() {
        let mut logged = Kept::default();
        Engine::builder()
            .ini("ignore_repeated_errors", "1")
            .ini("ignore_repeated_source", "1")
            .ini("display_errors", "0")
            .ini("log_errors", "1")
            .build()
            .serve(description("<?php\necho NAN, NAN;"), &mut logged);
        assert_eq!(
            logged.logged(),
            format!("PHP Warning:  {NAN_WARNING} in witness.php on line 2\n"),
            "one log line for the two warnings"
        );
    }

    /// The same request through the `Capture` sink, which is the shape
    /// every extension's own test reads (design §4.3 seam 4, §4.10).
    #[test]
    fn the_capture_sink_holds_what_a_zend_only_request_wrote() {
        let served = Engine::builder()
            .build()
            .serve_capture("<?php echo 1;", crate::test::Capture::default());
        assert_eq!(served.stdout, b"1");
        assert_eq!(served.exit_code, 0);
        assert_eq!(served.fatal, None);
        let served = Engine::builder().build().serve_capture(
            "<?php var_dump(extension_loaded('Core'));",
            crate::test::Capture::default(),
        );
        assert_eq!(served.stdout, b"bool(true)\n");
    }

    /// How many strings the engine has published, which is what says a
    /// constant was interned once.
    fn interned(engine: &Engine) -> usize {
        match engine.shared.strings.lock() {
            Ok(strings) => strings.count(),
            Err(poisoned) => poisoned.into_inner().count(),
        }
    }

    /// A unit published into the cache interns its constant strings once,
    /// and every request serving it reads the one `zend_string` (design
    /// §5.2, §5.14, §6.3): two requests over one file write the same
    /// bytes out of one allocation the engine owns, and the second
    /// compiles nothing.
    #[test]
    fn two_requests_serving_one_file_share_the_one_constant_string() {
        let engine = Engine::builder().build();
        let path = std::env::temp_dir().join(format!("dax-{}-shared.php", std::process::id()));
        assert!(std::fs::write(&path, b"<?php echo 'shared';").is_ok());
        let name = PhpString::from_bytes(path.to_string_lossy().as_bytes());
        for _ in 0..2 {
            let mut sink = Kept::default();
            let observables = engine.serve(
                RequestDescription {
                    sapi_name: String::from("cli"),
                    script_name: Some(name.clone()),
                    ..RequestDescription::default()
                },
                &mut sink,
            );
            assert_eq!(String::from_utf8_lossy(&sink.body), "shared");
            assert_eq!(observables.exit_code, 0);
        }
        assert_eq!(
            interned(&engine),
            1,
            "one string for the one constant, however many requests read it"
        );
        // What the cache holds after both: one unit, and the constant
        // string its `ECHO` names is the one allocation.
        let (Some(Ok(one)), Some(Ok(again))) = (
            engine.shared.cached(name.as_bytes()),
            engine.shared.cached(name.as_bytes()),
        ) else {
            return;
        };
        assert!(Arc::ptr_eq(&one, &again), "one published unit");
        let named = |unit: &Arc<Unit>| {
            unit.main
                .literals
                .first()
                .copied()
                .and_then(php_heap::Zval::as_str)
        };
        let (Some(first), Some(second)) = (named(&one), named(&again)) else {
            return;
        };
        assert_eq!(first.bytes(), b"shared");
        assert!(first.is_interned(), "a published constant is interned");
        assert!(first.is_same(second), "one `zend_string`, read by both");
        let _ = std::fs::remove_file(&path);
    }

    /// What `memory_get_usage()` reads (design §5.13) after a request
    /// that built strings: a request that compiles for itself interns
    /// into its own memory, and the next request starts where the first
    /// one did, because the first request's end took every byte of it
    /// (design §5.2). The native that asks is #165; the accountant it
    /// asks is this one.
    #[test]
    fn what_memory_get_usage_reads_starts_where_it_started_for_every_request() {
        let engine = Engine::builder().build();
        let source = b"<?php echo 'a constant string long enough to be allocated';";
        let mut started_at = Vec::new();
        for _ in 0..2 {
            let mut req = Request::new(Arc::clone(&engine.shared), description(""));
            started_at.push(req.memory_usage(false));
            let compiled = compile(source, b"witness.php", req.heap_mut());
            assert!(compiled.is_ok(), "the source compiles");
            assert!(
                req.memory_usage(false) > 0,
                "what the request interned is the request's own memory"
            );
        }
        assert_eq!(
            started_at,
            vec![0, 0],
            "every request starts where the one before it did"
        );
        assert_eq!(
            interned(&engine),
            0,
            "and the engine's table never heard of it"
        );
    }

    /// A source the host handed over is the request's alone, as php's
    /// `-r` and `eval` are: its constant strings are interned into the
    /// request's own memory and the engine's table never hears of them.
    #[test]
    fn what_the_host_handed_over_interns_into_the_request_and_not_the_engine() {
        let engine = Engine::builder().build();
        let (body, _) = served(&engine, "<?php echo 'inline';");
        assert_eq!(body, "inline");
        assert_eq!(
            interned(&engine),
            0,
            "nothing a request compiled for itself outlives it"
        );
    }

    #[test]
    fn the_call_sequence_reaches_a_native_and_writes_what_it_returned() {
        let engine = Engine::builder().build();
        let (body, _) = served(&engine, "<?php var_dump(extension_loaded('Core'));");
        assert_eq!(body, "bool(true)\n");
        let (body, _) = served(&engine, "<?php var_dump(extension_loaded('nope'));");
        assert_eq!(body, "bool(false)\n");
    }

    /// The four arithmetic opcodes end to end, each snippet the one its
    /// kernel's witness pins, so that what the fences claim is what this
    /// engine writes and not only what the oracle wrote.
    #[test]
    fn the_arithmetic_opcodes_write_what_their_witnesses_pin() {
        let engine = Engine::builder().build();
        for (source, pinned) in [
            (
                "<?php echo 1 + (2 + extension_loaded('Core')), \"|\", 1.5 + (2.5 + extension_loaded('Core')), \"|\", \"1\" + (1 + extension_loaded('Core')), \"|\", 9223372036854775807 + (1 + extension_loaded('Core'));",
                "4|5|3|9.2233720368548E+18",
            ),
            (
                "<?php echo 1 - (2 - extension_loaded('Core')), \"|\", 1.5 - (2.5 - extension_loaded('Core')), \"|\", \"3\" - (1 + extension_loaded('Core')), \"|\", -9223372036854775807 - (1 + extension_loaded('Core'));",
                "0|0|1|-9.2233720368548E+18",
            ),
            (
                "<?php echo 2 * (3 * extension_loaded('Core')), \"|\", 1.5 * (2.5 * extension_loaded('Core')), \"|\", \"3\" * (1 + extension_loaded('Core')), \"|\", 4611686018427387904 * (1 + extension_loaded('Core'));",
                "6|3.75|6|9.2233720368548E+18",
            ),
            (
                "<?php echo 6 / (1 + extension_loaded('Core')), \"|\", 7.5 / (1.5 * extension_loaded('Core')), \"|\", \"7\" / (1 + extension_loaded('Core')), \"|\", (-9223372036854775807 - extension_loaded('Core')) / (0 - extension_loaded('Core'));",
                "3|5|3.5|9.2233720368548E+18",
            ),
        ] {
            let (body, observables) = served(&engine, source);
            assert_eq!(body, pinned, "{source}");
            assert_eq!(observables.fatal, None, "{source}");
        }
    }

    /// `div_function`: the divisor of zero php refuses to fold, so the
    /// opcode throws `DivisionByZeroError` where it stands, on line 1 of
    /// a source whose `<?php` and whose `echo` share it. The witness on
    /// [`crate::ops::div`] pins the oracle's rendering of the display branch;
    /// what this holds is the log branch's spelling of the same fatal,
    /// the status, and the double divisor the witness does not reach.
    #[test]
    fn a_division_by_zero_throws_php_s_error_where_the_opcode_stands() {
        let engine = Engine::builder().build();
        let (_, observables) = served(&engine, "<?php echo 1/0;");
        assert_eq!(
            observables.fatal.as_deref(),
            Some(
                "PHP Fatal error:  Uncaught DivisionByZeroError: Division by zero \
                 in witness.php:1\nStack trace:\n#0 {main}\n  thrown in witness.php on line 1"
            )
        );
        assert_eq!(observables.exit_code, 255);
        let (_, observables) = served(&engine, "<?php echo 1/0.0;");
        assert_eq!(
            observables
                .fatal
                .as_deref()
                .map(|text| text.contains("Uncaught DivisionByZeroError: Division by zero")),
            Some(true)
        );
    }

    /// `zend_binop_error`: an operand php will not read as a number is a
    /// `TypeError` naming php's types around the operator as written, and
    /// one with a number in front of it is the warning and the number.
    #[test]
    fn an_operand_php_refuses_to_read_as_a_number_is_php_s_type_error() {
        let engine = Engine::builder().build();
        let (_, observables) = served(&engine, "<?php echo \"abc\" + extension_loaded('Core');");
        assert_eq!(
            observables.fatal.as_deref().map(|text| text
                .contains("Uncaught TypeError: Unsupported operand types: string + bool")),
            Some(true),
            "{:?}",
            observables.fatal
        );
        let (body, observables) = served(
            &engine,
            "<?php echo \"1abc\" + (1 + extension_loaded('Core'));",
        );
        assert_eq!(
            body,
            "\nWarning: A non-numeric value encountered in witness.php on line 1\n3"
        );
        assert_eq!(observables.fatal, None);
    }

    #[test]
    fn echo_writes_each_part_as_php_converts_it() {
        let engine = Engine::builder().build();
        let (body, _) = served(
            &engine,
            "<?php echo 1, \"|\", true, \"|\", null, \"|\", 2.5;",
        );
        assert_eq!(body, "1|1||2.5");
    }

    /// `zend_exception_error` through `php_error_cb`: the four lines the
    /// witness beside [`Request::uncaught`] pins against the oracle, with
    /// the throw site in php's two spellings.
    #[test]
    fn a_call_of_a_name_nothing_declares_ends_the_request_as_php_names_it() {
        let engine = Engine::builder().build();
        let (body, observables) = served(&engine, "<?php\nnope();");
        assert_eq!(
            body,
            "\nFatal error: Uncaught Error: Call to undefined function nope() \
             in witness.php:2\nStack trace:\n#0 {main}\n  thrown in witness.php on line 2\n"
        );
        assert_eq!(
            observables.fatal.as_deref(),
            Some(
                "PHP Fatal error:  Uncaught Error: Call to undefined function nope() \
                 in witness.php:2\nStack trace:\n#0 {main}\n  thrown in witness.php on line 2"
            )
        );
        assert_eq!(observables.exit_code, 255);
    }

    /// A throwable a native left pending names the native's call as its
    /// site, and the native's own frame stands above `{main}`.
    #[test]
    fn a_throwable_a_native_left_pending_names_the_natives_call() {
        let engine = Engine::builder().build();
        let (body, observables) = served(&engine, "<?php\nvar_dump();");
        assert_eq!(
            body,
            "\nFatal error: Uncaught ArgumentCountError: var_dump() expects at least 1 argument, \
             0 given in witness.php:2\nStack trace:\n#0 witness.php(2): var_dump()\n#1 {main}\n  \
             thrown in witness.php on line 2\n"
        );
        assert_eq!(observables.exit_code, 255);
    }

    /// The fatal is `php_error_cb`'s to render, once, on the streams the
    /// two ini entries name; `serve` reports its log form and writes
    /// nothing of its own.
    #[test]
    fn a_fatal_is_displayed_or_logged_as_the_ini_entries_say() {
        let mut logged = Kept::default();
        let observables = Engine::builder()
            .ini("display_errors", "0")
            .ini("log_errors", "1")
            .build()
            .serve(description("<?php\nnope();"), &mut logged);
        assert!(logged.body.is_empty(), "display_errors is off");
        assert_eq!(
            logged
                .log
                .first()
                .map(|entry| String::from_utf8_lossy(entry).into_owned()),
            observables.fatal,
            "what the log holds is the fatal, once"
        );
        assert_eq!(logged.log.len(), 1, "the four lines are one entry");

        let mut masked = Kept::default();
        Engine::builder()
            .ini("error_reporting", "0")
            .build()
            .serve(description("<?php\nnope();"), &mut masked);
        assert!(masked.body.is_empty(), "E_ERROR is masked out");
        assert!(masked.log.is_empty(), "log_errors is off");
    }

    /// `php_error_cb` builds one diagnostic with one `spprintf` and gives
    /// it to `php_log_err` once, the backtrace inside it, and
    /// `php_log_err_with_severity` stamps one date on it and writes it
    /// as one message (`Z/main/main.c:1459`). So the sink sees one call
    /// for the four-line fatal, and one call per warning for two
    /// warnings, with no terminator on any of them.
    #[test]
    fn the_sink_is_given_one_call_per_diagnostic_whatever_its_lines() {
        let mut fatal = Kept::default();
        Engine::builder()
            .ini("display_errors", "0")
            .ini("log_errors", "1")
            .build()
            .serve(description("<?php\nnope();"), &mut fatal);
        assert_eq!(
            fatal.log,
            vec![
                b"PHP Fatal error:  Uncaught Error: Call to undefined function nope() \
                   in witness.php:2\nStack trace:\n#0 {main}\n  thrown in witness.php on line 2"
                    .to_vec()
            ],
            "four lines, one call"
        );

        let mut warned = Kept::default();
        Engine::builder()
            .ini("display_errors", "0")
            .ini("log_errors", "1")
            .build()
            .serve(description("<?php\necho NAN;\necho NAN;"), &mut warned);
        assert_eq!(
            warned.log,
            vec![
                format!("PHP Warning:  {NAN_WARNING} in witness.php on line 2").into_bytes(),
                format!("PHP Warning:  {NAN_WARNING} in witness.php on line 3").into_bytes(),
            ],
            "two lines, two calls"
        );
    }

    #[test]
    fn an_opcode_with_no_handler_ends_the_request_naming_it() {
        let engine = Engine::builder().build();
        let mut req = Request::new(Arc::clone(&engine.shared), description(""));
        let unit = Unit {
            main: php_unit::OpArray {
                oplines: vec![php_unit::Opline {
                    opcode: php_unit::Opcode::Mod,
                    ..php_unit::Opline::default()
                }]
                .into_boxed_slice(),
                ..php_unit::OpArray::default()
            },
            ..Unit::default()
        };
        let frame = req.push_frame(Arc::new(unit));
        assert_eq!(
            crate::tier::execute::<Nop>(&mut req, frame),
            Err(Fatal::Error(String::from(
                "InternalError: ZEND_MOD has no handler in this engine yet"
            )))
        );
    }

    /// Design §4.3 seam 8: `Site::Entry` is raised for the entry frame of
    /// an `execute` whatever the caller is, and `Site::Return` for no
    /// entry frame, which the stack's entry slot says.
    #[test]
    fn the_loop_asks_the_tier_at_the_entry_and_at_no_entry_frames_return() {
        let engine = Engine::builder().tier::<Probe>().build();
        let mut req = Request::new(Arc::clone(&engine.shared), description(""));
        let unit = engine
            .unit(b"<?php echo 1;", b"witness.php")
            .unwrap_or_default();
        let unit = Unit::clone(&unit);
        let frame = req.push_frame(Arc::new(unit));
        assert_eq!(crate::tier::execute::<Probe>(&mut req, frame), Ok(()));
        assert_eq!(
            req.state::<Asked>().sites,
            vec![(Site::Entry, frame)],
            "the entry frame is activated, and its own return raises no site"
        );
    }
}
