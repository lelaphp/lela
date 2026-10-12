//! `fpm_conf.c`: `php-fpm.conf` read as php reads it, a line at a time
//! through php's INI grammar under the normal scanner ([`Engine::parse_ini`]),
//! `[global]` and each `[pool]` section's entries set by the table of
//! their names, `include` globbed after its line, arrays (`php_value[…]`,
//! `env[…]`, `access.suppress_path[]`) kept newest first, `$pool` and
//! `$prefix` expanded; then each pool checked and completed as
//! `fpm_conf_process_all_pools` does, every refusal php's sentence in the
//! log; and `-tt`'s dump of it all.
//!
//! [`Engine::parse_ini`]: php_sapi::Engine::parse_ini

use std::os::unix::ffi::OsStrExt;

use php_sapi::{Engine, IniEvent, IniValue, PHP_PREFIX, PHP_SYSCONFDIR};

use crate::zlog::Level;
use crate::{zlog, zlog_sys};

/// `FPM_BACKLOG_DEFAULT`.
pub(crate) const BACKLOG_DEFAULT: i32 = -1;

/// `pm`, the process manager of a pool.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Pm {
    Static,
    Dynamic,
    Ondemand,
}

impl Pm {
    /// `PM2STR`.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Pm::Static => "static",
            Pm::Dynamic => "dynamic",
            Pm::Ondemand => "ondemand",
        }
    }
}

/// `struct fpm_global_config_s`.
#[derive(Debug, Clone)]
pub(crate) struct Global {
    pub(crate) pid_file: Option<Vec<u8>>,
    pub(crate) error_log: Option<Vec<u8>>,
    pub(crate) syslog_ident: Option<Vec<u8>>,
    pub(crate) syslog_facility: i32,
    pub(crate) log_buffering: bool,
    pub(crate) log_level: i32,
    pub(crate) log_limit: i32,
    pub(crate) emergency_restart_threshold: i32,
    pub(crate) emergency_restart_interval: i32,
    pub(crate) process_control_timeout: i32,
    pub(crate) process_max: i32,
    pub(crate) process_priority: i32,
    pub(crate) daemonize: bool,
    pub(crate) rlimit_files: i32,
    pub(crate) rlimit_core: i32,
    pub(crate) events_mechanism: Option<Vec<u8>>,
}

impl Default for Global {
    fn default() -> Global {
        Global {
            pid_file: None,
            error_log: None,
            syslog_ident: None,
            syslog_facility: -1,
            log_buffering: true,
            log_level: 0,
            log_limit: i32::try_from(crate::zlog::DEFAULT_LIMIT).unwrap_or(1024),
            emergency_restart_threshold: 0,
            emergency_restart_interval: 0,
            process_control_timeout: 0,
            process_max: 0,
            process_priority: 64,
            daemonize: true,
            rlimit_files: 0,
            rlimit_core: 0,
            events_mechanism: None,
        }
    }
}

/// `struct fpm_worker_pool_config_s`.
#[derive(Debug, Clone)]
#[allow(clippy::struct_excessive_bools)] // php's config struct, one field per entry
pub(crate) struct PoolConfig {
    pub(crate) name: Vec<u8>,
    pub(crate) prefix: Option<Vec<u8>>,
    pub(crate) user: Option<Vec<u8>>,
    pub(crate) group: Option<Vec<u8>>,
    pub(crate) listen_address: Option<Vec<u8>>,
    pub(crate) listen_backlog: i32,
    pub(crate) listen_owner: Option<Vec<u8>>,
    pub(crate) listen_group: Option<Vec<u8>>,
    pub(crate) listen_mode: Option<Vec<u8>>,
    pub(crate) listen_allowed_clients: Option<Vec<u8>>,
    pub(crate) process_priority: i32,
    pub(crate) process_dumpable: bool,
    pub(crate) pm: Option<Pm>,
    pub(crate) pm_max_children: i32,
    pub(crate) pm_start_servers: i32,
    pub(crate) pm_min_spare_servers: i32,
    pub(crate) pm_max_spare_servers: i32,
    pub(crate) pm_max_spawn_rate: i32,
    pub(crate) pm_process_idle_timeout: i32,
    pub(crate) pm_max_requests: i32,
    pub(crate) pm_status_path: Option<Vec<u8>>,
    pub(crate) pm_status_listen: Option<Vec<u8>>,
    pub(crate) ping_path: Option<Vec<u8>>,
    pub(crate) ping_response: Option<Vec<u8>>,
    pub(crate) access_log: Option<Vec<u8>>,
    pub(crate) access_format: Option<Vec<u8>>,
    pub(crate) slowlog: Option<Vec<u8>>,
    pub(crate) request_slowlog_timeout: i32,
    pub(crate) request_slowlog_trace_depth: i32,
    pub(crate) request_terminate_timeout: i32,
    pub(crate) request_terminate_timeout_track_finished: bool,
    pub(crate) rlimit_files: i32,
    pub(crate) rlimit_core: i32,
    pub(crate) chroot: Option<Vec<u8>>,
    pub(crate) chdir: Option<Vec<u8>>,
    pub(crate) catch_workers_output: bool,
    pub(crate) decorate_workers_output: bool,
    pub(crate) clear_env: bool,
    pub(crate) security_limit_extensions: Option<Vec<u8>>,
    /// `env[…]`, newest first, as `fpm_conf_set_array` links them.
    pub(crate) env: Vec<(Vec<u8>, Vec<u8>)>,
    pub(crate) php_values: Vec<(Vec<u8>, Vec<u8>)>,
    pub(crate) php_admin_values: Vec<(Vec<u8>, Vec<u8>)>,
    pub(crate) access_suppress_paths: Vec<Vec<u8>>,
}

