//! The environment area of a request: `getenv()` and `putenv()` over
//! the host's environment, the locale `setlocale()` answers, the
//! build's definitions, the host's name, `argv`, the primary file's
//! name and the process title, the working directory, the temporary
//! directory, the umask, `strerror`, and when the request started. The
//! umask, the locale names and the start time are [`Environment`]'s,
//! whose fields no file but this one reads.

use std::os::unix::ffi::OsStrExt;
use std::path::PathBuf;

use crate::api::raising::Thrown;

use super::{BuildDefs, HostPath, Request};

/// The C library's `P_tmpdir` (`<stdio.h>`), the directory
/// `php_get_temporary_directory` falls back to where neither
/// `sys_temp_dir` nor `TMPDIR` names one, as the header spells it:
/// `/var/tmp/` on macOS, `/tmp` under glibc and musl.
const P_TMPDIR: &[u8] = if cfg!(target_vendor = "apple") {
    b"/var/tmp/"
} else {
    b"/tmp"
};

/// The umask, the locale names and the start time, whose fields no
/// file but this one reads.
pub(super) struct Environment {
    umask: u32,
    /// The name `setlocale()` last answered for each category it moved,
    /// by `<locale.h>` value, and php's `BG(ctype_string)`: the
    /// `LC_CTYPE` name while it is not `C` (revision 56).
    locale_names: Vec<(usize, Vec<u8>)>,
    ctype_locale: Option<Vec<u8>>,
    /// `sapi_get_request_time`: when the request started, since the
    /// epoch. The host keeps no time of its own, so it is the moment the
    /// request is made, as `php_time()` makes it.
    started: std::time::Duration,
}

impl Environment {
    /// The environment as the request is made: php's default umask, no
    /// locale moved, and the start time read now.
    pub(super) fn new() -> Environment {
        Environment {
            umask: 0o022,
            locale_names: Vec::new(),
            ctype_locale: None,
            started: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default(),
        }
    }
}

impl Request {
    /// `getenv(name)`: `sapi_getenv`, the request's own variable, which
    /// no name `HTTP_PROXY` begins with answers (bug #72573), then the
    /// bytes the host's environment holds. The CLI's is the process's,
    /// which the witness recipe clears and rebuilds (#1077):
    ///
    /// ```php
    /// var_dump(getenv('TZ'), $_ENV['LANG'] ?? null);
    /// ```
    /// ```expect
    /// string(3) "UTC"
    /// string(1) "C"
    /// ```
    #[must_use]
    pub fn env(&self, name: &[u8]) -> Option<&[u8]> {
        let proxy = b"HTTP_PROXY";
        let refused = name.len() <= proxy.len() && proxy[..name.len()].eq_ignore_ascii_case(name);
        let param = if refused {
            None
        } else {
            self.description
                .params
                .iter()
                .find(|(n, _)| n.as_slice() == name)
        };
        param
            .or_else(|| {
                self.description
                    .env
                    .iter()
                    .find(|(n, _)| n.as_slice() == name)
            })
            .map(|(_, v)| v.as_slice())
    }

    /// The environment's every pair, `environ`, what a spawned program
    /// inherits.
    #[must_use]
    pub fn env_pairs(&self) -> &[(Vec<u8>, Vec<u8>)] {
        &self.description.env
    }

    /// `php_load_environment_variables`, what `getenv()` with no name
    /// reads: the environment's pairs, then the request's own variables.
    #[must_use]
    pub fn loaded_env(&self) -> Vec<(Vec<u8>, Vec<u8>)> {
        let description = &self.description;
        description
            .env
            .iter()
            .chain(&description.params)
            .cloned()
            .collect()
    }

    /// `putenv()`: set, or unset with `None`.
    pub fn set_env(&mut self, name: &[u8], value: Option<&[u8]>) -> bool {
        self.description.env.retain(|(n, _)| n.as_slice() != name);
        if let Some(value) = value {
            self.description.env.push((name.to_vec(), value.to_vec()));
        }
        true
    }

    /// The name `setlocale()` last answered for `category`, by the
    /// pinned build's `<locale.h>` value, or `None` while the request
    /// has not moved it (design §4.4, revision 56). The names go with the
    /// request, as `basic_functions`'s `RSHUTDOWN` puts the locale back.
    #[must_use]
    pub fn locale_name(&self, category: usize) -> Option<&[u8]> {
        self.environment
            .locale_names
            .iter()
            .find(|(c, _)| *c == category)
            .map(|(_, name)| name.as_slice())
    }

