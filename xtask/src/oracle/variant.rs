//! Oracle variants (#1782): the pinned tag built with the main oracle's own
//! configure line plus only the flags a variant names, so it differs from
//! the oracle only where intended, and pinned per platform beside it. A
//! `[[crate]]` row names its variant, and `crosscheck`, `witness` and
//! `phpt` run that crate under it; every other crate runs under the main
//! oracle. The same mechanism serves any later custom build.
//!
//! A variant is never a second recipe: [`super::Pin::variant`] is the pin
//! with the variant's binary for this machine's [`Platform`] standing in
//! for `[oracle]`'s, so every lane, adapter and digest assertion reads it
//! exactly as it reads the main oracle, through [`super::Oracle`]. What a
//! run holds of them is [`Oracles`]: one [`super::Oracle`] per pin, so each
//! digest is asserted once per run however many crates run under it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Result, bail};
use serde::Deserialize;

use super::{ClosureDigest, Oracle, Pin};

/// One platform an oracle build is pinned for, by the key its table
/// carries in `oracle.toml`: the maintainer's macOS arm64, and the two
/// Linux architectures built in Docker (#1782, #1889).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    DarwinArm64,
    LinuxX86_64,
    LinuxAarch64,
}

impl Platform {
    const ALL: [Platform; 3] = [
        Platform::DarwinArm64,
        Platform::LinuxX86_64,
        Platform::LinuxAarch64,
    ];

    /// The key of the platform's table under `[variant.<name>]`.
    pub fn key(self) -> &'static str {
        match self {
            Platform::DarwinArm64 => "darwin-arm64",
            Platform::LinuxX86_64 => "linux-x86_64",
            Platform::LinuxAarch64 => "linux-aarch64",
        }
    }

    /// The platform this harness was built for, none where it is no
    /// platform a build is pinned for.
    pub fn this() -> Option<Platform> {
        Platform::of(std::env::consts::OS, std::env::consts::ARCH)
    }

    /// The platform of Rust's names for an operating system and an
    /// architecture (`std::env::consts`).
    fn of(os: &str, arch: &str) -> Option<Platform> {
        match (os, arch) {
            ("macos", "aarch64") => Some(Platform::DarwinArm64),
            ("linux", "x86_64") => Some(Platform::LinuxX86_64),
            ("linux", "aarch64") => Some(Platform::LinuxAarch64),
            _ => None,
        }
    }

    /// Whether a build for the platform is made in a container whose image
    /// the pin names by digest: Linux's are, macOS's is made on the
    /// maintainer's machine (#1782).
    fn imaged(self) -> bool {
        self != Platform::DarwinArm64
    }
}

/// `[variant.<name>]`: the configure flags the variant adds to the main
/// oracle's line, and one table per platform its binary is pinned for.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Variant {
    /// What the variant's build adds to the main oracle's configure line
    /// (`--enable-zend-test`); the recipe that builds it reads them (#1889),
    /// and a lane reads only the binary.
    pub flags: Vec<String>,
    #[serde(rename = "darwin-arm64", default)]
    darwin_arm64: Option<Build>,
    #[serde(rename = "linux-x86_64", default)]
    linux_x86_64: Option<Build>,
    #[serde(rename = "linux-aarch64", default)]
    linux_aarch64: Option<Build>,
}

impl Variant {
    /// The build pinned for `platform`, none where the variant pins none.
    pub fn build(&self, platform: Platform) -> Option<&Build> {
        match platform {
            Platform::DarwinArm64 => self.darwin_arm64.as_ref(),
            Platform::LinuxX86_64 => self.linux_x86_64.as_ref(),
            Platform::LinuxAarch64 => self.linux_aarch64.as_ref(),
        }
    }

    fn build_mut(&mut self, platform: Platform) -> Option<&mut Build> {
        match platform {
            Platform::DarwinArm64 => self.darwin_arm64.as_mut(),
            Platform::LinuxX86_64 => self.linux_x86_64.as_mut(),
            Platform::LinuxAarch64 => self.linux_aarch64.as_mut(),
        }
    }

    /// Hold each platform's table to what the platform's build is: a Linux
    /// build names the base image's manifest digest, which the recipe pins
    /// and rebuilds from (#1948), and the macOS one, made on no image,
    /// names none. `name` is the variant's, for the refusal.
    pub(super) fn held(&self, name: &str) -> Result<()> {
        for platform in Platform::ALL {
            let Some(build) = self.build(platform) else {
                continue;
            };
            match (platform.imaged(), &build.image) {
                (true, None) => bail!(
                    "[variant.{name}.{}] names no image; a Linux build is pinned by its base image's manifest digest beside its md5 (#1782, #1948)",
                    platform.key()
                ),
                (false, Some(image)) => bail!(
                    "[variant.{name}.{}] names the image {image}; the macOS build is made on the maintainer's machine and names none (#1782)",
                    platform.key()
                ),
                _ => {}
            }
        }
        Ok(())
    }

