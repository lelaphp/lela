//! `Z/ext/standard/iptc.c`: `iptcparse` over an IPTC block and
//! `iptcembed`, which copies a JPEG a byte at a time and writes a new
//! `APP13` Photoshop block at its first `APP0` or `APP1`, spooling the
//! copy to the `output`, to the string it answers, or to both.
// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use php_ext::abi::{OrFalse, Path, Union};
use php_ext::attr::php_function;
use php_ext::raising::{Level, Thrown, argument_error, docref};
use php_ext::request::Request;
use php_ext::values::{Owner, PhpArray, PhpString};

/// The JPEG markers `iptcembed` reads.
const M_EOI: u32 = 0xd9;
const M_SOS: u32 = 0xda;
const M_APP0: u32 = 0xe0;
const M_APP1: u32 = 0xe1;
const M_APP13: u32 = 0xed;

/// `psheader` (`Z/ext/standard/iptc.c:180`): the `APP13` marker, its
/// length's two bytes, and the Photoshop resource header.
const PSHEADER: &[u8; 28] = b"\xFF\xED\0\0Photoshop 3.0\x008BIM\x04\x04\0\0\0\0";

/// The file `iptcembed` reads and where its bytes go: `spool` above zero
/// writes each to the `output`, and below two keeps them in `buf`, whose
/// room is php's `spoolbuf`, a byte past which ends the copy.
struct Embedding {
    data: Vec<u8>,
    at: usize,
    spool: i64,
    out: Vec<u8>,
    buf: Option<Vec<u8>>,
    room: usize,
}

impl Embedding {
    /// `php_iptc_put1`: `false` for php's `EOF`, a full buffer.
    fn put1(&mut self, spooled: bool, c: u8) -> bool {
        if spooled && self.spool > 0 {
            self.out.push(c);
        }
        if spooled && let Some(buf) = &mut self.buf {
            if buf.len() >= self.room {
                return false;
            }
            buf.push(c);
        }
        true
    }

    /// `php_iptc_get1`: the next byte, copied where `spooled` asks, or
    /// `None` for `EOF`.
    fn get1(&mut self, spooled: bool) -> Option<u8> {
        let c = *self.data.get(self.at)?;
        self.at += 1;
        self.put1(spooled, c).then_some(c)
    }

    /// `php_iptc_read_remaining`.
    fn read_remaining(&mut self, spooled: bool) {
        while self.get1(spooled).is_some() {}
    }

    /// `php_iptc_skip_variable`: the segment's length and its body, the
    /// length counted with its own two bytes as a `unsigned int` does.
    fn skip_variable(&mut self, spooled: bool) {
        let (Some(c1), Some(c2)) = (self.get1(spooled), self.get1(spooled)) else {
            return;
        };
        let mut length = (u32::from(c1) << 8)
            .wrapping_add(u32::from(c2))
            .wrapping_sub(2);
        while length > 0 {
            length -= 1;
            if self.get1(spooled).is_none() {
                return;
            }
        }
    }

    /// `php_iptc_next_marker`: up to the next `0xff`, then the marker,
    /// the padding `0xff`s put back as php puts them.
    fn next_marker(&mut self) -> u32 {
        let Some(mut c) = self.get1(true) else {
            return M_EOI;
        };
        while c != 0xff {
            let Some(next) = self.get1(true) else {
                return M_EOI;
            };
            c = next;
        }
        loop {
            let Some(c) = self.get1(false) else {
                return M_EOI;
            };
            if c != 0xff {
                return u32::from(c);
            }
            self.put1(true, c);
        }
    }
}

