//! The ini area of a request: `EG(modified_ini_directives)` and what
//! reads and writes it, `zend_ini.c`'s `zend_alter_ini_entry_ex`,
//! `zend_restore_ini_entry` and `zend_ini_deactivate`, the core table's
//! `modifiable` bits and `on_modify` verdicts, `main/php_ini.c`'s
//! `display_ini_entries` and `cfg_get_entry`, and the listings
//! `ini_get_all()` and `ReflectionExtension::getINIEntries` read. The
//! request's ini state is [`Ini`], whose fields no other file under
//! `api/request` reads; they go through the methods here.

use std::sync::Arc;

use php_unit::decl::{IniDecl, Name};

use super::hash_bytes::HashBytes;
use super::{
    DISPLAY_ERRORS_STDERR, ExtensionId, InfoRow, IniFiles, Request, display_errors_mode_of,
};
use crate::api::abi::{IniHandler, IniStage, IniVerdict, LongHandler};
use crate::api::raising::{Level, Thrown};
use crate::api::values::{Owner, PhpString, Value, adopted_some};
use crate::c_number::{atoi, strtol, strtol_prefixed};
use crate::engine::{Configured, ConfiguredKey};

/// One ini entry as this request sees it, php's `zend_ini_entry` read
/// by `_extension_ini_string`, `ReflectionExtension::getINIEntries` and
/// `ini_get_all`: the declaration with its `modifiable` bits, the
/// extension that registered it (`module_number`), `Core` for the
/// engine's own table, the value in force, the value every request
/// starts with, which is php's `orig_value` where the request changed
/// the entry and its `value` where it did not (the master value), and
/// whether the request changed it, php's `modified`. A value is `None`
/// where the entry has none, php's `NULL`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IniEntry {
    pub decl: IniDecl,
    pub extension: ExtensionId,
    pub value: Option<Box<[u8]>>,
    pub master: Option<Box<[u8]>>,
    pub modified: bool,
}

/// One entry of `EG(modified_ini_directives)`: its name, the value it
/// holds, and the request string that value is where it is one.
type IniChanged = (Vec<u8>, Option<Vec<u8>>, Option<PhpString>);

/// The request's ini state, whose fields no file but this one reads.
pub(super) struct Ini {
    /// `EG(modified_ini_directives)`: each entry the request changed,
    /// in the order it first changed, with the value it holds now, `None`
    /// where it has none (php's `NULL`), and the request string that value
    /// is where it is one, php's `ini_entry->value` once it is not
    /// `orig_value`; the value it started with is the engine's
    /// ([`Request::ini_deactivate`]).
    changed: Vec<IniChanged>,
    /// The bytes `EG(modified_ini_directives)` takes on the request's
    /// heap, laid down by the first entry the request changes, a `@`'s
    /// `error_reporting` among them, and freed by the request's end.
    bytes: Option<HashBytes>,
    /// The entries this request's host locked past the engine's
    /// ([`crate::engine::IniOrigin::Admin`]).
    locked: Vec<Vec<u8>>,
    /// The stage an entry's `on_modify` is served at ([`IniStage`]).
    stage: IniStage,
    /// The entries a hot path reads, typed, as the request holds them
    /// now ([`TypedIni`]).
    typed: TypedIni,
    /// `PG(open_basedir)` where `OnUpdateBaseDir` set it at runtime, the
    /// list `ini_set()` took with each directory expanded
    /// (`PG(open_basedir_modified)`); `None` where it is the entry's value
    /// as set, at startup and once the request's end restored it.
    open_basedir: Option<Vec<u8>>,
    /// The order `EG(ini_directives)` is walked in ([`IniOrder`]).
    order: IniOrder,
}

/// The order php's `EG(ini_directives)` holds its entries in, which
/// every walk of the table lists them in: `display_ini_entries`,
/// `ini_get_all()` and `ReflectionExtension`'s `getINIEntries()` and
/// `__toString()`. Registration order until the first `phpinfo()` or
/// `ini_get_all()` sorts the table in place
/// ([`Request::ini_sort_entries`]), and sorted by name from then on.
///
/// php's table is the process's, or under ZTS the thread's, so a sort
/// outlives the request in php; here it is the request's, which is all
/// a host that serves one request per process (the CLI, CGI) can show,
/// and what a request served alone shows under any host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IniOrder {
    /// `zend_register_ini_entries`' order: the engine's own entries,
    /// then each extension's in bundle order, each as it declared them.
    Registration,
    /// `zend_ini_sort_entries`' order ([`ini_key_compare`]).
    Sorted,
}

impl IniOrder {
    /// `rows`, handed over in registration order, put in this order,
    /// each named by `name`.
    fn arrange<T>(self, rows: &mut [T], name: impl Fn(&T) -> &[u8]) {
        if self == IniOrder::Sorted {
            rows.sort_by(|a, b| ini_key_compare(name(a), name(b)));
        }
    }
}

/// `ini_key_compare` (`Z/Zend/zend_ini.c:178`) over two names:
/// `zend_binary_strcasecmp`, the bytes compared ASCII case-blind up to
/// the shorter, and the shorter first where one is the other's prefix.
fn ini_key_compare(a: &[u8], b: &[u8]) -> std::cmp::Ordering {
    a.iter()
        .map(u8::to_ascii_lowercase)
        .cmp(b.iter().map(u8::to_ascii_lowercase))
}

impl Ini {
    /// The ini state a request starts with: nothing changed, and the
    /// typed entries the engine's master values give.
    pub(super) fn new(typed: TypedIni) -> Ini {
        Ini {
            changed: Vec::new(),
            bytes: None,
            locked: Vec::new(),
            stage: IniStage::Runtime,
            typed,
            open_basedir: None,
            order: IniOrder::Registration,
        }
    }
}

/// The core entries a path reads once per opline, per throw or per
/// compile, typed as php keeps them in `EG`, `PG` and `CG` once the
/// entry's `OnUpdate*` has read its text:
/// `EG(precision)` and `PG(serialize_precision)`, `OnSetPrecision` and
/// `OnSetSerializePrecision`'s `ZEND_ATOL` cast to `int`
/// (`Z/main/main.c:120-141`), `EG(exception_ignore_args)`,
/// `OnUpdateBool`'s, `EG(exception_string_param_max_len)`,
/// `OnSetExceptionStringParamMaxLen`'s `ZEND_ATOL` (`Z/Zend/zend.c:177`),
/// and what every `eval` and include compiles under: `CG(short_tags)`,
/// `EG(assertions)` (`OnUpdateAssertions`' quantity) and
/// `CG(multibyte)`.
/// The engine lays them from its master values and each request keeps
/// its own, changed with the entry's value, so a read neither scans the
/// entries nor parses one. The text is php's `ZEND_ATOL`, so a value
/// `ini_set()` takes is read up to its first byte that is not a digit,
/// and one past `int` is cut to it as php's cast cuts it:
///
/// ```php
/// var_dump(ini_set('precision', '5x')); echo 1/3, "\n";
/// var_dump(ini_set('serialize_precision', ' 3abc')); var_dump(1/3); echo json_encode(1/3), "\n";
/// ini_restore('precision'); ini_restore('serialize_precision');
/// echo 1/3, "\n"; var_dump(1/3);
/// ini_set('precision', '4294967301'); echo 1/3, "\n";
/// ```
/// ```expect
/// string(2) "14"
/// 0.33333
/// string(2) "-1"
/// float(0.333)
/// 0.333
/// 0.33333333333333
/// float(0.3333333333333333)
/// 0.33333
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct TypedIni {
    pub(crate) precision: i32,
    pub(crate) serialize_precision: i32,
    pub(crate) exception_ignore_args: bool,
    pub(crate) exception_string_param_max_len: usize,
    pub(crate) short_open_tag: bool,
    pub(crate) assertions: i64,
    pub(crate) multibyte: bool,
}

impl TypedIni {
    /// The entries typed here, each in [`TypedIni::update`].
    const NAMES: [&'static [u8]; 7] = [
        b"precision",
        b"serialize_precision",
        b"zend.exception_ignore_args",
        b"zend.exception_string_param_max_len",
        b"short_open_tag",
        b"zend.assertions",
        b"zend.multibyte",
    ];

    /// What `value` gives each entry, `value` answering an entry's text,
    /// the empty string where it has none, as `zend_ini_get_value` does;
    /// php's built-in default where it has no such entry.
    pub(crate) fn of<'a>(value: impl Fn(&[u8]) -> Option<&'a [u8]>) -> TypedIni {
        let mut typed = TypedIni {
            precision: 14,
            serialize_precision: -1,
            exception_ignore_args: false,
            exception_string_param_max_len: 15,
            short_open_tag: true,
            assertions: 1,
            multibyte: false,
        };
        for name in TypedIni::NAMES {
            if let Some(text) = value(name) {
                typed.update(name, Some(text));
            }
        }
        typed
    }

    /// The entry `name` holding `value` now, `None` where it holds php's
    /// `NULL`, read as its `OnUpdate*` reads it; an entry not typed here
    /// changes nothing. php's `(int) ZEND_ATOL(...)` is C's `atoi`, which
    /// cuts a value past `int`.
    pub(crate) fn update(&mut self, name: &[u8], value: Option<&[u8]>) {
        let text = value.unwrap_or_default();
        match name {
            b"precision" => self.precision = atoi(text),
            b"serialize_precision" => self.serialize_precision = atoi(text),
            b"zend.exception_ignore_args" => {
                self.exception_ignore_args = Request::ini_parse_bool(text);
            }
            b"zend.exception_string_param_max_len" => {
                self.exception_string_param_max_len = usize::try_from(strtol(text)).unwrap_or(0);
            }
            b"short_open_tag" => self.short_open_tag = Request::ini_parse_bool(text),
            b"zend.assertions" => self.assertions = Request::ini_parse_bytes(text),
            b"zend.multibyte" => self.multibyte = Request::ini_parse_bool(text),
            _ => {}
        }
    }
}

/// How a core entry's handler reads its value, before anything else it
/// does: by `zend_ini_parse_quantity_warn`, as `OnUpdateLong` and
/// `OnUpdateLongGEZero` read and as `OnUpdateAssertions`,
/// `OnUpdateFiberStackSize` and `OnUpdateMaxAllowedStackSize` read
/// first, or by `zend_ini_parse_uquantity_warn`, as
/// `OnChangeMaxMemoryLimit`, `OnChangeMemoryLimit` and
/// `OnUpdateReservedStackSize` read first. The warning is
/// [`Request::ini_parse_quantity_warn`]'s, raised at startup, at
/// `ini_set()` and as the request ends; an extension's entry declared
/// with [`LongHandler`] is served the same rule through its handler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CoreQuantity {
    /// `zend_ini_parse_quantity_warn`, a negative refused where the
    /// handler is `OnUpdateLongGEZero`.
    Signed(LongHandler),
    /// `zend_ini_parse_uquantity_warn`.
    Unsigned,
}

