//! The `curl` extension: `php-src/ext/curl` as one crate under `ext/`
//! (ADR-0004) over the platform's libcurl, the library the oracle links,
//! bound in [`ffi`] alone (design §7.2). One entry, `extension()`; its
//! state on the value (ADR-0005). The phpt subset is
//! `php-src/ext/curl/tests`, through `xtask phpt --subset`, whose HTTP
//! tests are served by the engine's own `-S` server as php's are by
//! php's.
//!
//! `interface.c` is [`handle`] (the `CurlHandle` natives), [`options`]
//! (`curl_setopt`, one typed table decoded once), [`callbacks`] (what
//! libcurl calls back into) and [`info`] (`curl_getinfo`); `multi.c` is
//! [`multi`], `share.c` [`share`] and `curl_file.c` [`file`].
#![deny(unsafe_code)]
// seams.toml rule lints.no-panic: Internal invariant failures are InternalError fatals with a message, not panics (design §10).
#![deny(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable,
    clippy::todo,
    clippy::unimplemented
)]
// The `expect` fence is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

mod callbacks;
mod ffi;
mod file;
mod handle;
mod info;
mod multi;
mod options;
mod share;
mod throwables;
mod version;

use php_ext::abi::{Extension, ExtensionBuilder, Lent, Pool, Refusal};
use php_ext::attr::php_const;
use php_ext::decl::{Bytes, IniDecl, Name};

/// Request-scoped: built at request start, dropped after `rshutdown`,
/// private to this crate (ADR-0005); nothing in it is shared or
/// synchronised. The request's link to libcurl's callbacks, and the
/// persistent shares the request holds, each under the options it was
/// made with, taken out of the engine's pool or made by
/// `curl_share_init_persistent` and given back at `post_deactivate`
/// ([`share::give_back`]).
#[derive(Default)]
struct State {
    session: ffi::Session,
    persistent: Lent<u8, ffi::Persistent>,
}