impl PoolConfig {
    /// `fpm_worker_pool_config_alloc`'s defaults.
    fn named(name: Vec<u8>) -> PoolConfig {
        PoolConfig {
            name,
            prefix: None,
            user: None,
            group: None,
            listen_address: None,
            listen_backlog: BACKLOG_DEFAULT,
            listen_owner: None,
            listen_group: None,
            listen_mode: None,
            listen_allowed_clients: None,
            process_priority: 64,
            process_dumpable: false,
            pm: None,
            pm_max_children: 0,
            pm_start_servers: 0,
            pm_min_spare_servers: 0,
            pm_max_spare_servers: 0,
            pm_max_spawn_rate: 32,
            pm_process_idle_timeout: 10,
            pm_max_requests: 0,
            pm_status_path: None,
            pm_status_listen: None,
            ping_path: None,
            ping_response: None,
            access_log: None,
            access_format: None,
            slowlog: None,
            request_slowlog_timeout: 0,
            request_slowlog_trace_depth: 0,
            request_terminate_timeout: 0,
            request_terminate_timeout_track_finished: false,
            rlimit_files: 0,
            rlimit_core: 0,
            chroot: None,
            chdir: None,
            catch_workers_output: false,
            decorate_workers_output: true,
            clear_env: true,
            security_limit_extensions: None,
            env: Vec::new(),
            php_values: Vec::new(),
            php_admin_values: Vec::new(),
            access_suppress_paths: Vec::new(),
        }
    }
}

/// What reading the configuration made: the global section and each
/// pool in the order first named, with the shared status pool a
/// `pm.status_listen` makes standing after its pool's ([`Parsed::shared`]).
#[derive(Debug, Default)]
pub(crate) struct Parsed {
    pub(crate) global: Global,
    pub(crate) pools: Vec<PoolConfig>,
    /// For each pool, the pool whose scoreboard its status page shows:
    /// `wp->shared`.
    pub(crate) shared: Vec<Option<usize>>,
}

/// One entry's setter, what `ini_value_parser_s.parser` points at.
#[derive(Clone, Copy)]
enum Setter {
    Str,
    Int,
    Time,
    Bool,
    LogLevel,
    RlimitCore,
    Pm,
    SyslogFacility,
}

/// `ini_fpm_global_options`.
const GLOBAL_OPTIONS: &[(&str, Setter)] = &[
    ("pid", Setter::Str),
    ("error_log", Setter::Str),
    ("syslog.ident", Setter::Str),
    ("syslog.facility", Setter::SyslogFacility),
    ("log_buffering", Setter::Bool),
    ("log_level", Setter::LogLevel),
    ("log_limit", Setter::Int),
    ("emergency_restart_threshold", Setter::Int),
    ("emergency_restart_interval", Setter::Time),
    ("process_control_timeout", Setter::Time),
    ("process.max", Setter::Int),
    ("process.priority", Setter::Int),
    ("daemonize", Setter::Bool),
    ("rlimit_files", Setter::Int),
    ("rlimit_core", Setter::RlimitCore),
    ("events.mechanism", Setter::Str),
];

/// `ini_fpm_pool_options`, as php builds it on this platform: no ACL,
/// `SO_SETFIB` nor `AppArmor`.
const POOL_OPTIONS: &[(&str, Setter)] = &[
    ("prefix", Setter::Str),
    ("user", Setter::Str),
    ("group", Setter::Str),
    ("listen", Setter::Str),
    ("listen.backlog", Setter::Int),
    ("listen.owner", Setter::Str),
    ("listen.group", Setter::Str),
    ("listen.mode", Setter::Str),
    ("listen.allowed_clients", Setter::Str),
    ("process.priority", Setter::Int),
    ("process.dumpable", Setter::Bool),
    ("pm", Setter::Pm),
    ("pm.max_children", Setter::Int),
    ("pm.start_servers", Setter::Int),
    ("pm.min_spare_servers", Setter::Int),
    ("pm.max_spare_servers", Setter::Int),
    ("pm.max_spawn_rate", Setter::Int),
    ("pm.process_idle_timeout", Setter::Time),
    ("pm.max_requests", Setter::Int),
    ("pm.status_path", Setter::Str),
    ("pm.status_listen", Setter::Str),
    ("ping.path", Setter::Str),
    ("ping.response", Setter::Str),
    ("access.log", Setter::Str),
    ("access.format", Setter::Str),
    ("slowlog", Setter::Str),
    ("request_slowlog_timeout", Setter::Time),
    ("request_slowlog_trace_depth", Setter::Int),
    ("request_terminate_timeout", Setter::Time),
    ("request_terminate_timeout_track_finished", Setter::Bool),
    ("rlimit_files", Setter::Int),
    ("rlimit_core", Setter::RlimitCore),
    ("chroot", Setter::Str),
    ("chdir", Setter::Str),
    ("catch_workers_output", Setter::Bool),
    ("decorate_workers_output", Setter::Bool),
    ("clear_env", Setter::Bool),
    ("security.limit_extensions", Setter::Str),
];

/// What a setter made of a value.
enum Set {
    Str(Vec<u8>),
    Int(i32),
    Bool(bool),
    Pm(Pm),
}

