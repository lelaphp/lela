//! `[closure]` of the pin: the Homebrew kegs the pinned binaries load,
//! each at the version Homebrew names it by (its `pkg_version`,
//! `1.12_1`), held with php's digest as one oracle. The
//! digest holds php's own bytes and none of what it loads, so a `brew
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
//! **What holds a keg still** is its pin: every keg the record names is
//! `brew pin`ned, a link under `<prefix>/var/homebrew/pinned/`, so a `brew
//! upgrade` run for another reason passes it over. The walk sees a
//! keg move only after it moved, by when the recorded version is gone from
//! the Cellar; an unpinned keg is refused before it can. So is the keg the
//! pinned binaries stand in themselves, php's own: the digest sees it
//! move as the walk sees the others, after, when the pinned path is gone.
//! A machine with no Homebrew prefix under its binaries, a Linux runner,
//! holds neither, a binary under no Cellar, a variant's build, stands in
//! no keg to pin, and a macOS runner is held as the oracle's machine is,
//! since `scripts/ci/pinned-php.sh` pins php's keg and each keg it pours
//! before any lane. A refusal's `brew pin` names each keg as
//! `pinned-php.sh` pins it, by the tap its receipt records, so no alias of homebrew-core's
//! stands in for a keg poured from another tap.
//!
//! **What a build was made against** is the record's digest, [`Closure::digest`]: a
//! variant's macOS build is made against the Homebrew the record names, so
//! its pin names the closure's digest beside its own, as a Linux build names its
//! image.

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
    /// name order, so one closure always gives one digest.
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
    /// versions; so does a keg it names, installed at its version, that is
    /// not pinned, and the keg a binary stands in itself, with the `brew
    /// pin` that pins them. A keg that drifted is not offered the pin,
    /// which would hold it where it drifted to.
    pub(crate) fn held(&self, binaries: &[&Path]) -> Result<()> {
        let walked = Walked::from(binaries)?;
        let (mut drift, mut drifted) = (Vec::new(), BTreeSet::new());
        for (keg, version) in &walked.kegs {
            match self.0.get(keg) {
                Some(recorded) if recorded == version => continue,
                Some(recorded) => drift.push(format!("{keg} is {version}, recorded {recorded}")),
                None => drift.push(format!("{keg} {version} is loaded and not recorded")),
            }
            drifted.insert(keg.as_str());
        }
        let (mut own, mut unpinned, mut pin) = (Vec::new(), Vec::new(), Vec::new());
        if let Some(prefix) = &walked.prefix {
            let unloaded = self.0.iter().filter(|(keg, _)| !walked.names(keg));
            for (keg, recorded) in unloaded {
                match installed(prefix, keg) {
                    Some(version) if version == *recorded => continue,
                    Some(version) => drift.push(format!("{keg} is {version}, recorded {recorded}")),
                    None => drift.push(format!("{keg} is not installed, recorded {recorded}")),
                }
                drifted.insert(keg.as_str());
            }
            let pins = prefix.join("var/homebrew/pinned");
            let loose = |(keg, _): &(&str, &str)| pins.join(keg).symlink_metadata().is_err();
            // A keg the record names too is held as the record's.
            let stood_in = walked
                .own
                .iter()
                .filter(|(keg, _)| !self.0.contains_key(keg));
            let stood_in = stood_in.map(|(keg, version)| (keg.as_str(), version.as_str()));
            own.extend(stood_in.filter(loose));
            let at_record = self
                .0
                .iter()
                .filter(|(keg, _)| !drifted.contains(keg.as_str()));
            let at_record = at_record.map(|(keg, version)| (keg.as_str(), version.as_str()));
            unpinned.extend(at_record.filter(loose));
            let named = own.iter().chain(&unpinned);
            pin.extend(named.map(|(keg, version)| full_name(prefix, keg, version)));
        }
        let (own, unpinned): (Vec<_>, Vec<_>) = (
            own.into_iter().map(|(keg, _)| keg).collect(),
            unpinned.into_iter().map(|(keg, _)| keg).collect(),
        );
        let mut refusal = Vec::new();
        if !drift.is_empty() {
            refusal.push(format!(
                "the kegs the pinned php loads drifted from the pin's [closure]: {}. Report the drift; the pin moves only by a person's hand",
                drift.join("; ")
            ));
        }
        if !own.is_empty() || !unpinned.is_empty() {
            let (is, it) = if own.len() + unpinned.len() == 1 {
                ("is", "it")
            } else {
                ("are", "them")
            };
            let mut loose = Vec::new();
            if !own.is_empty() {
                let comma = if unpinned.is_empty() { "," } else { "" };
                loose.push(format!(
                    "{}, where the pinned binaries stand{comma}",
                    own.join(", ")
                ));
            }
            if !unpinned.is_empty() {
                loose.push(format!("{} of the pin's [closure]", unpinned.join(", ")));
            }
            refusal.push(format!(
                "{} {is} not `brew pin`ned, so a `brew upgrade` can move {it} before this walk sees the move: `brew pin {}`",
                loose.join(", and "),
                pin.join(" ")
            ));
        }
        if !refusal.is_empty() {
            bail!("{}", refusal.join(". "));
        }
        Ok(())
    }

    /// The record `binaries` stand to on this machine, walked as
    /// [`Closure::held`] walks them, which a bump writes: each keg
    /// they load at the version it resolved to, and each keg of `named`,
    /// the runtime kegs of their keg's receipt, that none loads, at the
    /// version `<prefix>/opt/<keg>` points at; nothing else. A keg
    /// loaded at two versions and a named keg not installed each refuse,
    /// since no record holds either.
    pub(crate) fn walked(binaries: &[&Path], named: &[String]) -> Result<Closure> {
        let walked = Walked::from(binaries)?;
        let mut kegs = BTreeMap::new();
        for (keg, version) in &walked.kegs {
            if let Some(other) = kegs.insert(keg.clone(), version.clone()) {
                bail!("{keg} is loaded at two versions, {other} and {version}");
            }
        }
        for keg in named.iter().filter(|keg| !walked.names(keg)) {
            let version = walked.prefix.as_deref().and_then(|p| installed(p, keg));
            let Some(version) = version else {
                bail!(
                    "{keg}, a runtime keg of the receipt, is loaded by none of the binaries and installed at no version"
                );
            };
            kegs.insert(keg.clone(), version);
        }
        Ok(Closure(kegs))
    }
}

