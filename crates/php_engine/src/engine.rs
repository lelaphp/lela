//! `Engine` and `serve` (design §5.14): the process-wide, `Sync` engine
//! and the one request transaction every host crosses. The skeleton
//! compiles the source through `php_compile`, builds the request with
//! every extension's request state, serves `rinit`, hands the entry frame
//! to the tier-bound loop, which has no handlers yet and ends the request
//! in its internal-error fatal, serves `rshutdown` in reverse, renders
//! what php renders, and hands the bytes to the sink.

use std::any::Any;
use std::path::PathBuf;
use std::sync::Arc;

use php_heap::{Fatal, FrameRef};
use php_unit::Unit;
use php_unit::decl::{ClassDecl, ConstDecl, ConstExpr, FunctionDecl, Name};

use crate::api::abi::{Extension, StartupIni};
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
    pub(crate) functions: Vec<FunctionDecl>,
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
        let functions = extensions
            .iter()
            .flat_map(|e| e.functions.iter().map(|(decl, _)| decl.clone()))
            .collect();
        let classes = extensions
            .iter()
            .flat_map(|e| e.classes.iter().cloned())
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
    shared: Arc<Shared>,
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
        let options = php_compile::Options {
            filename: Name(filename.into()),
            strict_types: false,
        };
        php_compile::compile_source(source, &options)
            .ok()
            .map(Arc::new)
    }

    /// The one request transaction (design §5.14): compile, seed, serve,
    /// commit, shut down, observe.
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
        let unit = source.and_then(|s| self.unit(&s, script.as_bytes()));
        if let Some(unit) = unit {
            serve_unit(&mut req, &unit, self.shared.entry);
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
            Some(Ending::Fatal(_)) => 255,
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

/// The unit's main op array served on a fresh entry frame; the loop's
/// fatal, if any, ends the request.
fn serve_unit(req: &mut Request, unit: &Unit, entry: Loop) {
    let slots = usize::try_from(unit.main.num_cvs + unit.main.num_tmps).unwrap_or(0);
    let frame = req.stack_mut().push_frame(slots);
    let outcome = {
        let mut guard = req.stack_mut().push_entry(frame);
        let _ = guard.stack();
        drop(guard);
        entry(req, frame)
    };
    if let Err(fatal) = outcome {
        req.end_in(&fatal);
    }
    req.stack_mut().pop_frame();
}
