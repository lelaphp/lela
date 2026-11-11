//! `fpm_conf_process_all_pools`: each pool checked and completed after
//! the configuration is read, each refusal php's sentence; the status
//! pool a `pm.status_listen` adds; the heartbeat the timeouts need; and
//! no two pools on one address.

use crate::conf::{self, Pm, evaluate_full_path, is_dir};
use crate::master::{Fpm, Pool};
use crate::sockets::{Domain, domain_of};
use crate::{zlog, zlog_sys};

/// `FPM_PHP_INI_TO_EXPAND`: the entries whose relative path a pool's
/// prefix completes.
const INI_TO_EXPAND: &[&str] = &[
    "error_log",
    "extension_dir",
    "sendmail_path",
    "session.cookie_path",
    "session_pgsql.sem_file_name",
    "soap.wsdl_cache_dir",
    "uploadprogress.file.filename_template",
    "xdebug.output_dir",
    "xdebug.profiler_output_dir",
    "xdebug.trace_output_dir",
    "xmms.path",
    "axis2.client_home",
    "blenc.key_file",
    "coin_acceptor.device",
];

/// A path the page names must be `/` and `[alphanum]/_-.~` alone.
fn path_chars(path: &[u8]) -> bool {
    path.iter()
        .all(|&b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'-' | b'_' | b'.' | b'~'))
}

/// The heartbeat a timeout of `seconds` asks: a third of it.
fn heartbeat(fpm: &mut Fpm, seconds: i32) {
    let ms = seconds.saturating_mul(1000) / 3;
    fpm.heartbeat = if fpm.heartbeat > 0 {
        fpm.heartbeat.min(ms)
    } else {
        ms
    };
}

