//! php's bundled glob, `Z/main/php_glob.c`: the OpenBSD glob(3) php 8.5
//! builds where `PHP_SYSTEM_GLOB` is undefined, as on macOS.
//!
//! A pattern is widened to `Char` (`u16`): the high bits mark a quoted byte
//! (`M_PROTECT`) or a compiled meta character (`M_QUOTE`). The walk keeps
//! php's path buffer and its bounds checks, so a path that would not fit
//! fails where php's fails; its length, the file's `PATH_MAX`, is the
//! host's, which `MAXPATHLEN` is and which the file falls back to
//! `MAXPATHLEN` for (`Z/main/php_glob.c:73`). Only what `glob()` reaches is copied:
//! it passes no `errfunc` and none of `PHP_GLOB_APPEND`, `PHP_GLOB_DOOFFS`,
//! `PHP_GLOB_ALTDIRFUNC`, `PHP_GLOB_TILDE`, `PHP_GLOB_NOMAGIC`,
//! `PHP_GLOB_LIMIT` or `PHP_GLOB_KEEPSTAT`, so `globtilde`, the limits and
//! `PHP_GLOB_MAGCHAR` (read by `PHP_GLOB_NOMAGIC` alone) are left out.

use std::ffi::OsStr;
use std::fs::{self, Metadata, ReadDir};
use std::os::unix::ffi::{OsStrExt, OsStringExt};

use php_ext::request::MAXPATHLEN;

/// `PHP_GLOB_ERR`, `Z/main/php_glob.h:140`: `glob()` accepts it, but it acts
/// only through an `errfunc` (`Z/main/php_glob.c:794`), and `glob()` passes
/// none, so it changes nothing.
pub(super) const PHP_GLOB_ERR: i32 = 0x0004;
/// `PHP_GLOB_MARK`, `Z/main/php_glob.h:141`: append `/` to a directory.
pub(super) const PHP_GLOB_MARK: i32 = 0x0008;
/// `PHP_GLOB_NOCHECK`, `Z/main/php_glob.h:142`: give the pattern back when
/// nothing matches.
pub(super) const PHP_GLOB_NOCHECK: i32 = 0x0010;
/// `PHP_GLOB_NOSORT`, `Z/main/php_glob.h:143`: keep directory order.
pub(super) const PHP_GLOB_NOSORT: i32 = 0x0020;
/// `PHP_GLOB_NOESCAPE`, `Z/main/php_glob.h:144`: `\` is an ordinary byte.
pub(super) const PHP_GLOB_NOESCAPE: i32 = 0x1000;
/// `PHP_GLOB_ABORTED`, `Z/main/php_glob.h:147`: here, the pattern php would
/// read past (`glob0_compile`).
pub(super) const PHP_GLOB_ABORTED: i32 = -2;
/// `PHP_GLOB_NOMATCH`, `Z/main/php_glob.h:148`: nothing matched.
pub(crate) const PHP_GLOB_NOMATCH: i32 = -3;
/// `PHP_GLOB_BRACE`, `Z/main/php_glob.h:152`: expand `{a,b}` as csh does.
pub(super) const PHP_GLOB_BRACE: i32 = 0x0080;
/// `PHP_GLOB_ONLYDIR`, `Z/main/php_glob.h:171`: `glob()` filters for
/// directories itself and masks this bit off before calling `php_glob`.
pub(super) const PHP_GLOB_ONLYDIR: i32 = 1 << 30;
/// `PHP_GLOB_FLAGMASK`, `Z/main/php_glob.h:172`.
pub(super) const PHP_GLOB_FLAGMASK: i32 = !PHP_GLOB_ONLYDIR;
/// `PHP_GLOB_AVAILABLE_FLAGS`, `Z/main/php_glob.h:177`: the bits `glob()`
/// accepts.
pub(super) const PHP_GLOB_AVAILABLE_FLAGS: i32 = PHP_GLOB_BRACE
    | PHP_GLOB_MARK
    | PHP_GLOB_NOSORT
    | PHP_GLOB_NOCHECK
    | PHP_GLOB_NOESCAPE
    | PHP_GLOB_ERR
    | PHP_GLOB_ONLYDIR;