/// `iptcembed(string $iptc_data, string $filename, int $spool = 0): string|bool`,
/// `PHP_FUNCTION(iptcembed)` (`Z/ext/standard/iptc.c:183`): the JPEG
/// copied with the data in a new `APP13` after its first `APP0` or `APP1`
/// and any old `APP13` dropped; the copy answered as a string below
/// `spool` 2, written to the `output` above 0; `false` for a file that
/// does not open, is no JPEG, or outgrows php's buffer.
///
/// ```php
/// $jpg = __DIR__ . '/iptc.jpg';
/// file_put_contents($jpg, hex2bin('ffd8ffe000104a46494600010100000100010000ffc0000b080005000701011100ffda0008010100003f00d2cf20ffd9'));
/// $data = "\x1c\x02\x05\x00\x04Tree";
/// $out = iptcembed($data, $jpg);
/// var_dump(bin2hex($out), iptcparse(substr($out, 20 + 30, 9)), iptcembed($data, __DIR__ . '/nope.jpg'), iptcembed($data, __FILE__));
/// ```
/// ```expect
/// Warning: iptcembed(): Unable to open %s/nope.jpg in %s on line 6
/// string(176) "ffd8ffe000104a46494600010100000100010000ffed002650686f746f73686f7020332e30003842494d040400000000000a1c020500045472656500ffc0000b080005000701011100ffda0008010100003f00d2cf20ffd9"
/// array(1) {
///   ["2#005"]=>
///   array(1) {
///     [0]=>
///     string(4) "Tree"
///   }
/// }
/// bool(false)
/// bool(false)
/// ```
///
/// The buffer is sized by what `fstat` says the file holds, so a FIFO,
/// which says nothing, keeps the data, the header and 1025 bytes more:
///
/// ```php
/// $p = __DIR__ . '/fifo.jpg'; posix_mkfifo($p, 0600);
/// $w = proc_open(['/bin/sh', '-c', '{ printf "\377\330\377\340\000\020JFIF"; head -c 3000 /dev/zero; } > "$0"', $p], [], $pipes);
/// var_dump(strlen(iptcembed('A', $p)), proc_close($w));
/// ```
/// ```expect
/// int(1055)
/// int(0)
/// ```
///
/// ```php
/// echo new ReflectionFunction('iptcembed');
/// ```
/// ```expect
/// Function [ <internal:standard> function iptcembed ] {
///
///   - Parameters [3] {
///     Parameter #0 [ <required> string $iptc_data ]
///     Parameter #1 [ <required> string $filename ]
///     Parameter #2 [ <optional> int $spool = 0 ]
///   }
///   - Return [ string|bool ]
/// }
/// ```
#[php_function]
fn iptcembed(
    req: &mut Request,
    iptc_data: PhpString,
    filename: Path,
    #[php(default = 0)] spool: i64,
) -> Result<Union<Owner<PhpString>, bool>, Thrown> {
    let (data, Path(file)) = (iptc_data.as_bytes().to_vec(), filename);
    let Ok(jpeg) = req.check_open_basedir(file.as_bytes(), true) else {
        return Ok(Union::B(false));
    };
    if data.len() >= usize::MAX - PSHEADER.len() - 1026 {
        return Err(argument_error(req, "ValueError", 1, "is too large"));
    }
    let Ok(mut fp) = std::fs::File::open(&jpeg) else {
        let text = format!(
            "Unable to open {}",
            String::from_utf8_lossy(file.as_bytes())
        );
        docref(req, Level::Warning, &text);
        return Ok(Union::B(false));
    };
    // php sizes its buffer by `fstat`'s `st_size`, which a FIFO answers
    // zero, and reads the file to its end (GH-20582).
    let Ok(st_size) = fp.metadata().map(|sb| sb.len()) else {
        return Ok(Union::B(false));
    };
    let mut bytes = Vec::new();
    if std::io::Read::read_to_end(&mut fp, &mut bytes).is_err() {
        return Ok(Union::B(false));
    }
    let st_size = usize::try_from(st_size).unwrap_or(usize::MAX);
    let room = (data.len() + PSHEADER.len() + 1 + 1024 + 1).saturating_add(st_size);
    let mut s = Embedding {
        data: bytes,
        at: 0,
        spool,
        out: Vec::new(),
        buf: (spool < 2).then(Vec::new),
        room,
    };
    let embedded = embed(&mut s, &data);
    if !s.out.is_empty() {
        req.response_mut().write(&s.out);
    }
    if !embedded {
        return Ok(Union::B(false));
    }
    match s.buf {
        Some(buf) => PhpString::new(req, &buf).map(Union::A),
        None => Ok(Union::B(true)),
    }
}

