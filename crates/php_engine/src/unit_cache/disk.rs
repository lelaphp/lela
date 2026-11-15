//! The unit cache's second level on disk, `opcache.file_cache` (design
//! §6.3): a unit kept as a file under the directory the extension names,
//! at the path php's `zend_file_cache_get_bin_file_path` makes, the
//! directory, `/`, the build's identity and the file's full path with
//! `.bin` after it, so a unit is found by the same full path the first
//! level files it under. The file is this engine's: a header, which
//! build wrote it, the file's `mtime` it was compiled at where timestamps
//! are validated, the source's size and a digest of the body, and then
//! the unit's bytes ([`super::wire`]). A file another build wrote, a
//! short one, one whose digest fails and one whose file changed since is
//! never read, and is removed where the cache may write, as
//! `zend_file_cache_script_load_ex` unlinks what it refuses.

use std::io::Write as _;
use std::path::{Path, PathBuf};

/// What begins every file the cache writes.
const MAGIC: &[u8; 8] = b"DAXUNIT\0";
/// The header: the magic, the build, the stamp, the size and the digest.
const HEADER: usize = MAGIC.len() + BUILD + 8 + 8 + 8;
/// The width of the build's identity, php's `zend_system_id`.
pub(crate) const BUILD: usize = 32;

/// One file cache as a request asks it: the directory, whether it may be
/// written, and the build whose files it reads.
#[derive(Clone, Copy)]
pub(crate) struct Disk<'a> {
    pub(crate) directory: &'a Path,
    pub(crate) read_only: bool,
    pub(crate) build: &'a [u8; BUILD],
}

/// A unit read back: its bytes, and the size of the source it was
/// compiled from.
pub(crate) struct Kept {
    pub(crate) body: Vec<u8>,
    pub(crate) size: u64,
}

/// The build's identity, php's `zend_system_id`: the digest of the
/// sources this engine was built from (`PHP_ENGINE_BUILD`, which
/// `build.rs` makes), beside the name of every function and class the
/// bundle declares, which a unit compiles against, and the execution
/// hooks the tier replaces, which pick its call opcodes, as
/// `zend_finalize_system_id` digests `ZEND_HOOK_EXECUTE_EX` and
/// `ZEND_HOOK_EXECUTE_INTERNAL` (`Z/Zend/zend_system_id.c:58-90`), so a
/// file cache phpdbg wrote is never read by the CLI; and whether units
/// are compiled with extended info, the host's `-e`, which php's
/// identity leaves out and the ruling of 2026-10-04 put in, a bit of the
/// same byte that none of php's hooks takes, so a unit compiled without
/// its `EXT_STMT`s is never read by an engine that serves them; in 32 hex
/// digits.
pub(crate) fn build_identity<'a>(
    hooked: php_compile::Hooked,
    extended_info: bool,
    names: impl Iterator<Item = &'a [u8]>,
) -> [u8; BUILD] {
    let hooks = u8::from(hooked.user) << 2
        | u8::from(hooked.natives) << 3
        | u8::from(extended_info) << EXTENDED_INFO_BIT;
    let mut low = mix(digest(env!("PHP_ENGINE_BUILD").as_bytes()), &[hooks]);
    let mut high = digest(b"php_engine");
    for name in names {
        low = mix(low, name);
        high = mix(high, name).rotate_left(17);
    }
    let hex = format!("{high:016x}{low:016x}");
    let mut build = [b'0'; BUILD];
    build.copy_from_slice(&hex.as_bytes()[..BUILD]);
    build
}

/// The bit of the hooks byte [`build_identity`] digests that says the
/// units are compiled with extended info: past php's five
/// (`ZEND_HOOK_AST_PROCESS` to `ZEND_HOOK_INTERRUPT_FUNCTION`).
const EXTENDED_INFO_BIT: u8 = 5;