/// The constants `register_curl_symbols` registers, in the stub's order,
/// each the number libcurl's headers give it at the version the oracle
/// is built against, and php's own `CURLOPT_RETURNTRANSFER`,
/// `CURLOPT_BINARYTRANSFER` and `CURLOPT_SAFE_UPLOAD`.
///
/// ```php
/// var_dump(CURLOPT_URL, CURLOPT_RETURNTRANSFER, CURLOPT_SAFE_UPLOAD, CURLINFO_HTTP_CODE, CURLE_OK, CURLMSG_DONE, CURL_LOCK_DATA_CONNECT, CURLOPT_BINARYTRANSFER);
/// ```
/// ```expect
///
/// Deprecated: Constant CURLOPT_BINARYTRANSFER is deprecated since 8.4, as it had no effect since 5.1.2 in %s on line 2
/// int(10002)
/// int(19913)
/// int(-1)
/// int(2097154)
/// int(0)
/// int(1)
/// int(5)
/// int(19914)
/// ```
#[php_const]
#[allow(clippy::unreadable_literal)]
pub(crate) mod constants {
    pub(crate) const CURLOPT_AUTOREFERER: i64 = 58;
    #[php(deprecated(since = "8.4", message = "as it had no effect since 5.1.2"))]
    pub(crate) const CURLOPT_BINARYTRANSFER: i64 = 19914;
    pub(crate) const CURLOPT_BUFFERSIZE: i64 = 98;
    pub(crate) const CURLOPT_CAINFO: i64 = 10065;
    pub(crate) const CURLOPT_CAPATH: i64 = 10097;
    pub(crate) const CURLOPT_CONNECTTIMEOUT: i64 = 78;
    pub(crate) const CURLOPT_COOKIE: i64 = 10022;
    pub(crate) const CURLOPT_COOKIEFILE: i64 = 10031;
    pub(crate) const CURLOPT_COOKIEJAR: i64 = 10082;
    pub(crate) const CURLOPT_COOKIESESSION: i64 = 96;
    pub(crate) const CURLOPT_CRLF: i64 = 27;
    pub(crate) const CURLOPT_CUSTOMREQUEST: i64 = 10036;
    pub(crate) const CURLOPT_DNS_CACHE_TIMEOUT: i64 = 92;
    pub(crate) const CURLOPT_DNS_USE_GLOBAL_CACHE: i64 = 91;
    pub(crate) const CURLOPT_EGDSOCKET: i64 = 10077;
    pub(crate) const CURLOPT_ENCODING: i64 = 10102;
    pub(crate) const CURLOPT_FAILONERROR: i64 = 45;
    pub(crate) const CURLOPT_FILE: i64 = 10001;
    pub(crate) const CURLOPT_FILETIME: i64 = 69;
    pub(crate) const CURLOPT_FOLLOWLOCATION: i64 = 52;
    pub(crate) const CURLOPT_FORBID_REUSE: i64 = 75;
    pub(crate) const CURLOPT_FRESH_CONNECT: i64 = 74;
    pub(crate) const CURLOPT_FTPAPPEND: i64 = 50;
    pub(crate) const CURLOPT_FTPLISTONLY: i64 = 48;
    pub(crate) const CURLOPT_FTPPORT: i64 = 10017;
    pub(crate) const CURLOPT_FTP_USE_EPRT: i64 = 106;
    pub(crate) const CURLOPT_FTP_USE_EPSV: i64 = 85;
    pub(crate) const CURLOPT_HEADER: i64 = 42;
    pub(crate) const CURLOPT_HEADERFUNCTION: i64 = 20079;
    pub(crate) const CURLOPT_HTTP200ALIASES: i64 = 10104;
    pub(crate) const CURLOPT_HTTPGET: i64 = 80;
    pub(crate) const CURLOPT_HTTPHEADER: i64 = 10023;
    pub(crate) const CURLOPT_HTTPPROXYTUNNEL: i64 = 61;
    pub(crate) const CURLOPT_HTTP_VERSION: i64 = 84;
    pub(crate) const CURLOPT_INFILE: i64 = 10009;
    pub(crate) const CURLOPT_INFILESIZE: i64 = 14;
    pub(crate) const CURLOPT_INFILESIZE_LARGE: i64 = 30115;
    pub(crate) const CURLOPT_INTERFACE: i64 = 10062;
    pub(crate) const CURLOPT_KRB4LEVEL: i64 = 10063;
    pub(crate) const CURLOPT_LOW_SPEED_LIMIT: i64 = 19;
    pub(crate) const CURLOPT_LOW_SPEED_TIME: i64 = 20;
    pub(crate) const CURLOPT_MAXCONNECTS: i64 = 71;
    pub(crate) const CURLOPT_MAXREDIRS: i64 = 68;
    pub(crate) const CURLOPT_NETRC: i64 = 51;
    pub(crate) const CURLOPT_NOBODY: i64 = 44;
    pub(crate) const CURLOPT_NOPROGRESS: i64 = 43;
    pub(crate) const CURLOPT_NOSIGNAL: i64 = 99;
    pub(crate) const CURLOPT_PORT: i64 = 3;
    pub(crate) const CURLOPT_POST: i64 = 47;
    pub(crate) const CURLOPT_POSTFIELDS: i64 = 10015;
    pub(crate) const CURLOPT_POSTQUOTE: i64 = 10039;
    pub(crate) const CURLOPT_PREQUOTE: i64 = 10093;
    pub(crate) const CURLOPT_PRIVATE: i64 = 10103;
    pub(crate) const CURLOPT_PROGRESSFUNCTION: i64 = 20056;
    pub(crate) const CURLOPT_PROXY: i64 = 10004;
    pub(crate) const CURLOPT_PROXYPORT: i64 = 59;
    pub(crate) const CURLOPT_PROXYTYPE: i64 = 101;
    pub(crate) const CURLOPT_PROXYUSERPWD: i64 = 10006;
    pub(crate) const CURLOPT_PUT: i64 = 54;
    pub(crate) const CURLOPT_QUOTE: i64 = 10028;
    pub(crate) const CURLOPT_RANDOM_FILE: i64 = 10076;
    pub(crate) const CURLOPT_RANGE: i64 = 10007;
    pub(crate) const CURLOPT_READDATA: i64 = 10009;
    pub(crate) const CURLOPT_READFUNCTION: i64 = 20012;
    pub(crate) const CURLOPT_REFERER: i64 = 10016;
    pub(crate) const CURLOPT_RESUME_FROM: i64 = 21;
    pub(crate) const CURLOPT_RETURNTRANSFER: i64 = 19913;
    pub(crate) const CURLOPT_SHARE: i64 = 10100;
    pub(crate) const CURLOPT_SSLCERT: i64 = 10025;
    pub(crate) const CURLOPT_SSLCERTPASSWD: i64 = 10026;
    pub(crate) const CURLOPT_SSLCERTTYPE: i64 = 10086;
    pub(crate) const CURLOPT_SSLENGINE: i64 = 10089;
    pub(crate) const CURLOPT_SSLENGINE_DEFAULT: i64 = 90;
    pub(crate) const CURLOPT_SSLKEY: i64 = 10087;
    pub(crate) const CURLOPT_SSLKEYPASSWD: i64 = 10026;
    pub(crate) const CURLOPT_SSLKEYTYPE: i64 = 10088;
    pub(crate) const CURLOPT_SSLVERSION: i64 = 32;
    pub(crate) const CURLOPT_SSL_CIPHER_LIST: i64 = 10083;
    pub(crate) const CURLOPT_SSL_VERIFYHOST: i64 = 81;
    pub(crate) const CURLOPT_SSL_VERIFYPEER: i64 = 64;
    pub(crate) const CURLOPT_STDERR: i64 = 10037;
    pub(crate) const CURLOPT_TCP_KEEPCNT: i64 = 326;
    pub(crate) const CURLOPT_TELNETOPTIONS: i64 = 10070;
    pub(crate) const CURLOPT_TIMECONDITION: i64 = 33;
    pub(crate) const CURLOPT_TIMEOUT: i64 = 13;
    pub(crate) const CURLOPT_TIMEVALUE: i64 = 34;
    pub(crate) const CURLOPT_TRANSFERTEXT: i64 = 53;
    pub(crate) const CURLOPT_UNRESTRICTED_AUTH: i64 = 105;
    pub(crate) const CURLOPT_UPLOAD: i64 = 46;
    pub(crate) const CURLOPT_URL: i64 = 10002;
    pub(crate) const CURLOPT_USERAGENT: i64 = 10018;
    pub(crate) const CURLOPT_USERPWD: i64 = 10005;
    pub(crate) const CURLOPT_VERBOSE: i64 = 41;
    pub(crate) const CURLOPT_WRITEFUNCTION: i64 = 20011;
    pub(crate) const CURLOPT_WRITEHEADER: i64 = 10029;
    pub(crate) const CURLOPT_XFERINFOFUNCTION: i64 = 20219;
    pub(crate) const CURLOPT_DEBUGFUNCTION: i64 = 20094;
    pub(crate) const CURLFOLLOW_ALL: i64 = 1;
    pub(crate) const CURLFOLLOW_OBEYCODE: i64 = 2;
    pub(crate) const CURLFOLLOW_FIRSTONLY: i64 = 3;
    pub(crate) const CURLINFO_TEXT: i64 = 0;
    pub(crate) const CURLINFO_HEADER_IN: i64 = 1;
    pub(crate) const CURLINFO_DATA_IN: i64 = 3;
    pub(crate) const CURLINFO_DATA_OUT: i64 = 4;
    pub(crate) const CURLINFO_SSL_DATA_OUT: i64 = 6;
    pub(crate) const CURLINFO_SSL_DATA_IN: i64 = 5;
    pub(crate) const CURLE_ABORTED_BY_CALLBACK: i64 = 42;
    pub(crate) const CURLE_BAD_CALLING_ORDER: i64 = 44;
    pub(crate) const CURLE_BAD_CONTENT_ENCODING: i64 = 61;
    pub(crate) const CURLE_BAD_DOWNLOAD_RESUME: i64 = 36;
    pub(crate) const CURLE_BAD_FUNCTION_ARGUMENT: i64 = 43;
    pub(crate) const CURLE_BAD_PASSWORD_ENTERED: i64 = 46;
    pub(crate) const CURLE_COULDNT_CONNECT: i64 = 7;
    pub(crate) const CURLE_COULDNT_RESOLVE_HOST: i64 = 6;
    pub(crate) const CURLE_COULDNT_RESOLVE_PROXY: i64 = 5;
    pub(crate) const CURLE_FAILED_INIT: i64 = 2;
    pub(crate) const CURLE_FILE_COULDNT_READ_FILE: i64 = 37;
    pub(crate) const CURLE_FTP_ACCESS_DENIED: i64 = 9;
    pub(crate) const CURLE_FTP_BAD_DOWNLOAD_RESUME: i64 = 36;
    pub(crate) const CURLE_FTP_CANT_GET_HOST: i64 = 15;
    pub(crate) const CURLE_FTP_CANT_RECONNECT: i64 = 16;
    pub(crate) const CURLE_FTP_COULDNT_GET_SIZE: i64 = 32;
    pub(crate) const CURLE_FTP_COULDNT_RETR_FILE: i64 = 19;
    pub(crate) const CURLE_FTP_COULDNT_SET_ASCII: i64 = 29;
    pub(crate) const CURLE_FTP_COULDNT_SET_BINARY: i64 = 17;
    pub(crate) const CURLE_FTP_COULDNT_STOR_FILE: i64 = 25;
    pub(crate) const CURLE_FTP_COULDNT_USE_REST: i64 = 31;
    pub(crate) const CURLE_FTP_PARTIAL_FILE: i64 = 18;
    pub(crate) const CURLE_FTP_PORT_FAILED: i64 = 30;
    pub(crate) const CURLE_FTP_QUOTE_ERROR: i64 = 21;
    pub(crate) const CURLE_FTP_USER_PASSWORD_INCORRECT: i64 = 10;
    pub(crate) const CURLE_FTP_WEIRD_227_FORMAT: i64 = 14;
    pub(crate) const CURLE_FTP_WEIRD_PASS_REPLY: i64 = 11;
    pub(crate) const CURLE_FTP_WEIRD_PASV_REPLY: i64 = 13;
    pub(crate) const CURLE_FTP_WEIRD_SERVER_REPLY: i64 = 8;
    pub(crate) const CURLE_FTP_WEIRD_USER_REPLY: i64 = 12;
    pub(crate) const CURLE_FTP_WRITE_ERROR: i64 = 20;
    pub(crate) const CURLE_FUNCTION_NOT_FOUND: i64 = 41;
    pub(crate) const CURLE_GOT_NOTHING: i64 = 52;
    pub(crate) const CURLE_HTTP_NOT_FOUND: i64 = 22;
    pub(crate) const CURLE_HTTP_PORT_FAILED: i64 = 45;
    pub(crate) const CURLE_HTTP_POST_ERROR: i64 = 34;
    pub(crate) const CURLE_HTTP_RANGE_ERROR: i64 = 33;
    pub(crate) const CURLE_HTTP_RETURNED_ERROR: i64 = 22;
    pub(crate) const CURLE_LDAP_CANNOT_BIND: i64 = 38;
    pub(crate) const CURLE_LDAP_SEARCH_FAILED: i64 = 39;
    pub(crate) const CURLE_LIBRARY_NOT_FOUND: i64 = 40;
    pub(crate) const CURLE_MALFORMAT_USER: i64 = 24;
    pub(crate) const CURLE_OBSOLETE: i64 = 50;
    pub(crate) const CURLE_OK: i64 = 0;
    pub(crate) const CURLE_OPERATION_TIMEDOUT: i64 = 28;
    pub(crate) const CURLE_OPERATION_TIMEOUTED: i64 = 28;
    pub(crate) const CURLE_OUT_OF_MEMORY: i64 = 27;
    pub(crate) const CURLE_PARTIAL_FILE: i64 = 18;
    pub(crate) const CURLE_READ_ERROR: i64 = 26;
    pub(crate) const CURLE_RECV_ERROR: i64 = 56;
    pub(crate) const CURLE_SEND_ERROR: i64 = 55;
    pub(crate) const CURLE_SHARE_IN_USE: i64 = 57;
    pub(crate) const CURLE_SSL_CACERT: i64 = 60;
    pub(crate) const CURLE_SSL_CERTPROBLEM: i64 = 58;
    pub(crate) const CURLE_SSL_CIPHER: i64 = 59;
    pub(crate) const CURLE_SSL_CONNECT_ERROR: i64 = 35;
    pub(crate) const CURLE_SSL_ENGINE_NOTFOUND: i64 = 53;
    pub(crate) const CURLE_SSL_ENGINE_SETFAILED: i64 = 54;
    pub(crate) const CURLE_SSL_PEER_CERTIFICATE: i64 = 60;
    pub(crate) const CURLE_SSL_PINNEDPUBKEYNOTMATCH: i64 = 90;
    pub(crate) const CURLE_TELNET_OPTION_SYNTAX: i64 = 49;
    pub(crate) const CURLE_TOO_MANY_REDIRECTS: i64 = 47;
    pub(crate) const CURLE_UNKNOWN_TELNET_OPTION: i64 = 48;
    pub(crate) const CURLE_UNSUPPORTED_PROTOCOL: i64 = 1;
    pub(crate) const CURLE_URL_MALFORMAT: i64 = 3;
    pub(crate) const CURLE_URL_MALFORMAT_USER: i64 = 4;
    pub(crate) const CURLE_WRITE_ERROR: i64 = 23;
    pub(crate) const CURLINFO_CONNECT_TIME: i64 = 3145733;
    pub(crate) const CURLINFO_CONTENT_LENGTH_DOWNLOAD: i64 = 3145743;
    pub(crate) const CURLINFO_CONTENT_LENGTH_UPLOAD: i64 = 3145744;
    pub(crate) const CURLINFO_CONTENT_TYPE: i64 = 1048594;
    pub(crate) const CURLINFO_EFFECTIVE_URL: i64 = 1048577;
    pub(crate) const CURLINFO_FILETIME: i64 = 2097166;
    pub(crate) const CURLINFO_HEADER_OUT: i64 = 2;
    pub(crate) const CURLINFO_HEADER_SIZE: i64 = 2097163;
    pub(crate) const CURLINFO_HTTP_CODE: i64 = 2097154;
    pub(crate) const CURLINFO_LASTONE: i64 = 71;
    pub(crate) const CURLINFO_NAMELOOKUP_TIME: i64 = 3145732;
    pub(crate) const CURLINFO_PRETRANSFER_TIME: i64 = 3145734;
    pub(crate) const CURLINFO_PRIVATE: i64 = 1048597;
    pub(crate) const CURLINFO_REDIRECT_COUNT: i64 = 2097172;
    pub(crate) const CURLINFO_REDIRECT_TIME: i64 = 3145747;
    pub(crate) const CURLINFO_REQUEST_SIZE: i64 = 2097164;
    pub(crate) const CURLINFO_SIZE_DOWNLOAD: i64 = 3145736;
    pub(crate) const CURLINFO_SIZE_UPLOAD: i64 = 3145735;
    pub(crate) const CURLINFO_SPEED_DOWNLOAD: i64 = 3145737;
    pub(crate) const CURLINFO_SPEED_UPLOAD: i64 = 3145738;
    pub(crate) const CURLINFO_SSL_VERIFYRESULT: i64 = 2097165;
    pub(crate) const CURLINFO_STARTTRANSFER_TIME: i64 = 3145745;
    pub(crate) const CURLINFO_TOTAL_TIME: i64 = 3145731;
    pub(crate) const CURLINFO_EFFECTIVE_METHOD: i64 = 1048634;
    pub(crate) const CURLINFO_CAPATH: i64 = 1048638;
    pub(crate) const CURLINFO_CAINFO: i64 = 1048637;
    pub(crate) const CURLINFO_HTTPAUTH_USED: i64 = 2097221;
    pub(crate) const CURLINFO_PROXYAUTH_USED: i64 = 2097222;
    pub(crate) const CURLMSG_DONE: i64 = 1;
    pub(crate) const CURLVERSION_NOW: i64 = 11;
    pub(crate) const CURLM_BAD_EASY_HANDLE: i64 = 2;
    pub(crate) const CURLM_BAD_HANDLE: i64 = 1;
    pub(crate) const CURLM_CALL_MULTI_PERFORM: i64 = -1;
    pub(crate) const CURLM_INTERNAL_ERROR: i64 = 4;
    pub(crate) const CURLM_OK: i64 = 0;
    pub(crate) const CURLM_OUT_OF_MEMORY: i64 = 3;
    pub(crate) const CURLM_ADDED_ALREADY: i64 = 7;
    pub(crate) const CURLPROXY_HTTP: i64 = 0;
    pub(crate) const CURLPROXY_SOCKS4: i64 = 4;
    pub(crate) const CURLPROXY_SOCKS5: i64 = 5;
    pub(crate) const CURLSHOPT_NONE: i64 = 0;
    pub(crate) const CURLSHOPT_SHARE: i64 = 1;
    pub(crate) const CURLSHOPT_UNSHARE: i64 = 2;
    pub(crate) const CURL_HTTP_VERSION_1_0: i64 = 1;
    pub(crate) const CURL_HTTP_VERSION_1_1: i64 = 2;
    pub(crate) const CURL_HTTP_VERSION_NONE: i64 = 0;
    pub(crate) const CURL_LOCK_DATA_COOKIE: i64 = 2;
    pub(crate) const CURL_LOCK_DATA_DNS: i64 = 3;
    pub(crate) const CURL_LOCK_DATA_SSL_SESSION: i64 = 4;
    pub(crate) const CURL_NETRC_IGNORED: i64 = 0;
    pub(crate) const CURL_NETRC_OPTIONAL: i64 = 1;
    pub(crate) const CURL_NETRC_REQUIRED: i64 = 2;
    pub(crate) const CURL_SSLVERSION_DEFAULT: i64 = 0;
    pub(crate) const CURL_SSLVERSION_SSLv2: i64 = 2;
    pub(crate) const CURL_SSLVERSION_SSLv3: i64 = 3;
    pub(crate) const CURL_SSLVERSION_TLSv1: i64 = 1;
    pub(crate) const CURL_TIMECOND_IFMODSINCE: i64 = 1;
    pub(crate) const CURL_TIMECOND_IFUNMODSINCE: i64 = 2;
    pub(crate) const CURL_TIMECOND_LASTMOD: i64 = 3;
    pub(crate) const CURL_TIMECOND_NONE: i64 = 0;
    pub(crate) const CURL_VERSION_ASYNCHDNS: i64 = 128;
    pub(crate) const CURL_VERSION_CONV: i64 = 4096;
    pub(crate) const CURL_VERSION_DEBUG: i64 = 64;
    pub(crate) const CURL_VERSION_GSSNEGOTIATE: i64 = 32;
    pub(crate) const CURL_VERSION_IDN: i64 = 1024;
    pub(crate) const CURL_VERSION_IPV6: i64 = 1;
    pub(crate) const CURL_VERSION_KERBEROS4: i64 = 2;
    pub(crate) const CURL_VERSION_LARGEFILE: i64 = 512;
    pub(crate) const CURL_VERSION_LIBZ: i64 = 8;
    pub(crate) const CURL_VERSION_NTLM: i64 = 16;
    pub(crate) const CURL_VERSION_SPNEGO: i64 = 256;
    pub(crate) const CURL_VERSION_SSL: i64 = 4;
    pub(crate) const CURL_VERSION_SSPI: i64 = 2048;
    pub(crate) const CURLOPT_HTTPAUTH: i64 = 107;
    // The keg's libcurl 8.23 headers, which the oracle compiled against,
    // leave `CURLAUTH_HTTPSIG` (bit 8) out of both, as `@cvalue` reads them.
    pub(crate) const CURLAUTH_ANY: i64 = 4294967023;
    pub(crate) const CURLAUTH_ANYSAFE: i64 = 4294967022;
    pub(crate) const CURLAUTH_BASIC: i64 = 1;
    pub(crate) const CURLAUTH_DIGEST: i64 = 2;
    pub(crate) const CURLAUTH_GSSNEGOTIATE: i64 = 4;
    pub(crate) const CURLAUTH_NONE: i64 = 0;
    pub(crate) const CURLAUTH_NTLM: i64 = 8;
    pub(crate) const CURLINFO_HTTP_CONNECTCODE: i64 = 2097174;
    pub(crate) const CURLOPT_FTP_CREATE_MISSING_DIRS: i64 = 110;
    pub(crate) const CURLOPT_PROXYAUTH: i64 = 111;
    pub(crate) const CURLE_FILESIZE_EXCEEDED: i64 = 63;
    pub(crate) const CURLE_LDAP_INVALID_URL: i64 = 62;
    pub(crate) const CURLINFO_HTTPAUTH_AVAIL: i64 = 2097175;
    pub(crate) const CURLINFO_RESPONSE_CODE: i64 = 2097154;
    pub(crate) const CURLINFO_PROXYAUTH_AVAIL: i64 = 2097176;
    pub(crate) const CURLOPT_FTP_RESPONSE_TIMEOUT: i64 = 112;
    pub(crate) const CURLOPT_SERVER_RESPONSE_TIMEOUT: i64 = 112;
    pub(crate) const CURLOPT_IPRESOLVE: i64 = 113;
    pub(crate) const CURLOPT_MAXFILESIZE: i64 = 114;
    pub(crate) const CURL_IPRESOLVE_V4: i64 = 1;
    pub(crate) const CURL_IPRESOLVE_V6: i64 = 2;
    pub(crate) const CURL_IPRESOLVE_WHATEVER: i64 = 0;
    pub(crate) const CURLE_FTP_SSL_FAILED: i64 = 64;
    pub(crate) const CURLFTPSSL_ALL: i64 = 3;
    pub(crate) const CURLFTPSSL_CONTROL: i64 = 2;
    pub(crate) const CURLFTPSSL_NONE: i64 = 0;
    pub(crate) const CURLFTPSSL_TRY: i64 = 1;
    pub(crate) const CURLOPT_FTP_SSL: i64 = 119;
    pub(crate) const CURLOPT_NETRC_FILE: i64 = 10118;
    pub(crate) const CURLOPT_MAXFILESIZE_LARGE: i64 = 30117;
    pub(crate) const CURLOPT_TCP_NODELAY: i64 = 121;
    pub(crate) const CURLFTPAUTH_DEFAULT: i64 = 0;
    pub(crate) const CURLFTPAUTH_SSL: i64 = 1;
    pub(crate) const CURLFTPAUTH_TLS: i64 = 2;
    pub(crate) const CURLOPT_FTPSSLAUTH: i64 = 129;
    pub(crate) const CURLOPT_FTP_ACCOUNT: i64 = 10134;
    pub(crate) const CURLINFO_OS_ERRNO: i64 = 2097177;
    pub(crate) const CURLINFO_NUM_CONNECTS: i64 = 2097178;
    pub(crate) const CURLINFO_SSL_ENGINES: i64 = 4194331;
    pub(crate) const CURLINFO_COOKIELIST: i64 = 4194332;
    pub(crate) const CURLOPT_COOKIELIST: i64 = 10135;
    pub(crate) const CURLOPT_IGNORE_CONTENT_LENGTH: i64 = 136;
    pub(crate) const CURLOPT_FTP_SKIP_PASV_IP: i64 = 137;
    pub(crate) const CURLOPT_FTP_FILEMETHOD: i64 = 138;
    pub(crate) const CURLOPT_CONNECT_ONLY: i64 = 141;
    pub(crate) const CURLOPT_LOCALPORT: i64 = 139;
    pub(crate) const CURLOPT_LOCALPORTRANGE: i64 = 140;
    pub(crate) const CURLFTPMETHOD_DEFAULT: i64 = 0;
    pub(crate) const CURLFTPMETHOD_MULTICWD: i64 = 1;
    pub(crate) const CURLFTPMETHOD_NOCWD: i64 = 2;
    pub(crate) const CURLFTPMETHOD_SINGLECWD: i64 = 3;
    pub(crate) const CURLINFO_FTP_ENTRY_PATH: i64 = 1048606;
    pub(crate) const CURLOPT_FTP_ALTERNATIVE_TO_USER: i64 = 10147;
    pub(crate) const CURLOPT_MAX_RECV_SPEED_LARGE: i64 = 30146;
    pub(crate) const CURLOPT_MAX_SEND_SPEED_LARGE: i64 = 30145;
    pub(crate) const CURLE_SSL_CACERT_BADFILE: i64 = 77;
    pub(crate) const CURLOPT_SSL_SESSIONID_CACHE: i64 = 150;
    pub(crate) const CURLMOPT_PIPELINING: i64 = 3;
    pub(crate) const CURLE_SSH: i64 = 79;
    pub(crate) const CURLOPT_FTP_SSL_CCC: i64 = 154;
    pub(crate) const CURLOPT_SSH_AUTH_TYPES: i64 = 151;
    pub(crate) const CURLOPT_SSH_PRIVATE_KEYFILE: i64 = 10153;
    pub(crate) const CURLOPT_SSH_PUBLIC_KEYFILE: i64 = 10152;
    pub(crate) const CURLFTPSSL_CCC_ACTIVE: i64 = 2;
    pub(crate) const CURLFTPSSL_CCC_NONE: i64 = 0;
    pub(crate) const CURLFTPSSL_CCC_PASSIVE: i64 = 1;
    pub(crate) const CURLOPT_CONNECTTIMEOUT_MS: i64 = 156;
    pub(crate) const CURLOPT_HTTP_CONTENT_DECODING: i64 = 158;
    pub(crate) const CURLOPT_HTTP_TRANSFER_DECODING: i64 = 157;
    pub(crate) const CURLOPT_TIMEOUT_MS: i64 = 155;
    pub(crate) const CURLMOPT_MAXCONNECTS: i64 = 6;
    pub(crate) const CURLOPT_KRBLEVEL: i64 = 10063;
    pub(crate) const CURLOPT_NEW_DIRECTORY_PERMS: i64 = 160;
    pub(crate) const CURLOPT_NEW_FILE_PERMS: i64 = 159;
    pub(crate) const CURLOPT_APPEND: i64 = 50;
    pub(crate) const CURLOPT_DIRLISTONLY: i64 = 48;
    pub(crate) const CURLOPT_USE_SSL: i64 = 119;
    pub(crate) const CURLUSESSL_ALL: i64 = 3;
    pub(crate) const CURLUSESSL_CONTROL: i64 = 2;
    pub(crate) const CURLUSESSL_NONE: i64 = 0;
    pub(crate) const CURLUSESSL_TRY: i64 = 1;
    pub(crate) const CURLOPT_SSH_HOST_PUBLIC_KEY_MD5: i64 = 10162;
    pub(crate) const CURLOPT_PROXY_TRANSFER_MODE: i64 = 166;
    pub(crate) const CURLPAUSE_ALL: i64 = 5;
    pub(crate) const CURLPAUSE_CONT: i64 = 0;
    pub(crate) const CURLPAUSE_RECV: i64 = 1;
    pub(crate) const CURLPAUSE_RECV_CONT: i64 = 0;
    pub(crate) const CURLPAUSE_SEND: i64 = 4;
    pub(crate) const CURLPAUSE_SEND_CONT: i64 = 0;
    pub(crate) const CURL_READFUNC_PAUSE: i64 = 268435457;
    pub(crate) const CURL_WRITEFUNC_PAUSE: i64 = 268435457;
    pub(crate) const CURLPROXY_SOCKS4A: i64 = 6;
    pub(crate) const CURLPROXY_SOCKS5_HOSTNAME: i64 = 7;
    pub(crate) const CURLINFO_REDIRECT_URL: i64 = 1048607;
    pub(crate) const CURLINFO_APPCONNECT_TIME: i64 = 3145761;
    pub(crate) const CURLINFO_PRIMARY_IP: i64 = 1048608;
    pub(crate) const CURLOPT_ADDRESS_SCOPE: i64 = 171;
    pub(crate) const CURLOPT_CRLFILE: i64 = 10169;
    pub(crate) const CURLOPT_ISSUERCERT: i64 = 10170;
    pub(crate) const CURLOPT_KEYPASSWD: i64 = 10026;
    pub(crate) const CURLSSH_AUTH_ANY: i64 = 4294967295;
    pub(crate) const CURLSSH_AUTH_DEFAULT: i64 = 4294967295;
    pub(crate) const CURLSSH_AUTH_HOST: i64 = 4;
    pub(crate) const CURLSSH_AUTH_KEYBOARD: i64 = 8;
    pub(crate) const CURLSSH_AUTH_NONE: i64 = 0;
    pub(crate) const CURLSSH_AUTH_PASSWORD: i64 = 2;
    pub(crate) const CURLSSH_AUTH_PUBLICKEY: i64 = 1;
    pub(crate) const CURLINFO_CERTINFO: i64 = 4194338;
    pub(crate) const CURLOPT_CERTINFO: i64 = 172;
    pub(crate) const CURLOPT_PASSWORD: i64 = 10174;
    pub(crate) const CURLOPT_POSTREDIR: i64 = 161;
    pub(crate) const CURLOPT_PROXYPASSWORD: i64 = 10176;
    pub(crate) const CURLOPT_PROXYUSERNAME: i64 = 10175;
    pub(crate) const CURLOPT_USERNAME: i64 = 10173;
    pub(crate) const CURL_REDIR_POST_301: i64 = 1;
    pub(crate) const CURL_REDIR_POST_302: i64 = 2;
    pub(crate) const CURL_REDIR_POST_ALL: i64 = 7;
    pub(crate) const CURLAUTH_DIGEST_IE: i64 = 16;
    pub(crate) const CURLINFO_CONDITION_UNMET: i64 = 2097187;
    pub(crate) const CURLOPT_NOPROXY: i64 = 10177;
    pub(crate) const CURLOPT_PROTOCOLS: i64 = 181;
    pub(crate) const CURLOPT_REDIR_PROTOCOLS: i64 = 182;
    pub(crate) const CURLOPT_SOCKS5_GSSAPI_NEC: i64 = 180;
    pub(crate) const CURLOPT_SOCKS5_GSSAPI_SERVICE: i64 = 10179;
    pub(crate) const CURLOPT_TFTP_BLKSIZE: i64 = 178;
    pub(crate) const CURLPROTO_ALL: i64 = 4294967295;
    pub(crate) const CURLPROTO_DICT: i64 = 512;
    pub(crate) const CURLPROTO_FILE: i64 = 1024;
    pub(crate) const CURLPROTO_FTP: i64 = 4;
    pub(crate) const CURLPROTO_FTPS: i64 = 8;
    pub(crate) const CURLPROTO_HTTP: i64 = 1;
    pub(crate) const CURLPROTO_HTTPS: i64 = 2;
    pub(crate) const CURLPROTO_LDAP: i64 = 128;
    pub(crate) const CURLPROTO_LDAPS: i64 = 256;
    pub(crate) const CURLPROTO_SCP: i64 = 16;
    pub(crate) const CURLPROTO_SFTP: i64 = 32;
    pub(crate) const CURLPROTO_TELNET: i64 = 64;
    pub(crate) const CURLPROTO_TFTP: i64 = 2048;
    pub(crate) const CURLPROXY_HTTP_1_0: i64 = 1;
    pub(crate) const CURLFTP_CREATE_DIR: i64 = 1;
    pub(crate) const CURLFTP_CREATE_DIR_NONE: i64 = 0;
    pub(crate) const CURLFTP_CREATE_DIR_RETRY: i64 = 2;
    pub(crate) const CURL_VERSION_CURLDEBUG: i64 = 8192;
    pub(crate) const CURLOPT_SSH_KNOWNHOSTS: i64 = 10183;
    pub(crate) const CURLKHMATCH_OK: i64 = 0;
    pub(crate) const CURLKHMATCH_MISMATCH: i64 = 1;
    pub(crate) const CURLKHMATCH_MISSING: i64 = 2;
    pub(crate) const CURLKHMATCH_LAST: i64 = 3;
    pub(crate) const CURLINFO_RTSP_CLIENT_CSEQ: i64 = 2097189;
    pub(crate) const CURLINFO_RTSP_CSEQ_RECV: i64 = 2097191;
    pub(crate) const CURLINFO_RTSP_SERVER_CSEQ: i64 = 2097190;
    pub(crate) const CURLINFO_RTSP_SESSION_ID: i64 = 1048612;
    pub(crate) const CURLOPT_FTP_USE_PRET: i64 = 188;
    pub(crate) const CURLOPT_MAIL_FROM: i64 = 10186;
    pub(crate) const CURLOPT_MAIL_RCPT: i64 = 10187;
    pub(crate) const CURLOPT_RTSP_CLIENT_CSEQ: i64 = 193;
    pub(crate) const CURLOPT_RTSP_REQUEST: i64 = 189;
    pub(crate) const CURLOPT_RTSP_SERVER_CSEQ: i64 = 194;
    pub(crate) const CURLOPT_RTSP_SESSION_ID: i64 = 10190;
    pub(crate) const CURLOPT_RTSP_STREAM_URI: i64 = 10191;
    pub(crate) const CURLOPT_RTSP_TRANSPORT: i64 = 10192;
    pub(crate) const CURLPROTO_IMAP: i64 = 4096;
    pub(crate) const CURLPROTO_IMAPS: i64 = 8192;
    pub(crate) const CURLPROTO_POP3: i64 = 16384;
    pub(crate) const CURLPROTO_POP3S: i64 = 32768;
    pub(crate) const CURLPROTO_RTSP: i64 = 262144;
    pub(crate) const CURLPROTO_SMTP: i64 = 65536;
    pub(crate) const CURLPROTO_SMTPS: i64 = 131072;
    pub(crate) const CURL_RTSPREQ_ANNOUNCE: i64 = 3;
    pub(crate) const CURL_RTSPREQ_DESCRIBE: i64 = 2;
    pub(crate) const CURL_RTSPREQ_GET_PARAMETER: i64 = 8;
    pub(crate) const CURL_RTSPREQ_OPTIONS: i64 = 1;
    pub(crate) const CURL_RTSPREQ_PAUSE: i64 = 6;
    pub(crate) const CURL_RTSPREQ_PLAY: i64 = 5;
    pub(crate) const CURL_RTSPREQ_RECEIVE: i64 = 11;
    pub(crate) const CURL_RTSPREQ_RECORD: i64 = 10;
    pub(crate) const CURL_RTSPREQ_SET_PARAMETER: i64 = 9;
    pub(crate) const CURL_RTSPREQ_SETUP: i64 = 4;
    pub(crate) const CURL_RTSPREQ_TEARDOWN: i64 = 7;
    pub(crate) const CURLINFO_LOCAL_IP: i64 = 1048617;
    pub(crate) const CURLINFO_LOCAL_PORT: i64 = 2097194;
    pub(crate) const CURLINFO_PRIMARY_PORT: i64 = 2097192;
    pub(crate) const CURLOPT_FNMATCH_FUNCTION: i64 = 20200;
    pub(crate) const CURLOPT_WILDCARDMATCH: i64 = 197;
    pub(crate) const CURLPROTO_RTMP: i64 = 524288;
    pub(crate) const CURLPROTO_RTMPE: i64 = 2097152;
    pub(crate) const CURLPROTO_RTMPS: i64 = 8388608;
    pub(crate) const CURLPROTO_RTMPT: i64 = 1048576;
    pub(crate) const CURLPROTO_RTMPTE: i64 = 4194304;
    pub(crate) const CURLPROTO_RTMPTS: i64 = 16777216;
    pub(crate) const CURL_FNMATCHFUNC_FAIL: i64 = 2;
    pub(crate) const CURL_FNMATCHFUNC_MATCH: i64 = 0;
    pub(crate) const CURL_FNMATCHFUNC_NOMATCH: i64 = 1;
    pub(crate) const CURLPROTO_GOPHER: i64 = 33554432;
    pub(crate) const CURLAUTH_ONLY: i64 = 2147483648;
    pub(crate) const CURLOPT_RESOLVE: i64 = 10203;
    pub(crate) const CURLOPT_TLSAUTH_PASSWORD: i64 = 10205;
    pub(crate) const CURLOPT_TLSAUTH_TYPE: i64 = 10206;
    pub(crate) const CURLOPT_TLSAUTH_USERNAME: i64 = 10204;
    pub(crate) const CURL_TLSAUTH_SRP: i64 = 1;
    pub(crate) const CURL_VERSION_TLSAUTH_SRP: i64 = 16384;
    pub(crate) const CURLOPT_ACCEPT_ENCODING: i64 = 10102;
    pub(crate) const CURLOPT_TRANSFER_ENCODING: i64 = 207;
    pub(crate) const CURLAUTH_NTLM_WB: i64 = 32;
    pub(crate) const CURLGSSAPI_DELEGATION_FLAG: i64 = 2;
    pub(crate) const CURLGSSAPI_DELEGATION_POLICY_FLAG: i64 = 1;
    pub(crate) const CURLOPT_GSSAPI_DELEGATION: i64 = 210;
    pub(crate) const CURL_VERSION_NTLM_WB: i64 = 32768;
    pub(crate) const CURLOPT_ACCEPTTIMEOUT_MS: i64 = 212;
    pub(crate) const CURLOPT_DNS_SERVERS: i64 = 10211;
    pub(crate) const CURLOPT_MAIL_AUTH: i64 = 10217;
    pub(crate) const CURLOPT_SSL_OPTIONS: i64 = 216;
    pub(crate) const CURLOPT_TCP_KEEPALIVE: i64 = 213;
    pub(crate) const CURLOPT_TCP_KEEPIDLE: i64 = 214;
    pub(crate) const CURLOPT_TCP_KEEPINTVL: i64 = 215;
    pub(crate) const CURLSSLOPT_ALLOW_BEAST: i64 = 1;
    pub(crate) const CURL_REDIR_POST_303: i64 = 4;
    pub(crate) const CURLSSH_AUTH_AGENT: i64 = 16;
    pub(crate) const CURLMOPT_CHUNK_LENGTH_PENALTY_SIZE: i64 = 30010;
    pub(crate) const CURLMOPT_CONTENT_LENGTH_PENALTY_SIZE: i64 = 30009;
    pub(crate) const CURLMOPT_MAX_HOST_CONNECTIONS: i64 = 7;
    pub(crate) const CURLMOPT_MAX_PIPELINE_LENGTH: i64 = 8;
    pub(crate) const CURLMOPT_MAX_TOTAL_CONNECTIONS: i64 = 13;
    pub(crate) const CURLOPT_SASL_IR: i64 = 218;
    pub(crate) const CURLOPT_DNS_INTERFACE: i64 = 10221;
    pub(crate) const CURLOPT_DNS_LOCAL_IP4: i64 = 10222;
    pub(crate) const CURLOPT_DNS_LOCAL_IP6: i64 = 10223;
    pub(crate) const CURLOPT_XOAUTH2_BEARER: i64 = 10220;
    pub(crate) const CURL_HTTP_VERSION_2_0: i64 = 3;
    pub(crate) const CURL_VERSION_HTTP2: i64 = 65536;
    pub(crate) const CURLOPT_LOGIN_OPTIONS: i64 = 10224;
    pub(crate) const CURL_SSLVERSION_TLSv1_0: i64 = 4;
    pub(crate) const CURL_SSLVERSION_TLSv1_1: i64 = 5;
    pub(crate) const CURL_SSLVERSION_TLSv1_2: i64 = 6;
    pub(crate) const CURLOPT_EXPECT_100_TIMEOUT_MS: i64 = 227;
    pub(crate) const CURLOPT_SSL_ENABLE_ALPN: i64 = 226;
    pub(crate) const CURLOPT_SSL_ENABLE_NPN: i64 = 225;
    pub(crate) const CURLHEADER_SEPARATE: i64 = 1;
    pub(crate) const CURLHEADER_UNIFIED: i64 = 0;
    pub(crate) const CURLOPT_HEADEROPT: i64 = 229;
    pub(crate) const CURLOPT_PROXYHEADER: i64 = 10228;
    pub(crate) const CURLAUTH_NEGOTIATE: i64 = 4;
    pub(crate) const CURL_VERSION_GSSAPI: i64 = 131072;
    pub(crate) const CURLOPT_PINNEDPUBLICKEY: i64 = 10230;
    pub(crate) const CURLOPT_UNIX_SOCKET_PATH: i64 = 10231;
    pub(crate) const CURLPROTO_SMB: i64 = 67108864;
    pub(crate) const CURLPROTO_SMBS: i64 = 134217728;
    pub(crate) const CURL_VERSION_KERBEROS5: i64 = 262144;
    pub(crate) const CURL_VERSION_UNIX_SOCKETS: i64 = 524288;
    pub(crate) const CURLOPT_SSL_VERIFYSTATUS: i64 = 232;
    pub(crate) const CURLOPT_PATH_AS_IS: i64 = 234;
    pub(crate) const CURLOPT_SSL_FALSESTART: i64 = 233;
    pub(crate) const CURL_HTTP_VERSION_2: i64 = 3;
    pub(crate) const CURLOPT_PIPEWAIT: i64 = 237;
    pub(crate) const CURLOPT_PROXY_SERVICE_NAME: i64 = 10235;
    pub(crate) const CURLOPT_SERVICE_NAME: i64 = 10236;
    pub(crate) const CURLPIPE_NOTHING: i64 = 0;
    pub(crate) const CURLPIPE_HTTP1: i64 = 1;
    pub(crate) const CURLPIPE_MULTIPLEX: i64 = 2;
    pub(crate) const CURLSSLOPT_NO_REVOKE: i64 = 2;
    pub(crate) const CURLOPT_DEFAULT_PROTOCOL: i64 = 10238;
    pub(crate) const CURLOPT_STREAM_WEIGHT: i64 = 239;
    pub(crate) const CURLMOPT_PUSHFUNCTION: i64 = 20014;
    pub(crate) const CURL_PUSH_OK: i64 = 0;
    pub(crate) const CURL_PUSH_DENY: i64 = 1;
    pub(crate) const CURL_HTTP_VERSION_2TLS: i64 = 4;
    pub(crate) const CURL_VERSION_PSL: i64 = 1048576;
    pub(crate) const CURLOPT_TFTP_NO_OPTIONS: i64 = 242;
    pub(crate) const CURL_HTTP_VERSION_2_PRIOR_KNOWLEDGE: i64 = 5;
    pub(crate) const CURLOPT_CONNECT_TO: i64 = 10243;
    pub(crate) const CURLOPT_TCP_FASTOPEN: i64 = 244;
    pub(crate) const CURLINFO_HTTP_VERSION: i64 = 2097198;
    pub(crate) const CURLE_WEIRD_SERVER_REPLY: i64 = 8;
    pub(crate) const CURLOPT_KEEP_SENDING_ON_ERROR: i64 = 245;
    pub(crate) const CURL_SSLVERSION_TLSv1_3: i64 = 7;
    pub(crate) const CURL_VERSION_HTTPS_PROXY: i64 = 2097152;
    pub(crate) const CURLINFO_PROTOCOL: i64 = 2097200;
    pub(crate) const CURLINFO_PROXY_SSL_VERIFYRESULT: i64 = 2097199;
    pub(crate) const CURLINFO_SCHEME: i64 = 1048625;
    pub(crate) const CURLOPT_PRE_PROXY: i64 = 10262;
    pub(crate) const CURLOPT_PROXY_CAINFO: i64 = 10246;
    pub(crate) const CURLOPT_PROXY_CAPATH: i64 = 10247;
    pub(crate) const CURLOPT_PROXY_CRLFILE: i64 = 10260;
    pub(crate) const CURLOPT_PROXY_KEYPASSWD: i64 = 10258;
    pub(crate) const CURLOPT_PROXY_PINNEDPUBLICKEY: i64 = 10263;
    pub(crate) const CURLOPT_PROXY_SSL_CIPHER_LIST: i64 = 10259;
    pub(crate) const CURLOPT_PROXY_SSL_OPTIONS: i64 = 261;
    pub(crate) const CURLOPT_PROXY_SSL_VERIFYHOST: i64 = 249;
    pub(crate) const CURLOPT_PROXY_SSL_VERIFYPEER: i64 = 248;
    pub(crate) const CURLOPT_PROXY_SSLCERT: i64 = 10254;
    pub(crate) const CURLOPT_PROXY_SSLCERTTYPE: i64 = 10255;
    pub(crate) const CURLOPT_PROXY_SSLKEY: i64 = 10256;
    pub(crate) const CURLOPT_PROXY_SSLKEYTYPE: i64 = 10257;
    pub(crate) const CURLOPT_PROXY_SSLVERSION: i64 = 250;
    pub(crate) const CURLOPT_PROXY_TLSAUTH_PASSWORD: i64 = 10252;
    pub(crate) const CURLOPT_PROXY_TLSAUTH_TYPE: i64 = 10253;
    pub(crate) const CURLOPT_PROXY_TLSAUTH_USERNAME: i64 = 10251;
    pub(crate) const CURLPROXY_HTTPS: i64 = 2;
    pub(crate) const CURL_MAX_READ_SIZE: i64 = 10485760;
    pub(crate) const CURLOPT_ABSTRACT_UNIX_SOCKET: i64 = 10264;
    pub(crate) const CURL_SSLVERSION_MAX_DEFAULT: i64 = 65536;
    pub(crate) const CURL_SSLVERSION_MAX_NONE: i64 = 0;
    pub(crate) const CURL_SSLVERSION_MAX_TLSv1_0: i64 = 262144;
    pub(crate) const CURL_SSLVERSION_MAX_TLSv1_1: i64 = 327680;
    pub(crate) const CURL_SSLVERSION_MAX_TLSv1_2: i64 = 393216;
    pub(crate) const CURL_SSLVERSION_MAX_TLSv1_3: i64 = 458752;
    pub(crate) const CURLOPT_SUPPRESS_CONNECT_HEADERS: i64 = 265;
    pub(crate) const CURLAUTH_GSSAPI: i64 = 4;
    pub(crate) const CURLINFO_CONTENT_LENGTH_DOWNLOAD_T: i64 = 6291471;
    pub(crate) const CURLINFO_CONTENT_LENGTH_UPLOAD_T: i64 = 6291472;
    pub(crate) const CURLINFO_SIZE_DOWNLOAD_T: i64 = 6291464;
    pub(crate) const CURLINFO_SIZE_UPLOAD_T: i64 = 6291463;
    pub(crate) const CURLINFO_SPEED_DOWNLOAD_T: i64 = 6291465;
    pub(crate) const CURLINFO_SPEED_UPLOAD_T: i64 = 6291466;
    pub(crate) const CURLOPT_REQUEST_TARGET: i64 = 10266;
    pub(crate) const CURLOPT_SOCKS5_AUTH: i64 = 267;
    pub(crate) const CURLOPT_SSH_COMPRESSION: i64 = 268;
    pub(crate) const CURL_VERSION_MULTI_SSL: i64 = 4194304;
    pub(crate) const CURL_VERSION_BROTLI: i64 = 8388608;
    pub(crate) const CURL_LOCK_DATA_CONNECT: i64 = 5;
    pub(crate) const CURLSSH_AUTH_GSSAPI: i64 = 32;
    pub(crate) const CURLINFO_FILETIME_T: i64 = 6291470;
    pub(crate) const CURLOPT_HAPPY_EYEBALLS_TIMEOUT_MS: i64 = 271;
    pub(crate) const CURLOPT_TIMEVALUE_LARGE: i64 = 30270;
    pub(crate) const CURLOPT_DNS_SHUFFLE_ADDRESSES: i64 = 275;
    pub(crate) const CURLOPT_HAPROXYPROTOCOL: i64 = 274;
    pub(crate) const CURL_LOCK_DATA_PSL: i64 = 6;
    pub(crate) const CURLAUTH_BEARER: i64 = 64;
    pub(crate) const CURLINFO_QUEUE_TIME_T: i64 = 6291521;
    pub(crate) const CURLINFO_APPCONNECT_TIME_T: i64 = 6291512;
    pub(crate) const CURLINFO_CONNECT_TIME_T: i64 = 6291508;
    pub(crate) const CURLINFO_NAMELOOKUP_TIME_T: i64 = 6291507;
    pub(crate) const CURLINFO_PRETRANSFER_TIME_T: i64 = 6291509;
    pub(crate) const CURLINFO_REDIRECT_TIME_T: i64 = 6291511;
    pub(crate) const CURLINFO_STARTTRANSFER_TIME_T: i64 = 6291510;
    pub(crate) const CURLINFO_TOTAL_TIME_T: i64 = 6291506;
    pub(crate) const CURLINFO_USED_PROXY: i64 = 2097218;
    pub(crate) const CURLINFO_POSTTRANSFER_TIME_T: i64 = 6291523;
    pub(crate) const CURLINFO_CONN_ID: i64 = 6291520;
    pub(crate) const CURLOPT_DISALLOW_USERNAME_IN_URL: i64 = 278;
    pub(crate) const CURLOPT_PROXY_TLS13_CIPHERS: i64 = 10277;
    pub(crate) const CURLOPT_TLS13_CIPHERS: i64 = 10276;
    pub(crate) const CURLOPT_DOH_URL: i64 = 10279;
    pub(crate) const CURLOPT_UPKEEP_INTERVAL_MS: i64 = 281;
    pub(crate) const CURLOPT_UPLOAD_BUFFERSIZE: i64 = 280;
    pub(crate) const CURLOPT_HTTP09_ALLOWED: i64 = 285;
    pub(crate) const CURLALTSVC_H1: i64 = 8;
    pub(crate) const CURLALTSVC_H2: i64 = 16;
    pub(crate) const CURLALTSVC_H3: i64 = 32;
    pub(crate) const CURLALTSVC_READONLYFILE: i64 = 4;
    pub(crate) const CURLOPT_ALTSVC: i64 = 10287;
    pub(crate) const CURLOPT_ALTSVC_CTRL: i64 = 286;
    pub(crate) const CURL_VERSION_ALTSVC: i64 = 16777216;
    pub(crate) const CURLOPT_MAXAGE_CONN: i64 = 288;
    pub(crate) const CURLOPT_SASL_AUTHZID: i64 = 10289;
    pub(crate) const CURL_VERSION_HTTP3: i64 = 33554432;
    pub(crate) const CURLINFO_RETRY_AFTER: i64 = 6291513;
    pub(crate) const CURL_HTTP_VERSION_3: i64 = 30;
    pub(crate) const CURLMOPT_MAX_CONCURRENT_STREAMS: i64 = 16;
    pub(crate) const CURLSSLOPT_NO_PARTIALCHAIN: i64 = 4;
    pub(crate) const CURLOPT_MAIL_RCPT_ALLLOWFAILS: i64 = 290;
    pub(crate) const CURLSSLOPT_REVOKE_BEST_EFFORT: i64 = 8;
    pub(crate) const CURLOPT_ISSUERCERT_BLOB: i64 = 40295;
    pub(crate) const CURLOPT_PROXY_ISSUERCERT: i64 = 10296;
    pub(crate) const CURLOPT_PROXY_ISSUERCERT_BLOB: i64 = 40297;
    pub(crate) const CURLOPT_PROXY_SSLCERT_BLOB: i64 = 40293;
    pub(crate) const CURLOPT_PROXY_SSLKEY_BLOB: i64 = 40294;
    pub(crate) const CURLOPT_SSLCERT_BLOB: i64 = 40291;
    pub(crate) const CURLOPT_SSLKEY_BLOB: i64 = 40292;
    pub(crate) const CURLPROTO_MQTT: i64 = 268435456;
    pub(crate) const CURLSSLOPT_NATIVE_CA: i64 = 16;
    pub(crate) const CURL_VERSION_UNICODE: i64 = 134217728;
    pub(crate) const CURL_VERSION_ZSTD: i64 = 67108864;
    pub(crate) const CURLE_PROXY: i64 = 97;
    pub(crate) const CURLINFO_PROXY_ERROR: i64 = 2097211;
    pub(crate) const CURLOPT_SSL_EC_CURVES: i64 = 10298;
    pub(crate) const CURLOPT_SSL_SIGNATURE_ALGORITHMS: i64 = 10328;
    pub(crate) const CURLPX_BAD_ADDRESS_TYPE: i64 = 1;
    pub(crate) const CURLPX_BAD_VERSION: i64 = 2;
    pub(crate) const CURLPX_CLOSED: i64 = 3;
    pub(crate) const CURLPX_GSSAPI: i64 = 4;
    pub(crate) const CURLPX_GSSAPI_PERMSG: i64 = 5;
    pub(crate) const CURLPX_GSSAPI_PROTECTION: i64 = 6;
    pub(crate) const CURLPX_IDENTD: i64 = 7;
    pub(crate) const CURLPX_IDENTD_DIFFER: i64 = 8;
    pub(crate) const CURLPX_LONG_HOSTNAME: i64 = 9;
    pub(crate) const CURLPX_LONG_PASSWD: i64 = 10;
    pub(crate) const CURLPX_LONG_USER: i64 = 11;
    pub(crate) const CURLPX_NO_AUTH: i64 = 12;
    pub(crate) const CURLPX_OK: i64 = 0;
    pub(crate) const CURLPX_RECV_ADDRESS: i64 = 13;
    pub(crate) const CURLPX_RECV_AUTH: i64 = 14;
    pub(crate) const CURLPX_RECV_CONNECT: i64 = 15;
    pub(crate) const CURLPX_RECV_REQACK: i64 = 16;
    pub(crate) const CURLPX_REPLY_ADDRESS_TYPE_NOT_SUPPORTED: i64 = 17;
    pub(crate) const CURLPX_REPLY_COMMAND_NOT_SUPPORTED: i64 = 18;
    pub(crate) const CURLPX_REPLY_CONNECTION_REFUSED: i64 = 19;
    pub(crate) const CURLPX_REPLY_GENERAL_SERVER_FAILURE: i64 = 20;
    pub(crate) const CURLPX_REPLY_HOST_UNREACHABLE: i64 = 21;
    pub(crate) const CURLPX_REPLY_NETWORK_UNREACHABLE: i64 = 22;
    pub(crate) const CURLPX_REPLY_NOT_ALLOWED: i64 = 23;
    pub(crate) const CURLPX_REPLY_TTL_EXPIRED: i64 = 24;
    pub(crate) const CURLPX_REPLY_UNASSIGNED: i64 = 25;
    pub(crate) const CURLPX_REQUEST_FAILED: i64 = 26;
    pub(crate) const CURLPX_RESOLVE_HOST: i64 = 27;
    pub(crate) const CURLPX_SEND_AUTH: i64 = 28;
    pub(crate) const CURLPX_SEND_CONNECT: i64 = 29;
    pub(crate) const CURLPX_SEND_REQUEST: i64 = 30;
    pub(crate) const CURLPX_UNKNOWN_FAIL: i64 = 31;
    pub(crate) const CURLPX_UNKNOWN_MODE: i64 = 32;
    pub(crate) const CURLPX_USER_REJECTED: i64 = 33;
    pub(crate) const CURLHSTS_ENABLE: i64 = 1;
    pub(crate) const CURLHSTS_READONLYFILE: i64 = 2;
    pub(crate) const CURLOPT_HSTS: i64 = 10300;
    pub(crate) const CURLOPT_HSTS_CTRL: i64 = 299;
    pub(crate) const CURL_VERSION_HSTS: i64 = 268435456;
    pub(crate) const CURLAUTH_AWS_SIGV4: i64 = 128;
    pub(crate) const CURLOPT_AWS_SIGV4: i64 = 10305;
    pub(crate) const CURLINFO_REFERER: i64 = 1048636;
    pub(crate) const CURLOPT_DOH_SSL_VERIFYHOST: i64 = 307;
    pub(crate) const CURLOPT_DOH_SSL_VERIFYPEER: i64 = 306;
    pub(crate) const CURLOPT_DOH_SSL_VERIFYSTATUS: i64 = 308;
    pub(crate) const CURL_VERSION_GSASL: i64 = 536870912;
    pub(crate) const CURLOPT_CAINFO_BLOB: i64 = 40309;
    pub(crate) const CURLOPT_PROXY_CAINFO_BLOB: i64 = 40310;
    pub(crate) const CURLSSLOPT_AUTO_CLIENT_CERT: i64 = 32;
    pub(crate) const CURLOPT_MAXLIFETIME_CONN: i64 = 314;
    pub(crate) const CURLOPT_SSH_HOST_PUBLIC_KEY_SHA256: i64 = 10311;
    pub(crate) const CURLOPT_PREREQFUNCTION: i64 = 20312;
    pub(crate) const CURL_PREREQFUNC_OK: i64 = 0;
    pub(crate) const CURL_PREREQFUNC_ABORT: i64 = 1;
    pub(crate) const CURLOPT_MIME_OPTIONS: i64 = 315;
    pub(crate) const CURLMIMEOPT_FORMESCAPE: i64 = 1;
    pub(crate) const CURLOPT_SSH_HOSTKEYFUNCTION: i64 = 20316;
    pub(crate) const CURLOPT_PROTOCOLS_STR: i64 = 10318;
    pub(crate) const CURLOPT_REDIR_PROTOCOLS_STR: i64 = 10319;
    pub(crate) const CURLOPT_WS_OPTIONS: i64 = 320;
    pub(crate) const CURLWS_RAW_MODE: i64 = 1;
    pub(crate) const CURLOPT_CA_CACHE_TIMEOUT: i64 = 321;
    pub(crate) const CURLOPT_QUICK_EXIT: i64 = 322;
    pub(crate) const CURL_HTTP_VERSION_3ONLY: i64 = 31;
    pub(crate) const CURLOPT_SAFE_UPLOAD: i64 = -1;
}