/// `fpm_conf_process_all_pools`.
#[allow(clippy::too_many_lines)] // php's one function, kept as one
pub(crate) fn process_all(fpm: &mut Fpm) -> bool {
    if fpm.pools.is_empty() {
        zlog!(
            Error @ fpm_conf_process_all_pools:844,
            "No pool defined. at least one pool section must be specified in config file"
        );
        return false;
    }
    let global_prefix = fpm.prefix.clone();
    let mut i = 0;
    while i < fpm.pools.len() {
        let name = fpm.pools[i].name();
        let is_shared = fpm.pools[i].shared.is_some();
        let c = &mut fpm.pools[i].config;
        if let Some(prefix) = c.prefix.clone().filter(|p| !p.is_empty()) {
            c.prefix = evaluate_full_path(&prefix, None, global_prefix.as_deref(), None, false);
            let prefix = c.prefix.clone().unwrap_or_default();
            if !is_dir(&prefix) {
                zlog!(
                    Error @ fpm_conf_process_all_pools:855,
                    bytes "[pool {}] the prefix '{}' does not exist or is not a directory",
                    name,
                    prefix
                );
                return false;
            }
        }
        if c.user.is_none() && nix::unistd::Uid::effective().is_root() && !fpm.run_as_root {
            zlog!(
                Alert @ fpm_conf_process_all_pools:862,
                bytes "[pool {}] 'user' directive has not been specified when running as a root without --allow-to-run-as-root",
                name
            );
            return false;
        }
        let c = &mut fpm.pools[i].config;
        let Some(address) = c.listen_address.clone().filter(|a| !a.is_empty()) else {
            zlog!(Alert @ fpm_conf_process_all_pools:874, bytes "[pool {}] no listen address have been defined!", name);
            return false;
        };
        let domain = domain_of(&address);
        if domain == Domain::Unix && address[0] != b'/' {
            c.listen_address = evaluate_full_path(
                &address,
                c.prefix.as_deref(),
                global_prefix.as_deref(),
                None,
                false,
            );
        }
        fpm.pools[i].domain = domain;
        let c = &mut fpm.pools[i].config;
        if c.process_priority != 64 && !(-19..=20).contains(&c.process_priority) {
            zlog!(
                Error @ fpm_conf_process_all_pools:879,
                bytes "[pool {}] process.priority must be included into [-19,20]",
                name
            );
            return false;
        }
        let Some(pm) = c.pm else {
            zlog!(
                Alert @ fpm_conf_process_all_pools:885,
                bytes "[pool {}] the process manager is missing (static, dynamic or ondemand)",
                name
            );
            return false;
        };
        if c.pm_max_children < 1 {
            zlog!(
                Alert @ fpm_conf_process_all_pools:891,
                bytes "[pool {}] pm.max_children must be a positive value",
                name
            );
            return false;
        }
        match pm {
            Pm::Dynamic => {
                let (min, max, children) = (
                    c.pm_min_spare_servers,
                    c.pm_max_spare_servers,
                    c.pm_max_children,
                );
                if min <= 0 {
                    zlog!(
                        Alert @ fpm_conf_process_all_pools:900,
                        bytes "[pool {}] pm.min_spare_servers({}) must be a positive value",
                        name,
                        min
                    );
                    return false;
                }
                if max <= 0 {
                    zlog!(
                        Alert @ fpm_conf_process_all_pools:905,
                        bytes "[pool {}] pm.max_spare_servers({}) must be a positive value",
                        name,
                        max
                    );
                    return false;
                }
                if min > children || max > children {
                    zlog!(
                        Alert @ fpm_conf_process_all_pools:911,
                        bytes "[pool {}] pm.min_spare_servers({}) and pm.max_spare_servers({}) cannot be greater than pm.max_children({})",
                        name,
                        min,
                        max,
                        children
                    );
                    return false;
                }
                if max < min {
                    zlog!(
                        Alert @ fpm_conf_process_all_pools:916,
                        bytes "[pool {}] pm.max_spare_servers({}) must not be less than pm.min_spare_servers({})",
                        name,
                        max,
                        min
                    );
                    return false;
                }
                if c.pm_start_servers <= 0 {
                    c.pm_start_servers = min + (max - min) / 2;
                    zlog!(
                        Notice @ fpm_conf_process_all_pools:922,
                        bytes "[pool {}] pm.start_servers is not set. It's been set to {}.",
                        name,
                        c.pm_start_servers
                    );
                } else if c.pm_start_servers < min || c.pm_start_servers > max {
                    zlog!(
                        Alert @ fpm_conf_process_all_pools:925,
                        bytes "[pool {}] pm.start_servers({}) must not be less than pm.min_spare_servers({}) and not greater than pm.max_spare_servers({})",
                        name,
                        c.pm_start_servers,
                        min,
                        max
                    );
                    return false;
                }
                if c.pm_max_spawn_rate < 1 {
                    zlog!(
                        Alert @ fpm_conf_process_all_pools:930,
                        bytes "[pool {}] pm.max_spawn_rate must be a positive value",
                        name
                    );
                    return false;
                }
            }
            Pm::Ondemand => {
                if c.pm_process_idle_timeout < 1 {
                    zlog!(
                        Alert @ fpm_conf_process_all_pools:942,
                        bytes "[pool {}] pm.process_idle_timeout({}s) must be greater than 0s",
                        name,
                        c.pm_process_idle_timeout
                    );
                    return false;
                }
                if c.listen_backlog < conf::BACKLOG_DEFAULT {
                    zlog!(
                        Warning @ fpm_conf_process_all_pools:947,
                        bytes "[pool {}] listen.backlog({}) was too low for the ondemand process manager. I updated it for you to {}.",
                        name,
                        c.listen_backlog,
                        conf::BACKLOG_DEFAULT
                    );
                    c.listen_backlog = conf::BACKLOG_DEFAULT;
                }
                c.pm_start_servers = 0;
                c.pm_min_spare_servers = 0;
                c.pm_max_spare_servers = 0;
            }
            Pm::Static => {}
        }
        if c.pm_status_listen.is_some() {
            let status = conf::shared_status(c);
            if c.pm_status_path.is_none() {
                c.pm_status_path = Some(b"/".to_vec());
            }
            fpm.pools.push(Pool {
                config: status,
                shared: Some(i),
                domain: Domain::Inet,
                listening: None,
                credentials: crate::unix::Credentials::default(),
                children: Vec::new(),
                idle_spawn_rate: 1,
                warn_max_children: false,
                scoreboard: None,
                access_log: None,
                limit_extensions: Vec::new(),
            });
        }
        let c = &mut fpm.pools[i].config;
        if let Some(status) = c.pm_status_path.clone().filter(|s| !s.is_empty()) {
            let shown = &status;
            if status[0] != b'/' {
                zlog!(
                    Error @ fpm_conf_process_all_pools:967,
                    bytes "[pool {}] the status path '{}' must start with a '/'",
                    name,
                    shown
                );
                return false;
            }
            if c.pm_status_listen.is_none() && !is_shared && status.len() < 2 {
                zlog!(
                    Error @ fpm_conf_process_all_pools:972,
                    bytes "[pool {}] the status path '{}' is not long enough",
                    name,
                    shown
                );
                return false;
            }
            if !path_chars(&status) {
                zlog!(
                    Error @ fpm_conf_process_all_pools:978,
                    bytes "[pool {}] the status path '{}' must contain only the following characters '[alphanum]/_-.~'",
                    name,
                    shown
                );
                return false;
            }
        }
        if let Some(ping) = c.ping_path.clone().filter(|p| !p.is_empty()) {
            let shown = &ping;
            if ping[0] != b'/' {
                zlog!(
                    Error @ fpm_conf_process_all_pools:990,
                    bytes "[pool {}] the ping path '{}' must start with a '/'",
                    name,
                    shown
                );
                return false;
            }
            if ping.len() < 2 {
                zlog!(
                    Error @ fpm_conf_process_all_pools:995,
                    bytes "[pool {}] the ping path '{}' is not long enough",
                    name,
                    shown
                );
                return false;
            }
            if !path_chars(&ping) {
                zlog!(
                    Error @ fpm_conf_process_all_pools:1001,
                    bytes "[pool {}] the ping path '{}' must contain only the following characters '[alphanum]/_-.~'",
                    name,
                    shown
                );
                return false;
            }
            match &c.ping_response {
                None => c.ping_response = Some(b"pong".to_vec()),
                Some(response) if response.is_empty() => {
                    zlog!(
                        Error @ fpm_conf_process_all_pools:1010,
                        bytes "[pool {}] the ping response page '{}' is not long enough",
                        name,
                        response
                    );
                    return false;
                }
                Some(_) => {}
            }
        } else {
            c.ping_response = None;
        }
        let prefix = c.prefix.clone();
        let full = |path: &[u8], expand: bool| {
            evaluate_full_path(
                path,
                prefix.as_deref(),
                global_prefix.as_deref(),
                None,
                expand,
            )
        };
        if let Some(log) = c.access_log.clone().filter(|l| !l.is_empty()) {
            c.access_log = full(&log, false);
            if c.access_format.is_none() {
                c.access_format = Some(b"%R - %u %t \"%m %r\" %s".to_vec());
            }
        }
        let terminate = c.request_terminate_timeout;
        if let Some(slowlog) = c.slowlog.clone().filter(|s| !s.is_empty()) {
            c.slowlog = full(&slowlog, false);
        }
        let slowlog_timeout = c.request_slowlog_timeout;
        let has_slowlog = c.slowlog.as_deref().is_some_and(|s| !s.is_empty());
        if slowlog_timeout != 0 {
            if !has_slowlog {
                zlog!(
                    Error @ fpm_conf_process_all_pools:1042,
                    bytes "[pool {}] 'slowlog' must be specified for use with 'request_slowlog_timeout'",
                    name
                );
                return false;
            }
            if let Some(slowlog) = c.slowlog.clone() {
                use std::os::unix::fs::OpenOptionsExt;
                if let Err(e) = std::fs::OpenOptions::new()
                    .append(true)
                    .create(true)
                    .mode(0o600)
                    .open(conf::path(&slowlog))
                {
                    zlog_sys!(
                        Error @ fpm_conf_process_all_pools:1062,
                        conf::errno(&e),
                        bytes "Unable to create or open slowlog({})",
                        slowlog
                    );
                    return false;
                }
            }
            if terminate != 0 && slowlog_timeout > terminate {
                zlog!(
                    Error @ fpm_conf_process_all_pools:1071,
                    bytes "[pool {}] 'request_slowlog_timeout' ({}) can't be greater than 'request_terminate_timeout' ({})",
                    name,
                    slowlog_timeout,
                    terminate
                );
                return false;
            }
        }
        let c = &mut fpm.pools[i].config;
        if c.request_slowlog_trace_depth != 0 {
            if !has_slowlog {
                zlog!(
                    Error @ fpm_conf_process_all_pools:1080,
                    bytes "[pool {}] 'slowlog' must be specified for use with 'request_slowlog_trace_depth'",
                    name
                );
                return false;
            }
            if c.request_slowlog_trace_depth <= 0 {
                zlog!(
                    Error @ fpm_conf_process_all_pools:1093,
                    bytes "[pool {}] 'request_slowlog_trace_depth' ({}) must be a positive value",
                    name,
                    c.request_slowlog_trace_depth
                );
                return false;
            }
        } else {
            c.request_slowlog_trace_depth = 20;
        }
        if let Some(chroot) = c.chroot.clone().filter(|r| !r.is_empty()) {
            let Some(chroot) = full(&chroot, true) else {
                return false;
            };
            c.chroot = Some(chroot.clone());
            let shown = &chroot;
            if chroot[0] != b'/' {
                zlog!(
                    Error @ fpm_conf_process_all_pools:1106,
                    bytes "[pool {}] the chroot path '{}' must start with a '/'",
                    name,
                    shown
                );
                return false;
            }
            if !is_dir(&chroot) {
                zlog!(
                    Error @ fpm_conf_process_all_pools:1111,
                    bytes "[pool {}] the chroot path '{}' does not exist or is not a directory",
                    name,
                    shown
                );
                return false;
            }
        }
        if let Some(chdir) = c.chdir.clone().filter(|d| !d.is_empty()) {
            let chdir = full(&chdir, false).unwrap_or(chdir);
            c.chdir = Some(chdir.clone());
            let shown = &chdir;
            if chdir[0] != b'/' {
                zlog!(
                    Error @ fpm_conf_process_all_pools:1122,
                    bytes "[pool {}] the chdir path '{}' must start with a '/'",
                    name,
                    shown
                );
                return false;
            }
            if let Some(chroot) = &c.chroot {
                let mut within = chroot.clone();
                within.push(b'/');
                within.extend_from_slice(&chdir);
                if !is_dir(&within) {
                    zlog!(
                        Error @ fpm_conf_process_all_pools:1132,
                        bytes "[pool {}] the chdir path '{}' within the chroot path '{}' ('{}') does not exist or is not a directory",
                        name,
                        shown,
                        chroot,
                        within
                    );
                    return false;
                }
            } else if !is_dir(&chdir) {
                zlog!(
                    Error @ fpm_conf_process_all_pools:1140,
                    bytes "[pool {}] the chdir path '{}' does not exist or is not a directory",
                    name,
                    shown
                );
                return false;
            }
        }
        if c.security_limit_extensions.is_none() {
            c.security_limit_extensions = Some(b".php .phar".to_vec());
        }
        let limits: Vec<Vec<u8>> = c
            .security_limit_extensions
            .as_deref()
            .unwrap_or_default()
            .split(|b| matches!(b, b' ' | b'\t'))
            .filter(|e| !e.is_empty())
            .map(<[u8]>::to_vec)
            .collect();
        if c.chroot.is_none() {
            for (key, value) in &mut c.php_values {
                if INI_TO_EXPAND
                    .iter()
                    .any(|name| key.eq_ignore_ascii_case(name.as_bytes()))
                {
                    *value = evaluate_full_path(
                        value,
                        prefix.as_deref(),
                        global_prefix.as_deref(),
                        None,
                        false,
                    )
                    .unwrap_or_default();
                }
            }
            for (key, value) in &mut c.php_admin_values {
                if key.eq_ignore_ascii_case(b"error_log") && value.eq_ignore_ascii_case(b"syslog") {
                    continue;
                }
                if INI_TO_EXPAND
                    .iter()
                    .any(|name| key.eq_ignore_ascii_case(name.as_bytes()))
                {
                    *value = evaluate_full_path(
                        value,
                        prefix.as_deref(),
                        global_prefix.as_deref(),
                        None,
                        false,
                    )
                    .unwrap_or_default();
                }
            }
        }
        fpm.pools[i].limit_extensions = limits;
        if terminate != 0 {
            heartbeat(fpm, terminate);
        }
        if slowlog_timeout != 0 {
            heartbeat(fpm, slowlog_timeout);
        }
        i += 1;
    }
    for a in 0..fpm.pools.len() {
        for b in 0..fpm.pools.len() {
            if a == b {
                continue;
            }
            let (x, y) = (
                &fpm.pools[a].config.listen_address,
                &fpm.pools[b].config.listen_address,
            );
            if x.as_deref().is_some_and(|x| !x.is_empty()) && x == y {
                zlog!(
                    Error @ fpm_conf_process_all_pools:1230,
                    bytes "[pool {}] unable to set listen address as it's already used in another pool '{}'",
                    fpm.pools[b].name(),
                    fpm.pools[a].name()
                );
                return false;
            }
        }
    }
    true
}