/// One of [`CoreQuantity::ENTRIES`]: the entry's name and its reading.
pub(crate) type CoreQuantityEntry = (&'static [u8], CoreQuantity);

impl CoreQuantity {
    /// The core entries read by the quantity rule, in the order
    /// `php_module_startup` registers them, which is the order their
    /// startup warnings are raised in: `main.c`'s in its `PHP_INI_BEGIN()`
    /// order (`Z/main/main.c:807-885`), then `zend.c`'s, which
    /// `zend_register_standard_ini_entries` registers after them
    /// (`Z/Zend/zend.c:262-279`).
    pub(crate) const ENTRIES: [CoreQuantityEntry; 18] = [
        (
            b"xmlrpc_error_number",
            CoreQuantity::Signed(LongHandler::Any),
        ),
        (b"max_input_time", CoreQuantity::Signed(LongHandler::Any)),
        (b"output_buffering", CoreQuantity::Signed(LongHandler::Any)),
        (b"error_log_mode", CoreQuantity::Signed(LongHandler::Any)),
        (
            b"upload_max_filesize",
            CoreQuantity::Signed(LongHandler::Any),
        ),
        (b"post_max_size", CoreQuantity::Signed(LongHandler::Any)),
        (
            b"max_input_nesting_level",
            CoreQuantity::Signed(LongHandler::AtLeastZero),
        ),
        (
            b"max_input_vars",
            CoreQuantity::Signed(LongHandler::AtLeastZero),
        ),
        (b"max_memory_limit", CoreQuantity::Unsigned),
        (b"memory_limit", CoreQuantity::Unsigned),
        (
            b"realpath_cache_size",
            CoreQuantity::Signed(LongHandler::Any),
        ),
        (
            b"realpath_cache_ttl",
            CoreQuantity::Signed(LongHandler::Any),
        ),
        (
            b"user_ini.cache_ttl",
            CoreQuantity::Signed(LongHandler::Any),
        ),
        (b"hard_timeout", CoreQuantity::Signed(LongHandler::Any)),
        (b"zend.assertions", CoreQuantity::Signed(LongHandler::Any)),
        (b"fiber.stack_size", CoreQuantity::Signed(LongHandler::Any)),
        (
            b"zend.max_allowed_stack_size",
            CoreQuantity::Signed(LongHandler::Any),
        ),
        (b"zend.reserved_stack_size", CoreQuantity::Unsigned),
    ];

    /// Whether the handler refuses `value`: `OnUpdateLongGEZero`'s
    /// negative quantity.
    pub(crate) fn refuses(self, value: &[u8]) -> bool {
        match self {
            CoreQuantity::Signed(handler) => !handler.takes(Request::ini_parse_bytes(value)),
            CoreQuantity::Unsigned => false,
        }
    }

    /// The reading of the core entry `name`, `None` where its handler
    /// reads no quantity.
    pub(crate) fn of(name: &[u8]) -> Option<CoreQuantity> {
        (CoreQuantity::ENTRIES.iter())
            .find(|(entry, _)| *entry == name)
            .map(|(_, reading)| *reading)
    }
}

impl Request {
    /// `ini_get()`, `zend_ini_get_value`: the request's override, else
    /// the engine's default, the empty string where the entry has no
    /// value (php's `NULL`); `None` where php has no such entry.
    #[must_use]
    pub fn ini_get(&self, name: &[u8]) -> Option<&[u8]> {
        match self.ini.changed.iter().find(|(n, ..)| n.as_slice() == name) {
            Some((_, value, _)) => Some(value.as_deref().unwrap_or_default()),
            None => self.shared.ini_default(name),
        }
    }

    /// `INI_STR`, `zend_ini_string_ex`: the entry's value in force as php
    /// holds it, `None` where it has none (`NULL`), as for a name php has
    /// no entry for. What a rule that tells `NULL` from the empty string
    /// reads, as `-d error_reporting=` sets the level to 0 where the
    /// entry's `NULL` is `E_ALL`:
    ///
    /// ```php
    /// $p = proc_open([PHP_BINARY, '-n', '-d', 'error_reporting=', '-r', 'var_dump(error_reporting(), ini_get("error_reporting")); echo $undefined;'], [1 => ['pipe', 'w'], 2 => ['pipe', 'w']], $pipes);
    /// echo stream_get_contents($pipes[1]); proc_close($p);
    /// ini_set('error_reporting', ''); var_dump(error_reporting());
    /// ```
    /// ```expect process
    /// int(0)
    /// string(0) ""
    /// int(0)
    /// ```
    #[must_use]
    pub fn ini_value(&self, name: &[u8]) -> Option<&[u8]> {
        match self.ini.changed.iter().find(|(n, ..)| n.as_slice() == name) {
            Some((_, value, _)) => value.as_deref(),
            None => self.shared.ini_default_value(name),
        }
    }

    /// `PG(open_basedir)`, the list the `open_basedir` guard reads
    /// ([`Request::check_open_basedir`]): the entry's value as it was set
    /// at startup, and, once `ini_set()` or a `.user.ini` tightened it, the
    /// list it took with each directory expanded (`OnUpdateBaseDir`,
    /// `Z/main/fopen_wrappers.c:74`), where `ini_get()` still answers the
    /// value as set. `None` where the entry holds php's `NULL`.
    ///
    /// ```php
    /// set_error_handler(function ($no, $msg) { echo str_replace(__DIR__, 'DIR', $msg), "\n"; return true; });
    /// ini_set('open_basedir', __DIR__ . '/./a/..' . PATH_SEPARATOR . __DIR__ . '/b/../');
    /// var_dump(str_replace(__DIR__, 'DIR', ini_get('open_basedir')), fopen('/', 'r'));
    /// var_dump(str_replace(__DIR__, 'DIR', ini_set('open_basedir', __DIR__ . '/.')), str_replace(__DIR__, 'DIR', ini_get('open_basedir')));
    /// ```
    /// ```expect
    /// fopen(): open_basedir restriction in effect. File(/) is not within the allowed path(s): (DIR:DIR/)
    /// fopen(/): Failed to open stream: Operation not permitted
    /// string(20) "DIR/./a/..:DIR/b/../"
    /// bool(false)
    /// string(20) "DIR/./a/..:DIR/b/../"
    /// string(5) "DIR/."
    /// ```
    #[must_use]
    pub fn open_basedir(&self) -> Option<&[u8]> {
        match &self.ini.open_basedir {
            Some(expanded) => Some(expanded),
            None => self.ini_value(b"open_basedir"),
        }
    }

    /// `ini_set()`: the previous value, or `None` for an entry that does
    /// not exist, may not change at runtime, or whose `on_modify` refuses
    /// the value ([`Request::ini_verdict`]). An entry with an on-modify
    /// handler runs it here: `max_execution_time` re-arms the execution
    /// timer (`OnUpdateTimeout`, `Z/main/main.c:467`).
    ///
    /// The entries of the table without `PHP_INI_USER`, `xmlrpc_errors`,
    /// `short_open_tag`, `sys_temp_dir`, the `mail.*` ones but `SMTP` and
    /// `smtp_port`, `sendmail_path`, two of the three `syslog.*` and
    /// `disable_functions`, are
    /// `PHP_INI_SYSTEM` or `PHP_INI_SYSTEM|PHP_INI_PERDIR`, which
    /// `zend_alter_ini_entry_ex` refuses at `PHP_INI_STAGE_RUNTIME` as it
    /// refuses every entry whose `modifiable` lacks the stage; the rest of
    /// the table is `PHP_INI_ALL`. An extension's entry is refused where
    /// its declaration lacks `PHP_INI_USER`, as `browscap` does.
    ///
    /// ```php
    /// var_dump(ini_set('xmlrpc_errors', '1'), ini_set('short_open_tag', '1'), ini_set('sendmail_path', 'x'), ini_set('mail.log', 'x'), ini_set('SMTP', 'mx'), ini_get('sendmail_path'), ini_get('mail.cr_lf_mode'), ini_set('browscap', 'x'), ini_set('assert.warning', '1'));
    /// echo NAN;
    /// ```
    /// ```expect
    /// bool(false)
    /// bool(false)
    /// bool(false)
    /// bool(false)
    /// string(9) "localhost"
    /// string(24) "/usr/sbin/sendmail -t -i"
    /// string(4) "crlf"
    /// bool(false)
    /// string(1) "1"
    ///
    /// Warning: unexpected NAN value was coerced to string in %s on line 3
    /// NAN
    /// ```
    ///
    /// The bytes are `zend_alter_ini_entry_chars`'s: a new request
    /// string, `zend_string_init` and never an interned one, which the
    /// entry that takes it keeps and which is released where it does not
    /// (`Z/Zend/zend_ini.c:339-349`), so `ignore_user_abort(true)` costs
    /// the entry's string of one byte:
    ///
    /// ```php
    /// ini_set('from', 'x');
    /// $m = memory_get_usage(); ignore_user_abort(true); echo memory_get_usage() - $m, "\n";
    /// $m = memory_get_usage(); ignore_user_abort(false); echo memory_get_usage() - $m, "\n";
    /// ```
    /// ```expect
    /// 32
    /// 0
    /// ```
    pub fn ini_set(&mut self, name: &[u8], value: &[u8]) -> Result<Option<Vec<u8>>, Thrown> {
        let string = self.string_with(value.len(), |bytes| bytes.copy_from_slice(value))?;
        let set = self.ini_set_string(name, string);
        self.release(Value::string(string));
        set
    }

    /// [`Request::ini_set`] over a request string, `ini_set()`'s: the entry
    /// that takes it is one more owner of it, `zend_string_copy`, until
    /// its next change, `ini_restore()` or the request's end releases it
    /// (`Z/Zend/zend_ini.c:398-411`), so a string made for it stays and
    /// one a variable held outlives the variable:
    ///
    /// ```php
    /// ini_set('from', 'x');
    /// $m = memory_get_usage(); ini_set('precision', str_repeat('1', 2)); echo memory_get_usage() - $m, "\n";
    /// $v = str_repeat('a', 40); $m = memory_get_usage(); ini_set('user_agent', $v); unset($v); echo memory_get_usage() - $m, "\n";
    /// $m = memory_get_usage(); ini_set('precision', '14'); ini_restore('user_agent'); echo memory_get_usage() - $m, "\n";
    /// ```
    /// ```expect
    /// 40
    /// 0
    /// -120
    /// ```
    ///
    /// The caller keeps its own owner: a native hands the string it was
    /// passed, as `set_include_path()`, `mb_language()` and
    /// `session_name()` hand theirs to `zend_alter_ini_entry`.
    pub fn ini_set_string(
        &mut self,
        name: &[u8],
        value: PhpString,
    ) -> Result<Option<Vec<u8>>, Thrown> {
        let allowed =
            Request::ini_modifiable(name).0 && self.ini_user(name) && !self.ini_locked(name);
        self.ini_alter(name, Some(value.as_bytes()), allowed, Some(value))
    }

    /// The host's entries for this request, each as its
    /// [`crate::engine::IniOrigin`] says, before anything reads them.
    pub(crate) fn apply_host_ini(&mut self) {
        use crate::engine::IniOrigin;
        for (name, value, mode) in std::mem::take(&mut self.description.ini) {
            let allowed =
                mode != IniOrigin::PerDir || (self.ini_per_dir(&name) && !self.ini_locked(&name));
            let taken = matches!(
                self.ini_alter(&name, Some(&value), allowed, None),
                Ok(Some(_))
            );
            if taken && mode == IniOrigin::Admin {
                self.ini.locked.push(name);
            }
        }
    }

    /// Whether a host's `php_admin_value` holds the entry.
    fn ini_locked(&self, name: &[u8]) -> bool {
        self.shared
            .locked
            .iter()
            .chain(&self.ini.locked)
            .any(|n| n == name)
    }

    /// Whether the entry is `PHP_INI_PERDIR`: its declaration's
    /// `per_dir`, or the core table's.
    fn ini_per_dir(&self, name: &[u8]) -> bool {
        self.shared
            .extension_ini(name)
            .map_or(Request::ini_modifiable(name).1, |decl| decl.per_dir)
    }

    /// `zend_alter_ini_entry_ex` once `allowed` has answered the entry's
    /// `modifiable` for the stage, over `new_value`, `None` for php's
    /// `NULL`: the previous value as `ini_get()` reads it, or `None` where
    /// the entry does not exist, is not allowed, or refuses the value.
    /// `string` is the request string the value is, which the entry takes
    /// one more owner of where it takes the value as it is.
    fn ini_alter(
        &mut self,
        name: &[u8],
        new_value: Option<&[u8]>,
        allowed: bool,
        string: Option<PhpString>,
    ) -> Result<Option<Vec<u8>>, Thrown> {
        let Some(previous) = self.ini_get(name).map(<[u8]>::to_vec) else {
            return Ok(None);
        };
        if !allowed {
            return Ok(None);
        }
        // Changed from here, whether the rules below take the value or
        // not, as `zend_alter_ini_entry_ex` marks the entry `modified`
        // before it serves `on_modify`.
        let held = self.ini_value(name).map(<[u8]>::to_vec);
        self.ini_hold(name, held.as_deref());
        // The core handler reads its quantity before anything else it
        // does, and warns where the rule interprets it.
        if let Some(reading) = CoreQuantity::of(name) {
            self.core_quantity_warn(name, reading, new_value.unwrap_or_default());
        }
        if name == b"fiber.stack_size" && !crate::fiber::stack_size_updated(self, new_value) {
            return Ok(None);
        }
        // The core table's other rules read `NULL` as the empty string,
        // as php's do.
        let value = new_value.unwrap_or_default();
        if Request::ini_verdict(name, value) == IniVerdict::Rejected {
            return Ok(None);
        }
        if name == b"zend.assertions" && !self.assertions_updated(value) {
            return Ok(None);
        }
        if name == b"zend.script_encoding" {
            let Some(listed) = crate::multibyte::script_encoding(self, value) else {
                return Ok(None);
            };
            self.script_encodings = Some(listed);
        }
        // `OnUpdateErrorLog` (`Z/main/main.c:697`): at runtime a file
        // outside `open_basedir` is refused, `syslog` and the empty value
        // never are.
        if name == b"error_log"
            && !value.is_empty()
            && value != b"syslog"
            && self.check_open_basedir(value, true).is_err()
        {
            return Ok(None);
        }
        let expanded = if name == b"open_basedir" {
            let Some(list) = self.tighter_open_basedir(value) else {
                return Ok(None);
            };
            Some(list)
        } else {
            None
        };
        let limited;
        let value = if name == b"memory_limit" {
            match self.memory_limit_verdict(value) {
                Some(text) => {
                    limited = text;
                    limited.as_slice()
                }
                None => return Ok(None),
            }
        } else {
            value
        };
        if name == b"report_memleaks" && !Request::ini_parse_bool(value) {
            let message = "Directive 'report_memleaks' is deprecated";
            crate::api::raising::docref(self, Level::Deprecated, message);
        }
        let value = new_value.map(|_| value);
        if let Some(handler) = self.ini_handler(name)
            && !handler.serve(self, name, value)?
        {
            return Ok(None);
        }
        let string = string.filter(|s| Some(s.as_bytes()) == value);
        if let Some(string) = string {
            self.addref(&Value::string(string));
        }
        self.ini_hold_string(name, value, string);
        match name {
            b"open_basedir" => self.ini.open_basedir = expanded,
            b"max_execution_time" => self.arm_timer(),
            b"error_reporting" => self.update_error_reporting(),
            b"zend.enable_gc" => {
                self.heap
                    .gc_enable(value.is_some_and(Request::ini_parse_bool));
            }
            b"display_errors" => self.reset_display_errors_word(),
            _ => {}
        }
        Ok(Some(previous))
    }

    /// The entry `name` holding `value` in the request: in its place
    /// among the entries the request changed, or last where it had not
    /// changed it yet.
    fn ini_hold(&mut self, name: &[u8], value: Option<&[u8]>) {
        self.ini.typed.update(name, value);
        let value = value.map(<[u8]>::to_vec);
        if let Some((_, held, _)) = self
            .ini
            .changed
            .iter_mut()
            .find(|(n, ..)| n.as_slice() == name)
        {
            *held = value;
        } else {
            self.ini.changed.push((name.to_vec(), value, None));
            self.ini_modified_bytes();
        }
    }

    /// [`Request::ini_hold`] with the entry the owner `string` hands it,
    /// a request string or none where the value is no request's, and the
    /// string it held before released, as `zend_alter_ini_entry_ex`
    /// releases a value that is not `orig_value`.
    pub(super) fn ini_hold_string(
        &mut self,
        name: &[u8],
        value: Option<&[u8]>,
        string: Option<PhpString>,
    ) {
        self.ini_hold(name, value);
        let before = self
            .ini
            .changed
            .iter_mut()
            .find(|(n, ..)| n.as_slice() == name)
            .and_then(|(.., held)| std::mem::replace(held, string));
        if let Some(before) = before {
            self.release(Value::string(before));
        }
    }

    /// `BEGIN_SILENCE` over an `error_reporting` entry not yet changed:
    /// the entry marked changed, holding the value it holds, as the
    /// handler adds it to `EG(modified_ini_directives)`.
    pub(crate) fn silence_modifies_error_reporting(&mut self) {
        if !self
            .ini
            .changed
            .iter()
            .any(|(n, ..)| n == b"error_reporting")
        {
            let held = self.ini_value(b"error_reporting").map(<[u8]>::to_vec);
            self.ini_hold(b"error_reporting", held.as_deref());
        }
    }

    /// The handler an extension gave the entry `name`.
    fn ini_handler(&self, name: &[u8]) -> Option<IniHandler> {
        self.shared.extensions.iter().find_map(|e| {
            e.on_modify
                .iter()
                .find(|(n, _)| n.as_slice() == name)
                .map(|(_, h)| *h)
        })
    }

    /// `ini_restore()`, `zend_restore_ini_entry` at
    /// `ZEND_INI_STAGE_RUNTIME` (`Z/Zend/zend_ini.c:360-381`): an entry
    /// the request changed set back to the value it started with through
    /// its rules, as `ini_set()` serves them, and changed no longer, so
    /// the request's end leaves it be; one its rules refuse stays changed.
    /// An entry `PHP_INI_USER` may not touch, or a host's
    /// `php_admin_value` holds, is left as it is.
    ///
    /// ```php
    /// ini_set('session.use_only_cookies', '1');
    /// ini_restore('session.use_only_cookies');
    /// var_dump(ini_get('session.use_only_cookies'));
    /// ```
    /// ```expect regime=session-startup
    /// Deprecated: PHP Startup: Disabling session.use_only_cookies INI setting is deprecated in Unknown on line 0
    ///
    /// Deprecated: PHP Startup: Enabling session.use_trans_sid INI setting is deprecated in Unknown on line 0
    ///
    /// Deprecated: ini_restore(): Disabling session.use_only_cookies INI setting is deprecated in %s on line 3
    /// string(1) "0"
    /// ```
    pub fn ini_restore(&mut self, name: &[u8]) -> Result<(), Thrown> {
        let user = Request::ini_modifiable(name).0 && self.ini_user(name);
        if !user || self.ini_locked(name) || !self.ini.changed.iter().any(|(n, ..)| n == name) {
            return Ok(());
        }
        let original = self.shared.ini_default_value(name).map(<[u8]>::to_vec);
        if self
            .ini_alter(name, original.as_deref(), true, None)?
            .is_some()
            && let Some(at) = self.ini.changed.iter().position(|(n, ..)| n == name)
        {
            self.ini.changed.remove(at);
            self.ini_unmodified_bytes(at == self.ini.changed.len());
        }
        Ok(())
    }

    /// `zend_ini_deactivate` (`Z/Zend/zend_ini.c:190-203`), from
    /// `zend_deactivate` after `shutdown_executor`: each entry the
    /// request changed, in the order it first changed, handed back the
    /// value it started with through its extension's `on_modify` at
    /// [`IniStage::Deactivate`], `zend_restore_ini_entry_cb`, and its
    /// startup value after whatever the rule answers. What a rule raises
    /// is displayed past the `php_output` layer, inactive by then, as
    /// `PHP Request Shutdown: …` at no file; `error_reporting` restored
    /// before it holds it again, as `OnUpdateErrorReporting` sets the
    /// word:
    ///
    /// ```php
    /// var_dump(ini_set('session.use_only_cookies', '1'));
    /// echo "end\n";
    /// ```
    /// ```expect regime=session-startup
    /// Deprecated: PHP Startup: Disabling session.use_only_cookies INI setting is deprecated in Unknown on line 0
    ///
    /// Deprecated: PHP Startup: Enabling session.use_trans_sid INI setting is deprecated in Unknown on line 0
    /// string(1) "0"
    /// end
    ///
    /// Deprecated: PHP Request Shutdown: Disabling session.use_only_cookies INI setting is deprecated in Unknown on line 0
    /// ```
    ///
    /// An entry changed after `error_reporting` is restored while the
    /// request's level still holds, so it raises nothing seen:
    ///
    /// ```php
    /// ini_set('session.use_only_cookies', '1'); error_reporting(0);
    /// ```
    /// ```expect regime=session-startup
    /// Deprecated: PHP Startup: Disabling session.use_only_cookies INI setting is deprecated in Unknown on line 0
    ///
    /// Deprecated: PHP Startup: Enabling session.use_trans_sid INI setting is deprecated in Unknown on line 0
    /// ```
    pub(crate) fn ini_deactivate(&mut self) {
        self.ini.stage = IniStage::Deactivate;
        let changed: Vec<Vec<u8>> = self.ini.changed.iter().map(|(n, ..)| n.clone()).collect();
        for name in changed {
            let original = self.shared.ini_default_value(&name).map(<[u8]>::to_vec);
            // A core handler that reads a quantity reads its startup
            // value again, and warns again where the rule interprets it.
            if let Some(reading) = CoreQuantity::of(&name) {
                self.core_quantity_warn(&name, reading, original.as_deref().unwrap_or_default());
            }
            if let Some(handler) = self.ini_handler(&name) {
                // Each rule under its own `zend_try`: what it bails out
                // with leaves it, and the next entry is restored.
                handler.serve(self, &name, original.as_deref()).ok();
                self.catch_bailout();
            }
            if let Some(at) = self.ini.changed.iter().position(|(n, ..)| *n == name) {
                let (.., string) = self.ini.changed.remove(at);
                if let Some(string) = string {
                    self.release(Value::string(string));
                }
                let master = self.shared.ini_default(&name);
                self.ini.typed.update(&name, master);
            }
            if name == b"open_basedir" {
                self.ini.open_basedir = None;
            }
            if name == b"error_reporting" {
                self.update_error_reporting();
            }
        }
        self.free_modified_ini_bytes();
    }

    /// Whether an extension's entry is `PHP_INI_USER`, its declaration's
    /// `user`; an entry no extension declared is the core table's, which
    /// [`Request::ini_modifiable`] answers for.
    fn ini_user(&self, name: &[u8]) -> bool {
        self.shared.extension_ini(name).is_none_or(|decl| decl.user)
    }

    /// A core entry's `modifiable` bits as [`IniDecl`] spells them,
    /// `PHP_INI_USER`, `PHP_INI_PERDIR` and `PHP_INI_SYSTEM`; the first
    /// is what `ini_set()` asks: every entry of the table is
    /// `PHP_INI_ALL` but `xmlrpc_errors`, `sys_temp_dir`,
    /// `sendmail_path`, `syslog.facility`, `syslog.ident`,
    /// `disable_functions`, `enable_dl`, `extension_dir` and `browscap`,
    /// `PHP_INI_SYSTEM` (`Z/main/main.c:806-890`), and
    /// `short_open_tag` and the four `mail.*` beside `sendmail_path`,
    /// `PHP_INI_SYSTEM|PHP_INI_PERDIR` (`Z/main/main.c:820-871`).
    ///
    /// `disable_functions` is read once, as the engine is built, so
    /// setting it from inside is refused and takes nothing out.
    ///
    /// ```php
    /// var_dump(ini_get('disable_functions'), ini_set('disable_functions', 'strlen'), function_exists('strlen'), strlen('abc'));
    /// ```
    /// ```expect
    /// string(0) ""
    /// bool(false)
    /// bool(true)
    /// int(3)
    /// ```
    pub(crate) fn ini_modifiable(name: &[u8]) -> (bool, bool, bool) {
        match name {
            b"xmlrpc_errors"
            | b"hard_timeout"
            | b"sys_temp_dir"
            | b"sendmail_path"
            | b"syslog.facility"
            | b"syslog.ident"
            | b"disable_functions"
            | b"enable_dl"
            | b"extension_dir"
            | b"allow_url_fopen"
            | b"allow_url_include"
            | b"max_memory_limit"
            | b"browscap"
            | b"doc_root"
            | b"zend.signal_check"
            | b"user_dir"
            | b"user_ini.filename"
            | b"user_ini.cache_ttl" => (false, false, true),
            b"short_open_tag"
            | b"zend.multibyte"
            | b"output_buffering"
            | b"output_handler"
            | b"mail.add_x_header"
            | b"mail.mixed_lf_and_crlf"
            | b"mail.cr_lf_mode"
            | b"mail.log"
            | b"mail.force_extra_parameters"
            | b"auto_prepend_file"
            | b"auto_append_file" => (false, true, true),
            _ => (true, true, true),
        }
    }

    /// What a core entry's `on_modify` says of a new value before
    /// anything is written, `zend_alter_ini_entry_ex` asking it first
    /// (`Z/Zend/zend_ini.c:342`): `zend.exception_string_param_max_len`
    /// is `OnSetExceptionStringParamMaxLen` (`Z/Zend/zend.c:175-184`),
    /// which reads the text with `ZEND_ATOL` and refuses a number outside
    /// `0..=1000000`; the rest of the table accepts anything. A refusal
    /// keeps the entry as it was and `ini_set()` answers `false`.
    ///
    /// ```php
    /// var_dump(ini_set("zend.exception_string_param_max_len", "-1"), ini_set("zend.exception_string_param_max_len", "1000001"), ini_set("zend.exception_string_param_max_len", "3"), ini_get("zend.exception_string_param_max_len"));
    /// ```
    /// ```expect
    /// bool(false)
    /// bool(false)
    /// string(2) "15"
    /// string(1) "3"
    /// ```
    ///
    /// `ZEND_ATOL` is `atoll`, so text that is not a number is zero and
    /// accepted, and the entry keeps the text it was given.
    ///
    /// ```php
    /// var_dump(ini_set("zend.exception_string_param_max_len", "abc"), ini_get("zend.exception_string_param_max_len"), ini_set("zend.exception_string_param_max_len", "99999999999999999999"));
    /// ```
    /// ```expect
    /// string(2) "15"
    /// string(3) "abc"
    /// bool(false)
    /// ```
    ///
    /// `default_charset` and `default_mimetype` refuse a value holding a
    /// NUL, a carriage return or a line feed (`OnUpdateDefaultCharset`,
    /// `OnUpdateDefaultMimeTye`, `Z/main/main.c:605-631`), which the
    /// default `Content-type` would carry into the head.
    ///
    /// ```php
    /// var_dump(ini_set("default_charset", "UTF-8\r\nX-Injected: 1"), ini_set("default_mimetype", "text/plain\n"), ini_get("default_mimetype"));
    /// ```
    /// ```expect
    /// bool(false)
    /// bool(false)
    /// string(9) "text/html"
    /// ```
    ///
    /// `arg_separator.output` and `arg_separator.input` are
    /// `OnUpdateStrNotEmpty` (`Z/main/main.c:824-825`): the empty value
    /// is refused.
    ///
    /// ```php
    /// var_dump(ini_set("arg_separator.output", ""), ini_get("arg_separator.output"), ini_set("arg_separator.output", ";"));
    /// ```
    /// ```expect
    /// bool(false)
    /// string(1) "&"
    /// string(1) "&"
    /// ```
    ///
    /// `OnUpdateStringUnempty`'s entries (`Z/main/main.c:829-851`)
    /// refuse the empty value too, at startup as at `ini_set`, and keep
    /// what they held: `-d extension_dir=` leaves `PHP_EXTENSION_DIR`,
    /// where an `extension=` line looks.
    ///
    /// ```php
    /// var_dump(ini_set('include_path', ''), ini_get('include_path') !== '');
    /// $p = proc_open([PHP_BINARY, '-n', '-d', 'extension_dir=', '-d', 'variables_order=', '-d', 'extension=x', '-r', 'var_dump(ini_get("extension_dir") === PHP_EXTENSION_DIR, ini_get("variables_order"));'], [1 => ['pipe', 'w'], 2 => ['pipe', 'w']], $pipes);
    /// $out = stream_get_contents($pipes[1]); echo stream_get_contents($pipes[2]), proc_close($p), "\n";
    /// preg_match('/^Warning: (.+) \(tried: (\S+) \(.+?\), (\/\S+) \(.+\)\) in Unknown on line 0$/m', $out, $m);
    /// echo str_replace(PHP_EXTENSION_DIR, 'DIR', "$m[1]: $m[2], $m[3]\n"), substr($out, strpos($out, 'bool'));
    /// ```
    /// ```expect process
    /// bool(false)
    /// bool(true)
    /// 0
    /// PHP Startup: Unable to load dynamic library 'x': DIR/x, DIR/x.so
    /// bool(true)
    /// string(5) "EGPCS"
    /// ```
    pub(crate) fn ini_verdict(name: &[u8], value: &[u8]) -> IniVerdict {
        match name {
            // `OnSetPrecision` and `OnSetSerializePrecision`
            // (`Z/main/main.c:120-141`): nothing below -1.
            b"precision" | b"serialize_precision" if strtol(value) < -1 => IniVerdict::Rejected,
            b"arg_separator.output"
            | b"arg_separator.input"
            | b"extension_dir"
            | b"include_path"
            | b"variables_order"
            | b"doc_root"
            | b"sys_temp_dir"
            | b"upload_tmp_dir"
                if value.is_empty() =>
            {
                IniVerdict::Rejected
            }
            b"zend.exception_string_param_max_len" if !(0..=1_000_000).contains(&strtol(value)) => {
                IniVerdict::Rejected
            }
            b"default_charset" | b"default_mimetype"
                if value.iter().any(|b| matches!(b, 0 | b'\r' | b'\n')) =>
            {
                IniVerdict::Rejected
            }
            _ if CoreQuantity::of(name).is_some_and(|reading| reading.refuses(value)) => {
                IniVerdict::Rejected
            }
            _ => IniVerdict::Accepted,
        }
    }

    /// `EG(ini_directives)`, what `ini_get_all()` walks: the engine's
    /// own entries, registered with `module_number` zero before any
    /// `MINIT`, then each extension's in bundle order, each as
    /// [`Request::extension_ini`] reads one. A core entry's `modifiable`
    /// bits are the table's ([`Request::ini_modifiable`]).
    ///
    /// ```php
    /// ini_set('precision', '10');
    /// var_dump(ini_get_all(null)['precision'], ini_get_all(null)['enable_dl'], ini_get_all('pcre', false));
    /// ```
    /// ```expect
    /// array(3) {
    ///   ["global_value"]=>
    ///   string(2) "14"
    ///   ["local_value"]=>
    ///   string(2) "10"
    ///   ["access"]=>
    ///   int(7)
    /// }
    /// array(3) {
    ///   ["global_value"]=>
    ///   string(1) "1"
    ///   ["local_value"]=>
    ///   string(1) "1"
    ///   ["access"]=>
    ///   int(4)
    /// }
    /// array(3) {
    ///   ["pcre.backtrack_limit"]=>
    ///   string(7) "1000000"
    ///   ["pcre.jit"]=>
    ///   string(1) "1"
    ///   ["pcre.recursion_limit"]=>
    ///   string(6) "100000"
    /// }
    /// ```
    #[must_use]
    pub fn ini_entries(&self) -> Vec<IniEntry> {
        let entries = (0..self.shared.extensions.len())
            .filter_map(|at| u32::try_from(at).ok())
            .flat_map(|at| self.registered_ini(ExtensionId(at)))
            .collect();
        self.in_table_order(entries)
    }

    /// `zend_ini_sort_entries()` (`Z/Zend/zend_ini.c:195`), which
    /// `phpinfo()` runs whatever it prints and `ini_get_all()` runs
    /// before it looks the extension up: the table sorted by
    /// [`ini_key_compare`], so every walk of it from here to the
    /// request's end lists its entries sorted by name ([`IniOrder`]).
    ///
    /// ```php
    /// $names = fn () => implode(' ', array_keys((new ReflectionExtension('iconv'))->getINIEntries())) . "\n";
    /// echo $names(); @ini_get_all('nope'); echo $names();
    /// ```
    /// ```expect
    /// iconv.input_encoding iconv.output_encoding iconv.internal_encoding
    /// iconv.input_encoding iconv.internal_encoding iconv.output_encoding
    /// ```
    pub fn ini_sort_entries(&mut self) {
        self.ini.order = IniOrder::Sorted;
    }

    /// Entries handed over in registration order, put in the order the
    /// table holds them now ([`IniOrder`]).
    fn in_table_order(&self, mut entries: Vec<IniEntry>) -> Vec<IniEntry> {
        self.ini
            .order
            .arrange(&mut entries, |entry| &entry.decl.name.0);
        entries
    }

    /// `zend_ini_parse_bool` (`Z/Zend/zend_ini.c:563`), the one rule
    /// every `OnUpdateBool` entry is read by: `on`, `yes` and `true` in
    /// any case are on, and any other text is on where C's `atoi` reads
    /// a number other than zero from it, so `abc` and `00` are off,
    /// `-1`, ` 1` and `1abc` are on, and `4294967296`, whose `int` is
    /// zero, is off.
    ///
    /// ```php
    /// foreach (['abc', '00', '4294967296', '-1', '1abc', ' 1', 'TRUE'] as $v) { ini_set('html_errors', $v); trigger_error($v); }
    /// ```
    /// ```expect
    /// Notice: abc in %s on line 2
    ///
    /// Notice: 00 in %s on line 2
    ///
    /// Notice: 4294967296 in %s on line 2
    /// <br />
    /// <b>Notice</b>:  -1 in <b>%s</b> on line <b>2</b><br />
    /// <br />
    /// <b>Notice</b>:  1abc in <b>%s</b> on line <b>2</b><br />
    /// <br />
    /// <b>Notice</b>:   1 in <b>%s</b> on line <b>2</b><br />
    /// <br />
    /// <b>Notice</b>:  TRUE in <b>%s</b> on line <b>2</b><br />
    /// ```
    #[must_use]
    pub fn ini_parse_bool(value: &[u8]) -> bool {
        value.eq_ignore_ascii_case(b"true")
            || value.eq_ignore_ascii_case(b"yes")
            || value.eq_ignore_ascii_case(b"on")
            || atoi(value) != 0
    }

    /// The entry `name` read as its `OnUpdateBool` reads it, by
    /// [`Request::ini_parse_bool`]; an entry that holds php's `NULL` or
    /// is not registered is off.
    pub(crate) fn ini_flag(&self, name: &[u8]) -> bool {
        self.ini_get(name).is_some_and(Request::ini_parse_bool)
    }

    /// The entry `name` as its `OnUpdateLong` or `OnUpdateLongGEZero`
    /// stored it in `PG`, `SG` or `EG`, `zend_ini_parse_quantity`'s value
    /// ([`crate::ini_quantity`]): `1K` is 1024 and `0x10` is 16. What the
    /// rule interprets for backwards compatibility was raised when the
    /// value was set ([`Request::ini_parse_quantity_warn`]), and a
    /// negative an `OnUpdateLongGEZero` entry refused never reached it.
    /// What an extension reads an entry it declared with
    /// [`LongHandler`] by, where php's reads the field the handler wrote.
    ///
    /// ```php
    /// $run = function (string ...$ini) {
    ///     $args = [PHP_BINARY, '-n'];
    ///     foreach ($ini as $set) { $args[] = '-d'; $args[] = $set; }
    ///     $args[] = '-r';
    ///     $args[] = '$s = ob_get_status(); echo $s["chunk_size"] ?? "none", " ", ini_get("max_input_vars"), " ", ini_get("max_input_nesting_level"), "\n";';
    ///     $p = proc_open($args, [1 => ['pipe', 'w'], 2 => ['pipe', 'w']], $pipes);
    ///     echo stream_get_contents($pipes[1]), stream_get_contents($pipes[2]), proc_close($p), "\n";
    /// };
    /// $run('output_buffering=1K');
    /// $run('output_buffering=0x10', 'max_input_vars=-5x', 'max_input_nesting_level=-0x1');
    /// $run('report_memleaks=0', 'max_input_nesting_level=3q', 'output_buffering=1Kx', 'max_input_time=2x', 'hard_timeout=-');
    /// ```
    /// ```expect process
    /// 1024 1000 64
    /// 0
    ///
    /// Warning: Invalid "max_input_vars" setting. Invalid quantity "-5x": unknown multiplier "x", interpreting as "-5" for backwards compatibility in Unknown on line 0
    /// 16 1000 64
    /// 0
    ///
    /// Warning: Invalid "max_input_time" setting. Invalid quantity "2x": unknown multiplier "x", interpreting as "2" for backwards compatibility in Unknown on line 0
    ///
    /// Deprecated: PHP Startup: Directive 'report_memleaks' is deprecated in Unknown on line 0
    ///
    /// Warning: Invalid "output_buffering" setting. Invalid quantity "1Kx": unknown multiplier "x", interpreting as "1" for backwards compatibility in Unknown on line 0
    ///
    /// Warning: Invalid "max_input_nesting_level" setting. Invalid quantity "3q": unknown multiplier "q", interpreting as "3" for backwards compatibility in Unknown on line 0
    ///
    /// Warning: Invalid "hard_timeout" setting. Invalid quantity "-": no valid leading digits, interpreting as "0" for backwards compatibility in Unknown on line 0
    /// 0 1000 3q
    /// 0
    /// ```
    #[must_use]
    pub fn ini_long(&self, name: &[u8]) -> i64 {
        self.ini_get(name).map_or(0, Request::ini_parse_bytes)
    }

    /// `INI_INT(name)`, `zend_ini_long` (`Z/Zend/zend_ini.c:459`): an
    /// entry with no handler, `max_file_uploads` and
    /// `max_multipart_body_parts`, read where it is used with
    /// `ZEND_STRTOL(value, NULL, 0)`, so `0x10` is 16 and `010` is 8.
    pub(crate) fn ini_int(&self, name: &[u8]) -> i64 {
        strtol_prefixed(self.ini_get(name).unwrap_or_default())
    }

    /// `zend_ini_parse_quantity_warn` (`Z/Zend/zend_ini.c:859`) for the
    /// entry `name` taking `value`: the quantity [`Request::ini_long`]
    /// reads, and php's `Invalid "name" setting.` warning before the
    /// rule's sentence where the rule interprets the value for backwards
    /// compatibility, at `Unknown` on line 0 while the engine starts and
    /// as the request ends, and where the program stands between. What
    /// an entry declared with [`LongHandler`] is read by, and what a
    /// handler an extension wrote reads its whole-number entry by, as
    /// `OnUpdateSessionGcProbability` and `OnUpdateBacktrackLimit` do.
    ///
    /// ```php
    /// var_dump(ini_set('error_log_mode', '0600 '), ini_set('error_log_mode', '7x'), ini_get('error_log_mode'));
    /// ```
    /// ```expect
    /// Warning: Invalid "error_log_mode" setting. Invalid quantity "7x": unknown multiplier "x", interpreting as "7" for backwards compatibility in %s on line 2
    /// string(4) "0644"
    /// string(5) "0600 "
    /// string(2) "7x"
    /// ```
    ///
    /// The request's end restores what it changed through the same
    /// handler, which reads the startup value again:
    ///
    /// ```php
    /// $p = proc_open([PHP_BINARY, '-n', '-d', 'error_log_mode=7x', '-r', 'ini_set("error_log_mode", "0600"); echo "end\n";'], [1 => ['pipe', 'w'], 2 => ['pipe', 'w']], $pipes);
    /// echo stream_get_contents($pipes[1]), stream_get_contents($pipes[2]), proc_close($p), "\n";
    /// ```
    /// ```expect process
    /// Warning: Invalid "error_log_mode" setting. Invalid quantity "7x": unknown multiplier "x", interpreting as "7" for backwards compatibility in Unknown on line 0
    /// end
    ///
    /// Warning: Invalid "error_log_mode" setting. Invalid quantity "7x": unknown multiplier "x", interpreting as "7" for backwards compatibility in Unknown on line 0
    /// 0
    /// ```
    pub fn ini_parse_quantity_warn(&mut self, name: &[u8], value: &[u8]) -> i64 {
        let (quantity, sentence) = crate::ini_quantity::parse(value);
        self.ini_setting_warn(name, sentence);
        quantity
    }

    /// `zend_ini_parse_uquantity_warn` (`Z/Zend/zend_ini.c:873`):
    /// [`Request::ini_parse_quantity_warn`] under the unsigned rule,
    /// where `-1` alone among the negatives is taken, as the maximum, and
    /// any other is out of range. `memory_limit`'s handler reads by it
    /// before it sets the limit, at startup, at `ini_set()` and as the
    /// request ends:
    ///
    /// ```php
    /// $code = 'echo ini_get("memory_limit"), "\n"; var_dump(ini_set("memory_limit", "200Mq"), ini_set("memory_limit", "-2"), ini_set("memory_limit", "256M")); echo "end\n";';
    /// $p = proc_open([PHP_BINARY, '-n', '-d', 'memory_limit=0x10000000x', '-r', $code], [1 => ['pipe', 'w'], 2 => ['pipe', 'w']], $pipes);
    /// echo stream_get_contents($pipes[1]), stream_get_contents($pipes[2]), proc_close($p), "\n";
    /// $p = proc_open([PHP_BINARY, '-n', '-d', 'memory_limit=100Mx', '-r', 'echo ini_get("memory_limit"), "\n";'], [1 => ['pipe', 'w'], 2 => ['pipe', 'w']], $pipes);
    /// echo stream_get_contents($pipes[1]), stream_get_contents($pipes[2]), proc_close($p), "\n";
    /// ```
    /// ```expect process
    /// Warning: Invalid "memory_limit" setting. Invalid quantity "0x10000000x": unknown multiplier "x", interpreting as "0x10000000" for backwards compatibility in Unknown on line 0
    /// 0x10000000x
    ///
    /// Warning: Invalid "memory_limit" setting. Invalid quantity "200Mq": unknown multiplier "q", interpreting as "200" for backwards compatibility in Command line code on line 1
    ///
    /// Warning: Failed to set memory limit to 200 bytes (Current memory usage is 2097152 bytes) in Command line code on line 1
    ///
    /// Warning: Invalid "memory_limit" setting. Invalid quantity "-2": value is out of range, using overflow result for backwards compatibility in Command line code on line 1
    ///
    /// Warning: Failed to set memory limit to 2 bytes (Current memory usage is 2097152 bytes) in Command line code on line 1
    /// bool(false)
    /// bool(false)
    /// string(11) "0x10000000x"
    /// end
    ///
    /// Warning: Invalid "memory_limit" setting. Invalid quantity "0x10000000x": unknown multiplier "x", interpreting as "0x10000000" for backwards compatibility in Unknown on line 0
    /// 0
    ///
    /// Warning: Invalid "memory_limit" setting. Invalid quantity "100Mx": unknown multiplier "x", interpreting as "100" for backwards compatibility in Unknown on line 0
    ///
    /// Warning: Failed to set memory limit to 100 bytes (Current memory usage is 2097152 bytes) in Unknown on line 0
    /// 128M
    /// 0
    /// ```
    pub(crate) fn ini_parse_uquantity_warn(&mut self, name: &[u8], value: &[u8]) -> u64 {
        let (quantity, sentence) = crate::ini_quantity::parse_unsigned(value);
        self.ini_setting_warn(name, sentence);
        quantity
    }

    /// The core entry `name` taking `value` read as its handler reads it
    /// first ([`CoreQuantity`]), with the warning. `zend.assertions`'
    /// `OnUpdateAssertions` warns at startup, after `main.c`'s entries,
    /// at `ini_set()`, before it refuses a move to or from a negative,
    /// and as the request ends:
    ///
    /// ```php
    /// $code = 'echo ini_get("zend.assertions"), "\n"; var_dump(ini_set("zend.assertions", "1y"), ini_set("zend.assertions", "0z"), ini_set("zend.assertions", "-1x")); echo "end\n";';
    /// $p = proc_open([PHP_BINARY, '-n', '-d', 'zend.assertions=0x', '-d', 'max_input_time=2x', '-r', $code], [1 => ['pipe', 'w'], 2 => ['pipe', 'w']], $pipes);
    /// echo stream_get_contents($pipes[1]), stream_get_contents($pipes[2]), proc_close($p), "\n";
    /// ```
    /// ```expect process
    /// Warning: Invalid "max_input_time" setting. Invalid quantity "2x": unknown multiplier "x", interpreting as "2" for backwards compatibility in Unknown on line 0
    ///
    /// Warning: Invalid "zend.assertions" setting. Invalid quantity "0x": no digits after base prefix, interpreting as "0" for backwards compatibility in Unknown on line 0
    /// 0x
    ///
    /// Warning: Invalid "zend.assertions" setting. Invalid quantity "1y": unknown multiplier "y", interpreting as "1" for backwards compatibility in Command line code on line 1
    ///
    /// Warning: Invalid "zend.assertions" setting. Invalid prefix "0z", interpreting as "0" for backwards compatibility in Command line code on line 1
    ///
    /// Warning: Invalid "zend.assertions" setting. Invalid quantity "-1x": unknown multiplier "x", interpreting as "-1" for backwards compatibility in Command line code on line 1
    ///
    /// Warning: zend.assertions may be completely enabled or disabled only in php.ini in Command line code on line 1
    /// string(2) "0x"
    /// string(2) "1y"
    /// bool(false)
    /// end
    ///
    /// Warning: Invalid "zend.assertions" setting. Invalid quantity "0x": no digits after base prefix, interpreting as "0" for backwards compatibility in Unknown on line 0
    /// 0
    /// ```
    pub(crate) fn core_quantity_warn(&mut self, name: &[u8], reading: CoreQuantity, value: &[u8]) {
        match reading {
            CoreQuantity::Signed(_) => {
                self.ini_parse_quantity_warn(name, value);
            }
            CoreQuantity::Unsigned => {
                self.ini_parse_uquantity_warn(name, value);
            }
        }
    }

    /// php's `Invalid "name" setting.` warning before the quantity
    /// rule's sentence, where it said one.
    fn ini_setting_warn(&mut self, name: &[u8], sentence: Option<String>) {
        if let Some(sentence) = sentence {
            let name = String::from_utf8_lossy(name);
            let message = format!("Invalid \"{name}\" setting. {sentence}");
            crate::api::raising::warning(self, &message);
        }
    }

    /// `cfg_get_entry` as `get_cfg_var()` hands it back
    /// (`Z/ext/standard/basic_functions.c:1306`): the row of
    /// `configuration_hash` under `name`, what the host's ini text set
    /// before any entry read it, a string or the array its `name[]=`
    /// lines built, as strings of the request's; `None` where the text
    /// set nothing under the name, a default no text named included.
    ///
    /// ```php
    /// var_dump(get_cfg_var('precision'), get_cfg_var('highlight.string'), get_cfg_var('nope'));
    /// ```
    /// ```expect
    /// string(2) "14"
    /// bool(false)
    /// bool(false)
    /// ```
    #[cfg_attr(debug_assertions, track_caller)]
    pub fn cfg_entry(&mut self, name: &[u8]) -> Result<Option<Owner<Value>>, Thrown> {
        let made = self.cfg_entry_made(name);
        adopted_some(made)
    }

    /// What [`cfg_entry`](Self::cfg_entry) makes, its one owner not yet named.
    pub(crate) fn cfg_entry_made(&mut self, name: &[u8]) -> Result<Option<Value>, Thrown> {
        let shared = Arc::clone(&self.shared);
        let Some(entry) = shared.configured(name) else {
            return Ok(None);
        };
        self.configured_value(entry).map(Some)
    }

    /// `add_config_entries`: a row of the table as a value of the
    /// request, an array's arrays copied as deep as they go.
    fn configured_value(&mut self, entry: &Configured) -> Result<Value, Thrown> {
        Ok(match entry {
            Configured::Str(bytes) => Value::string(self.string(bytes)?),
            Configured::Array(rows) => {
                let mut array = self.array()?;
                for (key, entry) in rows {
                    let key = match key {
                        ConfiguredKey::Index(n) => Owner::index(*n),
                        ConfiguredKey::Name(name) => PhpString::new(self, name)?.into_key(),
                    };
                    let value = self.configured_value(entry)?;
                    let value = Owner::adopt(value);
                    array.set(self, key, value)?;
                }
                Value::array(array)
            }
        })
    }

    /// `zend_ini_parse_quantity`'s value alone, what `OnUpdateLong`
    /// stores for a whole-number entry: [`Request::ini_parse_quantity`]
    /// with the sentence it would say left unsaid.
    #[must_use]
    pub fn ini_parse_bytes(value: &[u8]) -> i64 {
        crate::ini_quantity::parse(value).0
    }

    /// `zend_ini_parse_quantity` (`Z/Zend/zend_ini.c:613-841`): leading
    /// and trailing whitespace ignored, an optional sign, a base prefix
    /// (`0x`, `0o`, `0b`) or `strtoull`'s own reading under base zero,
    /// where a leading `0` is octal, whitespace, and an optional `K`,
    /// `M` or `G` read off the last byte; with the value, the sentence
    /// php's `errstr` holds for a shape it interprets for backwards
    /// compatibility, which `ini_parse_quantity()` raises as a warning
    /// and an entry's `on_modify` as its own.
    ///
    /// ```php
    /// foreach (['1M', ' 2k ', '0x10', '010', '0b101', '-0x0B', '', '1.5M', 'abc', '0x', '0z1', '5 KB', ' 3 KB', '99999999999999999999'] as $q) var_dump(ini_parse_quantity($q));
    /// ```
    /// ```expect
    /// int(1048576)
    /// int(2048)
    /// int(16)
    /// int(8)
    /// int(5)
    /// int(-11)
    /// int(0)
    ///
    /// Warning: Invalid quantity "1.5M", interpreting as "1M" for backwards compatibility in %s on line 2
    /// int(1048576)
    ///
    /// Warning: Invalid quantity "abc": no valid leading digits, interpreting as "0" for backwards compatibility in %s on line 2
    /// int(0)
    ///
    /// Warning: Invalid quantity "0x": no digits after base prefix, interpreting as "0" for backwards compatibility in %s on line 2
    /// int(0)
    ///
    /// Warning: Invalid prefix "0z", interpreting as "0" for backwards compatibility in %s on line 2
    /// int(0)
    ///
    /// Warning: Invalid quantity "5 KB": unknown multiplier "B", interpreting as "5 " for backwards compatibility in %s on line 2
    /// int(5)
    ///
    /// Warning: Invalid quantity " 3 KB": unknown multiplier "B", interpreting as " 3 " for backwards compatibility in %s on line 2
    /// int(3)
    ///
    /// Warning: Invalid quantity "99999999999999999999": value is out of range, using overflow result for backwards compatibility in %s on line 2
    /// int(-1)
    /// ```
    #[must_use]
    pub fn ini_parse_quantity(value: &[u8]) -> (i64, Option<String>) {
        crate::ini_quantity::parse(value)
    }

    /// `zend_ini_parse_uquantity`: [`Request::ini_parse_quantity`] held
    /// to the unsigned range, where `-1` alone among the negatives is
    /// taken, as the maximum, and any other is out of range. What the
    /// test extension's `zend_test_zend_ini_parse_uquantity` answers,
    /// the witness there.
    #[must_use]
    pub fn ini_parse_uquantity(value: &[u8]) -> (u64, Option<String>) {
        crate::ini_quantity::parse_unsigned(value)
    }

    /// The stage an entry's `on_modify` is served at, php's `stage`
    /// argument to a `PHP_INI_MH`. [`IniStage::Startup`] is also
    /// `php_during_module_startup()`, which `php_verror` names the origin
    /// of a docref by, `PHP Startup` for a native's name.
    #[must_use]
    pub fn ini_stage(&self) -> IniStage {
        self.ini.stage
    }

    /// The stage the next `on_modify` is served at.
    pub(crate) fn set_ini_stage(&mut self, stage: IniStage) {
        self.ini.stage = stage;
    }

    /// A startup value an entry's handler took, in force for the handlers
    /// after it, as `zend_register_ini_entries_ex` sets each entry's
    /// value before it walks to the next: `opcache.log_verbosity_level`
    /// decides what opcache's handlers after it log.
    pub(crate) fn take_startup_value(&mut self, name: &[u8], value: &[u8]) {
        self.ini_hold(name, Some(value));
    }

    /// The ini entries an extension declared, in the order
    /// `EG(ini_directives)` holds them ([`IniOrder`]), which is the
    /// order `zend_register_ini_entries` put them in until the request
    /// sorts the table. Each carries the value in force, the request's
    /// `ini_set` over the engine's startup value, and the startup value
    /// again as `original` where the request changed it.
    #[must_use]
    pub fn extension_ini(&self, extension: ExtensionId) -> Vec<IniEntry> {
        self.in_table_order(self.registered_ini(extension))
    }

    /// An extension's entries in the order it declared them.
    fn registered_ini(&self, extension: ExtensionId) -> Vec<IniEntry> {
        if extension == ExtensionId::CORE {
            return self.core_ini();
        }
        let Some(module) = self.extension_of(extension) else {
            return Vec::new();
        };
        module
            .ini
            .iter()
            .map(|decl| self.ini_entry(decl.clone(), extension))
            .collect()
    }

    /// `Core`'s entries, `main.c`'s and `zend.c`'s table: every entry of
    /// the engine's no extension declared.
    fn core_ini(&self) -> Vec<IniEntry> {
        self.shared
            .ini_defaults()
            .iter()
            .filter(|(name, _)| self.shared.extension_ini(name).is_none())
            .map(|(name, default)| {
                let (user, per_dir, system) = Request::ini_modifiable(name);
                let decl = IniDecl {
                    name: Name(name.as_slice().into()),
                    default: default.as_deref().map(Into::into),
                    user,
                    per_dir,
                    system,
                };
                self.ini_entry(decl, ExtensionId::CORE)
            })
            .collect()
    }

    /// One entry as the request sees it: the value in force, the
    /// request's `ini_set` over the engine's startup value, the startup
    /// value, and whether the request changed it.
    fn ini_entry(&self, decl: IniDecl, extension: ExtensionId) -> IniEntry {
        let name = decl.name.0.as_ref();
        let master = self
            .shared
            .ini_defaults()
            .iter()
            .find(|(n, _)| n.as_slice() == name)
            .map_or_else(
                || decl.default.clone(),
                |(_, v)| v.as_deref().map(Into::into),
            );
        let changed = self.ini.changed.iter().find(|(n, ..)| n.as_slice() == name);
        IniEntry {
            value: changed.map_or_else(|| master.clone(), |(_, v, _)| v.as_deref().map(Into::into)),
            master,
            modified: changed.is_some(),
            decl,
            extension,
        }
    }

    /// `DISPLAY_INI_ENTRIES()`, `display_ini_entries`
    /// (`Z/main/php_ini.c:115`), for the entries named, as an
    /// extension's `MINFO` ends with it: each with the request's value and
    /// the engine's default ([`Request::info_ini_table`]).
    ///
    /// ```php
    /// @ini_set('iconv.output_encoding', 'ISO-8859-1');
    /// $e = new ReflectionExtension('iconv'); ob_start(); $e->info(); echo strstr(ob_get_clean(), 'Directive');
    /// ```
    /// ```expect
    /// Directive => Local Value => Master Value
    /// iconv.input_encoding => no value => no value
    /// iconv.output_encoding => ISO-8859-1 => no value
    /// iconv.internal_encoding => no value => no value
    /// ```
    pub fn info_ini_entries(&mut self, names: &[&[u8]]) {
        let rows: Vec<[String; 3]> = names
            .iter()
            .map(|name| {
                let shown = |value: Option<&[u8]>| {
                    String::from_utf8_lossy(value.unwrap_or_default()).into_owned()
                };
                [
                    String::from_utf8_lossy(name).into_owned(),
                    shown(self.ini_get(name)),
                    shown(self.shared.ini_default(name)),
                ]
            })
            .collect();
        let rows: Vec<[&str; 3]> = rows
            .iter()
            .map(|[name, local, master]| [name.as_str(), local.as_str(), master.as_str()])
            .collect();
        self.info_ini_table(&rows);
    }

    /// `display_ini_entries(NULL)` (`Z/main/php_ini.c:131`): the entries
    /// no extension declared, the engine's own, in the table's order,
    /// which `phpinfo()` has sorted by name before it prints them
    /// ([`Request::ini_sort_entries`]), each value through the entry's
    /// displayer ([`IniDisplayer`]). What `PHP_MINFO_FUNCTION(php_core)` and
    /// `phpinfo()`'s `PHP Core` print.
    ///
    /// ```php
    /// ob_start(); phpinfo(INFO_CONFIGURATION); $page = ob_get_clean(); echo strstr($page, "PHP Core");
    /// ```
    /// ```expect
    /// PHP Core
    ///
    /// Directive => Local Value => Master Value
    /// allow_url_fopen => On => On
    /// allow_url_include => Off => Off
    /// arg_separator.input => & => &
    /// arg_separator.output => & => &
    /// auto_append_file => no value => no value
    /// auto_globals_jit => On => On
    /// auto_prepend_file => no value => no value
    /// browscap => no value => no value
    /// default_charset => UTF-8 => UTF-8
    /// default_mimetype => text/html => text/html
    /// disable_functions => no value => no value
    /// display_errors => STDOUT => STDOUT
    /// display_startup_errors => On => On
    /// doc_root => no value => no value
    /// docref_ext => .html => .html
    /// docref_root => no value => no value
    /// enable_dl => On => On
    /// enable_post_data_reading => On => On
    /// error_append_string => no value => no value
    /// error_log => no value => no value
    /// error_log_mode => 0644 => 0644
    /// error_prepend_string => no value => no value
    /// error_reporting => 30719 => 30719
    /// expose_php => On => On
    /// extension_dir => %s => %s
    /// fatal_error_backtraces => Off => Off
    /// fiber.stack_size => no value => no value
    /// file_uploads => On => On
    /// hard_timeout => 2 => 2
    /// highlight.comment => <span style="color: #FF8000">#FF8000</span> => <span style="color: #FF8000">#FF8000</span>
    /// highlight.default => <span style="color: #0000BB">#0000BB</span> => <span style="color: #0000BB">#0000BB</span>
    /// highlight.html => <span style="color: #000000">#000000</span> => <span style="color: #000000">#000000</span>
    /// highlight.keyword => <span style="color: #007700">#007700</span> => <span style="color: #007700">#007700</span>
    /// highlight.string => <span style="color: #DD0000">#DD0000</span> => <span style="color: #DD0000">#DD0000</span>
    /// html_errors => Off => Off
    /// ignore_repeated_errors => Off => Off
    /// ignore_repeated_source => Off => Off
    /// ignore_user_abort => Off => Off
    /// implicit_flush => On => On
    /// include_path => %s => %s
    /// input_encoding => no value => no value
    /// internal_encoding => no value => no value
    /// log_errors => Off => Off
    /// mail.add_x_header => Off => Off
    /// mail.cr_lf_mode => crlf => crlf
    /// mail.force_extra_parameters => no value => no value
    /// mail.log => no value => no value
    /// mail.mixed_lf_and_crlf => Off => Off
    /// max_execution_time => 0 => 0
    /// max_file_uploads => 20 => 20
    /// max_input_nesting_level => 64 => 64
    /// max_input_time => -1 => -1
    /// max_input_vars => 1000 => 1000
    /// max_memory_limit => -1 => -1
    /// max_multipart_body_parts => -1 => -1
    /// memory_limit => 128M => 128M
    /// open_basedir => no value => no value
    /// output_buffering => no value => no value
    /// output_encoding => no value => no value
    /// output_handler => no value => no value
    /// post_max_size => 8M => 8M
    /// precision => 14 => 14
    /// realpath_cache_size => 4096K => 4096K
    /// realpath_cache_ttl => 120 => 120
    /// register_argc_argv => Off => Off
    /// report_memleaks => On => On
    /// report_zend_debug => Off => Off
    /// request_order => no value => no value
    /// sendmail_from => no value => no value
    /// sendmail_path => /usr/sbin/sendmail -t -i => /usr/sbin/sendmail -t -i
    /// serialize_precision => -1 => -1
    /// short_open_tag => Off => Off
    /// SMTP => localhost => localhost
    /// smtp_port => 25 => 25
    /// sys_temp_dir => no value => no value
    /// syslog.facility => LOG_USER => LOG_USER
    /// syslog.filter => no-ctrl => no-ctrl
    /// syslog.ident => php => php
    /// unserialize_callback_func => no value => no value
    /// upload_max_filesize => 2M => 2M
    /// upload_tmp_dir => no value => no value
    /// user_dir => no value => no value
    /// user_ini.cache_ttl => 300 => 300
    /// user_ini.filename => .user.ini => .user.ini
    /// variables_order => EGPCS => EGPCS
    /// xmlrpc_error_number => 0 => 0
    /// xmlrpc_errors => Off => Off
    /// zend.assertions => 1 => 1
    /// zend.detect_unicode => On => On
    /// zend.enable_gc => On => On
    /// zend.exception_ignore_args => Off => Off
    /// zend.exception_string_param_max_len => 15 => 15
    /// zend.max_allowed_stack_size => 0 => 0
    /// zend.multibyte => Off => Off
    /// zend.reserved_stack_size => 0 => 0
    /// zend.script_encoding => no value => no value
    /// zend.signal_check => Off => Off
    /// ```
    ///
    /// Under php-cgi, whose `display_errors` displayer says `On`, in HTML,
    /// where a displayer's markup is written as it is and a value is
    /// escaped:
    ///
    /// ```php
    /// ob_start(); phpinfo(INFO_CONFIGURATION); $page = ob_get_clean(); echo strstr($page, "PHP Core");
    /// ```
    /// ```request
    /// --GET--
    /// q=1
    /// ```
    /// ```expect
    /// X-Powered-By: PHP/8.5.9
    /// Content-type: text/html; charset=UTF-8
    ///
    /// PHP Core</h2>
    /// <table>
    /// <tr class="h"><th>Directive</th><th>Local Value</th><th>Master Value</th></tr>
    /// <tr><td class="e">allow_url_fopen</td><td class="v">On</td><td class="v">On</td></tr>
    /// <tr><td class="e">allow_url_include</td><td class="v">Off</td><td class="v">Off</td></tr>
    /// <tr><td class="e">arg_separator.input</td><td class="v">&amp;</td><td class="v">&amp;</td></tr>
    /// <tr><td class="e">arg_separator.output</td><td class="v">&amp;</td><td class="v">&amp;</td></tr>
    /// <tr><td class="e">auto_append_file</td><td class="v"><i>no value</i></td><td class="v"><i>no value</i></td></tr>
    /// <tr><td class="e">auto_globals_jit</td><td class="v">On</td><td class="v">On</td></tr>
    /// <tr><td class="e">auto_prepend_file</td><td class="v"><i>no value</i></td><td class="v"><i>no value</i></td></tr>
    /// <tr><td class="e">browscap</td><td class="v"><i>no value</i></td><td class="v"><i>no value</i></td></tr>
    /// <tr><td class="e">default_charset</td><td class="v">UTF-8</td><td class="v">UTF-8</td></tr>
    /// <tr><td class="e">default_mimetype</td><td class="v">text/html</td><td class="v">text/html</td></tr>
    /// <tr><td class="e">disable_functions</td><td class="v"><i>no value</i></td><td class="v"><i>no value</i></td></tr>
    /// <tr><td class="e">display_errors</td><td class="v">On</td><td class="v">On</td></tr>
    /// <tr><td class="e">display_startup_errors</td><td class="v">On</td><td class="v">On</td></tr>
    /// <tr><td class="e">doc_root</td><td class="v"><i>no value</i></td><td class="v"><i>no value</i></td></tr>
    /// <tr><td class="e">docref_ext</td><td class="v">.html</td><td class="v">.html</td></tr>
    /// <tr><td class="e">docref_root</td><td class="v"><i>no value</i></td><td class="v"><i>no value</i></td></tr>
    /// <tr><td class="e">enable_dl</td><td class="v">On</td><td class="v">On</td></tr>
    /// <tr><td class="e">enable_post_data_reading</td><td class="v">On</td><td class="v">On</td></tr>
    /// <tr><td class="e">error_append_string</td><td class="v"><i>no value</i></td><td class="v"><i>no value</i></td></tr>
    /// <tr><td class="e">error_log</td><td class="v"><i>no value</i></td><td class="v"><i>no value</i></td></tr>
    /// <tr><td class="e">error_log_mode</td><td class="v">0644</td><td class="v">0644</td></tr>
    /// <tr><td class="e">error_prepend_string</td><td class="v"><i>no value</i></td><td class="v"><i>no value</i></td></tr>
    /// <tr><td class="e">error_reporting</td><td class="v">30719</td><td class="v">30719</td></tr>
    /// <tr><td class="e">expose_php</td><td class="v">On</td><td class="v">On</td></tr>
    /// <tr><td class="e">extension_dir</td><td class="v">%s</td><td class="v">%s</td></tr>
    /// <tr><td class="e">fatal_error_backtraces</td><td class="v">Off</td><td class="v">Off</td></tr>
    /// <tr><td class="e">fiber.stack_size</td><td class="v"><i>no value</i></td><td class="v"><i>no value</i></td></tr>
    /// <tr><td class="e">file_uploads</td><td class="v">On</td><td class="v">On</td></tr>
    /// <tr><td class="e">hard_timeout</td><td class="v">2</td><td class="v">2</td></tr>
    /// <tr><td class="e">highlight.comment</td><td class="v"><span style="color: #FF8000">#FF8000</span></td><td class="v"><span style="color: #FF8000">#FF8000</span></td></tr>
    /// <tr><td class="e">highlight.default</td><td class="v"><span style="color: #0000BB">#0000BB</span></td><td class="v"><span style="color: #0000BB">#0000BB</span></td></tr>
    /// <tr><td class="e">highlight.html</td><td class="v"><span style="color: #000000">#000000</span></td><td class="v"><span style="color: #000000">#000000</span></td></tr>
    /// <tr><td class="e">highlight.keyword</td><td class="v"><span style="color: #007700">#007700</span></td><td class="v"><span style="color: #007700">#007700</span></td></tr>
    /// <tr><td class="e">highlight.string</td><td class="v"><span style="color: #DD0000">#DD0000</span></td><td class="v"><span style="color: #DD0000">#DD0000</span></td></tr>
    /// <tr><td class="e">html_errors</td><td class="v">Off</td><td class="v">Off</td></tr>
    /// <tr><td class="e">ignore_repeated_errors</td><td class="v">Off</td><td class="v">Off</td></tr>
    /// <tr><td class="e">ignore_repeated_source</td><td class="v">Off</td><td class="v">Off</td></tr>
    /// <tr><td class="e">ignore_user_abort</td><td class="v">Off</td><td class="v">Off</td></tr>
    /// <tr><td class="e">implicit_flush</td><td class="v">Off</td><td class="v">Off</td></tr>
    /// <tr><td class="e">include_path</td><td class="v">%s</td><td class="v">%s</td></tr>
    /// <tr><td class="e">input_encoding</td><td class="v"><i>no value</i></td><td class="v"><i>no value</i></td></tr>
    /// <tr><td class="e">internal_encoding</td><td class="v"><i>no value</i></td><td class="v"><i>no value</i></td></tr>
    /// <tr><td class="e">log_errors</td><td class="v">Off</td><td class="v">Off</td></tr>
    /// <tr><td class="e">mail.add_x_header</td><td class="v">Off</td><td class="v">Off</td></tr>
    /// <tr><td class="e">mail.cr_lf_mode</td><td class="v">crlf</td><td class="v">crlf</td></tr>
    /// <tr><td class="e">mail.force_extra_parameters</td><td class="v"><i>no value</i></td><td class="v"><i>no value</i></td></tr>
    /// <tr><td class="e">mail.log</td><td class="v"><i>no value</i></td><td class="v"><i>no value</i></td></tr>
    /// <tr><td class="e">mail.mixed_lf_and_crlf</td><td class="v">Off</td><td class="v">Off</td></tr>
    /// <tr><td class="e">max_execution_time</td><td class="v">30</td><td class="v">30</td></tr>
    /// <tr><td class="e">max_file_uploads</td><td class="v">20</td><td class="v">20</td></tr>
    /// <tr><td class="e">max_input_nesting_level</td><td class="v">64</td><td class="v">64</td></tr>
    /// <tr><td class="e">max_input_time</td><td class="v">-1</td><td class="v">-1</td></tr>
    /// <tr><td class="e">max_input_vars</td><td class="v">1000</td><td class="v">1000</td></tr>
    /// <tr><td class="e">max_memory_limit</td><td class="v">-1</td><td class="v">-1</td></tr>
    /// <tr><td class="e">max_multipart_body_parts</td><td class="v">-1</td><td class="v">-1</td></tr>
    /// <tr><td class="e">memory_limit</td><td class="v">128M</td><td class="v">128M</td></tr>
    /// <tr><td class="e">open_basedir</td><td class="v"><i>no value</i></td><td class="v"><i>no value</i></td></tr>
    /// <tr><td class="e">output_buffering</td><td class="v"><i>no value</i></td><td class="v"><i>no value</i></td></tr>
    /// <tr><td class="e">output_encoding</td><td class="v"><i>no value</i></td><td class="v"><i>no value</i></td></tr>
    /// <tr><td class="e">output_handler</td><td class="v"><i>no value</i></td><td class="v"><i>no value</i></td></tr>
    /// <tr><td class="e">post_max_size</td><td class="v">8M</td><td class="v">8M</td></tr>
    /// <tr><td class="e">precision</td><td class="v">14</td><td class="v">14</td></tr>
    /// <tr><td class="e">realpath_cache_size</td><td class="v">4096K</td><td class="v">4096K</td></tr>
    /// <tr><td class="e">realpath_cache_ttl</td><td class="v">120</td><td class="v">120</td></tr>
    /// <tr><td class="e">register_argc_argv</td><td class="v">Off</td><td class="v">Off</td></tr>
    /// <tr><td class="e">report_memleaks</td><td class="v">On</td><td class="v">On</td></tr>
    /// <tr><td class="e">report_zend_debug</td><td class="v">Off</td><td class="v">Off</td></tr>
    /// <tr><td class="e">request_order</td><td class="v"><i>no value</i></td><td class="v"><i>no value</i></td></tr>
    /// <tr><td class="e">sendmail_from</td><td class="v"><i>no value</i></td><td class="v"><i>no value</i></td></tr>
    /// <tr><td class="e">sendmail_path</td><td class="v">/usr/sbin/sendmail -t -i</td><td class="v">/usr/sbin/sendmail -t -i</td></tr>
    /// <tr><td class="e">serialize_precision</td><td class="v">-1</td><td class="v">-1</td></tr>
    /// <tr><td class="e">short_open_tag</td><td class="v">Off</td><td class="v">Off</td></tr>
    /// <tr><td class="e">SMTP</td><td class="v">localhost</td><td class="v">localhost</td></tr>
    /// <tr><td class="e">smtp_port</td><td class="v">25</td><td class="v">25</td></tr>
    /// <tr><td class="e">sys_temp_dir</td><td class="v"><i>no value</i></td><td class="v"><i>no value</i></td></tr>
    /// <tr><td class="e">syslog.facility</td><td class="v">LOG_USER</td><td class="v">LOG_USER</td></tr>
    /// <tr><td class="e">syslog.filter</td><td class="v">no-ctrl</td><td class="v">no-ctrl</td></tr>
    /// <tr><td class="e">syslog.ident</td><td class="v">php</td><td class="v">php</td></tr>
    /// <tr><td class="e">unserialize_callback_func</td><td class="v"><i>no value</i></td><td class="v"><i>no value</i></td></tr>
    /// <tr><td class="e">upload_max_filesize</td><td class="v">2M</td><td class="v">2M</td></tr>
    /// <tr><td class="e">upload_tmp_dir</td><td class="v"><i>no value</i></td><td class="v"><i>no value</i></td></tr>
    /// <tr><td class="e">user_dir</td><td class="v"><i>no value</i></td><td class="v"><i>no value</i></td></tr>
    /// <tr><td class="e">user_ini.cache_ttl</td><td class="v">300</td><td class="v">300</td></tr>
    /// <tr><td class="e">user_ini.filename</td><td class="v">.user.ini</td><td class="v">.user.ini</td></tr>
    /// <tr><td class="e">variables_order</td><td class="v">EGPCS</td><td class="v">EGPCS</td></tr>
    /// <tr><td class="e">xmlrpc_error_number</td><td class="v">0</td><td class="v">0</td></tr>
    /// <tr><td class="e">xmlrpc_errors</td><td class="v">Off</td><td class="v">Off</td></tr>
    /// <tr><td class="e">zend.assertions</td><td class="v">1</td><td class="v">1</td></tr>
    /// <tr><td class="e">zend.detect_unicode</td><td class="v">On</td><td class="v">On</td></tr>
    /// <tr><td class="e">zend.enable_gc</td><td class="v">On</td><td class="v">On</td></tr>
    /// <tr><td class="e">zend.exception_ignore_args</td><td class="v">Off</td><td class="v">Off</td></tr>
    /// <tr><td class="e">zend.exception_string_param_max_len</td><td class="v">15</td><td class="v">15</td></tr>
    /// <tr><td class="e">zend.max_allowed_stack_size</td><td class="v">0</td><td class="v">0</td></tr>
    /// <tr><td class="e">zend.multibyte</td><td class="v">Off</td><td class="v">Off</td></tr>
    /// <tr><td class="e">zend.reserved_stack_size</td><td class="v">0</td><td class="v">0</td></tr>
    /// <tr><td class="e">zend.script_encoding</td><td class="v"><i>no value</i></td><td class="v"><i>no value</i></td></tr>
    /// <tr><td class="e">zend.signal_check</td><td class="v">Off</td><td class="v">Off</td></tr>
    /// </table>
    /// </div></body></html>
    /// ```
    pub fn core_ini_info(&mut self) {
        let shared = Arc::clone(&self.shared);
        let names: Vec<&[u8]> = shared
            .ini_defaults()
            .iter()
            .map(|(name, _)| name.as_slice())
            .filter(|name| shared.extension_ini(name).is_none())
            .collect();
        let rows: Vec<(String, IniCell, IniCell)> = names
            .iter()
            .map(|name| {
                let displayer = IniDisplayer::of(name);
                let local = self.ini_get(name).unwrap_or_default();
                let master = shared.ini_default(name).unwrap_or_default();
                (
                    String::from_utf8_lossy(name).into_owned(),
                    self.ini_display(displayer, local),
                    self.ini_display(displayer, master),
                )
            })
            .collect();
        self.ini_table(&rows);
    }

    /// One value of a core entry as `php_ini_displayer_cb` shows it
    /// through the entry's displayer (`Z/main/php_ini.c:67-111`).
    fn ini_display(&self, displayer: IniDisplayer, value: &[u8]) -> IniCell {
        let on_off = |on: bool| IniCell::Displayed(String::from(if on { "On" } else { "Off" }));
        match displayer {
            IniDisplayer::Plain => IniCell::Value(String::from_utf8_lossy(value).into_owned()),
            IniDisplayer::Boolean => on_off(Request::ini_parse_bool(value)),
            IniDisplayer::GcEnabled => on_off(self.gc_enabled()),
            // `zend_ini_color_displayer_cb` under `zend_uv.html_errors`,
            // which `php_module_startup` sets to 1 whatever the host is
            // (`Z/main/main.c:2326`), so a colour is marked up in text too.
            IniDisplayer::Color => {
                let value = String::from_utf8_lossy(value);
                IniCell::Displayed(format!("<span style=\"color: {value}\">{value}</span>"))
            }
            // `display_errors_mode`: the stream named where the host is
            // `cli`, `cgi` or `phpdbg`, `On` under any other.
            IniDisplayer::DisplayErrors => {
                IniCell::Displayed(String::from(match display_errors_mode_of(Some(value)) {
                    0 => "Off",
                    _ if !self.host_writes_stderr() => "On",
                    DISPLAY_ERRORS_STDERR => "STDERR",
                    _ => "STDOUT",
                }))
            }
        }
    }

    /// `display_ini_entries`' table (`Z/main/php_ini.c:115-146`) over
    /// rows an extension's `MINFO` shows, each its name, its local value
    /// and its master value as the entry's displayer would write them,
    /// the empty value being php's `no value`: nothing for no rows, else
    /// a table under the `Directive`, `Local Value` and `Master Value`
    /// header; in HTML the name as it is and each value through
    /// `php_html_puts`, `<i>no value</i>` for an empty one:
    ///
    /// ```php
    /// (new ReflectionExtension('iconv'))->info();
    /// ```
    /// ```request
    /// --GET--
    /// q=1
    /// ```
    /// ```expect
    /// X-Powered-By: PHP/8.5.9
    /// Content-type: text/html; charset=UTF-8
    ///
    /// <h2><a name="module_iconv" href="#module_iconv">iconv</a></h2>
    /// <table>
    /// <tr><td class="e">iconv support </td><td class="v">enabled </td></tr>
    /// <tr><td class="e">iconv implementation </td><td class="v">libiconv </td></tr>
    /// <tr><td class="e">iconv library version </td><td class="v">1.11 </td></tr>
    /// </table>
    /// <table>
    /// <tr class="h"><th>Directive</th><th>Local Value</th><th>Master Value</th></tr>
    /// <tr><td class="e">iconv.input_encoding</td><td class="v"><i>no value</i></td><td class="v"><i>no value</i></td></tr>
    /// <tr><td class="e">iconv.output_encoding</td><td class="v"><i>no value</i></td><td class="v"><i>no value</i></td></tr>
    /// <tr><td class="e">iconv.internal_encoding</td><td class="v"><i>no value</i></td><td class="v"><i>no value</i></td></tr>
    /// </table>
    /// ```
    ///
    /// The rows are listed in the order the table holds them
    /// ([`Request::ini_sort_entries`]): as the extension registered its
    /// entries, until the request's first `phpinfo()` or `ini_get_all()`
    /// sorts them by name.
    ///
    /// ```php
    /// $e = new ReflectionExtension('session'); ob_start(); $e->info();
    /// echo implode("\n", array_slice(explode("\n", strstr(ob_get_clean(), 'Directive')), 0, 4)), "\n";
    /// ```
    /// ```expect
    /// Directive => Local Value => Master Value
    /// session.save_path => no value => no value
    /// session.name => PHPSESSID => PHPSESSID
    /// session.save_handler => files => files
    /// ```
    ///
    /// ```php
    /// ini_get_all('session');
    /// $e = new ReflectionExtension('session'); ob_start(); $e->info();
    /// echo implode("\n", array_slice(explode("\n", strstr(ob_get_clean(), 'Directive')), 0, 4)), "\n";
    /// ```
    /// ```expect
    /// Directive => Local Value => Master Value
    /// session.auto_start => Off => Off
    /// session.cache_expire => 180 => 180
    /// session.cache_limiter => nocache => nocache
    /// ```
    pub fn info_ini_table(&mut self, rows: &[[&str; 3]]) {
        let rows: Vec<(String, IniCell, IniCell)> = rows
            .iter()
            .map(|[name, local, master]| {
                (
                    (*name).to_owned(),
                    IniCell::Value((*local).to_owned()),
                    IniCell::Value((*master).to_owned()),
                )
            })
            .collect();
        self.ini_table(&rows);
    }

    /// [`Request::info_ini_table`] over cells a displayer may have
    /// written ([`IniCell`]), the rows in the order the table holds
    /// them ([`IniOrder`]): `display_ini_entries` walks
    /// `EG(ini_directives)` for the extension's entries, so the rows it
    /// hands over in its registration order are listed sorted
    /// once the request has sorted the table.
    fn ini_table(&mut self, rows: &[(String, IniCell, IniCell)]) {
        if rows.is_empty() {
            return;
        }
        let mut rows: Vec<&(String, IniCell, IniCell)> = rows.iter().collect();
        self.ini
            .order
            .arrange(&mut rows, |(name, ..)| name.as_bytes());
        let text = self.phpinfo_as_text();
        let mut out = Vec::new();
        let value = |out: &mut Vec<u8>, cell: &IniCell| match cell {
            IniCell::Displayed(shown) => out.extend_from_slice(shown.as_bytes()),
            IniCell::Value(shown) => match (shown.is_empty(), text) {
                (true, true) => out.extend_from_slice(b"no value"),
                (true, false) => out.extend_from_slice(b"<i>no value</i>"),
                (false, true) => out.extend_from_slice(shown.as_bytes()),
                (false, false) => php_compile::syntax::html_puts(out, shown.as_bytes()),
            },
        };
        for (name, local, master) in rows {
            if text {
                out.extend_from_slice(name.as_bytes());
                out.extend_from_slice(b" => ");
                value(&mut out, local);
                out.extend_from_slice(b" => ");
                value(&mut out, master);
                out.push(b'\n');
            } else {
                out.extend_from_slice(b"<tr><td class=\"e\">");
                out.extend_from_slice(name.as_bytes());
                out.extend_from_slice(b"</td><td class=\"v\">");
                value(&mut out, local);
                out.extend_from_slice(b"</td><td class=\"v\">");
                value(&mut out, master);
                out.extend_from_slice(b"</td></tr>\n");
            }
        }
        self.info_rows(&[
            InfoRow::Header(&["Directive", "Local Value", "Master Value"]),
            InfoRow::Written(&out),
        ]);
    }

    /// One ini entry as php's `STR_PRINT(INI_STR(name))` reads it: its
    /// bytes, and nothing at all where php left the entry `NULL`.
    pub(super) fn ini_text(&self, name: &[u8]) -> String {
        self.ini_get(name)
            .map(|value| String::from_utf8_lossy(value).into_owned())
            .unwrap_or_default()
    }

    pub(super) fn ini_number(&self, name: &[u8], default: i32) -> i32 {
        self.ini_get(name)
            .and_then(|value| String::from_utf8_lossy(value).parse().ok())
            .unwrap_or(default)
    }

    /// The typed entries as this request holds them now ([`TypedIni`]).
    pub(crate) fn typed_ini(&self) -> &TypedIni {
        &self.ini.typed
    }

    pub(super) fn ini_parse_bytes_of(&self, name: &str) -> i64 {
        self.ini_get(name.as_bytes())
            .map_or(0, Request::ini_parse_bytes)
    }

    /// The php.ini files the engine read at startup ([`IniFiles`]).
    #[must_use]
    pub fn ini_files(&self) -> &IniFiles {
        &self.shared.ini_files
    }

    /// One entry added to `EG(modified_ini_directives)`, which the
    /// first lays down, `ALLOC_HASHTABLE` and `zend_hash_init(8)`, and
    /// the first write gives its data; a `@` is the first change of a
    /// request that changes no entry, and `error_clear_last()` gives
    /// none of it back (the warning first stores a last error, whose
    /// message the `@`'s replaces at its size):
    ///
    /// ```php
    /// echo $w;
    /// $m = memory_get_usage(); @$x; echo memory_get_usage() - $m, "\n";
    /// $m = memory_get_usage(); ini_set('precision', '10'); echo memory_get_usage() - $m, "\n";
    /// error_clear_last();
    /// $m = memory_get_usage(); for ($i = 0; $i < 7; $i++) ini_set(['user_agent', 'arg_separator.output', 'default_socket_timeout', 'from', 'serialize_precision', 'highlight.string', 'highlight.comment'][$i], '1'); echo memory_get_usage() - $m, "\n";
    /// ```
    /// ```expect
    /// Warning: Undefined variable $w in %s on line 2
    /// 376
    /// 0
    /// 320
    /// ```
    fn ini_modified_bytes(&mut self) {
        let table = match self.ini.bytes.take() {
            Some(table) => Ok(table),
            None => HashBytes::new_array(self, 8),
        };
        // The memory limit's fatal is recorded and ends the request.
        if let Ok(mut table) = table {
            table.add(self).ok();
            self.ini.bytes = Some(table);
        }
    }

    /// One entry deleted from `EG(modified_ini_directives)`, by
    /// `ini_restore()`, the last of the table's where `last` says so.
    fn ini_unmodified_bytes(&mut self, last: bool) {
        if let Some(table) = self.ini.bytes.as_mut() {
            table.del(last);
        }
    }

    /// `zend_hash_destroy` and `FREE_HASHTABLE` of
    /// `EG(modified_ini_directives)`, `zend_ini_deactivate`'s.
    fn free_modified_ini_bytes(&mut self) {
        if let Some(table) = self.ini.bytes.take() {
            table.free(self);
        }
    }

    /// `precision`, `EG(precision)`: the digits a float converts to a
    /// string with, and compares at, as `OnSetPrecision` read the entry.
    #[must_use]
    pub fn precision(&self) -> i32 {
        self.ini.typed.precision
    }

    /// `short_open_tag`, php's `CG(short_tags)`: whether a `<?` opens
    /// code in the source this request compiles, which its scanner reads
    /// at every tag it meets (`Z/Zend/zend_language_scanner.l:2331`).
    /// php's built-in default is on (`DEFAULT_SHORT_OPEN_TAG`), which is
    /// what an engine with no entry for it answers, and the entry is
    /// `PHP_INI_SYSTEM|PHP_INI_PERDIR`, so no `ini_set()` of the
    /// request's moves it. `token_get_all` reads the same entry, which
    /// is why it is the request's and not the engine's alone.
    pub(crate) fn short_open_tag(&self) -> bool {
        self.ini.typed.short_open_tag
    }

    /// `EG(assertions)`, php's `zend.assertions` read as
    /// `OnUpdateAssertions` reads it (`zend_ini_parse_quantity_warn`):
    /// `1` compiles and runs an assertion, `0` compiles it behind an
    /// `ASSERT_CHECK` that jumps over it, and `-1` compiles nothing of it.
    /// What `ini_set()` and `ini_restore()` leave is what the next
    /// compile reads:
    ///
    /// ```php
    /// ini_set('zend.assertions', '0'); eval('assert(false); echo "skipped\n";');
    /// ini_restore('zend.assertions');
    /// try { eval('assert(false);'); } catch (AssertionError $e) { echo get_class($e), "\n"; }
    /// ```
    /// ```expect capture
    /// skipped
    /// AssertionError
    /// ```
    pub(crate) fn assertions(&self) -> i64 {
        self.ini.typed.assertions
    }

    /// `OnUpdateAssertions` (`Z/Zend/zend.c:156-172`) for an
    /// `ini_set()`: a value that moves the entry to or from a negative
    /// one is php's warning and a refusal that keeps the entry as it
    /// was, since what is compiled already was compiled under it; any
    /// other value is taken.
    ///
    /// ```php
    /// var_dump(ini_get('zend.assertions'), ini_set('zend.assertions', '-1'), ini_set('zend.assertions', '0'), ini_get('zend.assertions'), ini_set('zend.assertions', '1'));
    /// ```
    /// ```expect
    /// Warning: zend.assertions may be completely enabled or disabled only in php.ini in %s on line 2
    /// string(1) "1"
    /// bool(false)
    /// string(1) "1"
    /// string(1) "0"
    /// string(1) "0"
    /// ```
    fn assertions_updated(&mut self, value: &[u8]) -> bool {
        let (current, next) = (self.assertions(), Request::ini_parse_bytes(value));
        if current != next && (current < 0 || next < 0) {
            crate::api::raising::warning(
                self,
                "zend.assertions may be completely enabled or disabled only in php.ini",
            );
            return false;
        }
        true
    }

    /// `serialize_precision`, what `var_dump`, `var_export`, `serialize`
    /// and `json_encode` print a float at; -1, its default, asks for the
    /// shortest text that reads back as the same double.
    #[must_use]
    pub fn serialize_precision(&self) -> i32 {
        self.ini.typed.serialize_precision
    }
}

/// A core entry's `displayer`, what `php_ini_displayer_cb` shows its
/// values through in phpinfo (`Z/main/php_ini.c:67-111`), as `main.c`'s
/// and `zend.c`'s tables pair them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IniDisplayer {
    /// None: the value as it is, escaped in HTML, `no value` for none.
    Plain,
    /// `zend_ini_boolean_displayer_cb`, every `STD_PHP_INI_BOOLEAN` and
    /// `STD_ZEND_INI_BOOLEAN`: `On` or `Off`.
    Boolean,
    /// `php_ini_color_displayer_cb`, the five `highlight.*` colours.
    Color,
    /// `display_errors_mode`, `display_errors`'.
    DisplayErrors,
    /// `zend_gc_enabled_displayer_cb`, `zend.enable_gc`'s: whether the
    /// collector runs, `On` or `Off`, in both columns.
    GcEnabled,
}

