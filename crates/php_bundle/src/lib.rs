//! The bundle (ADR-0004; design §4.1, §4.10, §7.1): the enabled set and
//! the tier's name, php-src's `internal_functions.c`. Data and nothing
//! else: `enabled()` is the extensions behind the crate's features in
//! startup order, php's registration order sorted as
//! `zend_sort_modules` sorts it, each feature switching on the features
//! of what its extension requires, which `api-check` holds; `Tier` is
//! the one alias the binary binds. The engine links whatever it is
//! handed; the tier is not in `enabled()` and is absent from
//! `get_loaded_extensions()`, as php's compiled tier is.
#![forbid(unsafe_code)]
// seams.toml rule lints.no-panic: Internal invariant failures are InternalError fatals with a message, not panics (design §10).
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::todo,
    clippy::unimplemented
)]

use php_ext::abi::Extension;

#[cfg(doc)]
mod handles;
#[cfg(doctest)]
mod refusals;

/// The extensions in php's registration order for the oracle's enabled
/// set, one `extension()` per feature that is on: the order
/// `internal_functions.c` lists them in, which configure writes through
/// `order_by_dep.awk` (`Z/build/genif.sh`) from the `config0.m4`, then
/// `config.m4`, then `config9.m4` of each directory in name order, each
/// extension after what its `PHP_ADD_EXTENSION_DEP` names. The engine
/// fills the function table in this order and then sorts it as
/// `zend_sort_modules` does over each extension's declared
/// dependencies, the order it starts, activates and lists them in; the
/// linker orders classes from their declarations, so the order matters
/// for the lifecycle phases and the four listings, which this witness
/// holds to the oracle for the bundled set, each listing as the
/// extensions it names in the order it first names them, past the
/// extensions the pinned build has and the bundle has not, `pcntl`,
/// which the harness's engine leaves out, and `zend_test`, which it
/// carries and the pinned build does not:
///
/// ```php
/// $order = function (iterable $names): string { $absent = ['ldap', 'odbc', 'pdo_dblib', 'pdo_odbc', 'pdo_pgsql', 'pgsql', 'snmp', 'pcntl', 'zend_test']; $seen = []; foreach ($names as $name) { $name = strtolower($name); if (!in_array($name, $absent, true)) { $seen[$name] = true; } } return implode(' ', array_keys($seen)) . "\n"; };
/// echo $order(get_loaded_extensions());
/// echo $order(array_map(fn ($f) => (new ReflectionFunction($f))->getExtensionName(), get_defined_functions()['internal']));
/// echo $order(array_map(fn ($c) => (new ReflectionClass($c))->getExtensionName(), get_declared_classes()));
/// echo $order(array_keys(get_defined_constants(true)));
/// ```
/// ```expect
/// core date lexbor openssl pcre sqlite3 zlib bcmath bz2 calendar ctype curl dba uri json mbstring spl ffi fileinfo filter ftp gd gettext gmp hash iconv intl session standard libxml mysqlnd mysqli zend opcache pdo pdo_mysql pdo_sqlite phar posix random readline reflection exif shmop simplexml soap sockets sodium sysvmsg sysvsem sysvshm tidy tokenizer dom xml xmlreader xmlwriter xsl zip
/// core date libxml openssl pcre zlib bcmath bz2 calendar ctype curl dba dom mbstring exif fileinfo filter ftp gd gettext gmp hash iconv intl json standard spl mysqli zend opcache pdo posix random readline session shmop simplexml soap sockets sodium sysvmsg sysvsem sysvshm tidy tokenizer xml xmlwriter zip
/// core date openssl sqlite3 zlib bcmath curl dba uri json spl ffi fileinfo filter ftp gd gmp hash intl session standard libxml mysqli pdo pdo_mysql pdo_sqlite phar random reflection shmop simplexml soap sockets sodium sysvmsg sysvsem sysvshm tidy tokenizer dom xml xmlreader xmlwriter xsl zip
/// core date openssl pcre sqlite3 zlib calendar curl json mbstring fileinfo filter ftp gd gmp hash iconv intl session standard libxml mysqli posix random readline exif soap sockets sodium sysvmsg tidy tokenizer dom xml xsl
/// ```
///
/// `xtask new-ext <name>` appends the line behind its feature, last,
/// and a person moves it to its registration place, which the witness
/// names until it is moved.
#[must_use]
#[expect(
    clippy::too_many_lines,
    reason = "a list of two lines per extension, as long as php's internal_functions.c"
)]
// Two lines an extension, as `new-ext` writes them; the list is the length
// of the bundle.
pub fn enabled() -> &'static [fn() -> Extension] {
    &[
        #[cfg(feature = "date")]
        php_ext_date::extension,
        #[cfg(feature = "libxml")]
        php_ext_libxml::extension,
        #[cfg(feature = "openssl")]
        php_ext_openssl::extension,
        #[cfg(feature = "pcre")]
        php_ext_pcre::extension,
        #[cfg(feature = "sqlite3")]
        php_ext_sqlite3::extension,
        #[cfg(feature = "zlib")]
        php_ext_zlib::extension,
        #[cfg(feature = "bcmath")]
        php_ext_bcmath::extension,
        #[cfg(feature = "bz2")]
        php_ext_bz2::extension,
        #[cfg(feature = "calendar")]
        php_ext_calendar::extension,
        #[cfg(feature = "ctype")]
        php_ext_ctype::extension,
        #[cfg(feature = "curl")]
        php_ext_curl::extension,
        #[cfg(feature = "dba")]
        php_ext_dba::extension,
        #[cfg(feature = "lexbor")]
        php_ext_lexbor::extension,
        #[cfg(feature = "dom")]
        php_ext_dom::extension,
        #[cfg(feature = "mbstring")]
        php_ext_mbstring::extension,
        #[cfg(feature = "exif")]
        php_ext_exif::extension,
        #[cfg(feature = "ffi")]
        php_ext_ffi::extension,
        #[cfg(feature = "fileinfo")]
        php_ext_fileinfo::extension,
        #[cfg(feature = "filter")]
        php_ext_filter::extension,
        #[cfg(feature = "ftp")]
        php_ext_ftp::extension,
        #[cfg(feature = "gd")]
        php_ext_gd::extension,
        #[cfg(feature = "gettext")]
        php_ext_gettext::extension,
        #[cfg(feature = "gmp")]
        php_ext_gmp::extension,
        #[cfg(feature = "hash")]
        php_ext_hash::extension,
        #[cfg(feature = "iconv")]
        php_ext_iconv::extension,
        #[cfg(feature = "intl")]
        php_ext_intl::extension,
        #[cfg(feature = "json")]
        php_ext_json::extension,
        #[cfg(feature = "standard")]
        php_ext_standard::extension,
        #[cfg(feature = "spl")]
        php_ext_spl::extension,
        #[cfg(feature = "mysqlnd")]
        php_ext_mysqlnd::extension,
        #[cfg(feature = "mysqli")]
        php_ext_mysqli::extension,
        #[cfg(feature = "opcache")]
        php_ext_opcache::extension,
        #[cfg(feature = "pcntl")]
        php_ext_pcntl::extension,
        #[cfg(feature = "pdo")]
        php_ext_pdo::extension,
        #[cfg(feature = "pdo_mysql")]
        php_ext_pdo_mysql::extension,
        #[cfg(feature = "pdo_sqlite")]
        php_ext_pdo_sqlite::extension,
        #[cfg(feature = "phar")]
        php_ext_phar::extension,
        #[cfg(feature = "posix")]
        php_ext_posix::extension,
        #[cfg(feature = "random")]
        php_ext_random::extension,
        #[cfg(feature = "readline")]
        php_ext_readline::extension,
        #[cfg(feature = "reflection")]
        php_ext_reflection::extension,
        #[cfg(feature = "session")]
        php_ext_session::extension,
        #[cfg(feature = "shmop")]
        php_ext_shmop::extension,
        #[cfg(feature = "simplexml")]
        php_ext_simplexml::extension,
        #[cfg(feature = "soap")]
        php_ext_soap::extension,
        #[cfg(feature = "sockets")]
        php_ext_sockets::extension,
        #[cfg(feature = "sodium")]
        php_ext_sodium::extension,
        #[cfg(feature = "sysvmsg")]
        php_ext_sysvmsg::extension,
        #[cfg(feature = "sysvsem")]
        php_ext_sysvsem::extension,
        #[cfg(feature = "sysvshm")]
        php_ext_sysvshm::extension,
        #[cfg(feature = "tidy")]
        php_ext_tidy::extension,
        #[cfg(feature = "tokenizer")]
        php_ext_tokenizer::extension,
        #[cfg(feature = "uri")]
        php_ext_uri::extension,
        #[cfg(feature = "xml")]
        php_ext_xml::extension,
        #[cfg(feature = "xmlreader")]
        php_ext_xmlreader::extension,
        #[cfg(feature = "xmlwriter")]
        php_ext_xmlwriter::extension,
        #[cfg(feature = "xsl")]
        php_ext_xsl::extension,
        #[cfg(feature = "zend_test")]
        php_ext_zend_test::extension,
        #[cfg(feature = "zip")]
        php_ext_zip::extension,
    ]
}