/// The copy loop of `PHP_FUNCTION(iptcembed)`: `false` where php
/// answers `false`.
fn embed(s: &mut Embedding, data: &[u8]) -> bool {
    if s.get1(true) != Some(0xff) || s.get1(true) != Some(0xd8) {
        return false;
    }
    let mut written = false;
    loop {
        let marker = s.next_marker();
        if marker == M_EOI {
            return true;
        }
        if marker != M_APP13 && !s.put1(true, marker.to_le_bytes()[0]) {
            return false;
        }
        match marker {
            M_APP13 => {
                s.skip_variable(false);
                s.at += 1;
                s.read_remaining(true);
                return true;
            }
            M_APP0 | M_APP1 => {
                if written {
                    continue;
                }
                written = true;
                s.skip_variable(true);
                let mut len = data.len();
                if len & 1 == 1 {
                    len += 1;
                }
                let mut header = *PSHEADER;
                let total = len + 28;
                header[2] = total.to_be_bytes()[6];
                header[3] = total.to_be_bytes()[7];
                let bytes = len.to_be_bytes();
                // The even length reads one byte past the data, which is
                // the NUL php's string ends in.
                let body = (0..len).map(|i| data.get(i).copied().unwrap_or(0));
                let all: Vec<u8> = header
                    .iter()
                    .copied()
                    .chain([bytes[6], bytes[7]])
                    .chain(body)
                    .collect();
                for c in all {
                    if !s.put1(true, c) {
                        return false;
                    }
                }
            }
            M_SOS => {
                s.read_remaining(true);
                return true;
            }
            _ => s.skip_variable(true),
        }
    }
}

/// `iptcparse(string $iptc_block): array|false`, `PHP_FUNCTION(iptcparse)`
/// (`Z/ext/standard/iptc.c:327`): from the first `0x1c` followed by
/// record 1 or 2, each tag's data under `record#dataset`, a long tag's
/// length in four bytes, until a byte that is no tag or a length that
/// runs past the end; `false` where no tag is found.
///
/// ```php
/// var_dump(iptcparse("junk\x1c\x02\x05\x00\x04Tree\x1c\x02\x19\x00\x01a\x1c\x02\x19\x00\x01b"), iptcparse("none"), iptcparse("\x1c\x02\x05\x00\x09short"));
/// ```
/// ```expect
/// array(2) {
///   ["2#005"]=>
///   array(1) {
///     [0]=>
///     string(4) "Tree"
///   }
///   ["2#025"]=>
///   array(2) {
///     [0]=>
///     string(1) "a"
///     [1]=>
///     string(1) "b"
///   }
/// }
/// bool(false)
/// bool(false)
/// ```
///
/// ```php
/// echo new ReflectionFunction('iptcparse');
/// ```
/// ```expect
/// Function [ <internal:standard> function iptcparse ] {
///
///   - Parameters [1] {
///     Parameter #0 [ <required> string $iptc_block ]
///   }
///   - Return [ array|false ]
/// }
/// ```
#[php_function]
fn iptcparse(req: &mut Request, iptc_block: PhpString) -> Result<OrFalse<Owner<PhpArray>>, Thrown> {
    let block = iptc_block;
    let buffer = block.as_bytes();
    let len = buffer.len();
    let at = |i: usize| buffer.get(i).copied().unwrap_or(0);
    let mut inx = 0;
    while inx < len && !(at(inx) == 0x1c && matches!(at(inx + 1), 1 | 2)) {
        inx += 1;
    }
    let mut tags: Vec<(String, Vec<Vec<u8>>)> = Vec::new();
    while inx < len {
        let tag = at(inx);
        inx += 1;
        if tag != 0x1c || inx + 4 >= len {
            break;
        }
        let dataset = at(inx);
        let recnum = at(inx + 1);
        inx += 2;
        let size = if at(inx) & 0x80 != 0 {
            if inx + 6 >= len {
                break;
            }
            let size = u32::from_be_bytes([at(inx + 2), at(inx + 3), at(inx + 4), at(inx + 5)]);
            inx += 6;
            usize::try_from(size).unwrap_or(usize::MAX)
        } else {
            let size = usize::from(u16::from_be_bytes([at(inx), at(inx + 1)]));
            inx += 2;
            size
        };
        if size > len || inx + size > len {
            break;
        }
        let key = format!("{dataset}#{recnum:03}");
        let value = buffer[inx..inx + size].to_vec();
        match tags.iter_mut().find(|(k, _)| *k == key) {
            Some((_, values)) => values.push(value),
            None => tags.push((key, vec![value])),
        }
        inx += size;
    }
    if tags.is_empty() {
        return Ok(OrFalse::False);
    }
    let mut array = PhpArray::new(req)?;
    for (key, values) in tags {
        let mut list = PhpArray::new(req)?;
        for value in values {
            let value = PhpString::new(req, &value)?.into_value();
            list.append(req, value)?;
        }
        array.set_str(req, key.as_bytes(), list.into_value())?;
    }
    Ok(OrFalse::Value(array))
}