/// `pathend_last`, `Z/main/php_glob.c:696`: the last usable slot of the path
/// buffer.
const PATHEND_LAST: usize = MAXPATHLEN - 1;

/// `Char`, `Z/main/php_glob.c:135`: a pattern byte widened for its marks.
type Char = u16;
/// `M_QUOTE`, `Z/main/php_glob.c:130`: the bit of a compiled meta character.
/// `M_MASK` (0xffff) is the whole `u16` and is left out.
const M_QUOTE: Char = 0x8000;
/// `M_PROTECT`, `Z/main/php_glob.c:131`: the bit of a `\`-quoted byte.
const M_PROTECT: Char = 0x4000;
/// `M_ASCII`, `Z/main/php_glob.c:133`: `CHAR(c)` keeps these bits.
const M_ASCII: Char = 0x00ff;
/// `M_ALL`, `Z/main/php_glob.c:151`: `META('*')`.
const M_ALL: Char = M_QUOTE | 0x2a;
/// `M_END`, `Z/main/php_glob.c:152`: `META(']')`.
const M_END: Char = M_QUOTE | 0x5d;
/// `M_NOT`, `Z/main/php_glob.c:153`: `META('!')`.
const M_NOT: Char = M_QUOTE | 0x21;
/// `M_ONE`, `Z/main/php_glob.c:154`: `META('?')`.
const M_ONE: Char = M_QUOTE | 0x3f;
/// `M_RNG`, `Z/main/php_glob.c:155`: `META('-')`.
const M_RNG: Char = M_QUOTE | 0x2d;
/// `M_SET`, `Z/main/php_glob.c:156`: `META('[')`.
const M_SET: Char = M_QUOTE | 0x5b;
/// `M_CLASS`, `Z/main/php_glob.c:157`: `META(':')`.
const M_CLASS: Char = M_QUOTE | 0x3a;

/// `EOS`, `Z/main/php_glob.c:112`.
const EOS: Char = 0;
/// `DOT`, `Z/main/php_glob.c:111`.
const DOT: Char = 0x2e;
/// `LBRACKET`, `Z/main/php_glob.c:113`.
const LBRACKET: Char = 0x5b;
/// `NOT`, `Z/main/php_glob.c:114`.
const NOT: Char = 0x21;
/// `QUESTION`, `Z/main/php_glob.c:115`.
const QUESTION: Char = 0x3f;
/// `QUOTE`, `Z/main/php_glob.c:116`.
const QUOTE: Char = 0x5c;
/// `RANGE`, `Z/main/php_glob.c:117`.
const RANGE: Char = 0x2d;
/// `RBRACKET`, `Z/main/php_glob.c:118`.
const RBRACKET: Char = 0x5d;
/// `SEP` and `SLASH`, `Z/main/php_glob.c:119` and `:125`: `/` on unix.
const SLASH: Char = 0x2f;
/// `STAR`, `Z/main/php_glob.c:120`.
const STAR: Char = 0x2a;
/// `LBRACE`, `Z/main/php_glob.c:123`.
const LBRACE: Char = 0x7b;
/// `RBRACE`, `Z/main/php_glob.c:124`.
const RBRACE: Char = 0x7d;
/// `COMMA`, `Z/main/php_glob.c:126`.
const COMMA: Char = 0x2c;
/// The `':'` of a character class, `Z/main/php_glob.c:528`.
const COLON: Char = 0x3a;