/// php's `configure` switches that build the bundle behind the features
/// that are on, as `CONFIGURE_COMMAND` names them and phpinfo prints
/// them: each extension php leaves out by default and the bundle carries,
/// in its `config.m4` spelling (`--enable-mbstring`, `--with-openssl`);
/// each php builds by default and the bundle leaves out, as its
/// `--disable` or `--without`; and nothing for the extensions php always
/// builds. By extension name, every switch behind its feature, which
/// `tests/configure.rs` holds to the features.
pub const CONFIGURE: &[&str] = &[
    #[cfg(feature = "bcmath")]
    "--enable-bcmath",
    #[cfg(feature = "bz2")]
    "--with-bz2",
    #[cfg(feature = "calendar")]
    "--enable-calendar",
    #[cfg(not(feature = "ctype"))]
    "--disable-ctype",
    #[cfg(feature = "curl")]
    "--with-curl",
    #[cfg(feature = "dba")]
    "--enable-dba",
    #[cfg(not(feature = "dom"))]
    "--disable-dom",
    #[cfg(feature = "exif")]
    "--enable-exif",
    #[cfg(feature = "ffi")]
    "--with-ffi",
    #[cfg(not(feature = "fileinfo"))]
    "--disable-fileinfo",
    #[cfg(not(feature = "filter"))]
    "--disable-filter",
    #[cfg(feature = "ftp")]
    "--enable-ftp",
    #[cfg(feature = "gd")]
    "--enable-gd",
    #[cfg(feature = "gd")]
    "--with-external-gd",
    #[cfg(feature = "gettext")]
    "--with-gettext",
    #[cfg(feature = "gmp")]
    "--with-gmp",
    #[cfg(not(feature = "iconv"))]
    "--without-iconv",
    #[cfg(feature = "intl")]
    "--enable-intl",
    #[cfg(not(feature = "libxml"))]
    "--without-libxml",
    #[cfg(feature = "mbstring")]
    "--enable-mbstring",
    #[cfg(feature = "mysqli")]
    "--with-mysqli",
    #[cfg(feature = "mysqlnd")]
    "--enable-mysqlnd",
    #[cfg(feature = "openssl")]
    "--with-openssl",
    #[cfg(feature = "pcntl")]
    "--enable-pcntl",
    #[cfg(not(feature = "pdo"))]
    "--disable-pdo",
    #[cfg(feature = "pdo_mysql")]
    "--with-pdo-mysql",
    #[cfg(not(feature = "pdo_sqlite"))]
    "--without-pdo-sqlite",
    #[cfg(not(feature = "phar"))]
    "--disable-phar",
    #[cfg(not(feature = "posix"))]
    "--disable-posix",
    #[cfg(feature = "readline")]
    "--with-libedit",
    #[cfg(not(feature = "session"))]
    "--disable-session",
    #[cfg(feature = "shmop")]
    "--enable-shmop",
    #[cfg(not(feature = "simplexml"))]
    "--disable-simplexml",
    #[cfg(feature = "soap")]
    "--enable-soap",
    #[cfg(feature = "sockets")]
    "--enable-sockets",
    #[cfg(feature = "sodium")]
    "--with-sodium",
    #[cfg(not(feature = "sqlite3"))]
    "--without-sqlite3",
    #[cfg(feature = "sysvmsg")]
    "--enable-sysvmsg",
    #[cfg(feature = "sysvsem")]
    "--enable-sysvsem",
    #[cfg(feature = "sysvshm")]
    "--enable-sysvshm",
    #[cfg(feature = "tidy")]
    "--with-tidy",
    #[cfg(not(feature = "tokenizer"))]
    "--disable-tokenizer",
    #[cfg(not(feature = "xml"))]
    "--disable-xml",
    #[cfg(not(feature = "xmlreader"))]
    "--disable-xmlreader",
    #[cfg(not(feature = "xmlwriter"))]
    "--disable-xmlwriter",
    #[cfg(feature = "xsl")]
    "--with-xsl",
    #[cfg(feature = "zend_test")]
    "--enable-zend-test",
    #[cfg(feature = "zip")]
    "--with-zip",
    #[cfg(feature = "zlib")]
    "--with-zlib",
];

