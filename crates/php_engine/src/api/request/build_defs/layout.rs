//! The install layout configure writes into php's `main/build-defs.h`,
//! read from the build's environment, one variable under each macro's
//! name, and where one is unset what `Z/configure.ac` computes with no
//! option given (design revision 60): the prefix first, then what it
//! derives from the prefix and from the directories before it, so a
//! packager who sets `PHP_PREFIX` alone moves every other value with it.
//! `build.rs` stamps what this gives, and the crate's test holds it.

/// `ZEND_MODULE_API_NO` (`Z/Zend/zend_modules.h:34`), the extension
/// directory's last part.
const MODULE_API_NO: &str = "20250925";

/// Each macro's name with its value, `var`'s where it names one.
/// `PEAR_EXTENSION_DIR` is none of them, being `PHP_EXTENSION_DIR`
/// (`Z/main/main_arginfo.h:22`).
pub(crate) fn layout(var: &dyn Fn(&str) -> Option<String>) -> Vec<(&'static str, String)> {
    let get = |name: &str, default: String| var(name).unwrap_or(default);
    // `prefix=/usr/local` and `exec_prefix='${prefix}'` (`:1326-1327`).
    let prefix = get("PHP_PREFIX", "/usr/local".to_owned());
    // `libdir` and `datadir` each with `/php` added (`:1334-1340`).
    let libdir = get("PHP_LIBDIR", format!("{prefix}/lib/php"));
    let datadir = get("PHP_DATADIR", format!("{prefix}/share/php"));
    // The non-GNU layout's `EXTENSION_DIR`, in a release, non-ZTS build
    // (`:1358-1364`).
    let extension_dir = get(
        "PHP_EXTENSION_DIR",
        format!("{libdir}/extensions/no-debug-non-zts-{MODULE_API_NO}"),
    );
    // `PEAR_INSTALLDIR` is set only under `--with-pear`, which is off
    // with no option (`:1154-1176`), so `INCLUDE_PATH` is `.:` (`:1388`).
    let pear = get("PEAR_INSTALL_DIR", String::new());
    let include_path = get("PHP_INCLUDE_PATH", format!(".:{pear}"));
    vec![
        ("PEAR_INSTALL_DIR", pear),
        ("PHP_INCLUDE_PATH", include_path),
        ("PHP_EXTENSION_DIR", extension_dir),
        ("PHP_BINDIR", get("PHP_BINDIR", format!("{prefix}/bin"))),
        ("PHP_SBINDIR", get("PHP_SBINDIR", format!("{prefix}/sbin"))),
        // `mandir` is autoconf's `${datarootdir}/man`, and the non-GNU
        // layout's `datarootdir` is `$prefix/php` (`:1372-1374`).
        ("PHP_MANDIR", get("PHP_MANDIR", format!("{prefix}/php/man"))),
        ("PHP_LIBDIR", libdir),
        ("PHP_DATADIR", datadir),
        (
            "PHP_SYSCONFDIR",
            get("PHP_SYSCONFDIR", format!("{prefix}/etc")),
        ),
        (
            "PHP_LOCALSTATEDIR",
            get("PHP_LOCALSTATEDIR", format!("{prefix}/var")),
        ),
        // `$libdir` before `/php` is added to it (`:905-908`).
        (
            "PHP_CONFIG_FILE_PATH",
            get("PHP_CONFIG_FILE_PATH", format!("{prefix}/lib")),
        ),
        // No scan directory (`:917`).
        (
            "PHP_CONFIG_FILE_SCAN_DIR",
            get("PHP_CONFIG_FILE_SCAN_DIR", String::new()),
        ),
        ("PHP_PREFIX", prefix),
    ]
}