/// C's `atoi`, which `fpm_conf.c` reads its numbers with: the engine's
/// one reading, which the sockets' and the per-directory cache's readers
/// take from here too.
pub(crate) use php_sapi::c_number::atoi;

/// `fpm_conf_set_boolean`'s reading.
pub(crate) fn boolean(value: &[u8]) -> Result<bool, &'static str> {
    let yes = [&b"1"[..], b"yes", b"true", b"on"].contains(&value);
    let no =
        yes || value.is_empty() || [&b"0"[..], b"no", b"none", b"false", b"off"].contains(&value);
    if !yes && !no {
        return Err("invalid boolean value");
    }
    Ok(yes)
}

/// `fpm_conf_set_integer`'s reading: digits alone, one leading `-`.
fn integer(value: &[u8]) -> Result<i32, &'static str> {
    for (i, &b) in value.iter().enumerate() {
        if i == 0 && b == b'-' {
            continue;
        }
        if !b.is_ascii_digit() {
            return Err("is not a valid number (greater or equal than zero)");
        }
    }
    Ok(atoi(value))
}

/// `fpm_conf_set_time`: seconds, or minutes, hours or days by suffix.
fn time(value: &[u8]) -> Result<i32, &'static str> {
    let Some(&suffix) = value.last() else {
        return Err("invalid time value");
    };
    let head = &value[..value.len() - 1];
    Ok(match suffix {
        b'm' => atoi(head).wrapping_mul(60),
        b'h' => atoi(head).wrapping_mul(3600),
        b'd' => atoi(head).wrapping_mul(86400),
        b's' => atoi(head),
        b'0'..=b'9' => atoi(value),
        _ => return Err("unknown suffix used in time value"),
    })
}

/// `fpm_conf_set_log_level`.
fn log_level(value: &[u8]) -> Result<i32, &'static str> {
    let is = |word: &[u8]| value.eq_ignore_ascii_case(word);
    Ok(if is(b"debug") {
        1
    } else if is(b"notice") {
        2
    } else if is(b"warning") || is(b"warn") {
        3
    } else if is(b"error") {
        4
    } else if is(b"alert") {
        5
    } else {
        return Err("invalid value for 'log_level'");
    })
}

/// `fpm_conf_set_syslog_facility`: the facilities `<syslog.h>` names.
fn syslog_facility(value: &[u8]) -> Result<i32, &'static str> {
    const FACILITIES: &[(&[u8], i32)] = &[
        (b"AUTH", libc::LOG_AUTH),
        (b"AUTHPRIV", libc::LOG_AUTHPRIV),
        (b"CRON", libc::LOG_CRON),
        (b"DAEMON", libc::LOG_DAEMON),
        (b"FTP", libc::LOG_FTP),
        (b"KERN", libc::LOG_KERN),
        (b"LPR", libc::LOG_LPR),
        (b"MAIL", libc::LOG_MAIL),
        (b"NEWS", libc::LOG_NEWS),
        (b"SYSLOG", libc::LOG_SYSLOG),
        (b"USER", libc::LOG_USER),
        (b"UUCP", libc::LOG_UUCP),
        (b"LOCAL0", libc::LOG_LOCAL0),
        (b"LOCAL1", libc::LOG_LOCAL1),
        (b"LOCAL2", libc::LOG_LOCAL2),
        (b"LOCAL3", libc::LOG_LOCAL3),
        (b"LOCAL4", libc::LOG_LOCAL4),
        (b"LOCAL5", libc::LOG_LOCAL5),
        (b"LOCAL6", libc::LOG_LOCAL6),
        (b"LOCAL7", libc::LOG_LOCAL7),
    ];
    FACILITIES
        .iter()
        .find(|(name, _)| value.eq_ignore_ascii_case(name))
        .map(|(_, facility)| *facility)
        .ok_or("invalid value")
}

/// `fpm_conf_expand_pool_name`: each `$pool` of `value` the pool's name;
/// `None` outside a pool where one stands.
fn expand_pool(value: &[u8], pool: Option<&[u8]>) -> Option<Vec<u8>> {
    let mut value = value.to_vec();
    while let Some(at) = find(&value, b"$pool") {
        let name = pool?;
        let mut expanded = value[..at].to_vec();
        expanded.extend_from_slice(name);
        expanded.extend_from_slice(&value[at + 5..]);
        value = expanded;
    }
    Some(value)
}

/// `strstr`.
pub(crate) fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// One setter over one value.
fn set(setter: Setter, value: &[u8], pool: Option<&[u8]>) -> Result<Set, &'static str> {
    Ok(match setter {
        Setter::Str => Set::Str(
            expand_pool(value, pool).ok_or("Can't use '$pool' when the pool is not defined")?,
        ),
        Setter::Int => Set::Int(integer(value)?),
        Setter::Time => Set::Int(time(value)?),
        Setter::Bool => Set::Bool(boolean(value)?),
        Setter::LogLevel => Set::Int(log_level(value)?),
        Setter::SyslogFacility => Set::Int(syslog_facility(value)?),
        Setter::RlimitCore => {
            if value.eq_ignore_ascii_case(b"unlimited") {
                Set::Int(-1)
            } else {
                let n = integer(value)?;
                if n < 0 {
                    return Err("must be greater than zero or 'unlimited'");
                }
                Set::Int(n)
            }
        }
        Setter::Pm => Set::Pm(if value.eq_ignore_ascii_case(b"static") {
            Pm::Static
        } else if value.eq_ignore_ascii_case(b"dynamic") {
            Pm::Dynamic
        } else if value.eq_ignore_ascii_case(b"ondemand") {
            Pm::Ondemand
        } else {
            return Err("invalid process manager (static, dynamic or ondemand)");
        }),
    })
}

