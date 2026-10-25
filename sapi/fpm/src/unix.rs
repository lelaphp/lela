//! `fpm_unix.c`: the users and groups a pool names, read from the
//! system's databases; a UNIX socket's owner and mode; the limits and
//! the priority; the daemonizing fork and the session; and a child's
//! chroot, working directory and credentials.

use std::ffi::CString;

use nix::unistd::{Gid, Group, Uid, User};

use crate::conf::{PoolConfig, path};
use crate::sockets::{Domain, Permissions};
use crate::{zlog, zlog_sys};

/// `fpm_unix_is_id`: digits alone.
fn is_id(name: &[u8]) -> bool {
    name.iter().all(u8::is_ascii_digit)
}

/// Whose ID [`id`] reads, a user's or a group's: `fpm_unix_parse_uid`'s
/// or `_gid`'s.
#[derive(Clone, Copy)]
enum Whose {
    User,
    Group,
}

/// `fpm_unix_parse_uid` and `fpm_unix_parse_gid`, php 8.5.12's
/// (gh19320): the digits as `strtoumax` reads them, refused where they
/// run past `uintmax_t` or past a `uid_t`'s or `gid_t`'s range, the
/// `(uid_t) -1` that means none among them, with the log line naming the
/// pool and the ID as written.
fn id(pool: &[u8], name: &[u8], kind: Whose) -> Option<u32> {
    let value = std::str::from_utf8(name)
        .ok()
        .and_then(|n| n.parse::<u64>().ok());
    if let Some(value) = value.and_then(|v| u32::try_from(v).ok())
        && value != u32::MAX
    {
        return Some(value);
    }
    let (what, function, line) = match kind {
        Whose::User => ("user", "fpm_unix_parse_uid", 69),
        Whose::Group => ("group", "fpm_unix_parse_gid", 87),
    };
    let text = format!(
        "[pool {}] {what} ID '{}' is out of range",
        String::from_utf8_lossy(pool),
        String::from_utf8_lossy(name)
    );
    crate::zlog::log(
        crate::zlog::Level::Error,
        crate::zlog::Site::at(function, line),
        text.as_bytes(),
        None,
    );
    None
}

/// `fpm_unix_get_passwd`: the user, or the log line where there is none.
fn passwd(pool: &[u8], name: &[u8], level: crate::zlog::Level, sys: bool) -> Option<User> {
    let found = std::str::from_utf8(name)
        .ok()
        .and_then(|n| User::from_name(n).ok().flatten());
    if found.is_none() {
        let line = format!(
            "[pool {}] cannot get uid for user '{}'",
            String::from_utf8_lossy(pool),
            String::from_utf8_lossy(name)
        );
        crate::zlog::log(
            level,
            crate::zlog::Site::at("fpm_unix_get_passwd", 60),
            line.as_bytes(),
            sys.then_some(nix::errno::Errno::last()),
        );
    }
    found
}

/// `fpm_unix_get_group`.
fn group(pool: &[u8], name: &[u8], level: crate::zlog::Level, sys: bool) -> Option<Group> {
    let found = std::str::from_utf8(name)
        .ok()
        .and_then(|n| Group::from_name(n).ok().flatten());
    if found.is_none() {
        let line = format!(
            "[pool {}] cannot get gid for group '{}'",
            String::from_utf8_lossy(pool),
            String::from_utf8_lossy(name)
        );
        crate::zlog::log(
            level,
            crate::zlog::Site::at("fpm_unix_get_group", 103),
            line.as_bytes(),
            sys.then_some(nix::errno::Errno::last()),
        );
    }
    found
}

