//! `[closure]` of the pin (#2654, #2657): the Homebrew kegs the pinned
//! binaries load, each at the version Homebrew names it by (its
//! `pkg_version`, `1.12_1`), held with php's digest as one oracle. The
//! md5 holds php's own bytes and none of what it loads, so a `brew
//! upgrade` that moved pcre2 or libzip under it moved the oracle with no
//! pin changing; this record is what such a move is refused against.
//!
//! **What the binaries load** is read off their Mach-O load commands and
//! the libraries those name, followed to the files they resolve to, as
//! dyld follows them: a file under `<prefix>/Cellar/<keg>/<version>/` is
//! that keg at that version, whatever `opt` link named it. No process is
//! spawned and only each file's load commands are read, so the walk costs
//! a few dozen small reads beside the digest's whole read of php. A system
//! library stands in the dyld shared cache and not on disk, and a
//! non-Mach-O binary (a Linux build, or a test's stand-in) loads nothing;
//! neither is a keg. A keg the record names that nothing loads, the trust
//! store `ca-certificates` among them, is read at `<prefix>/opt/<keg>`,
//! the link Homebrew points at the version it installed.
//!
//! **What a build was made against** is the record's digest, [`Closure::digest`]: a
//! variant's macOS build is made against the Homebrew the record names, so
//! its pin names the digest beside its md5, as a Linux build names its
//! image (#2935).

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::Read as _;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::sha256::Sha256;

/// The keg names and their versions as `oracle.toml` records them.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(transparent)]
pub(crate) struct Closure(BTreeMap<String, String>);

impl Closure {
    /// Each keg and its version, by name.
    pub(crate) fn kegs(&self) -> &BTreeMap<String, String> {
        &self.0
    }

    /// The record's digest: SHA-256 over each keg's `name=version\n`, in
    /// name order, so one closure always gives one digest (#2935).
    pub(crate) fn digest(&self) -> Sha256 {
        let lines = self
            .0
            .iter()
            .map(|(keg, version)| format!("{keg}={version}\n"));
        Sha256::of(lines.collect::<String>().as_bytes())
    }

    /// The closure `binaries` load against the record: a keg loaded at a
    /// version the record does not say, a keg loaded that it does not
    /// name, and a keg it names installed at another version or not at
    /// all each refuse, every one named in the one refusal with both
    /// versions.
    pub(crate) fn held(&self, binaries: &[&Path]) -> Result<()> {
        let walked = Walked::from(binaries)?;
        let mut drift = Vec::new();
        for (keg, version) in &walked.kegs {
            match self.0.get(keg) {
                Some(recorded) if recorded == version => {}
                Some(recorded) => drift.push(format!("{keg} is {version}, recorded {recorded}")),
                None => drift.push(format!("{keg} {version} is loaded and not recorded")),
            }
        }
        if let Some(prefix) = &walked.prefix {
            let unloaded = self.0.iter().filter(|(keg, _)| !walked.names(keg));
            for (keg, recorded) in unloaded {
                match installed(prefix, keg) {
                    Some(version) if version == *recorded => {}
                    Some(version) => drift.push(format!("{keg} is {version}, recorded {recorded}")),
                    None => drift.push(format!("{keg} is not installed, recorded {recorded}")),
                }
            }
        }
        if !drift.is_empty() {
            bail!(
                "the kegs the pinned php loads drifted from the pin's [closure]: {}. Report the drift; the pin moves only by a person's hand",
                drift.join("; ")
            );
        }
        Ok(())
    }
}

/// What a walk from the binaries found: every keg a library of theirs
/// resolved into, at the version it resolved to, and the Homebrew prefix
/// the first keg or binary stands under, none where nothing does.
#[derive(Debug, Default)]
struct Walked {
    kegs: BTreeSet<(String, String)>,
    prefix: Option<PathBuf>,
}

impl Walked {
    fn from(binaries: &[&Path]) -> Result<Walked> {
        let mut walked = Walked::default();
        let mut seen = BTreeSet::new();
        let mut queue = Vec::new();
        for binary in binaries {
            let real = fs::canonicalize(binary)
                .with_context(|| format!("resolving the pinned binary {}", binary.display()))?;
            if let Some((prefix, _)) = in_cellar(&real) {
                walked.prefix.get_or_insert(prefix);
            }
            queue.push(real);
        }
        while let Some(file) = queue.pop() {
            if !seen.insert(file.clone()) {
                continue;
            }
            let loads = Loads::of(&file)?;
            for name in &loads.names {
                let Some(real) = loads.resolved(&file, name)? else {
                    continue;
                };
                if let Some((prefix, keg)) = in_cellar(&real) {
                    walked.prefix.get_or_insert(prefix);
                    walked.kegs.insert(keg);
                }
                queue.push(real);
            }
        }
        Ok(walked)
    }