    /// Each build's path that is relative, made the directory `dir`'s: the
    /// pin's own, so a build a recipe lays down inside the checkout is named
    /// as the file stands and found from wherever a lane runs.
    pub(super) fn placed(&mut self, dir: &Path) {
        for platform in Platform::ALL {
            if let Some(build) = self.build_mut(platform)
                && build.path.is_relative()
            {
                build.path = dir.join(&build.path);
            }
        }
    }
}

/// One platform's build of a variant: where its binary is on a machine of
/// that platform, the binary's md5, which is its identity as `[oracle]`'s
/// md5 is the main oracle's, and for a Linux build the base image's
/// manifest digest, which the recipe pins and rebuilds from, the md5
/// holding the binary it rebuilt (#1948); for the macOS build, the digest
/// of the `[closure]` it was made against (#2935). The path is where a machine
/// finds the binary: on Linux, the install prefix the recipe builds it to;
/// a relative path is the pin's directory's.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Build {
    pub path: PathBuf,
    pub md5: String,
    #[serde(default)]
    pub image: Option<String>,
    #[serde(default)]
    pub closure: Option<ClosureDigest>,
}

/// The oracles one run holds: the main oracle's, and one for each variant
/// some `[[crate]]` row names, each made once, so a digest is asserted once
/// per run whatever number of crates or subsets run under it (harness
/// §4.3). Made from the pins [`crate::manifest::Manifest::variants`] gives.
#[derive(Debug)]
pub struct Oracles<'a> {
    main: Oracle<'a>,
    variants: BTreeMap<&'a str, Oracle<'a>>,
}

impl<'a> Oracles<'a> {
    /// The main oracle under `main`, and each of `variants` by its name.
    pub fn new(main: &'a Pin, variants: &'a BTreeMap<String, Pin>) -> Oracles<'a> {
        Oracles {
            main: main.oracle(),
            variants: variants
                .iter()
                .map(|(name, pin)| (name.as_str(), pin.oracle()))
                .collect(),
        }
    }

    /// The oracle a crate runs under: the variant its row names, or the
    /// main oracle where it names none. A name the pin does not carry is
    /// refused naming it beside the variants the pin has, which the lane
    /// writes as that crate's failing reading (#1890).
    pub fn of(&self, variant: Option<&str>) -> Result<&Oracle<'a>> {
        let Some(name) = variant else {
            return Ok(&self.main);
        };
        match self.variants.get(name) {
            Some(oracle) => Ok(oracle),
            None => Err(self.main.pin().unknown(name)),
        }
    }

    /// The main oracle's.
    pub fn main(&self) -> &Oracle<'a> {
        &self.main
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_platform_is_rusts_os_and_arch_and_every_other_pair_is_none() {
        assert_eq!(
            Platform::of("macos", "aarch64"),
            Some(Platform::DarwinArm64)
        );
        assert_eq!(Platform::of("linux", "x86_64"), Some(Platform::LinuxX86_64));
        assert_eq!(
            Platform::of("linux", "aarch64"),
            Some(Platform::LinuxAarch64)
        );
        assert_eq!(Platform::of("macos", "x86_64"), None);
        assert_eq!(Platform::of("windows", "x86_64"), None);
        let keys: Vec<&str> = Platform::ALL.map(Platform::key).to_vec();
        assert_eq!(keys, ["darwin-arm64", "linux-x86_64", "linux-aarch64"]);
    }

    fn variant(text: &str) -> Variant {
        toml::from_str(text).unwrap()
    }

    #[test]
    fn a_linux_build_names_its_image_and_the_macos_build_names_none() {
        let md5 = "md5 = \"0\"";
        let held = variant(&format!(
            "flags = []\ndarwin-arm64 = {{ path = \"php\", {md5} }}\nlinux-x86_64 = {{ path = \"php\", {md5}, image = \"sha256:1\" }}\n"
        ));
        held.held("zend-test").unwrap();

        let imageless = variant(&format!(
            "flags = []\nlinux-aarch64 = {{ path = \"php\", {md5} }}\n"
        ));
        let err = imageless.held("zend-test").unwrap_err().to_string();
        assert!(
            err.contains("[variant.zend-test.linux-aarch64] names no image"),
            "{err}"
        );

        let imaged = variant(&format!(
            "flags = []\ndarwin-arm64 = {{ path = \"php\", {md5}, image = \"sha256:1\" }}\n"
        ));
        let err = imaged.held("zend-test").unwrap_err().to_string();
        assert!(
            err.contains("[variant.zend-test.darwin-arm64] names the image sha256:1"),
            "{err}"
        );
    }

    #[test]
    fn a_relative_path_is_the_pins_directorys_and_an_absolute_one_is_kept() {
        let mut held = variant(
            "flags = []\ndarwin-arm64 = { path = \"build/php\", md5 = \"0\" }\nlinux-x86_64 = { path = \"/opt/php\", md5 = \"0\", image = \"sha256:1\" }\n",
        );
        held.placed(Path::new("/checkout"));
        let path = |platform| held.build(platform).map(|b| b.path.clone());
        assert_eq!(
            path(Platform::DarwinArm64),
            Some(PathBuf::from("/checkout/build/php"))
        );
        assert_eq!(path(Platform::LinuxX86_64), Some(PathBuf::from("/opt/php")));
        assert_eq!(path(Platform::LinuxAarch64), None);
    }
}