/// How a host serves its requests, which decides whether the extensions
/// that act on the process itself are in its set (design §7.2, revision
/// 35).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Serving {
    /// One request at a time on its process's one thread: the CLI, CGI,
    /// php-fpm's children and phpdbg, where `pcntl` and `posix` may fork,
    /// take signals and set the process's ids.
    OneThread,
    /// Requests on threads of one process, as the production server serves
    /// them: never `pcntl` nor `posix`, whose `fork()` would copy one
    /// thread of a process whose other threads are in the middle of their
    /// requests.
    Threads,
}

/// The extensions that act on the process itself, behind their features.
const PROCESS: &[fn() -> Extension] = &[
    #[cfg(feature = "pcntl")]
    php_ext_pcntl::extension,
    #[cfg(feature = "posix")]
    php_ext_posix::extension,
];

/// The enabled set for a host that serves as `serving` says: [`enabled()`]
/// whole for one thread, and on threads without `pcntl` and `posix`,
/// whichever features the build turned on. One build of the workspace
/// turns on the features every host names, so a host on threads cannot
/// leave them out by its own manifest alone.
#[must_use]
pub fn enabled_for(serving: Serving) -> Vec<fn() -> Extension> {
    let carried = |start: &&fn() -> Extension| {
        serving == Serving::OneThread
            || !PROCESS
                .iter()
                .any(|process| std::ptr::fn_addr_eq(*process, **start))
    };
    enabled().iter().filter(carried).copied().collect()
}

