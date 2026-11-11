//! `fpm_env.c`: a pool's `env[…]` values read from the master's
//! environment where they name a variable (`$NAME`), the master's title
//! `php-fpm: master process (<config>)`, and a child's environment made
//! from its pool's: cleared under `clear_env`, each `env[…]` set, and
//! `USER` and `HOME` for the user it runs as; its title `pool <name>`.

use crate::master::{Fpm, Pool};

/// `fpm_env_init_main`: a value naming a variable (`$NAME`) is looked up
/// by the name's bytes, as php's `getenv` is.
pub(crate) fn init_main(fpm: &mut Fpm) -> bool {
    use std::os::unix::ffi::OsStrExt;
    for pool in &mut fpm.pools {
        for (key, value) in &mut pool.config.env {
            if value.first() == Some(&b'$') {
                let name = std::ffi::OsStr::from_bytes(&value[1..]);
                *value = std::env::var_os(name)
                    .map(std::ffi::OsString::into_encoded_bytes)
                    .unwrap_or_default();
            }
            if key.as_slice() == b"USER" {
                pool.credentials.user = None;
            }
            if key.as_slice() == b"HOME" {
                pool.credentials.home = None;
            }
        }
    }
    crate::ffi::save_title_area();
    let mut title = b"master process (".to_vec();
    title.extend_from_slice(&fpm.config_path);
    title.push(b')');
    crate::ffi::set_title(&title);
    true
}

/// `fpm_env_init_child`.
pub(crate) fn init_child(pool: &Pool) {
    let mut title = b"pool ".to_vec();
    title.extend_from_slice(&pool.config.name);
    crate::ffi::set_title(&title);
    if pool.config.clear_env {
        crate::ffi::clear_env();
    }
    for (key, value) in &pool.config.env {
        crate::ffi::set_env(key, value);
    }
    if let Some(user) = &pool.credentials.user {
        crate::ffi::set_env(b"USER", user);
    }
    if let Some(home) = &pool.credentials.home {
        crate::ffi::set_env(b"HOME", home);
    }
}