/// `cclasses`, `Z/main/charclass.h:13`: each class name with its `isctype`
/// answer for bytes 0 to 255 as a bitmap, taken from darwin's ctype under
/// the `C.UTF-8` `LC_CTYPE` php sets at startup (bytes 128 to 255 read as
/// Latin-1 code points, so `0xc3` is alphabetic).
const CCLASSES: [(&[u8], [u64; 4]); 12] = [
    (
        b"alnum",
        [
            0x03ff_0000_0000_0000,
            0x07ff_fffe_07ff_fffe,
            0x0420_0400_0000_0000,
            0xff7f_ffff_ff7f_ffff,
        ],
    ),
    (
        b"alpha",
        [
            0x0000_0000_0000_0000,
            0x07ff_fffe_07ff_fffe,
            0x0420_0400_0000_0000,
            0xff7f_ffff_ff7f_ffff,
        ],
    ),
    (b"blank", [0x0000_0001_0000_0200, 0, 0, 0]),
    (
        b"cntrl",
        [
            0x0000_0000_ffff_ffff,
            0x8000_0000_0000_0000,
            0x0000_2000_ffff_ffff,
            0,
        ],
    ),
    (b"digit", [0x03ff_0000_0000_0000, 0, 0, 0]),
    (
        b"graph",
        [
            0xffff_fffe_0000_0000,
            0x7fff_ffff_ffff_ffff,
            0xffff_dffe_0000_0000,
            0xffff_ffff_ffff_ffff,
        ],
    ),
    (
        b"lower",
        [
            0,
            0x07ff_fffe_0000_0000,
            0x0020_0000_0000_0000,
            0xff7f_ffff_8000_0000,
        ],
    ),
    (
        b"print",
        [
            0xffff_ffff_0000_0000,
            0x7fff_ffff_ffff_ffff,
            0xffff_dfff_0000_0000,
            0xffff_ffff_ffff_ffff,
        ],
    ),
    (
        b"punct",
        [
            0xfc00_fffe_0000_0000,
            0x7800_0001_f800_0001,
            0x88c0_0882_0000_0000,
            0,
        ],
    ),
    (
        b"space",
        [0x0000_0001_0000_3e00, 0, 0x0000_0001_0000_0000, 0],
    ),
    (
        b"upper",
        [0, 0x0000_0000_07ff_fffe, 0, 0x0000_0000_7f7f_ffff],
    ),
    (
        b"xdigit",
        [0x03ff_0000_0000_0000, 0x0000_007e_0000_007e, 0, 0],
    ),
];

/// `php_glob_t`, `Z/main/php_glob.h:116`, less the parts `glob()` never
/// sets: the bits and the paths found so far. `gl_matchc` is only ever
/// incremented and is left out.
struct Glob {
    /// `gl_flags`.
    flags: i32,
    /// `gl_pathv`.
    pathv: Vec<Vec<u8>>,
}