/// FNV-1a over `bytes` from `hash` on, with a separator after them.
fn mix(hash: u64, bytes: &[u8]) -> u64 {
    bytes.iter().chain([&0u8]).fold(hash, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

/// FNV-1a over `bytes`, the body's digest.
fn digest(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

impl Disk<'_> {
    /// `zend_file_cache_get_bin_file_path`: where the unit of the file
    /// at the full path `full` is kept.
    pub(crate) fn path(&self, full: &[u8]) -> PathBuf {
        let mut path = self.directory.as_os_str().as_encoded_bytes().to_vec();
        path.push(b'/');
        path.extend_from_slice(self.build);
        path.extend_from_slice(full);
        path.extend_from_slice(b".bin");
        PathBuf::from(String::from_utf8_lossy(&path).into_owned())
    }

    /// `zend_file_cache_script_load_ex`: the unit kept for `full`, where
    /// this build wrote it and, where `stamp` names the file's `mtime` in
    /// whole seconds, it was compiled at that one; a file refused is
    /// removed where the cache may write.
    pub(crate) fn load(&self, full: &[u8], stamp: Option<i64>) -> Option<Kept> {
        let path = self.path(full);
        let bytes = std::fs::read(&path).ok()?;
        let kept = self.read(&bytes, stamp);
        if kept.is_none() {
            self.remove(&path);
        }
        kept
    }

    /// Whether a unit is kept for `full` that [`Disk::load`] would read,
    /// the file itself left as it is: `validate_only`.
    pub(crate) fn holds(&self, full: &[u8], stamp: Option<i64>) -> bool {
        std::fs::read(self.path(full)).is_ok_and(|bytes| self.read(&bytes, stamp).is_some())
    }

    /// The header of `bytes` held to this build and to `stamp`, and the
    /// body to its digest.
    fn read(&self, bytes: &[u8], stamp: Option<i64>) -> Option<Kept> {
        let (header, body) = bytes.split_at_checked(HEADER)?;
        let (magic, header) = header.split_at(MAGIC.len());
        let (build, header) = header.split_at(BUILD);
        let (kept_stamp, header) = header.split_at(8);
        let (size, sum) = header.split_at(8);
        let number = |bytes: &[u8]| bytes.try_into().ok().map(u64::from_le_bytes);
        let fresh = stamp.is_none_or(|stamp| number(kept_stamp) == Some(stamp.cast_unsigned()));
        (magic == MAGIC && build == self.build && fresh && number(sum)? == digest(body)).then(
            || Kept {
                body: body.to_vec(),
                size: number(size).unwrap_or(0),
            },
        )
    }

    /// `zend_file_cache_script_store`: `body`, a unit of the file at
    /// `full` compiled from `size` bytes at the `mtime` `stamp` (zero
    /// where timestamps are not validated), kept where no unit is kept
    /// for the file yet. It is written whole under another name and
    /// linked into place, so a reader finds a whole file or none, and a
    /// file another request put there first stands.
    pub(crate) fn store(&self, full: &[u8], stamp: i64, size: u64, body: &[u8]) {
        if self.read_only {
            return;
        }
        let path = self.path(full);
        let Some(dir) = path.parent() else {
            return;
        };
        if make_directory(dir).is_err() {
            return;
        }
        let mut bytes = Vec::with_capacity(HEADER + body.len());
        bytes.extend_from_slice(MAGIC);
        bytes.extend_from_slice(self.build);
        bytes.extend_from_slice(&stamp.to_le_bytes());
        bytes.extend_from_slice(&size.to_le_bytes());
        bytes.extend_from_slice(&digest(body).to_le_bytes());
        bytes.extend_from_slice(body);
        let mut partial = path.clone().into_os_string();
        partial.push(format!(
            ".{}.{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let partial = PathBuf::from(partial);
        if write_new(&partial, &bytes).is_ok() {
            std::fs::hard_link(&partial, &path).ok();
        }
        std::fs::remove_file(&partial).ok();
    }

    /// `zend_file_cache_invalidate`: the unit kept for `full` removed.
    pub(crate) fn invalidate(&self, full: &[u8]) {
        self.remove(&self.path(full));
    }

    fn remove(&self, path: &Path) {
        if !self.read_only {
            std::fs::remove_file(path).ok();
        }
    }
}

/// `zend_file_cache_mkdir`: every directory to `dir` made, readable and
/// writable by the owner alone.
fn make_directory(dir: &Path) -> std::io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(dir)
}

/// `bytes` into a file made new at `path`, the owner's alone.
fn write_new(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(path)?.write_all(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_kept_unit_is_read_by_its_build_at_its_stamp_and_refused_otherwise() {
        let files = crate::test::Files::new("disk", &[]);
        let build = [b'a'; BUILD];
        let disk = Disk {
            directory: files.path(),
            read_only: false,
            build: &build,
        };
        let full = b"/srv/app/index.php";
        disk.store(full, 7, 12, b"body");
        assert!(
            disk.path(full)
                .ends_with("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/srv/app/index.php.bin")
        );
        assert!(disk.holds(full, Some(7)));
        let kept = disk.load(full, None).map(|kept| (kept.body, kept.size));
        assert_eq!(kept, Some((b"body".to_vec(), 12)));
        let other = [b'b'; BUILD];
        let later = Disk {
            build: &other,
            ..disk
        };
        assert!(
            later.load(full, None).is_none(),
            "another build keeps its own"
        );
        let mut stale = std::fs::read(disk.path(full)).expect("the kept file is read");
        stale[MAGIC.len()] = b'b';
        std::fs::write(disk.path(full), &stale).expect("the kept file is written over");
        assert!(
            disk.load(full, None).is_none(),
            "a file another build wrote is never read"
        );
        assert!(!disk.path(full).exists(), "and it is removed");
        disk.store(full, 7, 12, b"body");
        let mut corrupt = std::fs::read(disk.path(full)).expect("the kept file is read");
        corrupt.push(b'!');
        std::fs::write(disk.path(full), &corrupt).expect("the kept file is written over");
        assert!(
            !disk.holds(full, None),
            "nor one whose body is not what it wrote"
        );
        std::fs::remove_file(disk.path(full)).expect("the kept file is removed");
        disk.store(full, 7, 12, b"body");
        assert!(
            disk.load(full, Some(8)).is_none(),
            "a changed file is not read"
        );
    }

    /// A tier that replaces `zend_execute_ex`, as phpdbg's does.
    struct Debugging;

    impl crate::tier::Tier for Debugging {
        type State = ();

        fn safepoint(
            _: &mut crate::tier::Request,
            _: crate::tier::FrameRef,
            _: crate::tier::Site,
            _: crate::tier::Held<'_>,
        ) -> crate::tier::Flow {
            crate::tier::Flow::Continue
        }

        const HOOKED: php_compile::Hooked = php_compile::Hooked {
            user: true,
            natives: false,
        };
    }

    /// A host's `-e` is part of the build's identity, so a unit
    /// compiled with extended info and one compiled without it are never
    /// read in each other's place.
    #[test]
    fn extended_info_keeps_its_units_apart() {
        let build = |engine: crate::Engine| engine.shared.units.build;
        let extended = build(crate::Engine::cli_builder().extended_info(true).build());
        assert_ne!(extended, build(crate::Engine::cli_builder().build()));
        assert_eq!(
            extended,
            build(crate::Engine::cli_builder().extended_info(true).build())
        );
    }

    /// The hooks a tier declares are part of the build's
    /// identity, so phpdbg and the CLI never share a file cache's units.
    #[test]
    fn a_tier_that_replaces_a_hook_keeps_its_units_apart() {
        let build = |engine: crate::Engine| engine.shared.units.build;
        let hooked = build(crate::Engine::cli_builder().tier::<Debugging>().build());
        assert_ne!(hooked, build(crate::Engine::cli_builder().build()));
        assert_eq!(
            hooked,
            build(crate::Engine::cli_builder().tier::<Debugging>().build())
        );
    }
}