/// `fpm_unix_test_config`: `-t`'s check of the users, groups and the
/// UNIX socket's path a pool names.
pub(crate) fn test_config(config: &PoolConfig, domain: Domain) -> bool {
    use crate::zlog::Level;
    let name = &config.name;
    // php 8.5.12: an empty name passes, and an ID is read in range.
    let check_passwd = |user: &Option<Vec<u8>>, sys: bool| match user.as_deref() {
        None | Some([]) => true,
        Some(u) if is_id(u) => id(name, u, Whose::User).is_some(),
        Some(u) => passwd(name, u, Level::Error, sys).is_some(),
    };
    let check_group = |g: &Option<Vec<u8>>, sys: bool| match g.as_deref() {
        None | Some([]) => true,
        Some(g) if is_id(g) => id(name, g, Whose::Group).is_some(),
        Some(g) => group(name, g, Level::Error, sys).is_some(),
    };
    check_passwd(&config.user, false)
        && check_group(&config.group, false)
        && check_listen_address(config, domain)
        && check_passwd(&config.listen_owner, true)
        && check_group(&config.listen_group, true)
}

/// `fpm_unix_check_listen_address`.
fn check_listen_address(config: &PoolConfig, domain: Domain) -> bool {
    const SUN_PATH: usize = if cfg!(target_os = "linux") { 108 } else { 104 };
    let address = config.listen_address.as_deref().unwrap_or_default();
    if domain != Domain::Unix || address.len() < SUN_PATH {
        return true;
    }
    zlog_sys!(
        Error @ fpm_unix_check_listen_address:89,
        nix::errno::Errno::last(),
        "[pool {}] cannot bind to UNIX socket '{}' as path is too long (found length: {}, maximal length: {SUN_PATH})",
        String::from_utf8_lossy(&config.name),
        String::from_utf8_lossy(address),
        address.len()
    );
    false
}

/// `fpm_unix_resolve_socket_permissions`: the mode, 0660 unless
/// `listen.mode` names one, and the owner and group `listen.owner` and
/// `listen.group` name.
pub(crate) fn socket_permissions(config: &PoolConfig) -> Option<Permissions> {
    use crate::zlog::Level;
    let mut permissions = Permissions {
        uid: None,
        gid: None,
        mode: 0o660,
    };
    if let Some(mode) = config.listen_mode.as_deref().filter(|m| !m.is_empty()) {
        let digits: Vec<u8> = mode
            .iter()
            .copied()
            .take_while(|b| (b'0'..=b'7').contains(b))
            .collect();
        permissions.mode =
            u32::from_str_radix(std::str::from_utf8(&digits).unwrap_or("0"), 8).unwrap_or(0);
    }
    if let Some(owner) = config.listen_owner.as_deref().filter(|o| !o.is_empty()) {
        if is_id(owner) {
            permissions.uid = Some(id(&config.name, owner, Whose::User)?);
        } else {
            let user = passwd(&config.name, owner, Level::Error, true)?;
            permissions.uid = Some(user.uid.as_raw());
            permissions.gid = Some(user.gid.as_raw());
        }
    }
    if let Some(g) = config.listen_group.as_deref().filter(|g| !g.is_empty()) {
        if is_id(g) {
            permissions.gid = Some(id(&config.name, g, Whose::Group)?);
        } else {
            permissions.gid = Some(group(&config.name, g, Level::Error, true)?.gid.as_raw());
        }
    }
    Some(permissions)
}

/// `fpm_unix_set_socket_permissions`: the socket's owner and group, where
/// either is named.
pub(crate) fn set_socket_permissions(
    pool: &[u8],
    socket: &[u8],
    address: &[u8],
    permissions: Permissions,
) -> bool {
    if permissions.uid.is_none() && permissions.gid.is_none() {
        return true;
    }
    let owner = permissions.uid.map(Uid::from_raw);
    let group = permissions.gid.map(Gid::from_raw);
    if let Err(e) = nix::unistd::chown(path(socket), owner, group) {
        zlog_sys!(
            Error @ fpm_unix_set_socket_permissions:330,
            e,
            "[pool {}] failed to chown() the socket '{}'",
            String::from_utf8_lossy(pool),
            String::from_utf8_lossy(address)
        );
        return false;
    }
    true
}

