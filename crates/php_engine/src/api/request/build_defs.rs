//! `main/build-defs.h` and `php_build_date`: the facts of the build an
//! engine is, stamped once by the crate's `build.rs` and read here by
//! every host's `-v`, by `PHP_BUILD_DATE` and `PHP_BUILD_PROVIDER`, and by
//! phpinfo's rows, so the binary gives one answer to each. Beside php's
//! facts stand the product's: its name and its version, which `-v` and
//! phpinfo print beside php's version (as Mitchell ruled 2026-11-21).

/// The Unix seconds the build was stamped at (`build.rs`).
const EPOCH: &str = env!("PHP_BUILD_EPOCH");

/// `PHP_BUILD_PROVIDER`, where the environment named one at build, which
/// the constant is declared with.
#[cfg(php_build_provider)]
pub(crate) const PROVIDER: Option<&str> = Some(env!("PHP_BUILD_PROVIDER"));
#[cfg(not(php_build_provider))]
pub(crate) const PROVIDER: Option<&str> = None;

/// What one build of the engine is, as php's `configure` and `main.c`
/// fix it for a build of php: its date, the system it was built for, who
/// built it where anyone said, and the `configure` switches that would
/// build the bundle it carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildDefs {
    date: String,
    configure: String,
    product: String,
}

impl BuildDefs {
    /// This build's facts, with `switches` as its `configure` line, one
    /// argument each, php's spelling (`--enable-mbstring`).
    pub(crate) fn of(switches: &[&str]) -> BuildDefs {
        BuildDefs {
            date: build_date(),
            configure: configure(switches),
            product: titled(crate::PRODUCT_NAME),
        }
    }

    /// The product's name as a sentence writes it, [`crate::PRODUCT_NAME`]
    /// with its first letter upper case: `Lela`, which `-v`'s lines and
    /// phpinfo's licence box name.
    #[must_use]
    pub fn product(&self) -> &str {
        &self.product
    }

    /// The command that names the product, the CLI's binary
    /// ([`crate::PRODUCT_NAME`]): `lela`, as `-v` and phpinfo write
    /// `lela --licenses`.
    #[must_use]
    pub fn product_command(&self) -> &'static str {
        crate::PRODUCT_NAME
    }

    /// The product's version, the workspace's `[workspace.package]`
    /// version, which every host inherits and this crate does too:
    /// `0.1.0-alpha.1`, what `-v`'s first line carries after php's and
    /// phpinfo's `Lela Version` row.
    #[must_use]
    pub fn product_version(&self) -> &'static str {
        env!("CARGO_PKG_VERSION")
    }

    /// `php_build_date`, `__DATE__ " " __TIME__` of the build as
    /// `Jul 28 2026 13:06:52`: what `-v` prints after `built:`, what
    /// `PHP_BUILD_DATE` is and phpinfo's `Build Date`.
    #[must_use]
    pub fn date(&self) -> &str {
        &self.date
    }

    /// `PHP_BUILD_SYSTEM`, phpinfo's `Build System`: the target triple
    /// the engine was built for, `aarch64-apple-darwin`.
    #[must_use]
    pub fn system(&self) -> &'static str {
        env!("PHP_BUILD_SYSTEM")
    }

    /// `PHP_BUILD_PROVIDER`: who built the binary, where the build named
    /// one, `-v`'s `Built by` line and phpinfo's `Build Provider`; none
    /// otherwise, when the constant is not declared either.
    #[must_use]
    pub fn provider(&self) -> Option<&'static str> {
        PROVIDER
    }

    /// `CONFIGURE_COMMAND`, phpinfo's `Configure Command`: the switches
    /// the host's bundle names, each in `configure`'s quotes. No
    /// `./configure` stands first, since none ran.
    #[must_use]
    pub fn configure(&self) -> &str {
        &self.configure
    }
}

/// `name` as a sentence writes it, its first letter upper case.
fn titled(name: &str) -> String {
    let mut chars = name.chars();
    chars
        .next()
        .map(|first| first.to_uppercase().chain(chars).collect())
        .unwrap_or_default()
}