impl Global {
    /// The global entry `name` given `made`.
    fn store(&mut self, name: &str, made: Set) {
        match (name, made) {
            ("pid", Set::Str(v)) => self.pid_file = Some(v),
            ("error_log", Set::Str(v)) => self.error_log = Some(v),
            ("syslog.ident", Set::Str(v)) => self.syslog_ident = Some(v),
            ("events.mechanism", Set::Str(v)) => self.events_mechanism = Some(v),
            ("syslog.facility", Set::Int(n)) => self.syslog_facility = n,
            ("log_level", Set::Int(n)) => self.log_level = n,
            ("log_limit", Set::Int(n)) => self.log_limit = n,
            ("emergency_restart_threshold", Set::Int(n)) => self.emergency_restart_threshold = n,
            ("emergency_restart_interval", Set::Int(n)) => self.emergency_restart_interval = n,
            ("process_control_timeout", Set::Int(n)) => self.process_control_timeout = n,
            ("process.max", Set::Int(n)) => self.process_max = n,
            ("process.priority", Set::Int(n)) => self.process_priority = n,
            ("rlimit_files", Set::Int(n)) => self.rlimit_files = n,
            ("rlimit_core", Set::Int(n)) => self.rlimit_core = n,
            ("log_buffering", Set::Bool(b)) => self.log_buffering = b,
            ("daemonize", Set::Bool(b)) => self.daemonize = b,
            _ => {}
        }
    }
}

impl PoolConfig {
    /// The pool entry `name` given `made`.
    fn store(&mut self, name: &str, made: Set) {
        match made {
            Set::Str(v) => {
                let field = match name {
                    "prefix" => &mut self.prefix,
                    "user" => &mut self.user,
                    "group" => &mut self.group,
                    "listen" => &mut self.listen_address,
                    "listen.owner" => &mut self.listen_owner,
                    "listen.group" => &mut self.listen_group,
                    "listen.mode" => &mut self.listen_mode,
                    "listen.allowed_clients" => &mut self.listen_allowed_clients,
                    "pm.status_path" => &mut self.pm_status_path,
                    "pm.status_listen" => &mut self.pm_status_listen,
                    "ping.path" => &mut self.ping_path,
                    "ping.response" => &mut self.ping_response,
                    "access.log" => &mut self.access_log,
                    "access.format" => &mut self.access_format,
                    "slowlog" => &mut self.slowlog,
                    "chroot" => &mut self.chroot,
                    "chdir" => &mut self.chdir,
                    "security.limit_extensions" => &mut self.security_limit_extensions,
                    _ => return,
                };
                *field = Some(v);
            }
            Set::Int(n) => {
                let field = match name {
                    "listen.backlog" => &mut self.listen_backlog,
                    "process.priority" => &mut self.process_priority,
                    "pm.max_children" => &mut self.pm_max_children,
                    "pm.start_servers" => &mut self.pm_start_servers,
                    "pm.min_spare_servers" => &mut self.pm_min_spare_servers,
                    "pm.max_spare_servers" => &mut self.pm_max_spare_servers,
                    "pm.max_spawn_rate" => &mut self.pm_max_spawn_rate,
                    "pm.process_idle_timeout" => &mut self.pm_process_idle_timeout,
                    "pm.max_requests" => &mut self.pm_max_requests,
                    "request_slowlog_timeout" => &mut self.request_slowlog_timeout,
                    "request_slowlog_trace_depth" => &mut self.request_slowlog_trace_depth,
                    "request_terminate_timeout" => &mut self.request_terminate_timeout,
                    "rlimit_files" => &mut self.rlimit_files,
                    "rlimit_core" => &mut self.rlimit_core,
                    _ => return,
                };
                *field = n;
            }
            Set::Bool(b) => {
                let field = match name {
                    "process.dumpable" => &mut self.process_dumpable,
                    "request_terminate_timeout_track_finished" => {
                        &mut self.request_terminate_timeout_track_finished
                    }
                    "catch_workers_output" => &mut self.catch_workers_output,
                    "decorate_workers_output" => &mut self.decorate_workers_output,
                    "clear_env" => &mut self.clear_env,
                    _ => return,
                };
                *field = b;
            }
            Set::Pm(pm) => self.pm = Some(pm),
        }
    }
}

/// The text of a value the normal scanner made, which is always a string.
fn text(value: &IniValue) -> Vec<u8> {
    match value {
        IniValue::Str(s) => s.clone(),
        IniValue::Bool(true) => b"1".to_vec(),
        IniValue::Long(n) => n.to_string().into_bytes(),
        IniValue::Double(d) => d.to_string().into_bytes(),
        IniValue::Bool(false) | IniValue::Null => Vec::new(),
    }
}

/// The reader's state, `fpm_conf.c`'s statics.
struct Reader<'e> {
    engine: &'e Engine,
    /// `fpm_globals.prefix`, `-p`'s.
    prefix: Option<Vec<u8>>,
    parsed: Parsed,
    /// `current_wp`: the section being read, `None` for `[global]`.
    current: Option<usize>,
    recursion: u32,
    filename: Vec<u8>,
    lineno: u32,
    include: Option<Vec<u8>>,
    error: bool,
}

/// Bytes as a path.
pub(crate) fn path(bytes: &[u8]) -> &std::path::Path {
    std::path::Path::new(std::ffi::OsStr::from_bytes(bytes))
}