/// The extension value (design §7.1): declarations paired with entries,
/// the constants, the six classes, the ini entry and the state declared
/// on it.
///
/// ```php
/// var_dump(extension_loaded('curl'), ini_get('curl.cainfo'));
/// ```
/// ```expect
/// bool(true)
/// string(0) ""
/// ```
pub fn extension() -> Extension {
    ExtensionBuilder::default()
        .constants(constants::DECLARED)
        .functions(&[
            handle::curl_close::DECLARED,
            handle::curl_copy_handle::DECLARED,
            handle::curl_errno::DECLARED,
            handle::curl_error::DECLARED,
            handle::curl_escape::DECLARED,
            handle::curl_unescape::DECLARED,
            multi::curl_multi_setopt::DECLARED,
            handle::curl_exec::DECLARED,
            file::curl_file_create::DECLARED,
            info::curl_getinfo::DECLARED,
            handle::curl_init::DECLARED,
            handle::curl_upkeep::DECLARED,
            multi::curl_multi_add_handle::DECLARED,
            multi::curl_multi_get_handles::DECLARED,
            multi::curl_multi_close::DECLARED,
            multi::curl_multi_errno::DECLARED,
            multi::curl_multi_exec::DECLARED,
            multi::curl_multi_getcontent::DECLARED,
            multi::curl_multi_info_read::DECLARED,
            multi::curl_multi_init::DECLARED,
            multi::curl_multi_remove_handle::DECLARED,
            multi::curl_multi_select::DECLARED,
            multi::curl_multi_strerror::DECLARED,
            handle::curl_pause::DECLARED,
            handle::curl_reset::DECLARED,
            options::curl_setopt_array::DECLARED,
            options::curl_setopt::DECLARED,
            share::curl_share_close::DECLARED,
            share::curl_share_errno::DECLARED,
            share::curl_share_init::DECLARED,
            share::curl_share_setopt::DECLARED,
            share::curl_share_strerror::DECLARED,
            share::curl_share_init_persistent::DECLARED,
            handle::curl_strerror::DECLARED,
            version::curl_version::DECLARED,
        ])
        .class::<handle::CurlHandle>()
        .class::<multi::CurlMultiHandle>()
        .class::<share::CurlShareHandle>()
        .class::<share::CurlSharePersistentHandle>()
        .class::<file::CURLFile>()
        .class::<file::CURLStringFile>()
        .ini(vec![IniDecl {
            name: Name(Bytes::borrowed(CAINFO)),
            default: Some(Bytes::borrowed(b"")),
            user: false,
            per_dir: false,
            system: true,
        }])
        .process_state(initialize)
        .request_state::<State>()
        .engine_state(|start| {
            Ok(share::Pooled {
                pool: Pool::new(share::POOLED),
                throwables: throwables::Throwables::start(start)?,
            })
        })
        .post_deactivate(post_deactivate)
        .info(version::curl_info)
        .build("curl")
}