/// `php_glob`, `Z/main/php_glob.c:243`: the paths `pattern` matches, or the
/// nonzero answer php returns (`PHP_GLOB_NOMATCH`, `PHP_GLOB_ABORTED`, or 1
/// when a path overflows `MAXPATHLEN`). `pattern`
/// ends at its first NUL, as a C string does. A relative pattern is walked
/// from the process's working directory. `PHPAPI` in `main/php_glob.h`:
/// what `glob()` expands, and what `GlobIterator` calls directly, as php's
/// `glob://` wrapper (`Z/main/streams/glob_wrapper.c`) does.
///
/// ```php
/// $d = __DIR__ . '/glob-spl'; @mkdir($d); foreach (['b.txt', 'a.txt', 'c.php'] as $f) touch("$d/$f");
/// chdir($d);
/// $g = new GlobIterator('*.txt', FilesystemIterator::KEY_AS_FILENAME);
/// foreach ($g as $k => $v) { echo $k, ' ', $v->getPathname(), "\n"; }
/// var_dump($g->count(), (new GlobIterator("glob://$d/*.php"))->count() === 1, (new GlobIterator('nope*'))->count());
/// ```
/// ```expect
/// a.txt a.txt
/// b.txt b.txt
/// int(2)
/// bool(true)
/// int(0)
/// ```
pub fn php_glob(pattern: &[u8], flags: i32) -> Result<Vec<Vec<u8>>, i32> {
    let pattern = pattern.split(|&b| b == 0).next().unwrap_or_default();
    let mut g = Glob {
        flags,
        pathv: Vec::new(),
    };
    // zend_strnlen(pattern, MAXPATHLEN) == MAXPATHLEN
    if pattern.len() >= MAXPATHLEN {
        return Err(PHP_GLOB_NOMATCH);
    }
    let bufend = MAXPATHLEN - 1;
    let mut patbuf: Vec<Char> = Vec::with_capacity(pattern.len());
    let mut next = 0;
    while patbuf.len() < bufend {
        let Some(&c) = pattern.get(next) else { break };
        next += 1;
        let c = Char::from(c);
        if flags & PHP_GLOB_NOESCAPE != 0 || c != QUOTE {
            patbuf.push(c);
        } else if let Some(&quoted) = pattern.get(next) {
            next += 1;
            patbuf.push(Char::from(quoted) | M_PROTECT);
        } else {
            // A trailing `\` is kept, quoted.
            patbuf.push(QUOTE | M_PROTECT);
        }
    }
    let rv = if flags & PHP_GLOB_BRACE != 0 {
        g.globexp1(&patbuf)
    } else {
        g.glob0(&patbuf)
    };
    if rv == 0 { Ok(g.pathv) } else { Err(rv) }
}

/// The `Char` at `i`, `EOS` at and past the end: the strings of
/// `Z/main/php_glob.c` end in `EOS`, the slices here do not.
fn at(s: &[Char], i: usize) -> Char {
    s.get(i).copied().unwrap_or(EOS)
}

/// A store into a fixed buffer, as `*p = c` in `Z/main/php_glob.c:718`; the
/// bounds checks keep every store inside it.
fn set(buf: &mut [Char], i: usize, c: Char) {
    if let Some(slot) = buf.get_mut(i) {
        *slot = c;
    }
}

/// `g_strchr`, `Z/main/php_glob.c:1101`: the index of the first `ch` at or
/// after `from`.
fn g_strchr(s: &[Char], from: usize, ch: Char) -> Option<usize> {
    let tail = s.get(from..)?;
    tail.iter().position(|&c| c == ch).map(|p| p + from)
}

/// `g_strncmp`, `Z/main/php_glob.c:506`: `strncmp` of `Char`s at `from`
/// against bytes.
fn g_strncmp(s1: &[Char], from: usize, s2: &[u8], n: usize) -> i32 {
    let mut rv = 0;
    for k in 0..n {
        let a = at(s1, from + k);
        let b = s2.get(k).copied().unwrap_or(0);
        rv = i32::from(a) - i32::from(b);
        if rv != 0 || a == EOS {
            break;
        }
    }
    rv
}

/// `g_Ctoc`, `Z/main/php_glob.c:1110`: `Char`s to bytes, each cut to its low
/// byte as C's `char` store does, up to `EOS`; `None` when no `EOS` falls
/// within `len`.
fn g_ctoc(s: &[Char], len: usize) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    for k in 0..len {
        let [low, _] = at(s, k).to_le_bytes();
        if low == 0 {
            return Some(out);
        }
        out.push(low);
    }
    None
}

/// `g_lstat`, `Z/main/php_glob.c:1079`.
fn g_lstat(path: &[Char]) -> Option<Metadata> {
    let buf = g_ctoc(path, MAXPATHLEN)?;
    fs::symlink_metadata(OsStr::from_bytes(&buf)).ok()
}

