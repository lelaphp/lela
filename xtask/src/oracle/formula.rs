//! `[oracle] formula` (#4383, ruled on #4373): the Homebrew formula the
//! pinned php is poured from. Homebrew's `php` is its newest line, and a
//! line it has moved past lives in `php@<major>.<minor>`, poured into the
//! keg of that name, `Cellar/php@8.5/<version>/`. So the pin names the
//! formula its line lives in, `scripts/ci/pinned-php.sh` pours that one,
//! and a pin that names none is poured from `php`.
//!
//! **What is held** is that the pin says one thing: a pinned binary whose
//! path stands under a Cellar stands in the formula's keg, at a bottle of
//! `[oracle].version`, and the formula of one line is the line of that
//! version. Each disagreement is a defect of the pin
//! ([`super::Pin::defects`]), read off the file alone, so it is refused on
//! a machine with no keg at all, a runner before its pour: `status
//! --section oracle` renders no pin whose formula and paths name two kegs,
//! or whose path names a bottle of another version, and a runner is handed
//! none to pour.
//!
//! **The bottle** is the keg's version as Homebrew names it, its
//! `pkg_version`: php's version, and the formula's revision after `_` where
//! Homebrew bottled that version again, `8.5.2_1`, which is the directory
//! the keg stands in. `[oracle].version` is php's and carries no revision,
//! so the bottle a runner pours is read off `[oracle].path`
//! ([`super::Binary::bottle`]).
//!
//! **The formula is digested**, as every key of the pin is but a build's
//! [`super::Provenance`] (#2969): [`super::Pin::digest`] moves when a pin
//! first names its formula, and with the paths and the md5 when the
//! formula changes.

use std::fmt;
use std::path::Path;
use std::str::FromStr;

use anyhow::{Error, Result, bail};
use serde::Deserialize;

use super::Binary;
use super::closure::in_cellar;

/// The Homebrew formula the pinned php is poured from, as `[oracle]`
/// names it, and the name of the keg it is poured into.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(try_from = "String")]
pub(crate) enum Formula {
    /// `php`: Homebrew's newest line, and the formula of a pin that names
    /// none.
    #[default]
    Newest,
    /// `php@8.5`: one line, as Homebrew names it once `php` has moved past
    /// it.
    Line { major: u32, minor: u32 },
}

impl Formula {
    /// The key `[oracle]` names the formula under.
    const KEY: &str = "formula";

    /// Whether the formula pours php `version`: the newest line's pours
    /// whichever line is newest, and one line's pours that line alone.
    fn pours(self, version: &str) -> bool {
        let Formula::Line { major, minor } = self else {
            return true;
        };
        let mut parts = version.split('.');
        let mut next = || parts.next().and_then(|part| part.parse::<u32>().ok());
        (next(), next()) == (Some(major), Some(minor))
    }

    /// What of the pin disagrees with its formula, each a refusal's text:
    /// `version` where the formula is another line's, and each of
    /// `binaries`, a pinned binary by the key that names it, whose path
    /// stands in another formula's keg, or in a keg that is no bottle of
    /// `version`. A path under no Cellar, a Linux build's or a test's
    /// stand-in, stands in no keg and is held to none.
    pub(super) fn held(self, version: &str, binaries: &[(&str, &Path)]) -> Vec<String> {
        let mut defects = Vec::new();
        if !self.pours(version) {
            defects.push(format!(
                "[oracle].{} is {self}, the formula of another line than [oracle].version, {version} (#4383); the pin moves only by a person's hand",
                Formula::KEY
            ));
        }
        for (key, path) in binaries {
            let Some((_, (keg, bottle))) = in_cellar(path) else {
                continue;
            };
            if keg != self.to_string() {
                defects.push(format!(
                    "{key} {} stands in the keg {keg}, and the pin is poured from the formula {self}, into the keg of that name: [oracle].{} names the formula, `php` where the pin names none (#4383); the pin moves only by a person's hand",
                    path.display(),
                    Formula::KEY
                ));
            }
            if !bottles(version, &bottle) {
                defects.push(format!(
                    "{key} {} stands in the keg {keg} at {bottle}, which is no bottle of [oracle].version, {version}: a keg stands at php's version, with the formula's revision after `_` where Homebrew bottled the version again, and `scripts/ci/pinned-php.sh` pours the bottle the path names (#4383); the pin moves only by a person's hand",
                    path.display()
                ));
            }
        }
        defects
    }
}