/// A pool's credentials, `wp->set_uid`, `set_gid`, `set_user`, and the
/// `USER` and `HOME` its children set.
#[derive(Debug, Clone, Default)]
pub(crate) struct Credentials {
    pub(crate) set_uid: u32,
    pub(crate) set_gid: u32,
    pub(crate) set_user: Option<Vec<u8>>,
    pub(crate) user: Option<Vec<u8>>,
    pub(crate) home: Option<Vec<u8>>,
}

/// `fpm_unix_conf_wp`: as root, the user and group a pool runs as;
/// otherwise a notice for each directive only root reads, and the
/// process's own user for `USER` and `HOME`.
pub(crate) fn conf_pool(config: &PoolConfig, run_as_root: bool) -> Option<Credentials> {
    use crate::zlog::Level;
    let name = String::from_utf8_lossy(&config.name).into_owned();
    let mut credentials = Credentials::default();
    if Uid::effective().is_root() {
        if let Some(user) = config.user.as_deref().filter(|u| !u.is_empty()) {
            if is_id(user) {
                credentials.set_uid = id(&config.name, user, Whose::User)?;
                if let Ok(Some(pwd)) = User::from_uid(Uid::from_raw(credentials.set_uid)) {
                    credentials.set_gid = pwd.gid.as_raw();
                    credentials.set_user = Some(pwd.name.into_bytes());
                }
            } else {
                let pwd = passwd(&config.name, user, Level::Error, false)?;
                credentials.set_uid = pwd.uid.as_raw();
                credentials.set_gid = pwd.gid.as_raw();
                credentials.user = Some(pwd.name.into_bytes());
                credentials.home = Some(pwd.dir.into_os_string().into_encoded_bytes());
            }
        }
        if let Some(g) = config.group.as_deref().filter(|g| !g.is_empty()) {
            credentials.set_gid = if is_id(g) {
                id(&config.name, g, Whose::Group)?
            } else {
                group(&config.name, g, Level::Error, false)?.gid.as_raw()
            };
        }
        if !run_as_root && (credentials.set_uid == 0 || credentials.set_gid == 0) {
            zlog!(
                Error @ fpm_unix_conf_wp:395,
                "[pool {name}] please specify user and group other than root"
            );
            return None;
        }
    } else {
        if config.user.as_deref().is_some_and(|u| !u.is_empty()) {
            zlog!(
                Notice @ fpm_unix_conf_wp:401,
                "[pool {name}] 'user' directive is ignored when FPM is not running as root"
            );
        }
        if config.group.as_deref().is_some_and(|g| !g.is_empty()) {
            zlog!(
                Notice @ fpm_unix_conf_wp:404,
                "[pool {name}] 'group' directive is ignored when FPM is not running as root"
            );
        }
        if config.chroot.as_deref().is_some_and(|c| !c.is_empty()) {
            zlog!(
                Notice @ fpm_unix_conf_wp:407,
                "[pool {name}] 'chroot' directive is ignored when FPM is not running as root"
            );
        }
        if config.process_priority != 64 {
            zlog!(
                Notice @ fpm_unix_conf_wp:410,
                "[pool {name}] 'process.priority' directive is ignored when FPM is not running as root"
            );
        }
        if let Ok(Some(pwd)) = User::from_uid(Uid::current()) {
            credentials.user = Some(pwd.name.into_bytes());
            credentials.home = Some(pwd.dir.into_os_string().into_encoded_bytes());
        }
    }
    Some(credentials)
}

/// `setrlimit` of one resource to `value` for both limits, `-1` the
/// infinity `rlimit_core = unlimited` asks for.
pub(crate) fn set_rlimit(resource: nix::sys::resource::Resource, value: i32) -> nix::Result<()> {
    let limit = if value == -1 {
        nix::sys::resource::RLIM_INFINITY
    } else {
        nix::sys::resource::rlim_t::try_from(value).unwrap_or(0)
    };
    nix::sys::resource::setrlimit(resource, limit, limit)
}