/// `fpm_conf_is_dir`.
pub(crate) fn is_dir(bytes: &[u8]) -> bool {
    std::fs::metadata(path(bytes)).is_ok_and(|meta| meta.is_dir())
}

impl Reader<'_> {
    /// The file and line an error names.
    fn at(&self) -> String {
        format!(
            "[{}:{}]",
            String::from_utf8_lossy(&self.filename),
            self.lineno
        )
    }

    /// `fpm_conf_ini_parser`: one event of a line.
    fn event(&mut self, event: IniEvent) {
        if self.error {
            return;
        }
        match event {
            IniEvent::Section(name) => self.section(&text(&name)),
            IniEvent::Entry(name, value) => self.entry(&text(&name), value.as_ref().map(text)),
            IniEvent::PopEntry(name, value, key) => {
                self.array(&text(&name), &text(&key), &text(&value));
            }
        }
    }

    /// `fpm_conf_ini_parser_section`.
    fn section(&mut self, name: &[u8]) {
        if name.eq_ignore_ascii_case(b"global") {
            self.current = None;
            return;
        }
        if let Some(at) = self
            .parsed
            .pools
            .iter()
            .position(|pool| pool.name.eq_ignore_ascii_case(name))
        {
            self.current = Some(at);
            return;
        }
        self.parsed.pools.push(PoolConfig::named(name.to_vec()));
        self.parsed.shared.push(None);
        self.current = Some(self.parsed.pools.len() - 1);
    }

    /// `fpm_conf_ini_parser_entry`.
    fn entry(&mut self, name: &[u8], value: Option<Vec<u8>>) {
        let Some(value) = value else {
            zlog!(
                Error @ fpm_conf_ini_parser_entry:1469,
                "{} value is NULL for a ZEND_INI_PARSER_ENTRY",
                self.at()
            );
            self.error = true;
            return;
        };
        if name == b"include" {
            if self.include.is_some() {
                zlog!(Error @ fpm_conf_ini_parser_entry:1476, "{} two includes at the same time !", self.at());
                self.error = true;
                return;
            }
            self.include = Some(value);
            return;
        }
        let options = if self.current.is_some() {
            POOL_OPTIONS
        } else {
            GLOBAL_OPTIONS
        };
        let Some(&(option, setter)) = options
            .iter()
            .find(|(option, _)| name.eq_ignore_ascii_case(option.as_bytes()))
        else {
            zlog!(
                Error @ fpm_conf_ini_parser_entry:1514,
                "{} unknown entry '{}'",
                self.at(),
                String::from_utf8_lossy(name)
            );
            self.error = true;
            return;
        };
        let pool = self.current.map(|at| self.parsed.pools[at].name.clone());
        match set(setter, &value, pool.as_deref()) {
            Ok(made) => match self.current {
                Some(at) => self.parsed.pools[at].store(option, made),
                None => self.parsed.global.store(option, made),
            },
            Err(why) => {
                zlog!(
                    Error @ fpm_conf_ini_parser_entry:1503,
                    "{} unable to parse value for entry '{option}': {why}",
                    self.at()
                );
                self.error = true;
            }
        }
    }

    /// `fpm_conf_ini_parser_array`.
    fn array(&mut self, name: &[u8], key: &[u8], value: &[u8]) {
        let name_text = String::from_utf8_lossy(name).into_owned();
        if name == b"access.suppress_path" {
            if !key.is_empty() {
                zlog!(
                    Error @ fpm_conf_ini_parser_array:1527,
                    "{} Keys provided to field 'access.suppress_path' are ignored",
                    self.at()
                );
                self.error = true;
            }
            if value.first() != Some(&b'/') {
                zlog!(
                    Error @ fpm_conf_ini_parser_array:1531,
                    "{} Values provided to field 'access.suppress_path' must begin with '/'",
                    self.at()
                );
                self.error = true;
            }
            if self.error {
                return;
            }
        } else if key.is_empty() {
            zlog!(
                Error @ fpm_conf_ini_parser_array:1538,
                "{} You must provide a key for field '{name_text}'",
                self.at()
            );
            self.error = true;
            return;
        }
        let Some(at) = self.current else {
            zlog!(
                Error @ fpm_conf_ini_parser_array:1544,
                "{} Array are not allowed in the global section",
                self.at()
            );
            self.error = true;
            return;
        };
        let pool_name = self.parsed.pools[at].name.clone();
        let expanded = |value: &[u8]| {
            expand_pool(value, Some(&pool_name))
                .ok_or("Can't use '$pool' when the pool is not defined")
        };
        let made: Result<(), &'static str> = (|| {
            let pool = &mut self.parsed.pools[at];
            match name {
                b"env" => {
                    if value.is_empty() {
                        return Err("");
                    }
                    pool.env.insert(0, (key.to_vec(), expanded(value)?));
                }
                b"php_value" => pool.php_values.insert(0, (key.to_vec(), expanded(value)?)),
                b"php_admin_value" => pool
                    .php_admin_values
                    .insert(0, (key.to_vec(), expanded(value)?)),
                b"php_flag" => pool.php_values.insert(0, (key.to_vec(), flag(value)?)),
                b"php_admin_flag" => pool
                    .php_admin_values
                    .insert(0, (key.to_vec(), flag(value)?)),
                b"access.suppress_path" => pool.access_suppress_paths.insert(0, expanded(value)?),
                _ => return Err("unknown"),
            }
            Ok(())
        })();
        match made {
            Ok(()) => {}
            Err("") => {
                zlog!(Error @ fpm_conf_ini_parser_array:1551, "{} empty value", self.at());
                self.error = true;
            }
            Err("unknown") => {
                zlog!(Error @ fpm_conf_ini_parser_array:1579, "{} unknown directive '{name_text}'", self.at());
                self.error = true;
            }
            Err(why) => {
                zlog!(
                    Error @ fpm_conf_ini_parser_array:1585,
                    "{} error while parsing '{name_text}[{}]' : {why}",
                    self.at(),
                    String::from_utf8_lossy(key)
                );
                self.error = true;
            }
        }
    }

    /// `fpm_conf_load_ini_file`: each line of `filename` parsed alone,
    /// and an `include` it named loaded after it.
    fn load(&mut self, filename: &[u8]) -> bool {
        if filename.is_empty() {
            zlog!(Error @ fpm_conf_load_ini_file:1630, "configuration filename is empty");
            return false;
        }
        let contents = match std::fs::read(path(filename)) {
            Ok(contents) => contents,
            Err(e) => {
                zlog_sys!(
                    Error @ fpm_conf_load_ini_file:1636,
                    errno(&e),
                    "failed to open configuration file '{}'",
                    String::from_utf8_lossy(filename)
                );
                return false;
            }
        };
        self.recursion += 1;
        if self.recursion > 5 {
            zlog!(Error @ fpm_conf_load_ini_file:1641, "failed to include more than 5 files recursively");
            self.recursion -= 1;
            return false;
        }
        self.lineno = 0;
        let mut lines = contents.split(|b| *b == b'\n').peekable();
        while let Some(line) = lines.next() {
            if lines.peek().is_none() && line.is_empty() {
                break;
            }
            self.lineno += 1;
            self.filename = filename.to_vec();
            if line.is_empty() {
                continue;
            }
            let mut buf = line.to_vec();
            buf.push(b'\n');
            let (events, failed) = self.engine.parse_ini(&buf);
            for event in events {
                self.event(event);
            }
            self.filename = filename.to_vec();
            if let Some((message, line)) = &failed {
                std::io::Write::write_all(
                    &mut std::io::stderr(),
                    format!("PHP:  {message} in Unknown on line {line}\n").as_bytes(),
                )
                .ok();
            }
            if self.error || failed.is_some() {
                self.include = None;
                self.recursion -= 1;
                return false;
            }
            if let Some(include) = self.include.take() {
                let include =
                    evaluate_full_path(&include, None, self.prefix.as_deref(), None, false)
                        .unwrap_or(include);
                if !self.include(&include) {
                    self.recursion -= 1;
                    return false;
                }
            }
        }
        self.recursion -= 1;
        true
    }

    /// `fpm_conf_ini_parser_include`: each file the pattern matches
    /// loaded in turn.
    fn include(&mut self, pattern: &[u8]) -> bool {
        let filename = self.filename.clone();
        let lineno = self.lineno;
        let matches = crate::glob::glob(pattern);
        if matches.is_empty() {
            zlog!(
                Warning @ fpm_conf_ini_parser_include:1400,
                "Nothing matches the include pattern '{}' from {} at line {lineno}.",
                String::from_utf8_lossy(pattern),
                String::from_utf8_lossy(&filename)
            );
            return true;
        }
        for file in matches {
            if file.last() == Some(&b'/') {
                continue;
            }
            if !self.load(&file) {
                zlog!(
                    Error @ fpm_conf_ini_parser_include:1414,
                    "Unable to include {} from {} at line {}",
                    String::from_utf8_lossy(&file),
                    String::from_utf8_lossy(&filename),
                    self.lineno
                );
                return false;
            }
        }
        true
    }
}