/// Whether a keg at `bottle` holds php `version`: the version itself, or
/// the version and a revision as Homebrew writes one, `8.5.2_1`. `_0` and
/// `_01` are not how it writes one, and `8.5.20` is another version.
fn bottles(version: &str, bottle: &str) -> bool {
    let revised = |revision: &str| {
        let written = revision.strip_prefix('_')?.parse::<u32>().ok()?;
        Some(written > 0 && format!("_{written}") == revision)
    };
    match bottle.strip_prefix(version) {
        Some("") => true,
        Some(revision) => revised(revision) == Some(true),
        None => false,
    }
}

impl Binary {
    /// The bottle the binary's keg is poured from, as Homebrew names it
    /// (its `pkg_version`): the version of the keg `path` stands in,
    /// `8.5.2_1` where the formula was revised, which [`Formula::held`]
    /// holds to `version`; and `version` itself where `path` stands under
    /// no Cellar.
    pub(crate) fn bottle(&self) -> String {
        in_cellar(&self.path).map_or_else(|| self.version.clone(), |(_, (_, bottle))| bottle)
    }
}

impl fmt::Display for Formula {
    /// The formula as Homebrew names it, which is its keg's name.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Formula::Newest => f.write_str("php"),
            Formula::Line { major, minor } => write!(f, "php@{major}.{minor}"),
        }
    }
}

impl FromStr for Formula {
    type Err = Error;

    /// `php`, or `php@<major>.<minor>` as Homebrew writes a line. Any
    /// other name is refused, a tap's `user/tap/php` and a patch's
    /// `php@8.5.7` among them: a runner pours homebrew-core's formula of
    /// this name and no other.
    fn from_str(name: &str) -> Result<Formula> {
        let line = || {
            let (major, minor) = name.strip_prefix("php@")?.split_once('.')?;
            Some(Formula::Line {
                major: major.parse().ok()?,
                minor: minor.parse().ok()?,
            })
        };
        let formula = if name == "php" {
            Some(Formula::Newest)
        } else {
            // `+8` and `08` parse as 8 and are not how Homebrew writes it.
            line().filter(|formula| formula.to_string() == name)
        };
        let Some(formula) = formula else {
            bail!(
                "{name:?} is no formula the oracle is poured from: [oracle].{} is `php`, Homebrew's newest line, or `php@<major>.<minor>`, one line's",
                Formula::KEY
            );
        };
        Ok(formula)
    }
}

impl TryFrom<String> for Formula {
    type Error = Error;

