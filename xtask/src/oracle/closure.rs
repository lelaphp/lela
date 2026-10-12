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

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::Read as _;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;

/// The keg names and their versions as `oracle.toml` records them.
#[derive(Debug, Clone, Default, Deserialize, PartialEq, Eq)]
#[serde(transparent)]
pub(crate) struct Closure(BTreeMap<String, String>);

impl Closure {
    /// Each keg and its version, by name.
    pub(crate) fn kegs(&self) -> &BTreeMap<String, String> {
        &self.0
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
            for name in dylibs(&file)? {
                let Some(real) = resolved(&file, &name) else {
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

/// A load command's library as the file it resolves to from `loader`, none
/// for a name relative to a search path or the executable, which no
/// library of the closure uses, and for one not on disk.
fn resolved(loader: &Path, name: &str) -> Option<PathBuf> {
    let path = match name.strip_prefix("@loader_path/") {
        Some(rest) => loader.parent()?.join(rest),
        None if name.starts_with('/') => PathBuf::from(name),
        None => return None,
    };
    fs::canonicalize(path).ok()
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

/// `MH_MAGIC_64`: a 64-bit Mach-O file in this machine's byte order.
const MAGIC: u32 = 0xfeed_facf;

/// The libraries a file's load commands name, none where it is not a
/// 64-bit Mach-O file.
fn dylibs(file: &Path) -> Result<Vec<String>> {
    let mut reader = File::open(file).with_context(|| format!("reading {}", file.display()))?;
    let mut header = [0; 32];
    if reader.read_exact(&mut header).is_err() || word(&header, 0) != Some(MAGIC) {
        return Ok(Vec::new());
    }
    let size = word(&header, 20).unwrap_or_default();
    let mut commands = vec![0; size as usize];
    reader
        .read_exact(&mut commands)
        .with_context(|| format!("reading the load commands of {}", file.display()))?;
    let mut names = Vec::new();
    let mut at = 0;
    while let (Some(command), Some(length)) = (word(&commands, at), word(&commands, at + 4)) {
        let end = at + length as usize;
        if LOADS.contains(&command)
            && let Some(name) =
                word(&commands, at + 8).and_then(|offset| commands.get(at + offset as usize..end))
        {
            let name = name.split(|&b| b == 0).next().unwrap_or_default();
            names.push(String::from_utf8_lossy(name).into_owned());
        }
        if length == 0 {
            break;
        }
        at = end;
    }
    Ok(names)
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
        let mut commands = Vec::new();
        for name in loads {
            let mut body = name.as_bytes().to_vec();
            body.resize((body.len() + 1).next_multiple_of(8), 0);
            let length = u32::try_from(24 + body.len()).unwrap();
            for w in [0xc, length, 24, 0, 0, 0] {
                commands.extend(u32::to_le_bytes(w));
            }
            commands.extend(body);
        }
        let mut file = Vec::new();
        let header = [
            MAGIC,
            0x0100_000c,
            0,
            2,
            u32::try_from(loads.len()).unwrap(),
            u32::try_from(commands.len()).unwrap(),
            0,
            0,
        ];
        for w in header {
            file.extend(u32::to_le_bytes(w));
        }
        file.extend(commands);
        file
    }

    /// A Homebrew prefix of the closure's shape: php loading zip by its
    /// `opt` link and the system library, zip loading zstd beside it by
    /// `@loader_path`, and ca-certificates installed and loaded by none.
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
        let libzip = macho(&["@loader_path/../../../zstd/1.5.7_1/lib/libzstd.dylib"]);
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
    fn a_binary_that_is_not_mach_o_loads_nothing() {
        let tree = Tree::new("closure-script").unwrap();
        let php = tree.path().join("php");
        fs::write(&php, "#!/bin/sh\n").unwrap();
        closure(&RECORD).held(&[&php]).unwrap();
    }
}
