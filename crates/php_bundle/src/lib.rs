//! The bundle (ADR-0004; design §4.1, §4.10, §7.1): the enabled set and
//! the tier's name, php-src's `internal_functions.c`. Data and nothing
//! else: `enabled()` is the extensions behind the crate's features in
//! startup order, a topological order of the `ext/*` graph that
//! `api-check` holds; `Tier` is the one alias the binary binds. The
//! engine links whatever it is handed; the tier is not in `enabled()`
//! and is absent from `get_loaded_extensions()`, as php's compiled tier
//! is.
#![forbid(unsafe_code)]

use php_ext::abi::Extension;

#[cfg(doctest)]
mod refusals;

/// The extensions in startup order, one `extension()` per feature that
/// is on. `xtask new-ext <name>` appends the line behind its feature,
/// last; a person moves it when the crate declares an edge, and
/// `api-check` names both crates until it is moved.
#[must_use]
pub fn enabled() -> &'static [fn() -> Extension] {
    &[
        #[cfg(feature = "date")]
        php_ext_date::extension,
        #[cfg(feature = "random")]
        php_ext_random::extension,
        #[cfg(feature = "lexbor")]
        php_ext_lexbor::extension,
        #[cfg(feature = "uri")]
        php_ext_uri::extension,
        #[cfg(feature = "standard")]
        php_ext_standard::extension,
        #[cfg(feature = "pcre")]
        php_ext_pcre::extension,
        #[cfg(feature = "spl")]
        php_ext_spl::extension,
        #[cfg(feature = "reflection")]
        php_ext_reflection::extension,
        #[cfg(feature = "mbstring")]
        php_ext_mbstring::extension,
        #[cfg(feature = "session")]
        php_ext_session::extension,
        #[cfg(feature = "pdo")]
        php_ext_pdo::extension,
        #[cfg(feature = "libxml")]
        php_ext_libxml::extension,
        #[cfg(feature = "dom")]
        php_ext_dom::extension,
        #[cfg(feature = "mysqlnd")]
        php_ext_mysqlnd::extension,
        #[cfg(feature = "pdo_mysql")]
        php_ext_pdo_mysql::extension,
        #[cfg(feature = "filter")]
        php_ext_filter::extension,
        #[cfg(feature = "tokenizer")]
        php_ext_tokenizer::extension,
        #[cfg(feature = "json")]
        php_ext_json::extension,
        #[cfg(feature = "openssl")]
        php_ext_openssl::extension,
        #[cfg(feature = "curl")]
        php_ext_curl::extension,
        #[cfg(feature = "iconv")]
        php_ext_iconv::extension,
        #[cfg(feature = "pdo_sqlite")]
        php_ext_pdo_sqlite::extension,
        #[cfg(feature = "ctype")]
        php_ext_ctype::extension,
        #[cfg(feature = "hash")]
        php_ext_hash::extension,
        #[cfg(feature = "zlib")]
        php_ext_zlib::extension,
        #[cfg(feature = "pcntl")]
        php_ext_pcntl::extension,
        #[cfg(feature = "posix")]
        php_ext_posix::extension,
        #[cfg(feature = "sockets")]
        php_ext_sockets::extension,
        #[cfg(feature = "phar")]
        php_ext_phar::extension,
        #[cfg(feature = "bz2")]
        php_ext_bz2::extension,
        #[cfg(feature = "opcache")]
        php_ext_opcache::extension,
        #[cfg(feature = "bcmath")]
        php_ext_bcmath::extension,
        #[cfg(feature = "simplexml")]
        php_ext_simplexml::extension,
        #[cfg(feature = "calendar")]
        php_ext_calendar::extension,
        #[cfg(feature = "exif")]
        php_ext_exif::extension,
        #[cfg(feature = "shmop")]
        php_ext_shmop::extension,
        #[cfg(feature = "sysvsem")]
        php_ext_sysvsem::extension,
        #[cfg(feature = "sysvmsg")]
        php_ext_sysvmsg::extension,
        #[cfg(feature = "sysvshm")]
        php_ext_sysvshm::extension,
        #[cfg(feature = "ftp")]
        php_ext_ftp::extension,
        #[cfg(feature = "xml")]
        php_ext_xml::extension,
        #[cfg(feature = "xmlreader")]
        php_ext_xmlreader::extension,
        #[cfg(feature = "xmlwriter")]
        php_ext_xmlwriter::extension,
        #[cfg(feature = "mysqli")]
        php_ext_mysqli::extension,
        #[cfg(feature = "sqlite3")]
        php_ext_sqlite3::extension,
        #[cfg(feature = "fileinfo")]
        php_ext_fileinfo::extension,
        #[cfg(feature = "dba")]
        php_ext_dba::extension,
    ]
}