/// [`CONFIGURE`] for a host that serves as `serving` says: on threads
/// without `--enable-pcntl` and with `--disable-posix`, as php's configure
/// would have built the set [`enabled_for`] hands it.
#[must_use]
pub fn configure_for(serving: Serving) -> &'static [&'static str] {
    static THREADS: std::sync::OnceLock<Vec<&'static str>> = std::sync::OnceLock::new();
    if serving == Serving::OneThread {
        return CONFIGURE;
    }
    THREADS.get_or_init(|| {
        let mut switches: Vec<&'static str> = (CONFIGURE.iter().copied())
            .filter(|switch| *switch != "--enable-pcntl")
            .collect();
        if !switches.contains(&"--disable-posix") {
            switches.push("--disable-posix");
        }
        switches
    })
}

/// The tier, named once (design §4.3 seam 8; `jit-design.md` §6): the
/// compiled tier where the `jit` feature is on and the target is one of
/// the four it is built for. Never in [`enabled()`].
#[cfg(all(
    feature = "jit",
    any(target_os = "linux", target_os = "macos"),
    any(target_arch = "x86_64", target_arch = "aarch64")
))]
pub type Tier = php_jit::Jit;
/// The tier, named once (design §4.3 seam 8; `jit-design.md` §6): the
/// interpreter alone, without the `jit` feature or off the four targets
/// the compiled tier is built for. Never in [`enabled()`].
#[cfg(not(all(
    feature = "jit",
    any(target_os = "linux", target_os = "macos"),
    any(target_arch = "x86_64", target_arch = "aarch64")
)))]
pub type Tier = php_ext::Nop;