    fn names(&self, keg: &str) -> bool {
        self.kegs.iter().any(|(name, _)| name == keg)
    }
}

/// The version `<prefix>/opt/<keg>` points at, none where no link is.
fn installed(prefix: &Path, keg: &str) -> Option<String> {
    let target = fs::read_link(prefix.join("opt").join(keg)).ok()?;
    Some(target.file_name()?.to_string_lossy().into_owned())
}

/// The prefix, keg and version of a file under `<prefix>/Cellar/<keg>/<version>/`.
fn in_cellar(real: &Path) -> Option<(PathBuf, (String, String))> {
    let parts: Vec<Component<'_>> = real.components().collect();
    let at = parts.iter().position(|part| part.as_os_str() == "Cellar")?;
    let name = |n: usize| Some(parts.get(n)?.as_os_str().to_string_lossy().into_owned());
    let keg = (name(at + 1)?, name(at + 2)?);
    Some((parts[..at].iter().collect(), keg))
}

/// The load commands that name a library a file loads: `LC_LOAD_DYLIB`,
/// `LC_LOAD_WEAK_DYLIB`, `LC_REEXPORT_DYLIB`, `LC_LAZY_LOAD_DYLIB` and
/// `LC_LOAD_UPWARD_DYLIB` (`<mach-o/loader.h>`).
const LOADS: [u32; 5] = [0xc, 0x8000_0018, 0x8000_001f, 0x20, 0x8000_0023];

/// `LC_RPATH`: a directory an `@rpath/` name is searched in.
const RPATH: u32 = 0x8000_001c;

/// `MH_MAGIC_64` as a little-endian word: a 64-bit Mach-O file of arm64 or
/// `x86_64`, the only shape Homebrew pours on either.
const MAGIC: u32 = 0xfeed_facf;

/// The other Mach-O shapes, a 32-bit file and a universal one (`MH_MAGIC`,
/// `FAT_MAGIC` read little-endian): refused, since their loads go unread.
const UNREAD: [u32; 2] = [0xfeed_face, 0xbeba_feca];

/// What one file's load commands say: the libraries it names and the
/// directories its `@rpath/` names are searched in, both empty where it
/// is not a Mach-O file.
#[derive(Debug, Default)]
struct Loads {
    names: Vec<String>,
    rpaths: Vec<String>,
}

impl Loads {
    fn of(file: &Path) -> Result<Loads> {
        let mut reader = File::open(file).with_context(|| format!("reading {}", file.display()))?;
        let mut header = [0; 32];
        if reader.read_exact(&mut header).is_err() {
            return Ok(Loads::default());
        }
        match word(&header, 0) {
            Some(MAGIC) => {}
            Some(magic) if UNREAD.contains(&magic) => bail!(
                "{} is a 32-bit or universal Mach-O file, whose loads the walk of the pin's [closure] does not read",
                file.display()
            ),
            _ => return Ok(Loads::default()),
        }
        let size = word(&header, 20).unwrap_or_default();
        let length = reader.metadata().map(|m| m.len()).unwrap_or_default();
        if u64::from(size) > length {
            bail!(
                "{} names {size} bytes of load commands, more than the file holds",
                file.display()
            );
        }
        let mut commands = vec![0; size as usize];
        reader
            .read_exact(&mut commands)
            .with_context(|| format!("reading the load commands of {}", file.display()))?;
        let mut loads = Loads::default();
        let mut at = 0;
        while let (Some(command), Some(length)) = (word(&commands, at), word(&commands, at + 4)) {
            let end = at + length as usize;
            // A dylib command and an rpath command both carry their string's
            // offset from the command's start at +8.
            let text = || {
                let bytes = word(&commands, at + 8)
                    .and_then(|offset| commands.get(at + offset as usize..end))?;
                let bytes = bytes.split(|&b| b == 0).next()?;
                Some(String::from_utf8_lossy(bytes).into_owned())
            };
            if LOADS.contains(&command) {
                loads.names.extend(text());
            } else if command == RPATH {
                loads.rpaths.extend(text());
            }
            if length == 0 {
                break;
            }
            at = end;
        }
        Ok(loads)
    }

