//! `Z/ext/standard/dir.c`'s constants, `dir.stub.php` in its order: the
//! two separators, the `glob()` flags at the values the host's
//! `glob.h` gives them, with php's own `GLOB_ONLYDIR` where the host
//! has none, and the three `scandir()` orders.

use php_ext::decl::{ConstDecl, ConstExpr, Name};

use crate::constant;

/// `DEFAULT_SLASH` and `ZEND_PATHS_SEPARATOR`.
const DIRECTORY_SEPARATOR: &[u8] = if cfg!(windows) { b"\\" } else { b"/" };
const PATH_SEPARATOR: &[u8] = if cfg!(windows) { b";" } else { b":" };

/// `GLOB_BRACE`, `GLOB_ERR`, `GLOB_MARK`, `GLOB_NOCHECK`,
/// `GLOB_NOESCAPE`, `GLOB_NOSORT` and `GLOB_ONLYDIR` as `dir.c` takes
/// them: glibc's on Linux, the BSD `glob.h`'s elsewhere, where
/// `GLOB_ONLYDIR` is php's `1 << 30`.
const GLOB: [i64; 7] = if cfg!(target_os = "linux") {
    [1 << 10, 1 << 0, 1 << 1, 1 << 4, 1 << 6, 1 << 2, 1 << 13]
} else {
    [0x0080, 0x0004, 0x0008, 0x0010, 0x1000, 0x0020, 1 << 30]
};

/// The constants, in the stub's order; `GLOB_AVAILABLE_FLAGS` is the
/// union of the seven.
///
/// ```php
/// var_dump(DIRECTORY_SEPARATOR, PATH_SEPARATOR, GLOB_BRACE, GLOB_ERR, GLOB_MARK, GLOB_NOCHECK, GLOB_NOESCAPE, GLOB_NOSORT, GLOB_ONLYDIR, GLOB_AVAILABLE_FLAGS, SCANDIR_SORT_ASCENDING, SCANDIR_SORT_DESCENDING, SCANDIR_SORT_NONE);
/// ```
/// ```expect
/// string(1) "/"
/// string(1) ":"
/// int(128)
/// int(4)
/// int(8)
/// int(16)
/// int(4096)
/// int(32)
/// int(1073741824)
/// int(1073746108)
/// int(0)
/// int(1)
/// int(2)
/// ```
pub(crate) fn constants() -> Vec<ConstDecl> {
    let string = |name: &[u8], value: &[u8]| ConstDecl {
        name: Name(name.into()),
        value: ConstExpr::Str(value.into()),
        attributes: Vec::new(),
    };
    let [brace, err, mark, nocheck, noescape, nosort, onlydir] = GLOB;
    vec![
        string(b"DIRECTORY_SEPARATOR", DIRECTORY_SEPARATOR),
        string(b"PATH_SEPARATOR", PATH_SEPARATOR),
        constant(b"GLOB_BRACE", brace),
        constant(b"GLOB_ERR", err),
        constant(b"GLOB_MARK", mark),
        constant(b"GLOB_NOCHECK", nocheck),
        constant(b"GLOB_NOESCAPE", noescape),
        constant(b"GLOB_NOSORT", nosort),
        constant(b"GLOB_ONLYDIR", onlydir),
        constant(
            b"GLOB_AVAILABLE_FLAGS",
            brace | err | mark | nocheck | noescape | nosort | onlydir,
        ),
        constant(b"SCANDIR_SORT_ASCENDING", 0),
        constant(b"SCANDIR_SORT_DESCENDING", 1),
        constant(b"SCANDIR_SORT_NONE", 2),
    ]
}