impl IniDisplayer {
    /// The displayer `Z/main/main.c:793-887` and `Z/Zend/zend.c:263-272`
    /// register the core entry `name` with.
    fn of(name: &[u8]) -> IniDisplayer {
        match name {
            b"display_startup_errors"
            | b"enable_dl"
            | b"expose_php"
            | b"html_errors"
            | b"xmlrpc_errors"
            | b"ignore_user_abort"
            | b"implicit_flush"
            | b"log_errors"
            | b"ignore_repeated_errors"
            | b"ignore_repeated_source"
            | b"report_memleaks"
            | b"report_zend_debug"
            | b"register_argc_argv"
            | b"auto_globals_jit"
            | b"short_open_tag"
            | b"file_uploads"
            | b"mail.add_x_header"
            | b"mail.mixed_lf_and_crlf"
            | b"allow_url_fopen"
            | b"allow_url_include"
            | b"enable_post_data_reading"
            | b"fatal_error_backtraces"
            | b"zend.multibyte"
            | b"zend.detect_unicode"
            | b"zend.signal_check"
            | b"zend.exception_ignore_args" => IniDisplayer::Boolean,
            b"highlight.comment" | b"highlight.default" | b"highlight.html"
            | b"highlight.keyword" | b"highlight.string" => IniDisplayer::Color,
            b"display_errors" => IniDisplayer::DisplayErrors,
            b"zend.enable_gc" => IniDisplayer::GcEnabled,
            _ => IniDisplayer::Plain,
        }
    }
}

