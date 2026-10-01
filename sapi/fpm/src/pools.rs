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
                    "[pool {name}] the prefix '{}' does not exist or is not a directory",
                    String::from_utf8_lossy(&prefix)
                );
                return false;
            }
        }
        if c.user.is_none() && nix::unistd::Uid::effective().is_root() && !fpm.run_as_root {
            zlog!(
                Alert @ fpm_conf_process_all_pools:862,
                "[pool {name}] 'user' directive has not been specified when running as a root without --allow-to-run-as-root"
            );
            return false;
        }
        let c = &mut fpm.pools[i].config;
        let Some(address) = c.listen_address.clone().filter(|a| !a.is_empty()) else {
            zlog!(Alert @ fpm_conf_process_all_pools:874, "[pool {name}] no listen address have been defined!");
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
                "[pool {name}] process.priority must be included into [-19,20]"
            );
            return false;
        }
        let Some(pm) = c.pm else {
            zlog!(
                Alert @ fpm_conf_process_all_pools:885,
                "[pool {name}] the process manager is missing (static, dynamic or ondemand)"
            );
            return false;
        };
        if c.pm_max_children < 1 {
            zlog!(
                Alert @ fpm_conf_process_all_pools:891,
                "[pool {name}] pm.max_children must be a positive value"
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
                        "[pool {name}] pm.min_spare_servers({min}) must be a positive value"
                    );
                    return false;
                }
                if max <= 0 {
                    zlog!(
                        Alert @ fpm_conf_process_all_pools:905,
                        "[pool {name}] pm.max_spare_servers({max}) must be a positive value"
                    );
                    return false;
                }
                if min > children || max > children {
                    zlog!(
                        Alert @ fpm_conf_process_all_pools:911,
                        "[pool {name}] pm.min_spare_servers({min}) and pm.max_spare_servers({max}) cannot be greater than pm.max_children({children})"
                    );
                    return false;
                }
                if max < min {
                    zlog!(
                        Alert @ fpm_conf_process_all_pools:916,
                        "[pool {name}] pm.max_spare_servers({max}) must not be less than pm.min_spare_servers({min})"
                    );
                    return false;
                }
                if c.pm_start_servers <= 0 {
                    c.pm_start_servers = min + (max - min) / 2;
                    zlog!(
                        Notice @ fpm_conf_process_all_pools:922,
                        "[pool {name}] pm.start_servers is not set. It's been set to {}.",
                        c.pm_start_servers
                    );
                } else if c.pm_start_servers < min || c.pm_start_servers > max {
                    zlog!(
                        Alert @ fpm_conf_process_all_pools:925,
                        "[pool {name}] pm.start_servers({}) must not be less than pm.min_spare_servers({min}) and not greater than pm.max_spare_servers({max})",
                        c.pm_start_servers
                    );
                    return false;
                }
                if c.pm_max_spawn_rate < 1 {
                    zlog!(
                        Alert @ fpm_conf_process_all_pools:930,
                        "[pool {name}] pm.max_spawn_rate must be a positive value"
                    );
                    return false;
                }
            }
            Pm::Ondemand => {
                if c.pm_process_idle_timeout < 1 {
                    zlog!(
                        Alert @ fpm_conf_process_all_pools:942,
                        "[pool {name}] pm.process_idle_timeout({}s) must be greater than 0s",
                        c.pm_process_idle_timeout
                    );
                    return false;
                }
                if c.listen_backlog < conf::BACKLOG_DEFAULT {
                    zlog!(
                        Warning @ fpm_conf_process_all_pools:947,
                        "[pool {name}] listen.backlog({}) was too low for the ondemand process manager. I updated it for you to {}.",
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
            let shown = String::from_utf8_lossy(&status).into_owned();
            if status[0] != b'/' {
                zlog!(
                    Error @ fpm_conf_process_all_pools:967,
                    "[pool {name}] the status path '{shown}' must start with a '/'"
                );
                return false;
            }
            if c.pm_status_listen.is_none() && !is_shared && status.len() < 2 {
                zlog!(
                    Error @ fpm_conf_process_all_pools:972,
                    "[pool {name}] the status path '{shown}' is not long enough"
                );
                return false;
            }
            if !path_chars(&status) {
                zlog!(
                    Error @ fpm_conf_process_all_pools:978,
                    "[pool {name}] the status path '{shown}' must contain only the following characters '[alphanum]/_-.~'"
                );
                return false;
            }
        }
        if let Some(ping) = c.ping_path.clone().filter(|p| !p.is_empty()) {
            let shown = String::from_utf8_lossy(&ping).into_owned();
            if ping[0] != b'/' {
                zlog!(
                    Error @ fpm_conf_process_all_pools:990,
                    "[pool {name}] the ping path '{shown}' must start with a '/'"
                );
                return false;
            }
            if ping.len() < 2 {
                zlog!(
                    Error @ fpm_conf_process_all_pools:995,
                    "[pool {name}] the ping path '{shown}' is not long enough"
                );
                return false;
            }
            if !path_chars(&ping) {
                zlog!(
                    Error @ fpm_conf_process_all_pools:1001,
                    "[pool {name}] the ping path '{shown}' must contain only the following characters '[alphanum]/_-.~'"
                );
                return false;
            }
            match &c.ping_response {
                None => c.ping_response = Some(b"pong".to_vec()),
                Some(response) if response.is_empty() => {
                    zlog!(
                        Error @ fpm_conf_process_all_pools:1010,
                        "[pool {name}] the ping response page '{}' is not long enough",
                        String::from_utf8_lossy(response)
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
                    "[pool {name}] 'slowlog' must be specified for use with 'request_slowlog_timeout'"
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
                        "Unable to create or open slowlog({})",
                        String::from_utf8_lossy(&slowlog)
                    );
                    return false;
                }
            }
            if terminate != 0 && slowlog_timeout > terminate {
                zlog!(
                    Error @ fpm_conf_process_all_pools:1071,
                    "[pool {name}] 'request_slowlog_timeout' ({slowlog_timeout}) can't be greater than 'request_terminate_timeout' ({terminate})"
                );
                return false;
            }
        }
        let c = &mut fpm.pools[i].config;
        if c.request_slowlog_trace_depth != 0 {
            if !has_slowlog {
                zlog!(
                    Error @ fpm_conf_process_all_pools:1080,
                    "[pool {name}] 'slowlog' must be specified for use with 'request_slowlog_trace_depth'"
                );
                return false;
            }
            if c.request_slowlog_trace_depth <= 0 {
                zlog!(
                    Error @ fpm_conf_process_all_pools:1093,
                    "[pool {name}] 'request_slowlog_trace_depth' ({}) must be a positive value",
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
            let shown = String::from_utf8_lossy(&chroot).into_owned();
            if chroot[0] != b'/' {
                zlog!(
                    Error @ fpm_conf_process_all_pools:1106,
                    "[pool {name}] the chroot path '{shown}' must start with a '/'"
                );
                return false;
            }
            if !is_dir(&chroot) {
                zlog!(
                    Error @ fpm_conf_process_all_pools:1111,
                    "[pool {name}] the chroot path '{shown}' does not exist or is not a directory"
                );
                return false;
            }
        }
        if let Some(chdir) = c.chdir.clone().filter(|d| !d.is_empty()) {
            let chdir = full(&chdir, false).unwrap_or(chdir);
            c.chdir = Some(chdir.clone());
            let shown = String::from_utf8_lossy(&chdir).into_owned();
            if chdir[0] != b'/' {
                zlog!(
                    Error @ fpm_conf_process_all_pools:1122,
                    "[pool {name}] the chdir path '{shown}' must start with a '/'"
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
                        "[pool {name}] the chdir path '{shown}' within the chroot path '{}' ('{}') does not exist or is not a directory",
                        String::from_utf8_lossy(chroot),
                        String::from_utf8_lossy(&within)
                    );
                    return false;
                }
            } else if !is_dir(&chdir) {
                zlog!(
                    Error @ fpm_conf_process_all_pools:1140,
                    "[pool {name}] the chdir path '{shown}' does not exist or is not a directory"
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
                    "[pool {}] unable to set listen address as it's already used in another pool '{}'",
                    fpm.pools[b].name(),
                    fpm.pools[a].name()
                );
                return false;
            }
        }
    }
    true
}