    fn try_from(name: String) -> Result<Formula> {
        name.parse()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::oracle::Pin;

    /// The grammar's least pin, poured from `formula` where one is named,
    /// its php and one host at `keg` of the Cellar, `php` or `php/8.5.2_1`
    /// where the keg stands at another bottle than `version` names.
    fn pin(formula: Option<&str>, version: &str, keg: &str) -> Result<Pin> {
        let formula = formula.map_or(String::new(), |name| format!("formula = {name:?}\n"));
        let cellar = match keg.split_once('/') {
            Some((keg, bottle)) => format!("/opt/homebrew/Cellar/{keg}/{bottle}"),
            None => format!("/opt/homebrew/Cellar/{keg}/{version}"),
        };
        Pin::parse(&format!(
            "[oracle]\nversion = {version:?}\n{formula}path = \"{cellar}/bin/php\"\nmd5 = \"0\"\n\
             [source]\ntag = \"t\"\ncommit = \"c\"\nphpt_files = 0\n\
             [recipe]\nscript_name = \"w.php\"\npreamble = \"\"\nargs = []\nini = []\nenv_clear = true\n\
             [recipe.limits]\ntimeout_s = 1\n\
             [recipe.normalise]\ncrlf_to_lf = true\ntrim = \"\"\nscratch_token = \"%S%\"\n\
             [sapi]\nfpm = {{ path = \"{cellar}/sbin/php-fpm\", md5 = \"0\" }}\n"
        ))
    }

    #[test]
    fn a_formula_is_php_or_one_line_of_it_as_homebrew_writes_it() {
        assert_eq!("php".parse::<Formula>().unwrap(), Formula::Newest);
        let line = "php@8.5".parse::<Formula>().unwrap();
        assert_eq!(line, Formula::Line { major: 8, minor: 5 });
        assert_eq!(line.to_string(), "php@8.5");
        assert_eq!(Formula::default().to_string(), "php");
        for name in [
            "",
            "PHP",
            "php@",
            "php@8",
            "php@8.5.7",
            "php@+8.5",
            "php@8.05",
            "php@8.x",
            "shivammathur/php/php@8.5",
            "php-cgi",
        ] {
            let err = name.parse::<Formula>().unwrap_err().to_string();
            assert!(err.contains("[oracle].formula"), "{name}: {err}");
        }
    }

    #[test]
    fn a_pin_that_names_no_formula_is_poured_from_php() {
        let pin = pin(None, "8.5.1", "php").unwrap();
        assert_eq!(pin.oracle.formula, Formula::Newest);
        assert_eq!(pin.defects(), &[] as &[String]);
        let named = self::pin(Some("php"), "8.5.1", "php").unwrap();
        assert_eq!(named.oracle.formula, Formula::Newest);
        assert_eq!(named.defects(), &[] as &[String]);
    }

    #[test]
    fn a_name_that_is_no_formula_of_php_does_not_parse() {
        let err = pin(Some("php@8.5.2"), "8.5.2", "php").unwrap_err();
        assert!(format!("{err:#}").contains("\"php@8.5.2\" is no formula"));
    }

    #[test]
    fn the_binaries_stand_in_the_keg_of_the_formula_the_pin_names() {
        let line = Formula::Line { major: 8, minor: 5 };
        let pin = pin(Some("php@8.5"), "8.5.2", "php@8.5").unwrap();
        assert_eq!(pin.oracle.formula, line);
        assert_eq!(pin.defects(), &[] as &[String]);
        // Each binary in another formula's keg is named by its key, with
        // both kegs: php's under the line's formula, and the line's under
        // php, named or not.
        for (formula, keg) in [
            (Some("php@8.5"), "php"),
            (Some("php"), "php@8.5"),
            (None, "php@8.5"),
        ] {
            let pin = self::pin(formula, "8.5.2", keg).unwrap();
            let [php, fpm] = pin.defects() else {
                panic!("{formula:?} over {keg}: {:?}", pin.defects());
            };
            let formula = formula.unwrap_or("php");
            let stands = format!(
                "[oracle].path /opt/homebrew/Cellar/{keg}/8.5.2/bin/php stands in the keg {keg}, and the pin is poured from the formula {formula},"
            );
            assert!(php.starts_with(&stands), "{php}");
            let stands = format!(
                "[sapi].fpm.path /opt/homebrew/Cellar/{keg}/8.5.2/sbin/php-fpm stands in the keg {keg},"
            );
            assert!(fpm.starts_with(&stands), "{fpm}");
        }
    }

    #[test]
    fn the_formula_of_one_line_is_the_line_of_the_pinned_version() {
        let pin = pin(Some("php@8.6"), "8.5.2", "php@8.6").unwrap();
        let [defect] = pin.defects() else {
            panic!("{:?}", pin.defects());
        };
        assert!(
            defect.starts_with(
                "[oracle].formula is php@8.6, the formula of another line than [oracle].version, 8.5.2"
            ),
            "{defect}"
        );
        let line = Formula::Line { major: 8, minor: 5 };
        assert!(line.pours("8.5.0") && !line.pours("8.50.1") && !line.pours("18.5.1"));
        assert!(Formula::Newest.pours("8.4.1") && Formula::Newest.pours("8.5.1"));
    }

    #[test]
    fn a_binary_under_no_cellar_stands_in_no_keg() {
        let binaries = [("[oracle].path", Path::new("/opt/dax-php/oracle/bin/php"))];
        let line = Formula::Line { major: 8, minor: 5 };
        assert_eq!(line.held("8.5.2", &binaries), Vec::<String>::new());
        let built = Binary {
            version: "8.5.2".to_owned(),
            path: binaries[0].1.to_owned(),
            ..Binary::default()
        };
        assert_eq!(built.bottle(), "8.5.2");
    }

    /// The bottle a runner pours is the keg the path names, the formula's
    /// revision with it (#4383): `[oracle].version` says php's version and
    /// no revision, and Homebrew bottles a version again under
    /// `<version>_<revision>`, in a commit whose message names it so,
    /// `php: update 8.5.2_1 bottle.`
    #[test]
    fn the_bottle_is_the_keg_the_path_names_with_its_revision() {
        let pin = pin(Some("php@8.5"), "8.5.2", "php@8.5").unwrap();
        assert_eq!(pin.oracle.bottle(), "8.5.2");
        let revised = self::pin(Some("php@8.5"), "8.5.2", "php@8.5/8.5.2_1").unwrap();
        assert_eq!(revised.defects(), &[] as &[String]);
        assert_eq!(revised.oracle.bottle(), "8.5.2_1");
        assert_eq!(revised.oracle.version, "8.5.2");
    }

    /// The negative control: a keg that is no bottle of the pinned
    /// version, the version before it or a revision Homebrew does not
    /// write, is a defect for each binary, naming its key, its path, the
    /// keg and both versions.
    #[test]
    fn a_keg_at_another_version_than_the_pins_is_no_bottle_of_it() {
        for bottle in ["8.5.1", "8.5.1_1", "8.5.20", "8.5.2_0", "8.5.2_01"] {
            let keg = format!("php@8.5/{bottle}");
            let pin = pin(Some("php@8.5"), "8.5.2", &keg).unwrap();
            let [php, fpm] = pin.defects() else {
                panic!("{bottle}: {:?}", pin.defects());
            };
            let stands = format!(
                "[oracle].path /opt/homebrew/Cellar/php@8.5/{bottle}/bin/php stands in the keg php@8.5 at {bottle}, which is no bottle of [oracle].version, 8.5.2:"
            );
            assert!(php.starts_with(&stands), "{php}");
            let stands = format!(
                "[sapi].fpm.path /opt/homebrew/Cellar/php@8.5/{bottle}/sbin/php-fpm stands in the keg php@8.5 at {bottle},"
            );
            assert!(fpm.starts_with(&stands), "{fpm}");
        }
        for (bottle, holds) in [
            ("8.5.2", true),
            ("8.5.2_1", true),
            ("8.5.2_12", true),
            ("8.5.2_", false),
            ("8.5.2_+1", false),
            ("8.5.2-1", false),
            ("8.5", false),
            ("", false),
        ] {
            assert_eq!(bottles("8.5.2", bottle), holds, "{bottle}");
        }
    }

    /// The formula is a value of the pin as any other, and no provenance
    /// of a build (#2969): a pin that names it has another digest than
    /// one that leaves it to the default, so a reading and a kept run
    /// under the recipe are stale across the key's first writing.
    #[test]
    fn naming_the_formula_moves_the_pins_digest() {
        let of = |formula: Option<&str>| pin(formula, "8.5.1", "php").unwrap().digest;
        assert_ne!(of(None), of(Some("php")));
        assert_eq!(of(Some("php")), of(Some("php")));
    }
}