/// One value cell of phpinfo's ini table: a value shown as it is, or
/// what a displayer wrote, which php writes as written in HTML too.
enum IniCell {
    Value(String),
    Displayed(String),
}

#[cfg(test)]
mod tests {
    use super::Request;
    use crate::api::raising::Level;
    use crate::engine::Engine;

    /// An extension entry's `on_modify` (`PHP_INI_MH` at
    /// `ZEND_INI_STAGE_RUNTIME`) as `ini_set()` serves it: before the
    /// value is taken, what it raises raised, and its refusal leaving the
    /// entry as it was. No witness holds it, because the entry is no
    /// entry of the oracle's; `assert.c`'s `OnUpdateAssert` family is
    /// the one php-src has, which reaches it with #753.
    #[test]
    fn an_extension_entrys_on_modify_is_served_by_ini_set() {
        #[allow(clippy::unnecessary_wraps)] // `OnModify`'s signature
        fn guarded(
            req: &mut Request,
            value: Option<&[u8]>,
        ) -> Result<bool, crate::api::raising::Thrown> {
            if value == Some(b"0") {
                crate::api::raising::docref(
                    req,
                    Level::Deprecated,
                    "t.flag INI setting is deprecated",
                );
            }
            Ok(value != Some(b"bad"))
        }
        let extension = crate::api::abi::ExtensionBuilder::default()
            .ini(vec![php_unit::decl::IniDecl {
                name: php_unit::decl::Name(b"t.flag".as_slice().into()),
                default: Some(b"1".as_slice().into()),
                user: true,
                per_dir: true,
                system: true,
            }])
            .on_modify(b"t.flag", guarded)
            .build("modified");
        let engine = Engine::cli_builder().extensions(&[extension]).build();
        let served = engine.serve_capture(
            "<?php var_dump(ini_set('t.flag', '0'), ini_set('t.flag', 'bad'), ini_get('t.flag'));",
            crate::test::Capture::default(),
        );
        let out = String::from_utf8_lossy(&served.stdout);
        assert!(
            out.contains("Deprecated: ini_set(): t.flag INI setting is deprecated"),
            "{out}"
        );
        assert!(
            out.ends_with("string(1) \"1\"\nbool(false)\nstring(1) \"0\"\n"),
            "{out}"
        );
    }