/// `fpm_conf_set_array`'s boolean half: `1` or `0`.
fn flag(value: &[u8]) -> Result<Vec<u8>, &'static str> {
    Ok(if boolean(value)? {
        b"1".to_vec()
    } else {
        b"0".to_vec()
    })
}

/// The errno an I/O error carries.
pub(crate) fn errno(e: &std::io::Error) -> nix::errno::Errno {
    nix::errno::Errno::from_raw(e.raw_os_error().unwrap_or(0))
}

/// `fpm_evaluate_full_path`: a relative `path` put under the pool's
/// prefix, else `-p`'s, else `default_prefix` or `PHP_PREFIX`; `$prefix`
/// at its start read as the prefix where `expand` says. `None` where
/// `$prefix` stands elsewhere, which is logged.
pub(crate) fn evaluate_full_path(
    path: &[u8],
    pool_prefix: Option<&[u8]>,
    global_prefix: Option<&[u8]>,
    default_prefix: Option<&[u8]>,
    expand: bool,
) -> Option<Vec<u8>> {
    if path.is_empty() || path[0] == b'/' {
        return Some(path.to_vec());
    }
    let prefix = pool_prefix
        .or(global_prefix)
        .or(default_prefix)
        .unwrap_or(PHP_PREFIX.as_bytes());
    let mut rest = Some(path.to_vec());
    if expand && let Some(at) = find(path, b"$prefix") {
        if at != 0 {
            zlog!(Error @ fpm_evaluate_full_path:808, "'$prefix' must be use at the beginning of the value");
            return None;
        }
        rest = (path.len() > 7).then(|| path[7..].to_vec());
    }
    let full = match rest {
        Some(rest) => {
            let mut full = prefix.to_vec();
            full.push(b'/');
            full.extend_from_slice(&rest);
            full
        }
        None => prefix.to_vec(),
    };
    if full.first() != Some(&b'/') && pool_prefix.is_some() {
        return evaluate_full_path(&full, None, global_prefix, default_prefix, expand);
    }
    Some(full)
}