/// The in-process host (design §4.3 seam 4, §8; `CONTEXT.md`, Host): a fresh
/// [`Engine`] of [`enabled()`] with [`Tier`] bound, served through the
/// `Capture` sink. Behind the `test` feature, so the crate's own surface
/// stays the enabled set and the tier's name; what turns the feature on is
/// an extension's own tests and the harness's witness lane, which serves a
/// snippet here where the binary is not built.
#[cfg(feature = "test")]
pub mod test {
    pub use php_ext::test::Entered;
    pub use php_ext::test::{Capture, Engine};

    /// A fresh engine of [`enabled()`](super::enabled), every extension in
    /// startup order and [`Tier`](super::Tier) bound, with `ini` laid over
    /// the engine's own defaults as a host's `-d` lays it: one `(name,
    /// value)` pair each, the last of a name winning; and compiling with
    /// extended info where `extended_info` says so, as a host's `-e`
    /// builds it. It is built for php's CLI, whose requests the lanes
    /// describe, so `PHP_SAPI` is the one name its cache substitutes.
    #[must_use]
    pub fn engine(ini: &[(&str, &str)], extended_info: bool) -> Engine {
        let extensions: Vec<_> = super::enabled().iter().map(|start| start()).collect();
        let mut builder = Engine::builder()
            .sapi_name("cli")
            .extensions(extensions)
            .configure(super::CONFIGURE)
            .tier::<super::Tier>()
            .extended_info(extended_info);
        for (name, value) in ini {
            builder = builder.ini(name, value);
        }
        builder.build()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// On threads the set is the enabled set less the extensions that act
    /// on the process, whichever of them the build carries; on one thread
    /// it is the enabled set whole, and so is its configure line.
    #[test]
    fn a_host_on_threads_carries_no_extension_that_acts_on_the_process() {
        let threads = enabled_for(Serving::Threads);
        assert_eq!(threads.len(), enabled().len() - PROCESS.len());
        assert!(!threads.iter().any(|start| {
            PROCESS
                .iter()
                .any(|process| std::ptr::fn_addr_eq(*process, *start))
        }));
        assert_eq!(enabled_for(Serving::OneThread).len(), enabled().len());
        assert_eq!(configure_for(Serving::OneThread), CONFIGURE);
        let on_threads = configure_for(Serving::Threads);
        assert!(!on_threads.contains(&"--enable-pcntl"));
        assert!(on_threads.contains(&"--disable-posix"));
    }

    /// The `jit` feature binds the compiled tier on the four targets, and
    /// every other build the interpreter alone (`jit-design.md` §6).
    #[test]
    fn the_tier_is_the_compiled_one_where_the_feature_and_the_target_are() {
        use std::any::TypeId;
        let compiled = cfg!(all(
            feature = "jit",
            any(target_os = "linux", target_os = "macos"),
            any(target_arch = "x86_64", target_arch = "aarch64")
        ));
        assert_eq!(
            TypeId::of::<Tier>() == TypeId::of::<php_ext::Nop>(),
            !compiled
        );
    }
}