    /// The same `on_modify` at `ZEND_INI_STAGE_STARTUP`, over the value
    /// the host set (`zend_register_ini_entries_ex`): a value it refuses
    /// leaves the entry at its default, for `ini_get()` and for what
    /// `ini_set()` answers was there, and its docref is raised
    /// first in every request as `PHP Startup: …` at no file; a value
    /// it takes is the entry's.
    #[test]
    fn a_startup_value_the_on_modify_refuses_keeps_the_default() {
        #[allow(clippy::unnecessary_wraps)] // `OnModify`'s signature
        fn guarded(
            req: &mut Request,
            value: Option<&[u8]>,
        ) -> Result<bool, crate::api::raising::Thrown> {
            if value == Some(b"bad") {
                crate::api::raising::docref(req, Level::Warning, "t.flag refused");
                return Ok(false);
            }
            Ok(true)
        }
        let extension = || {
            crate::api::abi::ExtensionBuilder::default()
                .ini(vec![php_unit::decl::IniDecl {
                    name: php_unit::decl::Name(b"t.flag".as_slice().into()),
                    default: Some(b"1".as_slice().into()),
                    user: true,
                    per_dir: true,
                    system: true,
                }])
                .on_modify(b"t.flag", guarded)
                .build("modified")
        };
        let script = "<?php var_dump(ini_get('t.flag'), ini_set('t.flag', '3'));";
        let refused = Engine::cli_builder()
            .extensions(&[extension()])
            .ini("t.flag", "bad")
            .build()
            .serve_capture(script, crate::test::Capture::default());
        assert_eq!(
            String::from_utf8_lossy(&refused.stdout),
            "\nWarning: PHP Startup: t.flag refused in Unknown on line 0\nstring(1) \"1\"\nstring(1) \"1\"\n"
        );
        let taken = Engine::cli_builder()
            .extensions(&[extension()])
            .ini("t.flag", "2")
            .build()
            .serve_capture(script, crate::test::Capture::default());
        assert_eq!(
            String::from_utf8_lossy(&taken.stdout),
            "string(1) \"2\"\nstring(1) \"2\"\n"
        );
    }