/// `g_stat`, `Z/main/php_glob.c:1090`.
fn g_stat(path: &[Char]) -> Option<Metadata> {
    let buf = g_ctoc(path, MAXPATHLEN)?;
    fs::metadata(OsStr::from_bytes(&buf)).ok()
}

/// `g_opendir`, `Z/main/php_glob.c:1062`: an empty path is `"."`.
fn g_opendir(path: &[Char]) -> Option<ReadDir> {
    let buf = if at(path, 0) == EOS {
        b".".to_vec()
    } else {
        g_ctoc(path, MAXPATHLEN)?
    };
    fs::read_dir(OsStr::from_bytes(&buf)).ok()
}

/// `isctype` of `cclasses[idx]`, `Z/main/php_glob.c:1008`, for the byte `k`.
fn isctype(idx: Char, k: Char) -> bool {
    let Some((_, bits)) = CCLASSES.get(usize::from(idx)) else {
        return false;
    };
    let Some(word) = bits.get(usize::from(k >> 6)) else {
        return false;
    };
    (word >> (k & 63)) & 1 == 1
}

/// `g_charclass`, `Z/main/php_glob.c:520`: compile the `[:name:]` whose `:`
/// is at `*q`. 1 when it is not a class, -1 for an unknown name, 0 once
/// `M_CLASS` and its index are pushed and `*q` is past the `]`.
fn g_charclass(src: &[Char], q: &mut usize, buf: &mut Vec<Char>) -> i32 {
    let pattern = *q + 1;
    let Some(colon) = g_strchr(src, pattern, COLON) else {
        return 1;
    };
    if at(src, colon + 1) != RBRACKET {
        return 1;
    }
    let len = colon - pattern;
    for (idx, (name, _)) in (0..).zip(CCLASSES.iter()) {
        if g_strncmp(src, pattern, name, len) == 0 && name.len() == len {
            buf.push(M_CLASS);
            buf.push(idx);
            *q += len + 3;
            return 0;
        }
    }
    -1
}

/// `match`, `Z/main/php_glob.c:973`: whether the name at `name[n..]` (to
/// `EOS`) matches `pat[p..patend]`, with Russ Cox's single backtrack point
/// for `*`. A set is read to its `M_END` even past `patend`, as php reads
/// it.
fn glob_match(name: &[Char], mut n: usize, pat: &[Char], mut p: usize, patend: usize) -> bool {
    let mut next: Option<(usize, usize)> = None;
    loop {
        let mut failed = false;
        while p < patend {
            let c = at(pat, p);
            p += 1;
            match c {
                M_ALL => {
                    while p < patend && at(pat, p) == M_ALL {
                        p += 1;
                    }
                    if p == patend {
                        return true;
                    }
                    if at(name, n) == EOS {
                        return false;
                    }
                    next = Some((p - 1, n + 1));
                }
                M_ONE => {
                    let k = at(name, n);
                    n += 1;
                    if k == EOS {
                        failed = true;
                        break;
                    }
                }
                M_SET => {
                    let mut ok = false;
                    let k = at(name, n);
                    n += 1;
                    if k == EOS {
                        failed = true;
                        break;
                    }
                    let negate = at(pat, p) == M_NOT;
                    if negate {
                        p += 1;
                    }
                    // php stops only at M_END; the bound keeps a set that
                    // lost its M_END from reading forever.
                    while p <= pat.len() {
                        let c = at(pat, p);
                        p += 1;
                        if c == M_END {
                            break;
                        }
                        if c == M_CLASS {
                            if isctype(at(pat, p), k) {
                                ok = true;
                            }
                            p += 1;
                        }
                        if at(pat, p) == M_RNG {
                            if c <= k && k <= at(pat, p + 1) {
                                ok = true;
                            }
                            p += 2;
                        } else if c == k {
                            ok = true;
                        }
                    }
                    if ok == negate {
                        failed = true;
                        break;
                    }
                }
                _ => {
                    let k = at(name, n);
                    n += 1;
                    if k != c {
                        failed = true;
                        break;
                    }
                }
            }
        }
        if !failed && at(name, n) == EOS {
            return true;
        }
        match next {
            Some((np, nn)) => {
                p = np;
                n = nn;
            }
            None => return false,
        }
    }
}

