//! `Z/ext/standard/pageinfo.c`: `getmyuid`, `getmygid`, `getmypid`,
//! `getmyinode` and `getlastmod`, the first two and the last two over
//! `php_statpage`, which stats the file the request serves once and
//! keeps what it found for the rest of the request, `BG(page_uid)` and
//! its kin.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use std::os::unix::fs::MetadataExt;

use php_ext::abi::OrFalse;
use php_ext::attr::php_function;
use php_ext::process;
use php_ext::request::Request;

use crate::State;

/// `BG(page_uid)`, `BG(page_gid)`, `BG(page_inode)` and `BG(page_mtime)`
/// once `php_statpage` has set them; `-1` is php's unset.
#[derive(Clone, Copy)]
pub(crate) struct Page {
    uid: i64,
    gid: i64,
    inode: i64,
    mtime: i64,
}

/// `php_statpage` (`Z/ext/standard/pageinfo.c:48`): the served file's
/// owner, group, inode and modification time, `sapi_get_stat`'s
/// `stat(2)` of the path the request was given; where there is none, as
/// under `php -r`, the process's user and group, and the other two
/// unset.
fn statpage(req: &mut Request) -> Page {
    if let Some(page) = req.state::<State>().page {
        return page;
    }
    let meta = req
        .environment()
        .script_name()
        .map(<[u8]>::to_vec)
        .and_then(|script| std::fs::metadata(req.environment().absolutize(&script)).ok());
    let page = match meta {
        Some(meta) => Page {
            uid: i64::from(meta.uid()),
            gid: i64::from(meta.gid()),
            inode: i64::try_from(meta.ino()).unwrap_or(-1),
            mtime: meta.mtime(),
        },
        None => Page {
            uid: i64::from(process::uid()),
            gid: i64::from(process::gid()),
            inode: -1,
            mtime: -1,
        },
    };
    req.state::<State>().page = Some(page);
    page
}

/// `php_getuid`, which `mail`'s `X-PHP-Originating-Script` reads.
pub(super) fn getuid(req: &mut Request) -> i64 {
    statpage(req).uid
}

/// A number, or `false` for one below zero, as each native here answers.
fn long_or_false(n: i64) -> OrFalse<i64> {
    if n < 0 {
        OrFalse::False
    } else {
        OrFalse::Value(n)
    }
}

/// `getmyuid(): int|false`, `PHP_FUNCTION(getmyuid)`
/// (`Z/ext/standard/pageinfo.c:83`): the owner of the file the request
/// serves.
///
/// ```php
/// var_dump(getmyuid() === fileowner(__FILE__), getmygid() === filegroup(__FILE__));
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// ```
///
/// ```php
/// echo new ReflectionFunction('getmyuid');
/// ```
/// ```expect
/// Function [ <internal:standard> function getmyuid ] {
///
///   - Parameters [0] {
///   }
///   - Return [ int|false ]
/// }
/// ```
#[php_function]
fn getmyuid(req: &mut Request) -> OrFalse<i64> {
    long_or_false(statpage(req).uid)
}

/// `getmygid(): int|false`, `PHP_FUNCTION(getmygid)`
/// (`Z/ext/standard/pageinfo.c:98`): its group.
///
/// ```php
/// var_dump(is_int(getmygid()));
/// ```
/// ```expect
/// bool(true)
/// ```
///
/// ```php
/// echo new ReflectionFunction('getmygid');
/// ```
/// ```expect
/// Function [ <internal:standard> function getmygid ] {
///
///   - Parameters [0] {
///   }
///   - Return [ int|false ]
/// }
/// ```
#[php_function]
fn getmygid(req: &mut Request) -> OrFalse<i64> {
    long_or_false(statpage(req).gid)
}

/// `getmypid(): int|false`, `PHP_FUNCTION(getmypid)`
/// (`Z/ext/standard/pageinfo.c:112`): `getpid(2)`, which differs from
/// one process to the next, so the witness pins its shape.
///
/// ```php
/// var_dump(getmypid() > 0, getmypid() === getmypid());
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// ```
///
/// ```php
/// echo new ReflectionFunction('getmypid');
/// ```
/// ```expect
/// Function [ <internal:standard> function getmypid ] {
///
///   - Parameters [0] {
///   }
///   - Return [ int|false ]
/// }
/// ```
#[php_function]
fn getmypid() -> OrFalse<i64> {
    OrFalse::Value(i64::from(std::process::id()))
}

/// `getmyinode(): int|false`, `PHP_FUNCTION(getmyinode)`
/// (`Z/ext/standard/pageinfo.c:128`): the served file's inode.
///
/// ```php
/// var_dump(getmyinode() === fileinode(__FILE__));
/// ```
/// ```expect
/// bool(true)
/// ```
///
/// ```php
/// echo new ReflectionFunction('getmyinode');
/// ```
/// ```expect
/// Function [ <internal:standard> function getmyinode ] {
///
///   - Parameters [0] {
///   }
///   - Return [ int|false ]
/// }
/// ```
#[php_function]
fn getmyinode(req: &mut Request) -> OrFalse<i64> {
    long_or_false(statpage(req).inode)
}

/// `getlastmod(): int|false`, `PHP_FUNCTION(getlastmod)`
/// (`Z/ext/standard/pageinfo.c:149`): the served file's modification
/// time, as `php_statpage` first read it.
///
/// ```php
/// $t = getlastmod(); touch(__FILE__, 1000);
/// var_dump($t === filemtime(__FILE__) || $t > 1000, getlastmod() === $t);
/// ```
/// ```expect
/// bool(true)
/// bool(true)
/// ```
///
/// ```php
/// echo new ReflectionFunction('getlastmod');
/// ```
/// ```expect
/// Function [ <internal:standard> function getlastmod ] {
///
///   - Parameters [0] {
///   }
///   - Return [ int|false ]
/// }
/// ```
#[php_function]
fn getlastmod(req: &mut Request) -> OrFalse<i64> {
    long_or_false(statpage(req).mtime)
}