/// `fpm_conf_load_ini_file` of the configuration file: what it set, or
/// nothing where a line could not be read.
pub(crate) fn read(engine: &Engine, config: &[u8], prefix: Option<&[u8]>) -> Option<Parsed> {
    let mut reader = Reader {
        engine,
        prefix: prefix.map(<[u8]>::to_vec),
        parsed: Parsed::default(),
        current: None,
        recursion: 0,
        filename: Vec::new(),
        lineno: 0,
        include: None,
        error: false,
    };
    reader.load(config).then_some(reader.parsed)
}

/// `fpm_conf_init_main`'s default configuration file: `php-fpm.conf`
/// under the engine's `PHP_SYSCONFDIR`, or under `-p`'s `etc`.
pub(crate) fn default_config(prefix: Option<&[u8]>) -> Vec<u8> {
    match prefix {
        None => format!("{PHP_SYSCONFDIR}/php-fpm.conf").into_bytes(),
        Some(prefix) => {
            let mut path = prefix.to_vec();
            path.extend_from_slice(b"/etc/php-fpm.conf");
            path
        }
    }
}

/// `STR2STR`.
fn show(value: Option<&[u8]>) -> String {
    value.map_or_else(
        || String::from("undefined"),
        |v| String::from_utf8_lossy(v).into_owned(),
    )
}

/// `BOOL2STR`.
fn yes(value: bool) -> &'static str {
    if value { "yes" } else { "no" }
}

/// `zlog_get_level_name`.
pub(crate) fn level_name(level: i32) -> &'static str {
    match level {
        1 => Level::Debug.name(),
        2 => Level::Notice.name(),
        3 => Level::Warning.name(),
        4 => Level::Error.name(),
        5 => Level::Alert.name(),
        _ => "unknown value",
    }
}