    /// What `setlocale()` writes once a category has moved to `name`.
    pub fn set_locale_name(&mut self, category: usize, name: Vec<u8>) {
        self.environment
            .locale_names
            .retain(|(c, _)| *c != category);
        self.environment.locale_names.push((category, name));
    }

    /// php's `BG(ctype_string)`: the name `setlocale()` answered when it
    /// last moved `LC_CTYPE`, `None` where that was `C` or nothing moved
    /// it, which ext/pcre keys its compiled patterns by (revision 56).
    #[must_use]
    pub fn ctype_locale(&self) -> Option<&[u8]> {
        self.environment.ctype_locale.as_deref()
    }

    /// What `setlocale()` writes to [`Request::ctype_locale`].
    pub fn set_ctype_locale(&mut self, name: Option<Vec<u8>>) {
        self.environment.ctype_locale = name;
    }

    /// The facts of the build serving the request, php's
    /// `main/build-defs.h`: its date, system, provider and `configure`
    /// line, which phpinfo's general table prints.
    #[must_use]
    pub fn build_defs(&self) -> &BuildDefs {
        &self.shared.build_defs
    }

    /// `php_sapi_name()`: `sapi_module.name`.
    #[must_use]
    pub fn sapi_name(&self) -> &str {
        &self.description.sapi_name
    }

    /// The host's `argv`, `php_self` first: what `php_build_argv` puts in
    /// `$_SERVER['argv']` (`Z/main/php_variables.c:657-713`), which a
    /// write to `$_SERVER` from PHP code leaves as it was, and which
    /// `getopt` reads; empty for a host that described none, php's web
    /// hosts.
    #[must_use]
    pub fn argv(&self) -> &[Vec<u8>] {
        &self.description.argv
    }

    /// The file the request serves, `$_SERVER['SCRIPT_NAME']`.
    #[must_use]
    pub fn script_name(&self) -> Option<&[u8]> {
        self.description.script_name.as_deref()
    }

    /// `getcwd()`.
    #[must_use]
    pub fn cwd(&self) -> PathBuf {
        self.cwd.clone()
    }

    /// `chdir()`'s `VCWD_CHDIR` past the checks the native makes: the
    /// working directory moved to what the guarded path resolves to,
    /// every link on the way replaced, as `getcwd(3)` answers after
    /// `chdir(2)`. The stat cache's relative entries are the native's to
    /// drop, as `PHP_FUNCTION(chdir)` drops them.
    pub fn set_cwd(&mut self, dir: &HostPath) -> Result<(), Thrown> {
        let real = self.realpath(dir.as_os_str().as_bytes());
        self.cwd = real.unwrap_or_else(|| dir.to_path_buf());
        Ok(())
    }

    /// `VCWD_CHDIR_FILE` over the program's name where the host serves
    /// it in its own directory ([`Chdir::IntoProgramDir`]): the working
    /// directory moved to the directory the name names, cut at its last
    /// `/` as `virtual_chdir_file` cuts it, where that resolves to a
    /// directory, as `virtual_chdir` moves only there; and the directory
    /// it stood in handed back for [`Request::leave_program_dir`]. `None`
    /// where the host stays or names no program, as
    /// `php_execute_script_ex` keeps no directory to go back to then.
    ///
    /// [`Chdir::IntoProgramDir`]: crate::engine::Chdir::IntoProgramDir
    pub(crate) fn enter_program_dir(&mut self) -> Option<PathBuf> {
        if self.description.chdir != crate::engine::Chdir::IntoProgramDir {
            return None;
        }
        let name = self.description.script_name.clone()?;
        let left = self.cwd.clone();
        // A name with no `/` is `ENOENT` to `virtual_chdir_file`, and one
        // whose only `/` leads keeps it, the root.
        if let Some(cut) = name.iter().rposition(|b| *b == b'/') {
            let dir = &name[..cut.max(1)];
            if let Some(real) = self.realpath(dir).filter(|real| real.is_dir()) {
                self.cwd = real;
            }
        }
        Some(left)
    }