impl Glob {
    /// `globexp1`, `Z/main/php_glob.c:306`: expand the first `{`, or glob
    /// the pattern when there is none or it is exactly `{}`.
    fn globexp1(&mut self, pattern: &[Char]) -> i32 {
        if pattern == [LBRACE, RBRACE] {
            return self.glob0(pattern);
        }
        if let Some(ptr) = g_strchr(pattern, 0, LBRACE) {
            return self.globexp2(ptr, pattern);
        }
        self.glob0(pattern)
    }

    /// `globexp2`, `Z/main/php_glob.c:326`: glob each alternative of the
    /// brace at `ptr` in turn. Unbalanced braces glob the part before the
    /// brace alone, as php does.
    fn globexp2(&mut self, ptr: usize, pattern: &[Char]) -> i32 {
        let prefix = pattern.get(..ptr).unwrap_or_default();
        let start = ptr + 1;

        // Find the balanced brace.
        let mut depth = 0usize;
        let mut pe = start;
        loop {
            let c = at(pattern, pe);
            if c == EOS {
                break;
            }
            if c == LBRACKET {
                // Ignore everything between [].
                let pm = pe;
                pe += 1;
                while at(pattern, pe) != RBRACKET && at(pattern, pe) != EOS {
                    pe += 1;
                }
                if at(pattern, pe) == EOS {
                    pe = pm;
                }
            } else if c == LBRACE {
                depth += 1;
            } else if c == RBRACE {
                if depth == 0 {
                    break;
                }
                depth -= 1;
            }
            pe += 1;
        }
        if depth != 0 || at(pattern, pe) == EOS {
            return self.glob0(prefix);
        }

        let rest = pattern.get(pe + 1..).unwrap_or_default();
        let mut depth = 0usize;
        let mut pl = start;
        let mut pm = start;
        while pm <= pe {
            match at(pattern, pm) {
                LBRACKET => {
                    let pb = pm;
                    pm += 1;
                    while at(pattern, pm) != RBRACKET && at(pattern, pm) != EOS {
                        pm += 1;
                    }
                    if at(pattern, pm) == EOS {
                        pm = pb;
                    }
                }
                LBRACE => depth += 1,
                RBRACE if depth != 0 => depth -= 1,
                COMMA if depth != 0 => {}
                RBRACE | COMMA => {
                    let mut patbuf = prefix.to_vec();
                    patbuf.extend_from_slice(pattern.get(pl..pm).unwrap_or_default());
                    patbuf.extend_from_slice(rest);
                    let rv = self.globexp1(&patbuf);
                    if rv != 0 && rv != PHP_GLOB_NOMATCH {
                        return rv;
                    }
                    pl = pm + 1;
                }
                _ => {}
            }
            pm += 1;
        }
        0
    }

    /// `glob0`, `Z/main/php_glob.c:553`: compile one brace-free pattern, walk
    /// it, and sort what it added with `strcmp` (`compare`,
    /// `Z/main/php_glob.c:676`). No match gives `PHP_GLOB_NOMATCH`, or with
    /// `PHP_GLOB_NOCHECK` the pattern itself, its `\` quotes removed.
    fn glob0(&mut self, pattern: &[Char]) -> i32 {
        let oldpathc = self.pathv.len();
        let compiled = match self.glob0_compile(pattern) {
            Ok(compiled) => compiled,
            Err(rv) => return rv,
        };
        let err = self.glob1(&compiled);
        if err != 0 {
            return err;
        }
        if self.pathv.len() == oldpathc {
            if self.flags & PHP_GLOB_NOCHECK != 0 {
                self.globextend(pattern);
                return 0;
            }
            return PHP_GLOB_NOMATCH;
        }
        if self.flags & PHP_GLOB_NOSORT == 0
            && let Some(added) = self.pathv.get_mut(oldpathc..)
        {
            // qsort is not stable, but equal paths are the same bytes.
            added.sort_unstable();
        }
        0
    }