/// What a walk from the binaries found: every keg a library of theirs
/// resolved into, at the version it resolved to, the kegs the binaries
/// stand in themselves, each at its version, and the Homebrew prefix the first keg or binary
/// stands under, none where nothing does.
#[derive(Debug, Default)]
struct Walked {
    kegs: BTreeSet<(String, String)>,
    own: BTreeSet<(String, String)>,
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
            if let Some((prefix, keg)) = in_cellar(&real) {
                walked.prefix.get_or_insert(prefix);
                walked.own.insert(keg);
            }
            queue.push(real);
        }
        while let Some(file) = queue.pop() {
            if !seen.insert(file.clone()) {
                continue;
            }
            for real in Loads::libraries(&file)? {
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

/// The libraries one binary names in its own load commands, each by the
/// file it resolves to and the keg that file stands in, beside the
/// Homebrew prefix the binary stands under. A crate built against
/// a keg is held to these, so it links the keg the oracle loads itself
/// and not another the closure carries only because a library php loads
/// links it.
#[derive(Debug)]
pub(crate) struct Own {
    prefix: PathBuf,
    libraries: Vec<(PathBuf, String)>,
}

impl Own {
    /// What `binary` loads itself; none where it is not on this machine
    /// or stands under no Cellar, as a Linux build does.
    pub(crate) fn of(binary: &Path) -> Result<Option<Own>> {
        let Ok(real) = fs::canonicalize(binary) else {
            return Ok(None);
        };
        let Some((prefix, _)) = in_cellar(&real) else {
            return Ok(None);
        };
        let libraries = Loads::libraries(&real)?.into_iter().filter_map(|file| {
            let (_, (keg, _)) = in_cellar(&file)?;
            Some((file, keg))
        });
        Ok(Some(Own {
            prefix,
            libraries: libraries.collect(),
        }))
    }

    /// The keg the binary loads a library of `stem` from: `openssl@4` for
    /// `libssl`, by `libssl.4.dylib`.
    pub(crate) fn keg_loading(&self, stem: &str) -> Option<&str> {
        let named = |file: &Path| {
            let name = file.file_name()?.to_str()?;
            Some(name.strip_prefix(stem)?.starts_with('.'))
        };
        (self.libraries.iter())
            .find(|(file, _)| named(file) == Some(true))
            .map(|(_, keg)| keg.as_str())
    }

    /// The keg of the binary's prefix `path` stands in once its links are
    /// followed (`/opt/homebrew/opt/openssl@4` is `openssl@4`); none where
    /// it is not on this machine or in no keg of that prefix.
    pub(crate) fn keg_at(&self, path: &Path) -> Option<String> {
        let (prefix, (keg, _)) = in_cellar(&fs::canonicalize(path).ok()?)?;
        (prefix == self.prefix).then_some(keg)
    }
}

/// The name a `brew pin` of the keg `<prefix>/Cellar/<keg>/<version>`
/// gives the formula it was poured from, as `scripts/ci/pinned-php.sh`
/// pins it and as brew names a formula (`Formula#full_name`):
/// `<tap>/<keg>` where the keg's receipt records a tap other than
/// homebrew-core, `dax/pin/php@8.5` for one `pinned-php.sh` poured, and the
/// bare name otherwise. A bare name brew resolves through homebrew-core's
/// aliases before a tap's formula, so `php@8.5` would pin `php` while it
/// is an alias of `php` there.
fn full_name(prefix: &Path, keg: &str, version: &str) -> String {
    #[derive(Deserialize)]
    struct Receipt {
        source: Source,
    }
    #[derive(Deserialize)]
    struct Source {
        tap: Option<String>,
    }
    let receipt = prefix
        .join("Cellar")
        .join(keg)
        .join(version)
        .join("INSTALL_RECEIPT.json");
    let read = fs::read_to_string(receipt).ok();
    let receipt = read.and_then(|text| serde_json::from_str::<Receipt>(&text).ok());
    match receipt.and_then(|receipt| receipt.source.tap).as_deref() {
        None | Some("homebrew/core") => keg.to_owned(),
        Some(tap) => format!("{tap}/{keg}"),
    }
}

/// The version `<prefix>/opt/<keg>` points at, none where no link is.
fn installed(prefix: &Path, keg: &str) -> Option<String> {
    let target = fs::read_link(prefix.join("opt").join(keg)).ok()?;
    Some(target.file_name()?.to_string_lossy().into_owned())
}

/// The prefix, keg and version of a file under `<prefix>/Cellar/<keg>/<version>/`.
pub(super) fn in_cellar(real: &Path) -> Option<(PathBuf, (String, String))> {
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

/// `LC_BUILD_VERSION`: the platform a file was linked for, at +8, and its
/// `minos`, at +12.
const BUILD_VERSION: u32 = 0x32;

/// `PLATFORM_MACOS`, the platform of `LC_BUILD_VERSION` a macOS file names.
const MACOS: u32 = 1;

/// `LC_VERSION_MIN_MACOSX`: the macOS a file was linked for, at +8, which
/// an older ld writes in place of `LC_BUILD_VERSION`.
const VERSION_MIN_MACOSX: u32 = 0x24;

/// The oldest macOS a Mach-O file was linked to run on, `LC_BUILD_VERSION`'s
/// `minos` as the command encodes it, `xxxx.yy.zz` in one word, and the
/// version a `MACOSX_DEPLOYMENT_TARGET` names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Minos(u32);

impl Minos {
    /// The macOS `binary` was linked for, none where it is no macOS
    /// Mach-O file.
    pub(crate) fn of(binary: &Path) -> Result<Option<Minos>> {
        Ok(Loads::of(binary)?.minos)
    }

    /// The version `15.0` or `15.4.1` names, as rustc and ld read
    /// `MACOSX_DEPLOYMENT_TARGET`; none for another shape.
    pub(crate) fn parse(text: &str) -> Option<Minos> {
        let mut parts = text.split('.').map(str::parse::<u32>);
        let major = parts.next()?.ok().filter(|&n| n <= 0xffff)?;
        let mut next = || {
            parts
                .next()
                .map_or(Some(0), |p| p.ok().filter(|&n| n <= 0xff))
        };
        let (minor, patch) = (next()?, next()?);
        parts
            .next()
            .is_none()
            .then_some(Minos((major << 16) | (minor << 8) | patch))
    }
}

impl std::fmt::Display for Minos {
    /// As `otool -l` prints it: `15.0`, and `15.4.1` where there is a patch.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (major, minor, patch) = (self.0 >> 16, (self.0 >> 8) & 0xff, self.0 & 0xff);
        write!(f, "{major}.{minor}")?;
        if patch != 0 {
            write!(f, ".{patch}")?;
        }
        Ok(())
    }
}

/// `MH_MAGIC_64` as a little-endian word: a 64-bit Mach-O file of arm64 or
/// `x86_64`, the only shape Homebrew pours on either.
const MAGIC: u32 = 0xfeed_facf;

/// The other Mach-O shapes, a 32-bit file and a universal one (`MH_MAGIC`,
/// `FAT_MAGIC` read little-endian): refused, since their loads go unread.
const UNREAD: [u32; 2] = [0xfeed_face, 0xbeba_feca];

/// What one file's load commands say: the libraries it names, the
/// directories its `@rpath/` names are searched in and the macOS it was
/// linked for, all empty where it is not a Mach-O file.
#[derive(Debug, Default)]
struct Loads {
    names: Vec<String>,
    rpaths: Vec<String>,
    minos: Option<Minos>,
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
            } else if command == BUILD_VERSION && word(&commands, at + 8) == Some(MACOS) {
                loads.minos = word(&commands, at + 12).map(Minos);
            } else if command == VERSION_MIN_MACOSX {
                loads.minos = word(&commands, at + 8).map(Minos);
            }
            if length == 0 {
                break;
            }
            at = end;
        }
        Ok(loads)
    }

    /// Every library `file` names that resolves to a file on disk, as
    /// [`Loads::resolved`] places each.
    fn libraries(file: &Path) -> Result<Vec<PathBuf>> {
        let loads = Loads::of(file)?;
        let mut out = Vec::new();
        for name in &loads.names {
            out.extend(loads.resolved(file, name)?);
        }
        Ok(out)
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

/// Mach-O stand-ins for the binaries a pin names, which the tests here
/// and `api_check::binding`'s lay down.
#[cfg(test)]
pub(crate) mod stand_in {
    use super::{LOADS, MAGIC};

    /// A 64-bit Mach-O file whose load commands name `loads`.
    pub(crate) fn macho(loads: &[&str]) -> Vec<u8> {
        let loads: Vec<(u32, &str)> = loads.iter().map(|name| (LOADS[0], *name)).collect();
        commanded(&loads)
    }

    /// A 64-bit Mach-O file of `commands`, each a command and its string.
    pub(crate) fn commanded(commands: &[(u32, &str)]) -> Vec<u8> {
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

    /// A Mach-O stand-in linked for `minos` of `platform`, its one command
    /// the `LC_BUILD_VERSION` `ld -platform_version <platform> <minos>
    /// 15.4` writes.
    pub(crate) fn built_for(platform: u32, minos: u32) -> Vec<u8> {
        let mut file = macho(&[]);
        file[16..24].copy_from_slice(&[1, 0, 0, 0, 24, 0, 0, 0]);
        for w in [super::BUILD_VERSION, 24, platform, minos, 0x000f_0400, 0] {
            file.extend(u32::to_le_bytes(w));
        }
        file
    }

    /// A stand-in linked for macOS 15.0.
    pub(crate) fn macos_15() -> Vec<u8> {
        built_for(super::MACOS, 0x000f_0000)
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use super::stand_in::{built_for, commanded, macho};
    use super::*;
    use crate::scratch::Tree;

    /// A Homebrew prefix of the closure's shape: php loading zip by its
    /// `opt` link and the system library, zip loading zstd beside it by
    /// `@rpath` through an rpath under `@loader_path`, and ca-certificates
    /// installed and loaded by none; every keg pinned, as `brew pin` links
    /// it under `var/homebrew/pinned/`.
    fn prefix(tree: &Tree) -> (PathBuf, PathBuf) {
        let root = tree.path().join("brew");
        let keg = |name: &str, version: &str, file: &str, bytes: Vec<u8>| {
            let dir = root.join("Cellar").join(name).join(version);
            fs::create_dir_all(dir.join("lib")).unwrap();
            fs::write(dir.join(file), bytes).unwrap();
            for links in ["opt", "var/homebrew/pinned"] {
                let at = root.join(links);
                fs::create_dir_all(&at).unwrap();
                let up = "../".repeat(links.matches('/').count() + 1);
                symlink(format!("{up}Cellar/{name}/{version}"), at.join(name)).unwrap();
            }
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
    fn a_keg_installed_at_its_version_and_not_pinned_refuses_naming_the_pin() {
        let tree = Tree::new("closure-unpinned").unwrap();
        let (php, root) = prefix(&tree);
        for keg in ["ca-certificates", "zip"] {
            fs::remove_file(root.join("var/homebrew/pinned").join(keg)).unwrap();
        }
        let err = closure(&RECORD).held(&[&php]).unwrap_err().to_string();
        assert_eq!(
            err,
            "ca-certificates, zip of the pin's [closure] are not `brew pin`ned, so a `brew upgrade` \
             can move them before this walk sees the move: `brew pin ca-certificates zip`"
        );
        // A keg that drifted is named as drift and offered no pin, which
        // would hold it at the version it drifted to.
        fs::remove_file(root.join("var/homebrew/pinned/zstd")).unwrap();
        let mut record = RECORD;
        record[2].1 = "1.5.6";
        let err = closure(&record).held(&[&php]).unwrap_err().to_string();
        assert!(err.contains("zstd is 1.5.7_1, recorded 1.5.6"), "{err}");
        assert!(err.ends_with("`brew pin ca-certificates zip`"), "{err}");
        // So is one loaded by nothing, installed at another version or not
        // at all: brew refuses the pin of a keg not installed.
        let mut record = RECORD;
        record[0].1 = "2026-08-01";
        let err = closure(&record).held(&[&php]).unwrap_err().to_string();
        assert!(
            err.contains("ca-certificates is 2026-09-25, recorded 2026-08-01"),
            "{err}"
        );
        assert!(err.ends_with("`brew pin zip zstd`"), "{err}");
        fs::remove_file(root.join("opt/ca-certificates")).unwrap();
        let err = closure(&RECORD).held(&[&php]).unwrap_err().to_string();
        assert!(
            err.contains("ca-certificates is not installed, recorded 2026-09-25"),
            "{err}"
        );
        assert!(err.ends_with("`brew pin zip zstd`"), "{err}");
    }

    /// The keg the pinned php stands in is held pinned as each keg of the
    /// record is: unpinned, it refuses with its `brew pin`, alone
    /// and beside a keg of the record, in the one sentence.
    #[test]
    fn the_keg_the_pinned_php_stands_in_refuses_unpinned_naming_the_pin() {
        let tree = Tree::new("closure-own").unwrap();
        let (php, root) = prefix(&tree);
        let walked = Walked::from(&[&php]).unwrap();
        let own = ("php".to_owned(), "1".to_owned());
        assert_eq!(walked.own, BTreeSet::from([own]));
        fs::remove_file(root.join("var/homebrew/pinned/php")).unwrap();
        let err = closure(&RECORD).held(&[&php]).unwrap_err().to_string();
        assert_eq!(
            err,
            "php, where the pinned binaries stand, is not `brew pin`ned, so a `brew upgrade` can \
             move it before this walk sees the move: `brew pin php`"
        );
        fs::remove_file(root.join("var/homebrew/pinned/zip")).unwrap();
        let err = closure(&RECORD).held(&[&php]).unwrap_err().to_string();
        assert_eq!(
            err,
            "php, where the pinned binaries stand, and zip of the pin's [closure] are not `brew \
             pin`ned, so a `brew upgrade` can move them before this walk sees the move: \
             `brew pin php zip`"
        );
    }

    /// The pin a refusal asks for names each keg as brew names the formula
    /// its receipt says it was poured from, as `scripts/ci/pinned-php.sh`
    /// pins it: a php poured from the tap `dax/pin` by its tap, so
    /// homebrew-core's alias of the same name, `php@8.5` of `php`, cannot
    /// stand in for it, and a keg of homebrew-core's bare; the sentence
    /// names each by its keg.
    #[test]
    fn the_pin_a_refusal_asks_for_names_the_tap_a_keg_was_poured_from() {
        let tree = Tree::new("closure-tapped").unwrap();
        let (php, root) = prefix(&tree);
        let receipt = |keg: &str, tap: &str| {
            let json = format!(r#"{{"source":{{"tap":"{tap}"}}}}"#);
            fs::write(
                root.join("Cellar").join(keg).join("INSTALL_RECEIPT.json"),
                json,
            )
            .unwrap();
        };
        receipt("php/1", "dax/pin");
        receipt("zip/1.12_1", "homebrew/core");
        for keg in ["php", "zip"] {
            fs::remove_file(root.join("var/homebrew/pinned").join(keg)).unwrap();
        }
        let err = closure(&RECORD).held(&[&php]).unwrap_err().to_string();
        assert_eq!(
            err,
            "php, where the pinned binaries stand, and zip of the pin's [closure] are not `brew \
             pin`ned, so a `brew upgrade` can move them before this walk sees the move: \
             `brew pin dax/pin/php zip`"
        );
    }

    /// A php built outside the Cellar, a variant's, stands in no keg: it
    /// loads the record's kegs, which are held pinned, and no pin is
    /// asked for the php the prefix holds beside them.
    #[test]
    fn a_php_under_no_cellar_stands_in_no_keg_to_pin() {
        let tree = Tree::new("closure-built").unwrap();
        let (poured, root) = prefix(&tree);
        let built = tree.path().join("built/bin/php");
        fs::create_dir_all(built.parent().unwrap()).unwrap();
        fs::copy(&poured, &built).unwrap();
        fs::remove_file(root.join("var/homebrew/pinned/php")).unwrap();
        let walked = Walked::from(&[&built]).unwrap();
        assert_eq!(walked.own, BTreeSet::new());
        assert_eq!(walked.kegs.len(), 2, "{walked:?}");
        closure(&RECORD).held(&[&built]).unwrap();
        fs::remove_file(root.join("var/homebrew/pinned/zip")).unwrap();
        let err = closure(&RECORD).held(&[&built]).unwrap_err().to_string();
        assert!(
            err.starts_with("zip of the pin's [closure] is not"),
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
        let md5 = |path: &Path| {
            crate::oracle::Digest::Md5(format!("{:x}", md5::compute(fs::read(path).unwrap())))
        };
        let mut pin = crate::oracle::Pin::default();
        (pin.oracle.path, pin.oracle.digest) = (php.clone(), md5(&php));
        pin.closure = Some(closure(&[RECORD[0], RECORD[1]]));
        // php reaches zstd only through zip, so take that load away.
        fs::write(root.join("Cellar/zip/1.12_1/lib/libzip.dylib"), macho(&[])).unwrap();
        pin.oracle().asserted().unwrap();
        let host = crate::oracle::Host {
            digest: md5(&fpm),
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
    fn the_macos_a_binary_is_linked_for_is_its_build_version_minos() {
        let tree = Tree::new("closure-minos").unwrap();
        let php = tree.path().join("php");
        fs::write(&php, built_for(MACOS, 0x000f_0000)).unwrap();
        let minos = Minos::of(&php).unwrap().unwrap();
        assert_eq!(minos.to_string(), "15.0");
        assert_eq!(Minos::parse("15.0"), Some(minos));
        assert_eq!(Minos::parse("15"), Some(minos));
        assert_ne!(Minos::parse("11.0"), Some(minos));
        assert_eq!(Minos::parse("15.4.1").unwrap().to_string(), "15.4.1");
        for shape in ["", "15.x", "15.0.0.1", "15.256"] {
            assert_eq!(Minos::parse(shape), None, "{shape}");
        }
        // An iOS binary (platform 2) and one with no such command are no
        // macOS file's.
        fs::write(&php, built_for(2, 0x000f_0000)).unwrap();
        assert_eq!(Minos::of(&php).unwrap(), None);
        fs::write(&php, macho(&[])).unwrap();
        assert_eq!(Minos::of(&php).unwrap(), None);
    }

    #[test]
    fn a_binary_that_is_not_mach_o_loads_nothing() {
        let tree = Tree::new("closure-script").unwrap();
        let php = tree.path().join("php");
        fs::write(&php, "#!/bin/sh\n").unwrap();
        closure(&RECORD).held(&[&php]).unwrap();
    }
}