    /// The working directory `php_execute_script_ex` restores once the
    /// program has been served, `left`, which
    /// [`Request::enter_program_dir`] handed back, whatever the program
    /// moved it to.
    pub(crate) fn leave_program_dir(&mut self, left: PathBuf) {
        self.cwd = left;
    }

    /// `php_get_temporary_directory`
    /// (`Z/main/php_open_temporary_file.c:236`): what `sys_get_temp_dir()`
    /// answers, and where `tempnam()` falls back to and `tmpfile()` and a
    /// spilling `php://temp` make their files. `sys_temp_dir` where it is
    /// set to more than a lone `/`, else `TMPDIR` from the environment,
    /// one trailing slash taken off either; else the C library's
    /// `P_tmpdir` as it stands ([`P_TMPDIR`]), trailing slash and all.
    ///
    /// The recipe names a `TMPDIR` of the harness's own (#1730); emptied
    /// before the first answer, the directory is the platform's own:
    ///
    /// ```php
    /// putenv('TMPDIR=');
    /// $t = sys_get_temp_dir();
    /// var_dump($t === (PHP_OS_FAMILY === 'Darwin' ? '/var/tmp/' : '/tmp'), getenv('TMPDIR'), is_dir($t));
    /// ```
    /// ```expect
    /// bool(true)
    /// string(0) ""
    /// bool(true)
    /// ```
    #[must_use]
    pub fn temp_dir(&self) -> PathBuf {
        let chosen = self
            .ini_get(b"sys_temp_dir")
            .filter(|dir| !dir.is_empty() && *dir != b"/")
            .or_else(|| self.env(b"TMPDIR").filter(|dir| !dir.is_empty()))
            .map(|dir| dir.strip_suffix(b"/").unwrap_or(dir));
        PathBuf::from(std::ffi::OsStr::from_bytes(chosen.unwrap_or(P_TMPDIR)))
    }

    /// `umask()` as it stands.
    #[must_use]
    pub fn umask(&self) -> u32 {
        self.environment.umask
    }

    /// `umask(int)`: the previous mask.
    pub fn set_umask(&mut self, mask: u32) -> u32 {
        std::mem::replace(&mut self.environment.umask, mask)
    }

    /// `strerror` text for an OS error code, as php's `%s` of it: the
    /// system's sentence alone, without the ` (os error N)` Rust's
    /// rendering of an OS error puts after it.
    #[must_use]
    pub fn os_error_text(&self, code: i32) -> String {
        let text = std::io::Error::from_raw_os_error(code).to_string();
        let suffix = format!(" (os error {code})");
        match text.strip_suffix(&suffix) {
            Some(sentence) => sentence.to_owned(),
            None => text,
        }
    }

    /// `sapi_get_request_time`, when the request started, as `$_SERVER`
    /// and opcache's `ZCG(request_time)` read it.
    ///
    /// ```php
    /// $t = $_SERVER['REQUEST_TIME_FLOAT'];
    /// usleep(10000);
    /// var_dump($t <= microtime(true), (int) $t === $_SERVER['REQUEST_TIME'], $_SERVER['REQUEST_TIME'] <= time());
    /// ```
    /// ```expect
    /// bool(true)
    /// bool(true)
    /// bool(true)
    /// ```
    pub(crate) fn started(&self) -> std::time::Duration {
        self.environment.started
    }

    /// `ZCG(request_time)` and `sapi_get_request_time()` as a `zend_long`:
    /// when the request started, in whole seconds.
    #[must_use]
    pub fn request_time(&self) -> i64 {
        i64::try_from(self.environment.started.as_secs()).unwrap_or(i64::MAX)
    }

    /// `PHP_CLI_PROCESS_TITLE` as the host registers it.
    pub(crate) fn process_title(&self) -> crate::engine::ProcessTitle {
        self.description.process_title
    }
}

#[cfg(test)]
mod tests {
    use super::Request;
    use crate::engine::{Engine, RequestDescription};

    /// `strerror(ENOENT)` is the sentence alone, which php prints inside
    /// `open(%s, O_RDWR) failed: %s (%d)` and every message like it.
    #[test]
    fn os_error_text_is_strerror() {
        let engine = Engine::cli_builder().build();
        let req = Request::new(
            std::sync::Arc::clone(&engine.shared),
            RequestDescription::default(),
        );
        assert_eq!(req.os_error_text(2), "No such file or directory");
    }
}