/// `post_deactivate`, once the request's objects are freed and with
/// them every `CURL *`: the file parts of the MIME trees the request's
/// link kept closed, as their `free_cb` would have closed them, and the
/// persistent shares given back to the engine's pool
/// ([`share::give_back`]).
fn post_deactivate(req: &mut php_ext::request::Request) {
    let session = req.state::<State>().session.clone();
    session.close_kept(req);
    share::give_back(req);
}

/// The process scope's proof that libcurl's global state stands
/// (ADR-0010).
struct Initialized;

/// `PHP_MINIT_FUNCTION(curl)`'s `curl_global_init(CURL_GLOBAL_DEFAULT)`
/// (`Z/ext/curl/interface.c:357-359`), the process start: called once for
/// the process, the first time an engine carrying curl is built, before
/// any `CURL *` is made. Where the library refuses, php's `MINIT` returns
/// `FAILURE` with nothing raised before it, and the engine says
/// `Unable to start curl module`. php's `curl_global_cleanup` at `MSHUTDOWN` has
/// no counterpart: no host ends the process scope, and the call is
/// documented unsafe while another thread may use the library.
fn initialize() -> Result<Initialized, Refusal> {
    if ffi::global_init() {
        Ok(Initialized)
    } else {
        Err(Refusal::new())
    }
}

/// `curl.cainfo`, `PHP_INI_SYSTEM`.
const CAINFO: &[u8] = b"curl.cainfo";