/// php's `configure` switches that build the bundle behind the features
/// that are on, as `CONFIGURE_COMMAND` names them and phpinfo prints
/// them: each extension php leaves out by default and the bundle carries,
/// in its `config.m4` spelling (`--enable-mbstring`, `--with-openssl`);
/// each php builds by default and the bundle leaves out, as its
/// `--disable` or `--without`, the four no crate holds yet among them;
/// and nothing for the extensions php always builds. By extension name.
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
    #[cfg(not(feature = "fileinfo"))]
    "--disable-fileinfo",
    #[cfg(not(feature = "filter"))]
    "--disable-filter",
    #[cfg(not(feature = "iconv"))]
    "--without-iconv",
    #[cfg(not(feature = "libxml"))]
    "--without-libxml",
    #[cfg(feature = "mbstring")]
    "--enable-mbstring",
    #[cfg(feature = "openssl")]
    "--with-openssl",
    #[cfg(feature = "pcntl")]
    "--enable-pcntl",
    #[cfg(not(feature = "pdo"))]
    "--disable-pdo",
    #[cfg(feature = "mysqli")]
    "--with-mysqli",
    #[cfg(feature = "pdo_mysql")]
    "--with-pdo-mysql",
    #[cfg(not(feature = "pdo_sqlite"))]
    "--without-pdo-sqlite",
    #[cfg(not(feature = "phar"))]
    "--disable-phar",
    #[cfg(not(feature = "posix"))]
    "--disable-posix",
    #[cfg(not(feature = "session"))]
    "--disable-session",
    #[cfg(not(feature = "simplexml"))]
    "--disable-simplexml",
    #[cfg(feature = "sockets")]
    "--enable-sockets",
    "--without-sqlite3",
    #[cfg(not(feature = "tokenizer"))]
    "--disable-tokenizer",
    "--disable-xml",
    "--disable-xmlreader",
    "--disable-xmlwriter",
    #[cfg(feature = "zlib")]
    "--with-zlib",
];

/// The tier, named once (design §4.3 seam 8): the interpreter alone until
/// the `jit` feature binds the compiled tier. Never in [`enabled()`].
pub type Tier = php_ext::Nop;

/// The in-process host (design §4.3 seam 4, §8; `CONTEXT.md`, Host): a fresh
/// [`Engine`] of [`enabled()`] with [`Tier`] bound, served through the
/// `Capture` sink. Behind the `test` feature, so the crate's own surface
/// stays the enabled set and the tier's name; what turns the feature on is
/// an extension's own tests and the harness's witness lane, which serves a
/// snippet here where the binary is not built.
#[cfg(feature = "test")]
pub mod test {
    #[cfg(feature = "witness-trace")]
    pub use php_ext::test::Entered;
    pub use php_ext::test::{Capture, Engine};

    /// A fresh engine of [`enabled()`](super::enabled), every extension in
    /// startup order and [`Tier`](super::Tier) bound, with `ini` laid over
    /// the engine's own defaults as a host's `-d` lays it: one `(name,
    /// value)` pair each, the last of a name winning.
    #[must_use]
    pub fn engine(ini: &[(&str, &str)]) -> Engine {
        let extensions: Vec<_> = super::enabled().iter().map(|start| start()).collect();
        let mut builder = Engine::builder()
            .extensions(&extensions)
            .configure(super::CONFIGURE)
            .tier::<super::Tier>();
        for (name, value) in ini {
            builder = builder.ini(name, value);
        }
        builder.build()
    }
}