    /// An extension's entries declared with `OnUpdateLong` and
    /// `OnUpdateLongGEZero` ([`crate::api::abi::LongHandler`]), served by
    /// the engine as php serves them: the quantity's warning at startup,
    /// in declaration order, at `ini_set()` and as the request ends, and
    /// a negative the second refuses, at startup keeping the default and
    /// at `ini_set()` answering `false`. No witness holds it, because the
    /// entries are no entries of the oracle's; the extensions' own
    /// witnesses hold php's (`session.cache_expire`,
    /// `mysqli.default_port`).
    #[test]
    fn an_extension_entry_declared_on_update_long_is_read_by_the_quantity_rule() {
        use crate::api::abi::LongHandler;
        let entry = |name: &[u8], default: &[u8]| php_unit::decl::IniDecl {
            name: php_unit::decl::Name(name.into()),
            default: Some(default.into()),
            user: true,
            per_dir: true,
            system: true,
        };
        let extension = crate::api::abi::ExtensionBuilder::default()
            .ini(vec![entry(b"t.any", b"1"), entry(b"t.positive", b"2")])
            .on_update_long(b"t.positive", LongHandler::AtLeastZero)
            .on_update_long(b"t.any", LongHandler::Any)
            .build("longs");
        let served = Engine::cli_builder()
            .extensions(&[extension])
            .ini("t.positive", "-1x")
            .ini("t.any", "3q")
            .build()
            .serve_capture(
                "<?php var_dump(ini_get('t.positive'), ini_set('t.positive', '-1'), ini_set('t.any', '1K'), ini_set('t.positive', '0x'), ini_get('t.positive'));",
                crate::test::Capture::default(),
            );
        assert_eq!(
            String::from_utf8_lossy(&served.stdout),
            "\nWarning: Invalid \"t.any\" setting. Invalid quantity \"3q\": unknown multiplier \"q\", interpreting as \"3\" for backwards compatibility in Unknown on line 0\n\
             \nWarning: Invalid \"t.positive\" setting. Invalid quantity \"-1x\": unknown multiplier \"x\", interpreting as \"-1\" for backwards compatibility in Unknown on line 0\n\
             \nWarning: Invalid \"t.positive\" setting. Invalid quantity \"0x\": no digits after base prefix, interpreting as \"0\" for backwards compatibility in Standard input code on line 1\n\
             string(1) \"2\"\nbool(false)\nstring(2) \"3q\"\nstring(1) \"2\"\nstring(2) \"0x\"\n\
             \nWarning: Invalid \"t.any\" setting. Invalid quantity \"3q\": unknown multiplier \"q\", interpreting as \"3\" for backwards compatibility in Unknown on line 0\n"
        );
    }
}
