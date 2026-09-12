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
use std::sync::Arc;

use php_compile::CompileError;
use php_heap::{Fatal, FrameRef};
use php_unit::Unit;
use php_unit::decl::{ClassDecl, ConstDecl, ConstExpr, FunctionDecl, Name};

use crate::api::abi::{Entry, Extension, StartupIni};
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
    /// A line for the host's log.
    fn log(&mut self, line: &[u8]);
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
}

impl Shared {
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

/// The core ini entries and their defaults, php's `main.c` table.
const CORE_INI: [(&str, &str); 8] = [
    ("precision", "14"),
    ("serialize_precision", "-1"),
    ("error_reporting", "32767"),
    ("display_errors", "1"),
    ("log_errors", "0"),
    ("memory_limit", "128M"),
    ("max_execution_time", "0"),
    ("default_charset", "UTF-8"),
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

    /// The compiled unit for `source`, as the cache would hold it.
    #[must_use]
    pub fn unit(&self, source: &[u8], filename: &[u8]) -> Option<Arc<Unit>> {
        compile(source, filename).ok()
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
        let source = description.source.clone().or_else(|| {
            description
                .script_name
                .as_ref()
                .and_then(|s| std::fs::read(String::from_utf8_lossy(s.as_bytes()).as_ref()).ok())
        });
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
        match source.map(|s| compile(&s, script.as_bytes())) {
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
        let fatal = match req.ending() {
            Some(Ending::Fatal(message)) => {
                let text = format!(
                    "PHP Fatal error:  {message} in {} on line 0\n",
                    String::from_utf8_lossy(script.as_bytes())
                );
                req.write(
                    format!(
                        "\nFatal error: {message} in {} on line 0\n",
                        String::from_utf8_lossy(script.as_bytes())
                    )
                    .as_bytes(),
                );
                Some(text)
            }
            _ => None,
        };
        let exit_code = match req.ending() {
            Some(Ending::Fatal(_) | Ending::Parse) => 255,
            Some(Ending::Exit(code)) => *code,
            None => 0,
        };
        let headers = req.header_block();
        let body = req.take_body();
        let log = req.take_log();
        for line in log.split_inclusive(|b| *b == b'\n') {
            sink.log(line);
        }
        if let Some(text) = &fatal {
            sink.log(text.as_bytes());
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
/// [`Engine::unit`] is the lossy face over.
fn compile(source: &[u8], filename: &[u8]) -> Result<Arc<Unit>, CompileError> {
    let options = php_compile::Options {
        filename: Name(filename.into()),
        strict_types: false,
    };
    php_compile::compile_source(source, &options).map(Arc::new)
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

    /// Everything the request wrote, kept, as `Capture` keeps it behind
    /// the `test` feature.
    #[derive(Default)]
    struct Kept {
        body: Vec<u8>,
        log: Vec<u8>,
    }

    impl OutputSink for Kept {
        fn write(&mut self, bytes: &[u8]) {
            self.body.extend_from_slice(bytes);
        }
        fn flush(&mut self) {}
        fn send_headers(&mut self, _: &[u8]) {}
        fn log(&mut self, line: &[u8]) {
            self.log.extend_from_slice(line);
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
            String::from_utf8_lossy(&logged.log),
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
            String::from_utf8_lossy(&logged.log),
            "PHP Warning:  unexpected NAN value was coerced to string in witness.php on line 1\n"
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

    #[test]
    fn the_call_sequence_reaches_a_native_and_writes_what_it_returned() {
        let engine = Engine::builder().build();
        let (body, _) = served(&engine, "<?php var_dump(extension_loaded('Core'));");
        assert_eq!(body, "bool(true)\n");
        let (body, _) = served(&engine, "<?php var_dump(extension_loaded('nope'));");
        assert_eq!(body, "bool(false)\n");
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

    #[test]
    fn a_call_of_a_name_nothing_declares_ends_the_request_as_php_names_it() {
        let engine = Engine::builder().build();
        let (_, observables) = served(&engine, "<?php nope();");
        assert_eq!(
            observables
                .fatal
                .as_deref()
                .map(|text| text.contains("Uncaught Error: Call to undefined function nope()")),
            Some(true),
            "{:?}",
            observables.fatal
        );
        assert_eq!(observables.exit_code, 255);
    }

    #[test]
    fn an_opcode_with_no_handler_ends_the_request_naming_it() {
        let engine = Engine::builder().build();
        let mut req = Request::new(Arc::clone(&engine.shared), description(""));
        let unit = Unit {
            main: php_unit::OpArray {
                oplines: vec![php_unit::Opline {
                    opcode: php_unit::Opcode::Add,
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
                "InternalError: ZEND_ADD has no handler in this engine yet"
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
            req.state::<Asked>().0,
            vec![(Site::Entry, frame)],
            "the entry frame is activated, and its own return raises no site"
        );
    }
}