    /// A library `loader` names as the file it resolves to, none for one
    /// not on disk, a system library's: an absolute name, one under
    /// `@loader_path/`, or one under `@rpath/` found in the first of the
    /// loader's rpaths that holds it. A name the walk cannot place,
    /// `@executable_path/` or an `@rpath/` name in none of them, is refused
    /// rather than passed over, so a keg reached through one is never a
    /// keg unseen.
    fn resolved(&self, loader: &Path, name: &str) -> Result<Option<PathBuf>> {
        let placed = |name: &str| match name.strip_prefix("@loader_path/") {
            Some(rest) => loader.parent().map(|dir| dir.join(rest)),
            None => name.starts_with('/').then(|| PathBuf::from(name)),
        };
        if let Some(rest) = name.strip_prefix("@rpath/") {
            let found = self
                .rpaths
                .iter()
                .filter_map(|rpath| placed(rpath))
                .find_map(|dir| fs::canonicalize(dir.join(rest)).ok());
            if found.is_some() {
                return Ok(found);
            }
        } else if let Some(path) = placed(name) {
            return Ok(fs::canonicalize(path).ok());
        }
        bail!(
            "{} loads {name}, which the walk of the pin's [closure] does not resolve",
            loader.display()
        )
    }
}

fn word(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(bytes.get(at..at + 4)?.try_into().ok()?))
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use super::*;
    use crate::scratch::Tree;

    /// A 64-bit Mach-O file whose load commands name `loads`.
    fn macho(loads: &[&str]) -> Vec<u8> {
        let loads: Vec<(u32, &str)> = loads.iter().map(|name| (LOADS[0], *name)).collect();
        commanded(&loads)
    }

    /// A 64-bit Mach-O file of `commands`, each a command and its string.
    fn commanded(commands: &[(u32, &str)]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for (command, text) in commands {
            let mut body = text.as_bytes().to_vec();
            body.resize((body.len() + 1).next_multiple_of(8), 0);
            let length = u32::try_from(24 + body.len()).unwrap();
            for w in [*command, length, 24, 0, 0, 0] {
                bytes.extend(u32::to_le_bytes(w));
            }
            bytes.extend(body);
        }
        let count = u32::try_from(commands.len()).unwrap();
        let size = u32::try_from(bytes.len()).unwrap();
        let mut file = Vec::new();
        for w in [MAGIC, 0x0100_000c, 0, 2, count, size, 0, 0] {
            file.extend(u32::to_le_bytes(w));
        }
        file.extend(bytes);
        file
    }

    /// A Homebrew prefix of the closure's shape: php loading zip by its
    /// `opt` link and the system library, zip loading zstd beside it by
    /// `@rpath` through an rpath under `@loader_path`, and ca-certificates
    /// installed and loaded by none.
    fn prefix(tree: &Tree) -> (PathBuf, PathBuf) {
        let root = tree.path().join("brew");
        let keg = |name: &str, version: &str, file: &str, bytes: Vec<u8>| {
            let dir = root.join("Cellar").join(name).join(version);
            fs::create_dir_all(dir.join("lib")).unwrap();
            fs::create_dir_all(root.join("opt")).unwrap();
            fs::write(dir.join(file), bytes).unwrap();
            symlink(
                format!("../Cellar/{name}/{version}"),
                root.join("opt").join(name),
            )
            .unwrap();
        };
        let zip = root.join("opt/zip/lib/libzip.dylib");
        let php = macho(&[zip.to_str().unwrap(), "/usr/lib/libSystem.B.dylib"]);
        keg("php", "1", "php", php);
        let libzip = commanded(&[
            (RPATH, "@loader_path/../../../zstd/1.5.7_1/lib"),
            (LOADS[0], "@rpath/libzstd.dylib"),
        ]);
        keg("zip", "1.12_1", "lib/libzip.dylib", libzip);
        keg("zstd", "1.5.7_1", "lib/libzstd.dylib", macho(&[]));
        keg("ca-certificates", "2026-09-25", "lib/cert.pem", Vec::new());
        (root.join("Cellar/php/1/php"), root)
    }

    fn closure(kegs: &[(&str, &str)]) -> Closure {
        Closure(
            kegs.iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        )
    }

    const RECORD: [(&str, &str); 3] = [
        ("ca-certificates", "2026-09-25"),
        ("zip", "1.12_1"),
        ("zstd", "1.5.7_1"),
    ];

    #[test]
    fn the_digest_is_sha256_over_each_name_and_version_in_name_order() {
        let digest = closure(&RECORD).digest().to_string();
        // `printf 'ca-certificates=2026-09-25\nzip=1.12_1\nzstd=1.5.7_1\n' | shasum -a 256`
        let sum = "2193cacfbfb406fc46a4d40757fc61cd4d645c785025a210eeede5bfbf697fd9";
        assert_eq!(digest, format!("sha256:{sum}"));
        let mut reversed = RECORD;
        reversed.reverse();
        assert_eq!(closure(&reversed).digest().to_string(), digest);
        assert_ne!(closure(&RECORD[..2]).digest().to_string(), digest);
    }

    #[test]
    fn the_closure_a_binary_loads_holds_where_it_is_the_record() {
        let tree = Tree::new("closure-held").unwrap();
        let (php, root) = prefix(&tree);
        let walked = Walked::from(&[&php]).unwrap();
        assert_eq!(walked.prefix, Some(fs::canonicalize(root).unwrap()));
        assert_eq!(walked.kegs.len(), 2, "{walked:?}");
        closure(&RECORD).held(&[&php]).unwrap();
    }

    #[test]
    fn a_keg_at_another_version_refuses_naming_both() {
        let tree = Tree::new("closure-moved").unwrap();
        let (php, _) = prefix(&tree);
        let mut record = RECORD;
        record[1].1 = "1.11.4_1";
        record[0].1 = "2026-08-01";
        let err = closure(&record).held(&[&php]).unwrap_err().to_string();
        assert!(err.contains("zip is 1.12_1, recorded 1.11.4_1"), "{err}");
        assert!(
            err.contains("ca-certificates is 2026-09-25, recorded 2026-08-01"),
            "{err}"
        );
    }

    #[test]
    fn a_keg_loaded_and_not_recorded_refuses_naming_it() {
        let tree = Tree::new("closure-unnamed").unwrap();
        let (php, _) = prefix(&tree);
        let err = closure(&RECORD[..2]).held(&[&php]).unwrap_err().to_string();
        assert!(
            err.contains("zstd 1.5.7_1 is loaded and not recorded"),
            "{err}"
        );
    }

    #[test]
    fn a_name_the_walk_does_not_resolve_refuses_naming_it() {
        let tree = Tree::new("closure-rpath").unwrap();
        let php = tree.path().join("php");
        fs::write(&php, macho(&["@rpath/libzip.dylib"])).unwrap();
        let err = closure(&RECORD).held(&[&php]).unwrap_err().to_string();
        assert!(err.contains("loads @rpath/libzip.dylib"), "{err}");
    }

    #[test]
    fn a_run_is_refused_where_a_pinned_host_loads_a_keg_the_record_does_not_name() {
        let tree = Tree::new("closure-pin").unwrap();
        let (php, root) = prefix(&tree);
        let fpm = root.join("Cellar/php/1/php-fpm");
        let zstd = root.join("opt/zstd/lib/libzstd.dylib");
        fs::write(&fpm, macho(&[zstd.to_str().unwrap()])).unwrap();
        let md5 = |path: &Path| format!("{:x}", md5::compute(fs::read(path).unwrap()));
        let mut pin = crate::oracle::Pin::default();
        (pin.oracle.path, pin.oracle.md5) = (php.clone(), md5(&php));
        pin.closure = Some(closure(&[RECORD[0], RECORD[1]]));
        // php reaches zstd only through zip, so take that load away.
        fs::write(root.join("Cellar/zip/1.12_1/lib/libzip.dylib"), macho(&[])).unwrap();
        pin.oracle().asserted().unwrap();
        let host = crate::oracle::Host {
            md5: md5(&fpm),
            path: fpm,
        };
        pin.sapi.fpm = Some(host.clone());
        let err = pin.oracle().host_asserted(&host).unwrap_err().to_string();
        assert!(
            err.contains("zstd 1.5.7_1 is loaded and not recorded"),
            "{err}"
        );
    }

    #[test]
    fn a_binary_that_is_not_mach_o_loads_nothing() {
        let tree = Tree::new("closure-script").unwrap();
        let php = tree.path().join("php");
        fs::write(&php, "#!/bin/sh\n").unwrap();
        closure(&RECORD).held(&[&php]).unwrap();
    }
}