/// `CONFIGURE_COMMAND` as `configure` writes it: each argument in single
/// quotes, one space between.
fn configure(switches: &[&str]) -> String {
    switches
        .iter()
        .map(|switch| format!("'{switch}'"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// `__DATE__ " " __TIME__` of `seconds`: `Mmm dd yyyy hh:mm:ss`, the day
/// padded with a space as `__DATE__` pads it, in UTC, as a compiler
/// renders it under `SOURCE_DATE_EPOCH`.
fn date(seconds: i64) -> String {
    let (year, name, day, [h, m, s]) = super::zone::civil_parts(seconds);
    format!("{name} {day:>2} {year} {h:02}:{m:02}:{s:02}")
}

/// `php_build_date`, this build's ([`BuildDefs::date`]), which
/// `PHP_BUILD_DATE` is declared with.
pub(crate) fn build_date() -> String {
    date(EPOCH.parse().unwrap_or(0))
}

#[cfg(test)]
mod layout;

#[cfg(test)]
mod tests {
    use super::{configure, date, layout::layout, titled};

    #[test]
    fn the_products_name_is_titled_from_the_constant() {
        assert_eq!(titled("lela"), "Lela");
        assert_eq!(titled("x"), "X");
        assert_eq!(titled(""), "");
    }

    #[test]
    fn the_build_date_is_phps_date_and_time_in_utc() {
        assert_eq!(date(0), "Jan  1 1970 00:00:00");
        assert_eq!(date(1_785_243_412), "Jul 28 2026 12:56:52");
        assert_eq!(date(951_782_400), "Feb 29 2000 00:00:00");
    }

    #[test]
    fn the_configure_line_quotes_each_switch() {
        assert_eq!(configure(&[]), "");
        assert_eq!(
            configure(&["--enable-mbstring", "--with-openssl"]),
            "'--enable-mbstring' '--with-openssl'"
        );
    }

    /// What `./configure --disable-all` writes into `main/build-defs.h`
    /// at the pin, and with `--prefix=/opt/x`: every value follows the
    /// prefix, and php-fpm's default configuration is
    /// `$sysconfdir/php-fpm.conf`.
    #[test]
    fn the_layout_is_configures_with_no_option_and_follows_the_prefix() {
        let at = |prefix: &str| {
            let named = |name: &str| {
                (name == "PHP_PREFIX" && !prefix.is_empty()).then(|| prefix.to_owned())
            };
            layout(&named)
        };
        let value = |layout: &[(&str, String)], name: &str| {
            layout
                .iter()
                .find(|(each, _)| *each == name)
                .unwrap_or_else(|| panic!("{name} is in the layout"))
                .1
                .clone()
        };
        for (prefix, root) in [("", "/usr/local"), ("/opt/x", "/opt/x")] {
            let layout = at(prefix);
            let expected = [
                ("PEAR_INSTALL_DIR", String::new()),
                ("PHP_INCLUDE_PATH", ".:".to_owned()),
                (
                    "PHP_EXTENSION_DIR",
                    format!("{root}/lib/php/extensions/no-debug-non-zts-20250925"),
                ),
                ("PHP_PREFIX", root.to_owned()),
                ("PHP_BINDIR", format!("{root}/bin")),
                ("PHP_SBINDIR", format!("{root}/sbin")),
                ("PHP_MANDIR", format!("{root}/php/man")),
                ("PHP_LIBDIR", format!("{root}/lib/php")),
                ("PHP_DATADIR", format!("{root}/share/php")),
                ("PHP_SYSCONFDIR", format!("{root}/etc")),
                ("PHP_LOCALSTATEDIR", format!("{root}/var")),
                ("PHP_CONFIG_FILE_PATH", format!("{root}/lib")),
                ("PHP_CONFIG_FILE_SCAN_DIR", String::new()),
            ];
            assert_eq!(layout.len(), expected.len());
            for (name, want) in expected {
                assert_eq!(value(&layout, name), want, "{name} under {root}");
            }
        }
        assert_eq!(
            format!("{}/php-fpm.conf", value(&at("/opt/x"), "PHP_SYSCONFDIR")),
            "/opt/x/etc/php-fpm.conf"
        );
    }

    #[test]
    fn a_named_value_is_taken_as_it_is_named() {
        let named = |name: &str| match name {
            "PHP_PREFIX" => Some("/p".to_owned()),
            "PHP_LIBDIR" => Some("/l".to_owned()),
            "PEAR_INSTALL_DIR" => Some("/pear".to_owned()),
            "PHP_CONFIG_FILE_SCAN_DIR" => Some("/p/conf.d".to_owned()),
            _ => None,
        };
        let layout = layout(&named);
        let get = |name: &str| {
            layout
                .iter()
                .find(|(each, _)| *each == name)
                .map(|(_, v)| v.as_str())
        };
        assert_eq!(
            get("PHP_EXTENSION_DIR"),
            Some("/l/extensions/no-debug-non-zts-20250925")
        );
        assert_eq!(get("PHP_INCLUDE_PATH"), Some(".:/pear"));
        assert_eq!(get("PHP_CONFIG_FILE_SCAN_DIR"), Some("/p/conf.d"));
        assert_eq!(get("PHP_SYSCONFDIR"), Some("/p/etc"));
    }
}