/// `fpm_conf_dump`: every entry of `[global]` and of each pool but a
/// shared status pool, in `php-fpm.conf.in`'s order, a line each.
#[allow(clippy::too_many_lines)] // `fpm_conf_dump`, a line per entry
pub(crate) fn dump(
    global: &Global,
    log_level: i32,
    mechanism: &str,
    pools: &[(&PoolConfig, bool)],
) {
    let notice = |line: u32, text: String| {
        zlog!(Notice @ (crate::zlog::Site::at("fpm_conf_dump", line)), "{text}");
    };
    notice(1714, String::from("[global]"));
    notice(
        1715,
        format!("\tpid = {}", show(global.pid_file.as_deref())),
    );
    notice(
        1716,
        format!("\terror_log = {}", show(global.error_log.as_deref())),
    );
    notice(
        1718,
        format!("\tsyslog.ident = {}", show(global.syslog_ident.as_deref())),
    );
    notice(
        1719,
        format!("\tsyslog.facility = {}", global.syslog_facility),
    );
    notice(
        1721,
        format!("\tlog_buffering = {}", yes(global.log_buffering)),
    );
    notice(1722, format!("\tlog_level = {}", level_name(log_level)));
    notice(1723, format!("\tlog_limit = {}", global.log_limit));
    notice(
        1724,
        format!(
            "\temergency_restart_interval = {}s",
            global.emergency_restart_interval
        ),
    );
    notice(
        1725,
        format!(
            "\temergency_restart_threshold = {}",
            global.emergency_restart_threshold
        ),
    );
    notice(
        1726,
        format!(
            "\tprocess_control_timeout = {}s",
            global.process_control_timeout
        ),
    );
    notice(1727, format!("\tprocess.max = {}", global.process_max));
    if global.process_priority == 64 {
        notice(1729, String::from("\tprocess.priority = undefined"));
    } else {
        notice(
            1731,
            format!("\tprocess.priority = {}", global.process_priority),
        );
    }
    notice(1733, format!("\tdaemonize = {}", yes(global.daemonize)));
    notice(1734, format!("\trlimit_files = {}", global.rlimit_files));
    notice(1735, format!("\trlimit_core = {}", global.rlimit_core));
    notice(1736, format!("\tevents.mechanism = {mechanism}"));
    notice(1740, String::from(" "));
    for &(pool, shared) in pools {
        if shared {
            continue;
        }
        let s = |v: &Option<Vec<u8>>| show(v.as_deref());
        notice(1749, format!("[{}]", String::from_utf8_lossy(&pool.name)));
        notice(1750, format!("\tprefix = {}", s(&pool.prefix)));
        notice(1751, format!("\tuser = {}", s(&pool.user)));
        notice(1752, format!("\tgroup = {}", s(&pool.group)));
        notice(1753, format!("\tlisten = {}", s(&pool.listen_address)));
        notice(1754, format!("\tlisten.backlog = {}", pool.listen_backlog));
        notice(1759, format!("\tlisten.owner = {}", s(&pool.listen_owner)));
        notice(1760, format!("\tlisten.group = {}", s(&pool.listen_group)));
        notice(1761, format!("\tlisten.mode = {}", s(&pool.listen_mode)));
        notice(
            1762,
            format!(
                "\tlisten.allowed_clients = {}",
                s(&pool.listen_allowed_clients)
            ),
        );
        if pool.process_priority == 64 {
            notice(1767, String::from("\tprocess.priority = undefined"));
        } else {
            notice(
                1769,
                format!("\tprocess.priority = {}", pool.process_priority),
            );
        }
        notice(
            1771,
            format!("\tprocess.dumpable = {}", yes(pool.process_dumpable)),
        );
        notice(
            1772,
            format!("\tpm = {}", pool.pm.map_or("(null)", Pm::name)),
        );
        notice(
            1773,
            format!("\tpm.max_children = {}", pool.pm_max_children),
        );
        notice(
            1774,
            format!("\tpm.start_servers = {}", pool.pm_start_servers),
        );
        notice(
            1775,
            format!("\tpm.min_spare_servers = {}", pool.pm_min_spare_servers),
        );
        notice(
            1776,
            format!("\tpm.max_spare_servers = {}", pool.pm_max_spare_servers),
        );
        notice(
            1777,
            format!("\tpm.max_spawn_rate = {}", pool.pm_max_spawn_rate),
        );
        notice(
            1778,
            format!(
                "\tpm.process_idle_timeout = {}",
                pool.pm_process_idle_timeout
            ),
        );
        notice(
            1779,
            format!("\tpm.max_requests = {}", pool.pm_max_requests),
        );
        notice(
            1780,
            format!("\tpm.status_path = {}", s(&pool.pm_status_path)),
        );
        notice(
            1781,
            format!("\tpm.status_listen = {}", s(&pool.pm_status_listen)),
        );
        notice(1782, format!("\tping.path = {}", s(&pool.ping_path)));
        notice(
            1783,
            format!("\tping.response = {}", s(&pool.ping_response)),
        );
        notice(1784, format!("\taccess.log = {}", s(&pool.access_log)));
        notice(
            1785,
            format!("\taccess.format = {}", s(&pool.access_format)),
        );
        for path in &pool.access_suppress_paths {
            notice(
                1787,
                format!(
                    "\taccess.suppress_path[] = {}",
                    String::from_utf8_lossy(path)
                ),
            );
        }
        notice(1789, format!("\tslowlog = {}", s(&pool.slowlog)));
        notice(
            1790,
            format!(
                "\trequest_slowlog_timeout = {}s",
                pool.request_slowlog_timeout
            ),
        );
        notice(
            1791,
            format!(
                "\trequest_slowlog_trace_depth = {}",
                pool.request_slowlog_trace_depth
            ),
        );
        notice(
            1792,
            format!(
                "\trequest_terminate_timeout = {}s",
                pool.request_terminate_timeout
            ),
        );
        notice(
            1793,
            format!(
                "\trequest_terminate_timeout_track_finished = {}",
                yes(pool.request_terminate_timeout_track_finished)
            ),
        );
        notice(1794, format!("\trlimit_files = {}", pool.rlimit_files));
        notice(1795, format!("\trlimit_core = {}", pool.rlimit_core));
        notice(1796, format!("\tchroot = {}", s(&pool.chroot)));
        notice(1797, format!("\tchdir = {}", s(&pool.chdir)));
        notice(
            1798,
            format!(
                "\tcatch_workers_output = {}",
                yes(pool.catch_workers_output)
            ),
        );
        notice(
            1799,
            format!(
                "\tdecorate_workers_output = {}",
                yes(pool.decorate_workers_output)
            ),
        );
        notice(1800, format!("\tclear_env = {}", yes(pool.clear_env)));
        notice(
            1801,
            format!(
                "\tsecurity.limit_extensions = {}",
                s(&pool.security_limit_extensions)
            ),
        );
        for (key, value) in &pool.env {
            notice(
                1804,
                format!(
                    "\tenv[{}] = {}",
                    String::from_utf8_lossy(key),
                    String::from_utf8_lossy(value)
                ),
            );
        }
        for (key, value) in &pool.php_values {
            notice(
                1808,
                format!(
                    "\tphp_value[{}] = {}",
                    String::from_utf8_lossy(key),
                    String::from_utf8_lossy(value)
                ),
            );
        }
        for (key, value) in &pool.php_admin_values {
            notice(
                1812,
                format!(
                    "\tphp_admin_value[{}] = {}",
                    String::from_utf8_lossy(key),
                    String::from_utf8_lossy(value)
                ),
            );
        }
        notice(1814, String::from(" "));
    }
}

/// `fpm_worker_pool_shared_status_alloc`: the pool `pm.status_listen`
/// makes, `<name>_status`, `ondemand` with two children, serving the
/// status and ping pages of the pool it copies from.
pub(crate) fn shared_status(of: &PoolConfig) -> PoolConfig {
    let mut name = of.name.clone();
    name.extend_from_slice(b"_status");
    let mut config = PoolConfig::named(name);
    config.listen_address.clone_from(&of.pm_status_listen);
    config
        .listen_allowed_clients
        .clone_from(&of.listen_allowed_clients);
    config.listen_group.clone_from(&of.listen_group);
    config.listen_owner.clone_from(&of.listen_owner);
    config.listen_mode.clone_from(&of.listen_mode);
    config.user.clone_from(&of.user);
    config.group.clone_from(&of.group);
    config.pm_status_path = Some(of.pm_status_path.clone().unwrap_or_else(|| b"/".to_vec()));
    config.ping_path.clone_from(&of.ping_path);
    config.ping_response.clone_from(&of.ping_response);
    for (key, value) in &of.php_values {
        config.php_values.insert(0, (key.clone(), value.clone()));
    }
    for (key, value) in &of.php_admin_values {
        config
            .php_admin_values
            .insert(0, (key.clone(), value.clone()));
    }
    config.pm = Some(Pm::Ondemand);
    config.pm_max_children = 2;
    config
}