/// `fpm_unix_init_child`: the pool's limits, chroot, working directory,
/// priority and credentials in a new child; `false` where one failed as
/// php fails the child.
pub(crate) fn init_child(config: &PoolConfig, credentials: &Credentials) -> bool {
    use nix::sys::resource::Resource;
    let name = String::from_utf8_lossy(&config.name).into_owned();
    let is_root = Uid::effective().is_root();
    if config.rlimit_files != 0
        && let Err(e) = set_rlimit(Resource::RLIMIT_NOFILE, config.rlimit_files)
    {
        zlog_sys!(
            Error @ fpm_unix_init_child:435,
            e,
            "[pool {name}] failed to set rlimit_files for this pool. Please check your system limits or decrease rlimit_files. setrlimit(RLIMIT_NOFILE, {})",
            config.rlimit_files
        );
    }
    if config.rlimit_core != 0
        && let Err(e) = set_rlimit(Resource::RLIMIT_CORE, config.rlimit_core)
    {
        zlog_sys!(
            Error @ fpm_unix_init_child:445,
            e,
            "[pool {name}] failed to set rlimit_core for this pool. Please check your system limits or decrease rlimit_core. setrlimit(RLIMIT_CORE, {})",
            config.rlimit_core
        );
    }
    let mut made_chroot = false;
    if is_root && let Some(root) = config.chroot.as_deref().filter(|c| !c.is_empty()) {
        if let Err(e) = nix::unistd::chroot(path(root)) {
            zlog_sys!(
                Error @ fpm_unix_init_child:451,
                e,
                "[pool {name}] failed to chroot({})",
                String::from_utf8_lossy(root)
            );
            return false;
        }
        made_chroot = true;
    }
    if let Some(dir) = config.chdir.as_deref().filter(|c| !c.is_empty()) {
        if let Err(e) = nix::unistd::chdir(path(dir)) {
            zlog_sys!(
                Error @ fpm_unix_init_child:459,
                e,
                "[pool {name}] failed to chdir({})",
                String::from_utf8_lossy(dir)
            );
            return false;
        }
    } else if made_chroot && nix::unistd::chdir("/").is_err() {
        zlog!(Warning @ fpm_unix_init_child:464, "[pool {name}] failed to chdir(/)");
    }
    if is_root {
        if config.process_priority != 64 && !crate::ffi::set_priority(config.process_priority) {
            zlog_sys!(
                Error @ fpm_unix_init_child:472,
                nix::errno::Errno::last(),
                "[pool {name}] Unable to set priority for this new process"
            );
            return false;
        }
        if credentials.set_gid != 0
            && let Err(e) = nix::unistd::setgid(Gid::from_raw(credentials.set_gid))
        {
            zlog_sys!(
                Error @ fpm_unix_init_child:479,
                e,
                "[pool {name}] failed to setgid({})",
                credentials.set_gid
            );
            return false;
        }
        if credentials.set_uid != 0 {
            let user = credentials
                .set_user
                .clone()
                .or_else(|| config.user.clone())
                .unwrap_or_default();
            let user = CString::new(user).unwrap_or_default();
            if let Err(e) = crate::ffi::initgroups(&user, credentials.set_gid) {
                zlog_sys!(
                    Error @ fpm_unix_init_child:485,
                    e,
                    "[pool {name}] failed to initgroups({}, {})",
                    String::from_utf8_lossy(config.user.as_deref().unwrap_or_default()),
                    credentials.set_gid
                );
                return false;
            }
            if let Err(e) = nix::unistd::setuid(Uid::from_raw(credentials.set_uid)) {
                zlog_sys!(
                    Error @ fpm_unix_init_child:489,
                    e,
                    "[pool {name}] failed to setuid({})",
                    credentials.set_uid
                );
                return false;
            }
        }
    }
    true
}