    /// The compile loop of `glob0`, `Z/main/php_glob.c:565`: `[`, `?` and
    /// `*` to their meta `Char`s, everything else to `CHAR(c)`.
    ///
    /// A read beyond the pattern's `EOS` gives `PHP_GLOB_ABORTED`.
    fn glob0_compile(&self, src: &[Char]) -> Result<Vec<Char>, i32> {
        let rd = |i: usize| -> Result<Char, i32> {
            match src.get(i) {
                Some(&c) => Ok(c),
                None if i == src.len() => Ok(EOS),
                None => Err(PHP_GLOB_ABORTED),
            }
        };
        let mut buf: Vec<Char> = Vec::with_capacity(src.len());
        let mut q = 0;
        loop {
            let c = rd(q)?;
            q += 1;
            match c {
                EOS => break,
                LBRACKET => {
                    let first = rd(q)?;
                    if first == NOT {
                        q += 1;
                    }
                    if rd(q)? == EOS || g_strchr(src, q + 1, RBRACKET).is_none() {
                        buf.push(LBRACKET);
                        if first == NOT {
                            q -= 1;
                        }
                        continue;
                    }
                    buf.push(M_SET);
                    if first == NOT {
                        buf.push(M_NOT);
                    }
                    let mut c = rd(q)?;
                    q += 1;
                    loop {
                        if c == LBRACKET && rd(q)? == COLON {
                            let mut err;
                            loop {
                                err = g_charclass(src, &mut q, &mut buf);
                                if err != 0 {
                                    break;
                                }
                                c = rd(q)?;
                                q += 1;
                                if !(c == LBRACKET && rd(q)? == COLON) {
                                    break;
                                }
                            }
                            if err == -1 && self.flags & PHP_GLOB_NOCHECK == 0 {
                                return Err(PHP_GLOB_NOMATCH);
                            }
                            if c == RBRACKET {
                                break;
                            }
                        }
                        buf.push(c & M_ASCII);
                        if rd(q)? == RANGE {
                            let hi = rd(q + 1)?;
                            if hi != RBRACKET {
                                buf.push(M_RNG);
                                buf.push(hi & M_ASCII);
                                q += 2;
                            }
                        }
                        c = rd(q)?;
                        q += 1;
                        if c == RBRACKET {
                            break;
                        }
                    }
                    buf.push(M_END);
                }
                QUESTION => buf.push(M_ONE),
                STAR => {
                    // Collapse adjacent stars to one.
                    if buf.last() != Some(&M_ALL) {
                        buf.push(M_ALL);
                    }
                }
                _ => buf.push(c & M_ASCII),
            }
        }
        Ok(buf)
    }

    /// `glob1`, `Z/main/php_glob.c:689`: an empty pattern matches nothing.
    fn glob1(&mut self, pattern: &[Char]) -> i32 {
        if at(pattern, 0) == EOS {
            return 0;
        }
        let mut pathbuf = vec![EOS; MAXPATHLEN];
        self.glob2(&mut pathbuf, 0, pattern, 0)
    }

