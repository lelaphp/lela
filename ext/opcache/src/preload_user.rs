//! `opcache.preload_user` under uid 0, `accel_finish_startup_preload_subprocess`
//! (`Z/ext/opcache/ZendAccelerator.c:4979-5045`): the user's account as
//! `getpwnam` answers it, resolved once as the engine finishes starting
//! up, and the thread the preload is served on taking its ids before it
//! serves any PHP, where php forks a child that takes them (#1258's
//! ruling, 2026-09-30). Everything the preload does, the files it opens
//! and the processes it starts among it, it does with the user's
//! rights; the process's other threads keep the process's ids.

use std::ffi::CString;

/// The user `getpwnam` names: its name, as `initgroups` takes it, and
/// its user and group ids.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Account {
    name: CString,
    pub(crate) uid: u32,
    gid: u32,
}

impl Account {
    /// `getpwnam(user)`, or `None` where no account has that name, the
    /// name holds a NUL or is not UTF-8 (no account's is), or the user
    /// database could not be read.
    pub(crate) fn named(user: &[u8]) -> Option<Account> {
        let name = std::str::from_utf8(user).ok()?;
        let found = nix::unistd::User::from_name(name).ok()??;
        Some(Account {
            name: CString::new(found.name).ok()?,
            uid: found.uid.as_raw(),
            gid: found.gid.as_raw(),
        })
    }

    /// The child's three calls, made on the calling thread alone
    /// ([`crate::ffi::take`]): whether they all took, and php's warning
    /// for the one that did not, which the child writes before it
    /// `exit(1)`s.
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    pub(crate) fn take(&self, req: &mut php_ext::request::Request) -> bool {
        let Err(refused) = crate::ffi::take(&self.name, self.uid, self.gid) else {
            return true;
        };
        // php prints each id with `%d`, so an id past `INT_MAX`, macOS's
        // `nobody` among them, is printed negative.
        let (uid, gid) = (signed(self.uid), signed(self.gid));
        let warning = match refused {
            crate::ffi::Refused::Setgid => format!("Preloading failed to setgid({gid})"),
            // php names the user id here, not the group's.
            crate::ffi::Refused::Initgroups => format!(
                "Preloading failed to initgroups(\"{}\", {uid})",
                self.name.to_string_lossy()
            ),
            #[cfg(target_os = "linux")]
            crate::ffi::Refused::Setuid => format!("Preloading failed to setuid({uid})"),
        };
        crate::ini::accel_log(req, crate::ini::Log::Warning, &warning);
        false
    }
}

/// An id as `printf`'s `%d` prints it.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn signed(id: u32) -> i32 {
    i32::from_ne_bytes(id.to_ne_bytes())
}

#[cfg(all(test, any(target_os = "linux", target_os = "macos")))]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use php_ext::process::{egid, euid, gid, uid};

    use super::Account;
    use crate::ffi::{Refused, take};

    /// `getpwnam`: `nobody` is an account on every platform the engine
    /// builds for, and a name no account has is none.
    #[test]
    fn an_account_is_named_by_the_user_database() {
        let nobody = Account::named(b"nobody");
        assert!(
            nobody
                .as_ref()
                .is_some_and(|nobody| nobody.uid != 0 && nobody.gid != 0)
        );
        assert_eq!(Account::named(b"no-such-user-of-1258"), None);
        assert_eq!(Account::named(b"\xff"), None);
    }

    /// The ids are the calling thread's alone. Under root the thread
    /// takes `nobody`'s user, group and groups: its ids are nobody's, a
    /// file only root may read refuses it, and a process it starts runs
    /// as nobody, while this thread keeps root's. Under any other user
    /// the first call is refused, as php's child's `setgid` is, and no
    /// thread's ids move.
    #[test]
    fn the_ids_are_taken_by_the_calling_thread_alone() {
        let nobody = Account::named(b"nobody").expect("nobody is an account of the user database");
        let files = php_ext::test::Files::new("opcache-preload-user", &[("root-only", "root")]);
        let secret = files.path().join("root-only");
        std::fs::set_permissions(&secret, std::fs::Permissions::from_mode(0o600))
            .expect("the file is its owner's alone");
        let before = (uid(), euid(), gid(), egid());
        let (name, user, group) = (nobody.name.clone(), nobody.uid, nobody.gid);
        #[allow(clippy::disallowed_methods)] // a test's own thread, which no fork waits on
        let taken = std::thread::spawn(move || {
            let took = take(&name, user, group);
            let ids = (uid(), euid(), gid(), egid());
            let opened = std::fs::File::open(&secret).is_ok();
            let child = std::process::Command::new("id")
                .arg("-u")
                .output()
                .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned())
                .unwrap_or_default();
            (took, ids, opened, child)
        })
        .join();
        let after = (uid(), euid(), gid(), egid());
        let (took, ids, opened, child) = taken.expect("the thread runs to its end");
        assert_eq!(after, before, "this thread keeps its ids");
        if before.1 == 0 {
            assert_eq!(took, Ok(()));
            assert_eq!(ids, (user, user, group, group));
            assert!(!opened, "a file only root may read refuses nobody");
            assert_eq!(
                child,
                user.to_string(),
                "a process the thread starts is nobody's"
            );
        } else {
            assert_eq!(took, Err(Refused::Setgid));
            assert_eq!(ids, before);
            assert!(opened);
            assert_eq!(child, before.0.to_string());
        }
    }
}