    /// `glob2`, `Z/main/php_glob.c:706`: copy segments without meta
    /// characters onto the path, hand the first one with them to `glob3`,
    /// and at the pattern's end add the path if `lstat` finds it.
    fn glob2(
        &mut self,
        pathbuf: &mut [Char],
        mut pathend: usize,
        pattern: &[Char],
        mut p: usize,
    ) -> i32 {
        let mut anymeta = false;
        loop {
            if at(pattern, p) == EOS {
                set(pathbuf, pathend, EOS);
                let Some(sb) = g_lstat(pathbuf) else { return 0 };
                let last = pathend.checked_sub(1).map_or(EOS, |i| at(pathbuf, i));
                if self.flags & PHP_GLOB_MARK != 0
                    && last != SLASH
                    && (sb.is_dir()
                        || (sb.file_type().is_symlink()
                            && g_stat(pathbuf).is_some_and(|s| s.is_dir())))
                {
                    if pathend + 1 > PATHEND_LAST {
                        return 1;
                    }
                    set(pathbuf, pathend, SLASH);
                    pathend += 1;
                    set(pathbuf, pathend, EOS);
                }
                self.globextend(pathbuf);
                return 0;
            }

            // Find the end of the next segment, copied tentatively.
            let mut q = pathend;
            let mut seg = p;
            while at(pattern, seg) != EOS && at(pattern, seg) != SLASH {
                if at(pattern, seg) & M_QUOTE != 0 {
                    anymeta = true;
                }
                if q + 1 > PATHEND_LAST {
                    return 1;
                }
                set(pathbuf, q, at(pattern, seg));
                q += 1;
                seg += 1;
            }

            if anymeta {
                return self.glob3(pathbuf, pathend, pattern, p, seg);
            }
            pathend = q;
            p = seg;
            while at(pattern, p) == SLASH {
                if pathend + 1 > PATHEND_LAST {
                    return 1;
                }
                set(pathbuf, pathend, SLASH);
                pathend += 1;
                p += 1;
            }
        }
    }

    /// `glob3`, `Z/main/php_glob.c:772`: read the directory on the path so
    /// far and go on with `glob2` for each entry that matches the segment
    /// `pattern[p..rest]`. `readdir`'s `.` and `..`, which `read_dir` leaves
    /// out, come first, as darwin returns them.
    fn glob3(
        &mut self,
        pathbuf: &mut [Char],
        pathend: usize,
        pattern: &[Char],
        p: usize,
        rest: usize,
    ) -> i32 {
        if pathend > PATHEND_LAST {
            return 1;
        }
        set(pathbuf, pathend, EOS);
        // With no `errfunc`, a directory that will not open is no match.
        let Some(dirp) = g_opendir(pathbuf) else {
            return 0;
        };
        let names = [b".".to_vec(), b"..".to_vec()]
            .into_iter()
            .chain(dirp.map_while(Result::ok).map(|e| e.file_name().into_vec()));

        let mut err = 0;
        for name in names {
            // An initial dot must be matched by a dot.
            if name.first() == Some(&b'.') && at(pattern, p) != DOT {
                continue;
            }
            let mut dc = pathend;
            let mut sc = 0;
            while dc < PATHEND_LAST {
                let ch = name.get(sc).map_or(EOS, |&b| Char::from(b));
                sc += 1;
                set(pathbuf, dc, ch);
                dc += 1;
                if ch == EOS {
                    break;
                }
            }
            if dc >= PATHEND_LAST {
                set(pathbuf, dc, EOS);
                err = 1;
                break;
            }
            if !glob_match(pathbuf, pathend, pattern, p, rest) {
                set(pathbuf, pathend, EOS);
                continue;
            }
            // `dc` is one past the copied EOS, so at least `pathend + 1`.
            err = self.glob2(pathbuf, dc - 1, pattern, rest);
            if err != 0 {
                break;
            }
        }
        err
    }

    /// `globextend`, `Z/main/php_glob.c:869`: add `path`, to its `EOS`. The
    /// `SSIZE_MAX` checks cannot fire for a list that fits in memory, and
    /// the copy cannot miss its `EOS`.
    fn globextend(&mut self, path: &[Char]) {
        let len = path.iter().position(|&c| c == EOS).unwrap_or(path.len()) + 1;
        if let Some(copy) = g_ctoc(path, len) {
            self.pathv.push(copy);
        }
    }
}
