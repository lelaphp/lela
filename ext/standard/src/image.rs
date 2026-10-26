//! `Z/ext/standard/image.c`: `getimagesize`, `getimagesizefromstring`,
//! `image_type_to_mime_type` and `image_type_to_extension`, with the
//! `IMAGETYPE_*` constants.
//!
//! php reads the image through a `php_stream`; here the whole file is a byte
//! slice behind [`Stream`], a cursor whose reads come back short at the end
//! exactly as a plain-file or memory stream does at eof. The diagnostics php
//! raises on the way are handed back in order as [`Warning`]s, for the
//! native to raise.

// The `expect` fence's `exit=` is read by `xtask witness`, not by rustdoc (design §8).
#![allow(rustdoc::invalid_codeblock_attributes)]

use php_ext::abi::{OrFalse, Path, Ref, Untyped};
use php_ext::attr::{php_const, php_function};
use php_ext::coerce::is_space;
use php_ext::decl::{ConstDecl, ConstExpr, Name};
use php_ext::raising::Thrown;
use php_ext::request::{OwnedStream as _, Request, Streams as _};
use php_ext::values as php;

use constants::{
    IMAGETYPE_AVIF, IMAGETYPE_BMP, IMAGETYPE_GIF, IMAGETYPE_HEIF, IMAGETYPE_ICO, IMAGETYPE_IFF,
    IMAGETYPE_JB2, IMAGETYPE_JP2, IMAGETYPE_JPC, IMAGETYPE_JPEG, IMAGETYPE_JPX, IMAGETYPE_PNG,
    IMAGETYPE_PSD, IMAGETYPE_SWC, IMAGETYPE_SWF, IMAGETYPE_TIFF_II, IMAGETYPE_TIFF_MM,
    IMAGETYPE_UNKNOWN, IMAGETYPE_WBMP, IMAGETYPE_WEBP, IMAGETYPE_XBM,
};

mod avif;

/// `IMAGE_FILETYPE_FIXED_COUNT`: the first number an extension's
/// `php_image_register_handler` hands out (ext/libxml takes it for SVG).
const TYPE_FIXED_COUNT: i64 = 21;

/// The `IMAGETYPE_*` constants in the order
/// `Z/ext/standard/basic_functions_arginfo.h:3626` registers them.
///
/// `IMAGETYPE_SWC` is registered only when php links zlib in (not as a
/// shared extension); it is always here, since SWC is always decoded.
/// `IMAGETYPE_COUNT` is the fixed count; each `php_image_register_handler`
/// call raises it by one and adds its own constant (ext/libxml adds
/// `IMAGETYPE_SVG` = 21, making the count 22).
#[php_const]
pub(crate) mod constants {
    /// `IMAGE_FILETYPE_GIF`.
    pub(crate) const IMAGETYPE_GIF: i64 = 1;
    /// `IMAGE_FILETYPE_JPEG`.
    pub(crate) const IMAGETYPE_JPEG: i64 = 2;
    /// `IMAGE_FILETYPE_PNG`.
    pub(crate) const IMAGETYPE_PNG: i64 = 3;
    /// `IMAGE_FILETYPE_SWF`.
    pub(crate) const IMAGETYPE_SWF: i64 = 4;
    /// `IMAGE_FILETYPE_PSD`.
    pub(crate) const IMAGETYPE_PSD: i64 = 5;
    /// `IMAGE_FILETYPE_BMP`.
    pub(crate) const IMAGETYPE_BMP: i64 = 6;
    /// `IMAGE_FILETYPE_TIFF_II` (Intel byte order).
    pub(crate) const IMAGETYPE_TIFF_II: i64 = 7;
    /// `IMAGE_FILETYPE_TIFF_MM` (Motorola byte order).
    pub(crate) const IMAGETYPE_TIFF_MM: i64 = 8;
    /// `IMAGE_FILETYPE_JPC`.
    pub(crate) const IMAGETYPE_JPC: i64 = 9;
    /// `IMAGE_FILETYPE_JP2`.
    pub(crate) const IMAGETYPE_JP2: i64 = 10;
    /// `IMAGE_FILETYPE_JPX`; never detected, only named.
    pub(crate) const IMAGETYPE_JPX: i64 = 11;
    /// `IMAGE_FILETYPE_JB2`; never detected, only named.
    pub(crate) const IMAGETYPE_JB2: i64 = 12;
    /// `IMAGE_FILETYPE_SWC` (zlib-compressed SWF).
    pub(crate) const IMAGETYPE_SWC: i64 = 13;
    /// `IMAGE_FILETYPE_IFF`.
    pub(crate) const IMAGETYPE_IFF: i64 = 14;
    /// `IMAGE_FILETYPE_WBMP`.
    pub(crate) const IMAGETYPE_WBMP: i64 = 15;
    pub(crate) const IMAGETYPE_JPEG2000: i64 = IMAGETYPE_JPC;
    /// `IMAGE_FILETYPE_XBM`.
    pub(crate) const IMAGETYPE_XBM: i64 = 16;
    /// `IMAGE_FILETYPE_ICO`.
    pub(crate) const IMAGETYPE_ICO: i64 = 17;
    /// `IMAGE_FILETYPE_WEBP`.
    pub(crate) const IMAGETYPE_WEBP: i64 = 18;
    /// `IMAGE_FILETYPE_AVIF`.
    pub(crate) const IMAGETYPE_AVIF: i64 = 19;
    /// `IMAGE_FILETYPE_HEIF`.
    pub(crate) const IMAGETYPE_HEIF: i64 = 20;
    /// `IMAGE_FILETYPE_UNKNOWN`, `Z/ext/standard/php_image.h:28`.
    pub(crate) const IMAGETYPE_UNKNOWN: i64 = 0;
    /// The fixed count, which each registered handler raises by one as
    /// the engine links the constants ([`super::link_constants`]).
    pub(crate) const IMAGETYPE_COUNT: i64 = super::TYPE_FIXED_COUNT;
}

// ---------------------------------------------------------------------------
// Signatures, `Z/ext/standard/image.c:36`.

const SIG_GIF: [u8; 3] = *b"GIF";
const SIG_PSD: [u8; 4] = *b"8BPS";
const SIG_BMP: [u8; 2] = *b"BM";
const SIG_SWF: [u8; 3] = *b"FWS";
const SIG_SWC: [u8; 3] = *b"CWS";
const SIG_JPG: [u8; 3] = [0xff, 0xd8, 0xff];
const SIG_PNG: [u8; 8] = [0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];
const SIG_TIF_II: [u8; 4] = [b'I', b'I', 0x2a, 0x00];
const SIG_TIF_MM: [u8; 4] = [b'M', b'M', 0x00, 0x2a];
const SIG_JPC: [u8; 3] = [0xff, 0x4f, 0xff];
const SIG_JP2: [u8; 12] = [
    0x00, 0x00, 0x00, 0x0c, 0x6a, 0x50, 0x20, 0x20, 0x0d, 0x0a, 0x87, 0x0a,
];
const SIG_IFF: [u8; 4] = *b"FORM";
const SIG_ICO: [u8; 4] = [0x00, 0x00, 0x01, 0x00];
const SIG_RIFF: [u8; 4] = *b"RIFF";
const SIG_WEBP: [u8; 4] = *b"WEBP";
const SIG_FTYP: [u8; 4] = *b"ftyp";
const SIG_MIF1: [u8; 4] = *b"mif1";
const SIG_HEIC: [u8; 4] = *b"heic";
const SIG_HEIX: [u8; 4] = *b"heix";

// ---------------------------------------------------------------------------
// The stream.

/// A read cursor over the whole file, standing in for the `php_stream` that
/// `Z/main/streams/streams.c:736` (`_php_stream_read`) reads from.
///
/// A plain file and a memory stream behave alike for every call image.c
/// makes: a read is greedy and comes back short only at the end, a short read
/// sets eof, a seek clears it and may go past the end (reads there come back
/// empty), and only a seek to a negative position fails.
struct Stream<'a> {
    data: &'a [u8],
    pos: u64,
    eof: bool,
}

impl<'a> Stream<'a> {
    /// A cursor at the start of `data`, as `php_stream_open_wrapper` or
    /// `php_stream_memory_open` leave it.
    pub(super) fn new(data: &'a [u8]) -> Self {
        Stream {
            data,
            pos: 0,
            eof: false,
        }
    }

    /// The bytes from the cursor to the end; empty past the end.
    fn rest(&self) -> &'a [u8] {
        usize::try_from(self.pos)
            .ok()
            .and_then(|p| self.data.get(p..))
            .unwrap_or(&[])
    }

    /// `php_stream_read`, `Z/main/streams/streams.c:736`: fills `buf` as far
    /// as the data goes and says how many bytes it copied.
    pub(super) fn read(&mut self, buf: &mut [u8]) -> usize {
        if buf.is_empty() {
            return 0;
        }
        let rest = self.rest();
        let n = rest.len().min(buf.len());
        if let (Some(dst), Some(src)) = (buf.get_mut(..n), rest.get(..n)) {
            dst.copy_from_slice(src);
        }
        self.pos = self.pos.saturating_add(wide64(n));
        if n < buf.len() {
            self.eof = true;
        }
        n
    }

    /// A read of exactly `N` bytes, `None` when it came back short (the
    /// bytes it did get are still consumed, as in php).
    fn read_exact<const N: usize>(&mut self) -> Option<[u8; N]> {
        let mut buf = [0u8; N];
        (self.read(&mut buf) == N).then_some(buf)
    }

    /// `php_stream_getc`, `Z/main/streams/streams.c:867`: a byte, or -1 at eof.
    pub(super) fn getc(&mut self) -> i32 {
        let mut b = [0u8; 1];
        if self.read(&mut b) > 0 {
            i32::from(b[0])
        } else {
            -1
        }
    }

    /// `php_stream_seek(stream, offset, SEEK_SET)`,
    /// `Z/main/streams/streams.c:1385`; `true` when it succeeded (php's 0).
    pub(super) fn seek_set(&mut self, offset: i64) -> bool {
        match u64::try_from(offset) {
            Ok(p) => {
                self.pos = p;
                self.eof = false;
                true
            }
            Err(_) => false,
        }
    }

    /// `php_stream_seek(stream, offset, SEEK_CUR)`,
    /// `Z/main/streams/streams.c:1385`; `true` when it succeeded.
    pub(super) fn seek_cur(&mut self, offset: i64) -> bool {
        let base = i64::try_from(self.pos).unwrap_or(i64::MAX);
        // php clamps a sum past ZEND_LONG_MAX to ZEND_LONG_MAX.
        self.seek_set(base.saturating_add(offset))
    }

    /// `php_stream_rewind`; `true` when it succeeded.
    pub(super) fn rewind(&mut self) -> bool {
        self.seek_set(0)
    }

    /// `php_stream_eof`, `Z/main/streams/streams.c:840`.
    pub(super) fn eof(&self) -> bool {
        self.eof
    }

    /// `php_stream_gets(stream, NULL, 0)`, `Z/main/streams/streams.c:951`:
    /// the next line with its `\n`, or the tail without one; `None` at eof.
    fn gets(&mut self) -> Option<&'a [u8]> {
        let rest = self.rest();
        if rest.is_empty() {
            self.eof = true;
            return None;
        }
        let len = rest
            .iter()
            .position(|&b| b == b'\n')
            .map_or(rest.len(), |i| i + 1);
        let line = rest.get(..len)?;
        self.pos = self.pos.saturating_add(wide64(len));
        Some(line)
    }

    /// `php_stream_copy_to_mem(stream, PHP_STREAM_COPY_ALL, 0)`: the rest of
    /// the data, leaving the cursor at the end.
    fn copy_rest(&mut self) -> &'a [u8] {
        let rest = self.rest();
        self.pos = self.pos.saturating_add(wide64(rest.len()));
        self.eof = true;
        rest
    }
}

/// A length as a stream position (`zend_off_t` in `Z/main/streams/streams.c:1385`).
fn wide64(n: usize) -> u64 {
    u64::try_from(n).unwrap_or(u64::MAX)
}

// ---------------------------------------------------------------------------
// Diagnostics.

/// The level php raises a [`Warning`] at through `php_error_docref`,
/// `Z/ext/standard/image.c:366`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Level {
    /// `E_WARNING` (2).
    Warning,
    /// `E_NOTICE` (8).
    Notice,
}

/// One diagnostic image.c raises through `php_error_docref`, in the order
/// it raises them. The native prefixes the function name as docref does.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Warning {
    /// `E_NOTICE` "Error reading from %s!", `Z/ext/standard/image.c:1377`
    /// (also 1388, 1409, 1420, 1458): too few bytes to tell the type.
    ErrorReading,
    /// `E_WARNING` "PNG file corrupted by ASCII conversion",
    /// `Z/ext/standard/image.c:1394`.
    PngAscii,
    /// `E_WARNING` "Corrupt JPEG data: %zu extraneous bytes before marker",
    /// `Z/ext/standard/image.c:366`.
    CorruptJpeg(usize),
    /// `E_WARNING` "JPEG2000 codestream corrupt(Expected SIZ marker not
    /// found after SOC)", `Z/ext/standard/image.c:597`.
    Jpeg2000NoSiz,
    /// `E_WARNING` "JP2 file has no codestreams at root level",
    /// `Z/ext/standard/image.c:700`.
    Jp2NoCodestreams,
}

impl Warning {
    /// The level php raises it at.
    pub(super) fn level(&self) -> Level {
        match self {
            Warning::ErrorReading => Level::Notice,
            _ => Level::Warning,
        }
    }

    /// The message text. `input` is what php formats into "Error reading
    /// from %s!": the file name for `getimagesize` and the data itself for
    /// `getimagesizefromstring`, in both cases cut at the first NUL byte, as
    /// C's `%s` stops there. The other messages do not use it.
    pub(super) fn text(&self, input: &[u8]) -> Vec<u8> {
        match self {
            Warning::ErrorReading => {
                let name = php_ext::coerce::c_str(input);
                let mut t = b"Error reading from ".to_vec();
                t.extend_from_slice(name);
                t.push(b'!');
                t
            }
            Warning::PngAscii => b"PNG file corrupted by ASCII conversion".to_vec(),
            Warning::CorruptJpeg(n) => {
                format!("Corrupt JPEG data: {n} extraneous bytes before marker").into_bytes()
            }
            Warning::Jpeg2000NoSiz => {
                b"JPEG2000 codestream corrupt(Expected SIZ marker not found after SOC)".to_vec()
            }
            Warning::Jp2NoCodestreams => b"JP2 file has no codestreams at root level".to_vec(),
        }
    }
}

// ---------------------------------------------------------------------------
// Answers.

/// `struct php_gfxinfo`, `Z/ext/standard/php_image.h:62`, less the unit
/// strings, which only ext/libxml's SVG reader sets.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct GfxInfo {
    width: u32,
    height: u32,
    bits: u32,
    channels: u32,
}

/// What `getimagesize` puts in its array, `Z/ext/standard/image.c:1569`.
#[derive(Clone, Debug, PartialEq, Eq)]
struct ImageData {
    /// Key 0.
    pub(super) width: u32,
    /// Key 1.
    pub(super) height: u32,
    /// Key `"bits"`, present when nonzero.
    pub(super) bits: u32,
    /// Key `"channels"`, present when nonzero.
    pub(super) channels: u32,
    /// Key 2, an `IMAGETYPE_*` value.
    pub(super) image_type: i64,
    /// A registered handler's MIME type, which stands in for the table's.
    pub(super) mime: Option<&'static str>,
    /// `"width_unit"` and `"height_unit"`, `"px"` where a reader set none.
    pub(super) width_unit: Option<Vec<u8>>,
    pub(super) height_unit: Option<Vec<u8>>,
}

/// A key of the array `getimagesize` returns, `Z/ext/standard/image.c:1570`.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Key {
    /// An integer key.
    Index(i64),
    /// A string key.
    Name(&'static str),
}

/// A value of the array `getimagesize` returns, `Z/ext/standard/image.c:1570`.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Value {
    /// An int.
    Int(i64),
    /// A string.
    Str(String),
}

impl ImageData {
    /// The `width="%d" height="%d"` string of key 3. php formats the unsigned
    /// fields with `%d`, so a value past `i32::MAX` prints negative.
    pub(super) fn size_attr(&self) -> String {
        // `%d` of an unsigned int reinterprets its bits as signed, on purpose.
        #[allow(clippy::cast_possible_wrap)]
        let (w, h) = (self.width as i32, self.height as i32);
        format!("width=\"{w}\" height=\"{h}\"")
    }

    /// The array in php's key order, `Z/ext/standard/image.c:1570`: 0, 1, 2,
    /// 3 where both units are pixels, then `"bits"` and `"channels"` when
    /// nonzero, then `"mime"`, `"width_unit"` and `"height_unit"` (both
    /// `"px"` for every format image.c reads itself).
    pub(super) fn entries(&self) -> Vec<(Key, Value)> {
        let pixels = |unit: &Option<Vec<u8>>| unit.as_deref().is_none_or(|u| u == b"px");
        let mut e = vec![
            (Key::Index(0), Value::Int(i64::from(self.width))),
            (Key::Index(1), Value::Int(i64::from(self.height))),
            (Key::Index(2), Value::Int(self.image_type)),
        ];
        if pixels(&self.width_unit) && pixels(&self.height_unit) {
            e.push((Key::Index(3), Value::Str(self.size_attr())));
        }
        if self.bits != 0 {
            e.push((Key::Name("bits"), Value::Int(i64::from(self.bits))));
        }
        if self.channels != 0 {
            e.push((Key::Name("channels"), Value::Int(i64::from(self.channels))));
        }
        e.push((
            Key::Name("mime"),
            Value::Str(
                self.mime
                    .unwrap_or_else(|| mime_type(self.image_type))
                    .to_owned(),
            ),
        ));
        let unit = |u: &Option<Vec<u8>>| {
            Value::Str(u.as_deref().map_or_else(
                || "px".to_owned(),
                |u| String::from_utf8_lossy(u).into_owned(),
            ))
        };
        e.push((Key::Name("width_unit"), unit(&self.width_unit)));
        e.push((Key::Name("height_unit"), unit(&self.height_unit)));
        e
    }
}

/// The key names `php_read_APP` gives, `Z/ext/standard/image.c:436`.
const APP_NAMES: [&str; 16] = [
    "APP0", "APP1", "APP2", "APP3", "APP4", "APP5", "APP6", "APP7", "APP8", "APP9", "APP10",
    "APP11", "APP12", "APP13", "APP14", "APP15",
];

/// One JPEG `APPn` segment for `$image_info`, `Z/ext/standard/image.c:416`.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Segment {
    /// n in `APPn`, 0 to 15.
    pub(super) marker: u8,
    /// The segment body without its length word.
    pub(super) bytes: Vec<u8>,
}

impl Segment {
    /// The key php stores it under, `"APP0"` to `"APP15"`.
    pub(super) fn name(&self) -> &'static str {
        APP_NAMES
            .get(usize::from(self.marker))
            .copied()
            .unwrap_or("APP0")
    }
}

/// What `php_getimagesize_from_stream` found, `Z/ext/standard/image.c:1479`.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Answer {
    /// The array; `None` is php's `false`.
    pub(super) data: Option<ImageData>,
    /// The `$image_info` entries in php's order: only JPEG `APPn` segments, the
    /// first of each n (later ones are read and dropped), and only when
    /// `want_info` was set. They are kept also when `data` is `None`.
    pub(super) segments: Vec<Segment>,
    /// Diagnostics, in order.
    pub(super) warnings: Vec<Warning>,
}

// ---------------------------------------------------------------------------
// Public entry points.

/// `php_getimagesize_from_stream`, `Z/ext/standard/image.c:1479`, over a
/// whole file. `want_info` is whether the caller passed `$image_info`;
/// `handlers` are the bundle's registered readers, which a file every
/// fixed format refused after twelve bytes is handed to in turn
/// (`Z/ext/standard/image.c:1468`).
fn image_size(
    req: &mut Request,
    bytes: &[u8],
    want_info: bool,
    handlers: &[ImageHandler],
) -> Result<Answer, Thrown> {
    let mut s = Stream::new(bytes);
    let mut warnings = Vec::new();
    let mut segments = Vec::new();
    let (itype, unmatched) = get_image_type(req, &mut s, &mut warnings);
    if unmatched {
        for (at, handler) in (TYPE_FIXED_COUNT..).zip(handlers) {
            if let Some(size) = (handler.read)(bytes) {
                return Ok(Answer {
                    data: Some(ImageData {
                        width: size.width,
                        height: size.height,
                        bits: 0,
                        channels: 0,
                        image_type: at,
                        mime: Some(handler.mime_type),
                        width_unit: size.width_unit,
                        height_unit: size.height_unit,
                    }),
                    segments,
                    warnings,
                });
            }
        }
    }
    let gfx = match itype {
        IMAGETYPE_GIF => handle_gif(&mut s),
        IMAGETYPE_JPEG => {
            let info = if want_info { Some(&mut segments) } else { None };
            handle_jpeg(&mut s, info, &mut warnings)
        }
        IMAGETYPE_PNG => handle_png(&mut s),
        IMAGETYPE_SWF => handle_swf(&mut s),
        IMAGETYPE_SWC => handle_swc(req, &mut s)?,
        IMAGETYPE_PSD => handle_psd(&mut s),
        IMAGETYPE_BMP => handle_bmp(&mut s),
        IMAGETYPE_TIFF_II => handle_tiff(&mut s, false),
        IMAGETYPE_TIFF_MM => handle_tiff(&mut s, true),
        IMAGETYPE_JPC => handle_jpc(&mut s, &mut warnings),
        IMAGETYPE_JP2 => handle_jp2(&mut s, &mut warnings),
        IMAGETYPE_IFF => handle_iff(&mut s),
        IMAGETYPE_WBMP => get_wbmp(&mut s, true),
        IMAGETYPE_XBM => get_xbm(req, &mut s),
        IMAGETYPE_ICO => handle_ico(&mut s),
        IMAGETYPE_WEBP => handle_webp(&mut s),
        IMAGETYPE_AVIF => handle_avif(&mut s),
        IMAGETYPE_HEIF => {
            if s.rewind() {
                handle_avif(&mut s)
            } else {
                None
            }
        }
        _ => None,
    };
    Ok(Answer {
        data: gfx.map(|g| ImageData {
            width: g.width,
            height: g.height,
            bits: g.bits,
            channels: g.channels,
            image_type: itype,
            mime: None,
            width_unit: None,
            height_unit: None,
        }),
        segments,
        warnings,
    })
}

/// `struct php_image_handler` (`Z/ext/standard/php_image.h`): an image type
/// another extension reads, handed to this one with
/// `ExtensionBuilder::register` as that extension's `MINIT` hands it to
/// `php_image_register_handler`. Its number is
/// `IMAGE_FILETYPE_FIXED_COUNT` and its place among the bundle's handlers,
/// its constant stands after every constant of this extension's own, and
/// `IMAGETYPE_COUNT` counts it (ext/libxml's SVG reader is php-src's one).
#[derive(Debug, Clone, Copy)]
pub struct ImageHandler {
    pub mime_type: &'static str,
    /// With its dot, `".svg"`.
    pub extension: &'static str,
    pub const_name: &'static str,
    /// `identify` and `get_info` in one: the size, where the bytes are an
    /// image of this type.
    pub read: fn(&[u8]) -> Option<ImageSize>,
}

/// `struct php_gfxinfo` as a handler fills it: the size, and the unit each
/// dimension was written in, none for pixels.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageSize {
    pub width: u32,
    pub height: u32,
    pub width_unit: Option<Vec<u8>>,
    pub height_unit: Option<Vec<u8>>,
}

/// `php_image_register_handler`'s constants, as the engine links this
/// extension's: each handler's `IMAGETYPE_*` after every constant of its
/// own, numbered from `IMAGE_FILETYPE_FIXED_COUNT`, and `IMAGETYPE_COUNT`
/// raised by one for each.
///
/// ```php
/// $c = (new ReflectionExtension('standard'))->getConstants();
/// var_dump(array_slice($c, -1, 1, true), $c['IMAGETYPE_COUNT'], array_key_last(get_defined_constants(true)['standard']));
/// ```
/// ```expect
/// array(1) {
///   ["IMAGETYPE_SVG"]=>
///   int(21)
/// }
/// int(22)
/// string(13) "IMAGETYPE_SVG"
/// ```
pub(crate) fn link_constants(
    mut declared: Vec<ConstDecl>,
    handlers: &[&ImageHandler],
) -> Vec<ConstDecl> {
    let count = TYPE_FIXED_COUNT + i64::try_from(handlers.len()).unwrap_or(0);
    for decl in &mut declared {
        if &*decl.name.0 == b"IMAGETYPE_COUNT" {
            decl.value = ConstExpr::long(count);
        }
    }
    for (at, handler) in (TYPE_FIXED_COUNT..).zip(handlers) {
        declared.push(ConstDecl {
            name: Name(handler.const_name.as_bytes().into()),
            value: ConstExpr::long(at),
            attributes: Vec::new(),
        });
    }
    declared
}

/// The handler a number past the fixed types names, `zend_hash_index_find_ptr(&php_image_handlers, image_type)`.
fn handler_of(req: &Request, image_type: i64) -> Option<ImageHandler> {
    let at = usize::try_from(image_type.checked_sub(TYPE_FIXED_COUNT)?).ok()?;
    req.registered_by::<ImageHandler>().nth(at).map(|(_, h)| *h)
}

/// `php_image_type_to_mime_type`, `Z/ext/standard/image.c:1220`. php passes
/// the `zend_long` through an `int` parameter, so only the low 32 bits
/// count. A type from `php_image_register_handler` has its own MIME type,
/// which the native looks up before falling back to this.
fn mime_type(image_type: i64) -> &'static str {
    // The C call truncates zend_long to int, on purpose here.
    #[allow(clippy::cast_possible_truncation)]
    let t = i64::from(image_type as i32);
    match t {
        IMAGETYPE_GIF => "image/gif",
        IMAGETYPE_JPEG => "image/jpeg",
        IMAGETYPE_PNG => "image/png",
        IMAGETYPE_SWF | IMAGETYPE_SWC => "application/x-shockwave-flash",
        IMAGETYPE_PSD => "image/psd",
        IMAGETYPE_BMP => "image/bmp",
        IMAGETYPE_TIFF_II | IMAGETYPE_TIFF_MM => "image/tiff",
        IMAGETYPE_IFF => "image/iff",
        IMAGETYPE_WBMP => "image/vnd.wap.wbmp",
        IMAGETYPE_JP2 => "image/jp2",
        IMAGETYPE_XBM => "image/xbm",
        IMAGETYPE_ICO => "image/vnd.microsoft.icon",
        IMAGETYPE_WEBP => "image/webp",
        IMAGETYPE_AVIF => "image/avif",
        IMAGETYPE_HEIF => "image/heif",
        _ => "application/octet-stream",
    }
}

/// The table of `image_type_to_extension`, `Z/ext/standard/image.c:1296`,
/// without the leading dot. The native returns `"." + ext` when
/// `$include_dot` is true (php's default) and `ext` when it is false, and
/// `false` for `None`. Unlike [`mime_type`] the whole `zend_long` is matched.
/// A registered handler's extension is looked up by the native first.
fn extension(image_type: i64) -> Option<&'static str> {
    Some(match image_type {
        IMAGETYPE_GIF => "gif",
        IMAGETYPE_JPEG => "jpeg",
        IMAGETYPE_PNG => "png",
        IMAGETYPE_SWF | IMAGETYPE_SWC => "swf",
        IMAGETYPE_PSD => "psd",
        IMAGETYPE_BMP | IMAGETYPE_WBMP => "bmp",
        IMAGETYPE_TIFF_II | IMAGETYPE_TIFF_MM => "tiff",
        IMAGETYPE_IFF => "iff",
        IMAGETYPE_JPC => "jpc",
        IMAGETYPE_JP2 => "jp2",
        IMAGETYPE_JPX => "jpx",
        IMAGETYPE_JB2 => "jb2",
        IMAGETYPE_XBM => "xbm",
        IMAGETYPE_ICO => "ico",
        IMAGETYPE_WEBP => "webp",
        IMAGETYPE_AVIF => "avif",
        IMAGETYPE_HEIF => "heif",
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// Type detection.

/// `php_getimagetype`, `Z/ext/standard/image.c:1370`. The second value is
/// whether every fixed format failed with twelve bytes read, which is
/// where php asks each `php_image_register_handler` handler in turn
/// (`Z/ext/standard/image.c:1468`), which [`image_size`] does with the
/// bundle's registered [`ImageHandler`]s.
fn get_image_type(req: &Request, s: &mut Stream<'_>, w: &mut Vec<Warning>) -> (i64, bool) {
    let mut ft = [0u8; 12];
    if s.read(&mut ft[..3]) != 3 {
        w.push(Warning::ErrorReading);
        return (IMAGETYPE_UNKNOWN, false);
    }
    let first3 = [ft[0], ft[1], ft[2]];
    if first3 == SIG_GIF {
        return (IMAGETYPE_GIF, false);
    } else if first3 == SIG_JPG {
        return (IMAGETYPE_JPEG, false);
    } else if ft[..3] == SIG_PNG[..3] {
        if s.read(&mut ft[3..8]) != 5 {
            w.push(Warning::ErrorReading);
            return (IMAGETYPE_UNKNOWN, false);
        }
        if ft[..8] == SIG_PNG {
            return (IMAGETYPE_PNG, false);
        }
        w.push(Warning::PngAscii);
        return (IMAGETYPE_UNKNOWN, false);
    } else if first3 == SIG_SWF {
        return (IMAGETYPE_SWF, false);
    } else if first3 == SIG_SWC {
        return (IMAGETYPE_SWC, false);
    } else if ft[..3] == SIG_PSD[..3] {
        return (IMAGETYPE_PSD, false);
    } else if ft[..2] == SIG_BMP {
        return (IMAGETYPE_BMP, false);
    } else if first3 == SIG_JPC {
        return (IMAGETYPE_JPC, false);
    } else if ft[..3] == SIG_RIFF[..3] {
        if s.read(&mut ft[3..12]) != 9 {
            w.push(Warning::ErrorReading);
            return (IMAGETYPE_UNKNOWN, false);
        }
        if ft[8..12] == SIG_WEBP {
            return (IMAGETYPE_WEBP, false);
        }
        return (IMAGETYPE_UNKNOWN, false);
    }

    if s.read(&mut ft[3..4]) != 1 {
        w.push(Warning::ErrorReading);
        return (IMAGETYPE_UNKNOWN, false);
    }
    let first4 = [ft[0], ft[1], ft[2], ft[3]];
    if first4 == SIG_TIF_II {
        return (IMAGETYPE_TIFF_II, false);
    } else if first4 == SIG_TIF_MM {
        return (IMAGETYPE_TIFF_MM, false);
    } else if first4 == SIG_IFF {
        return (IMAGETYPE_IFF, false);
    } else if first4 == SIG_ICO {
        return (IMAGETYPE_ICO, false);
    }

    // WBMP may be shorter than twelve bytes, so the error waits.
    let twelve = s.read(&mut ft[4..12]) == 8;
    if twelve && ft == SIG_JP2 {
        return (IMAGETYPE_JP2, false);
    }
    if s.rewind() && avif::identify(s) {
        return (IMAGETYPE_AVIF, false);
    }
    // GH-20201: after the AVIF test, so AVIF is not taken for HEIF.
    if twelve
        && ft[4..8] == SIG_FTYP
        && (ft[8..12] == SIG_MIF1 || ft[8..12] == SIG_HEIC || ft[8..12] == SIG_HEIX)
    {
        return (IMAGETYPE_HEIF, false);
    }
    if get_wbmp(s, false).is_some() {
        return (IMAGETYPE_WBMP, false);
    }
    if !twelve {
        w.push(Warning::ErrorReading);
        return (IMAGETYPE_UNKNOWN, false);
    }
    if get_xbm(req, s).is_some() {
        return (IMAGETYPE_XBM, false);
    }
    (IMAGETYPE_UNKNOWN, true)
}

// ---------------------------------------------------------------------------
// Byte helpers.

/// Big-endian 32 bits, as the shifts at `Z/ext/standard/image.c:101` build them.
fn be32(b: [u8; 4]) -> u32 {
    u32::from_be_bytes(b)
}

/// `php_read2`, `Z/ext/standard/image.c:338`: big-endian 16 bits, 0 when
/// the read came back short.
fn read2(s: &mut Stream<'_>) -> u16 {
    s.read_exact::<2>().map_or(0, u16::from_be_bytes)
}

/// `php_read4`, `Z/ext/standard/image.c:538`: big-endian 32 bits, 0 when
/// the read came back short.
fn read4(s: &mut Stream<'_>) -> u32 {
    s.read_exact::<4>().map_or(0, be32)
}

/// Two bytes at `at` of `b`, as image.c's `dim[i]` reads
/// (`Z/ext/standard/image.c:121`); zero past the end, which callers never
/// reach since they read fixed-size buffers.
fn two(b: &[u8], at: usize) -> [u8; 2] {
    [
        b.get(at).copied().unwrap_or(0),
        b.get(at + 1).copied().unwrap_or(0),
    ]
}

/// Four bytes at `at` of `b` (`Z/ext/standard/image.c:121`), zero past the end.
fn four(b: &[u8], at: usize) -> [u8; 4] {
    [
        b.get(at).copied().unwrap_or(0),
        b.get(at + 1).copied().unwrap_or(0),
        b.get(at + 2).copied().unwrap_or(0),
        b.get(at + 3).copied().unwrap_or(0),
    ]
}

/// `php_ifd_get16u`, `Z/ext/standard/image.c:733`.
fn ifd_get16u(b: [u8; 2], motorola: bool) -> u16 {
    if motorola {
        u16::from_be_bytes(b)
    } else {
        u16::from_le_bytes(b)
    }
}

/// `php_ifd_get32u`, `Z/ext/standard/image.c:767`.
fn ifd_get32u(b: [u8; 4], motorola: bool) -> u32 {
    if motorola {
        u32::from_be_bytes(b)
    } else {
        u32::from_le_bytes(b)
    }
}

// ---------------------------------------------------------------------------
// Formats.

/// `php_handle_gif`, `Z/ext/standard/image.c:67`.
fn handle_gif(s: &mut Stream<'_>) -> Option<GfxInfo> {
    if !s.seek_cur(3) {
        return None;
    }
    let dim = s.read_exact::<5>()?;
    Some(GfxInfo {
        width: u32::from(dim[0]) | (u32::from(dim[1]) << 8),
        height: u32::from(dim[2]) | (u32::from(dim[3]) << 8),
        bits: if dim[4] & 0x80 != 0 {
            (u32::from(dim[4]) & 0x07) + 1
        } else {
            0
        },
        channels: 3,
    })
}

/// `php_handle_psd`, `Z/ext/standard/image.c:89`.
fn handle_psd(s: &mut Stream<'_>) -> Option<GfxInfo> {
    if !s.seek_cur(11) {
        return None;
    }
    let dim = s.read_exact::<8>()?;
    Some(GfxInfo {
        height: be32(four(&dim, 0)),
        width: be32(four(&dim, 4)),
        ..GfxInfo::default()
    })
}

/// `php_handle_bmp`, `Z/ext/standard/image.c:109`.
fn handle_bmp(s: &mut Stream<'_>) -> Option<GfxInfo> {
    if !s.seek_cur(11) {
        return None;
    }
    let dim = s.read_exact::<16>()?;
    let size = u32::from_le_bytes(four(&dim, 0));
    if size == 12 {
        Some(GfxInfo {
            width: u32::from(u16::from_le_bytes(two(&dim, 4))),
            height: u32::from(u16::from_le_bytes(two(&dim, 6))),
            bits: u32::from(dim[11]),
            channels: 0,
        })
    } else if size > 12 && (size <= 64 || size == 108 || size == 124) {
        // `abs((int32_t)height)`: abs(INT_MIN) stays INT_MIN in practice.
        let height = i32::from_le_bytes(four(&dim, 8))
            .wrapping_abs()
            .cast_unsigned();
        Some(GfxInfo {
            width: u32::from_le_bytes(four(&dim, 4)),
            height,
            bits: u32::from(u16::from_le_bytes(two(&dim, 14))),
            channels: 0,
        })
    } else {
        None
    }
}

/// `php_swf_get_bits`, `Z/ext/standard/image.c:143`: `count` bits from bit
/// `pos`, most significant first.
fn swf_get_bits(buffer: &[u8], pos: u32, count: u32) -> u64 {
    let mut value: u64 = 0;
    let end = pos.wrapping_add(count);
    let mut bit = pos;
    while bit < end {
        let byte = buffer
            .get(usize::try_from(bit / 8).unwrap_or(usize::MAX))
            .copied()
            .unwrap_or(0);
        let one = u64::from((byte >> (7 - (bit % 8))) & 0x01);
        // count - (bit - pos) - 1 is below count, at most 30 here.
        value = value.wrapping_add(one << (count - (bit - pos) - 1));
        bit += 1;
    }
    value
}

/// The frame rectangle of an SWF header, shared by `php_handle_swf` and
/// `php_handle_swc`, `Z/ext/standard/image.c:229`.
fn swf_rect(b: &[u8]) -> (u32, u32) {
    // The five-bit field is below 32, so every position fits in u32.
    let bits = u32::try_from(swf_get_bits(b, 0, 5)).unwrap_or(0);
    let x_min = swf_get_bits(b, 5, bits);
    let x_max = swf_get_bits(b, 5 + bits, bits);
    let y_min = swf_get_bits(b, 5 + 2 * bits, bits);
    let y_max = swf_get_bits(b, 5 + 3 * bits, bits);
    // unsigned long difference, then truncated into the unsigned int field.
    #[allow(clippy::cast_possible_truncation)]
    let width = (x_max.wrapping_sub(x_min) / 20) as u32;
    #[allow(clippy::cast_possible_truncation)]
    let height = (y_max.wrapping_sub(y_min) / 20) as u32;
    (width, height)
}

/// `php_handle_swf`, `Z/ext/standard/image.c:245`.
fn handle_swf(s: &mut Stream<'_>) -> Option<GfxInfo> {
    if !s.seek_cur(5) {
        return None;
    }
    let a = s.read_exact::<32>()?;
    let (width, height) = swf_rect(&a);
    Some(GfxInfo {
        width,
        height,
        bits: 0,
        channels: 0,
    })
}

/// zlib's status words as `uncompress` gives them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Z {
    /// `Z_OK`: the stream ended and its checksum matched.
    Ok,
    /// `Z_BUF_ERROR`: the destination filled before the stream ended.
    BufError,
    /// `Z_DATA_ERROR`: bad data, or the input ended first.
    DataError,
}

/// zlib's `uncompress` into a destination of `dest_len` bytes: the status
/// and what it wrote (`*destLen` afterwards is its length). The destination grows
/// as it is written, so a large `dest_len` costs only what the data needs.
fn uncompress(source: &[u8], dest_len: usize) -> (Z, Vec<u8>) {
    use miniz_oxide::inflate::TINFLStatus;
    use miniz_oxide::inflate::core::{DecompressorOxide, decompress, inflate_flags};
    // HAS_MORE_INPUT makes a truncated input stop with everything decodable
    // written, as zlib's inflate does.
    let flags = inflate_flags::TINFL_FLAG_PARSE_ZLIB_HEADER
        | inflate_flags::TINFL_FLAG_USING_NON_WRAPPING_OUTPUT_BUF
        | inflate_flags::TINFL_FLAG_HAS_MORE_INPUT;
    // One byte past the destination tells a stream that ends exactly at
    // `dest_len` (Z_OK) from one that needs more room (Z_BUF_ERROR).
    let cap = dest_len.saturating_add(1);
    let mut out = vec![0u8; source.len().saturating_mul(2).clamp(1, cap)];
    let mut state = DecompressorOxide::new();
    let mut input = source;
    let mut pos = 0usize;
    loop {
        let (status, consumed, written) = decompress(&mut state, input, &mut out, pos, flags);
        pos = pos.saturating_add(written);
        input = input.get(consumed..).unwrap_or(&[]);
        match status {
            TINFLStatus::Done => {
                out.truncate(pos);
                if pos <= dest_len {
                    return (Z::Ok, out);
                }
                out.truncate(dest_len);
                return (Z::BufError, out);
            }
            TINFLStatus::HasMoreOutput => {
                if out.len() >= cap {
                    out.truncate(dest_len.min(pos));
                    return (Z::BufError, out);
                }
                let grown = out.len().saturating_mul(2).min(cap);
                out.resize(grown, 0);
            }
            _ => {
                out.truncate(pos.min(dest_len));
                return (Z::DataError, out);
            }
        }
    }
}

/// `php_handle_swc`, `Z/ext/standard/image.c:159`: the SWF header behind a
/// zlib stream. The `Err` is what the memory limit threw outside the
/// fallible ask: the request's end.
fn handle_swc(req: &mut Request, s: &mut Stream<'_>) -> Result<Option<GfxInfo>, Thrown> {
    if !s.seek_cur(5) {
        return Ok(None);
    }
    let Some(a) = s.read_exact::<64>() else {
        return Ok(None);
    };
    // b is ecalloc(1, len + 1): 65 zero bytes.
    let mut b = [0u8; 65];
    let (first, got) = uncompress(&a, 64);
    let status = if first == Z::Ok {
        if let Some(dst) = b.get_mut(..got.len()) {
            dst.copy_from_slice(&got);
        }
        Z::Ok
    } else {
        // `len` is what the failed first call wrote.
        let len = got.len();
        if !s.seek_set(8) {
            return Ok(None);
        }
        let bufz = s.copy_rest();
        // php retries with room for the rest's length times 2, 4, ...
        // 32768 while the room is too small, each room `buf` reallocated
        // to, fallibly since 8.5.12 (`safe_ferealloc`): a room past the
        // memory limit is refused and the image is no image, the
        // limit's fatal raised as `getimagesize` returns.
        let mut buf: Option<php::Emalloc> = None;
        let mut factor: usize = 1;
        let mut status;
        let mut decoded = Vec::new();
        loop {
            factor <<= 1;
            let Some(szlength) = bufz.len().checked_mul(factor) else {
                // `ZSTR_LEN(bufz) > ULONG_MAX / factor`: Z_MEM_ERROR.
                status = Z::DataError;
                break;
            };
            let grown = req.memory_mut().fallible(|req| match buf.take() {
                Some(held) => held.resize(req, szlength),
                None => php::Emalloc::new(req, szlength),
            })?;
            let Some(grown) = grown else {
                return Ok(None);
            };
            buf = Some(grown);
            (status, decoded) = uncompress(bufz, szlength);
            if status != Z::BufError || factor >= 1 << 15 {
                break;
            }
        }
        if status == Z::Ok
            && let (Some(dst), Some(src)) = (b.get_mut(..len), decoded.get(..len))
        {
            dst.copy_from_slice(src);
        }
        if let Some(held) = buf {
            held.free(req);
        }
        status
    };
    if status != Z::Ok {
        return Ok(None);
    }
    let (width, height) = swf_rect(&b);
    Ok(Some(GfxInfo {
        width,
        height,
        bits: 0,
        channels: 0,
    }))
}

/// `php_handle_png`, `Z/ext/standard/image.c:271`.
fn handle_png(s: &mut Stream<'_>) -> Option<GfxInfo> {
    if !s.seek_cur(8) {
        return None;
    }
    let dim = s.read_exact::<9>()?;
    Some(GfxInfo {
        width: be32(four(&dim, 0)),
        height: be32(four(&dim, 4)),
        bits: u32::from(dim[8]),
        channels: 0,
    })
}

// JPEG markers, `Z/ext/standard/image.c:301`.
const M_SOF0: u32 = 0xc0;
const M_SOF15: u32 = 0xcf;
const M_EOI: u32 = 0xd9;
const M_SOS: u32 = 0xda;
const M_APP0: u32 = 0xe0;
const M_APP15: u32 = 0xef;

/// Whether `m` is one of the `SOFn` markers php lists: C0 to CF less C4, C8
/// and CC, `Z/ext/standard/image.c:460`.
fn is_sof(m: u32) -> bool {
    (M_SOF0..=M_SOF15).contains(&m) && m != 0xc4 && m != 0xc8 && m != 0xcc
}

/// `php_next_marker`, `Z/ext/standard/image.c:351`.
fn next_marker(s: &mut Stream<'_>, ff_read: bool, w: &mut Vec<Warning>) -> u32 {
    if !ff_read {
        let mut extraneous: usize = 0;
        loop {
            let marker = s.getc();
            if marker == 0xff {
                break;
            }
            if marker < 0 {
                return M_EOI;
            }
            extraneous += 1;
        }
        if extraneous != 0 {
            w.push(Warning::CorruptJpeg(extraneous));
        }
    }
    // php counts the 0xff bytes in `a` and needs two; the first call's 0xff
    // was read by the type test and every other path has one, so it holds.
    loop {
        let marker = s.getc();
        if marker < 0 {
            return M_EOI;
        }
        if marker != 0xff {
            return marker.cast_unsigned();
        }
    }
}

/// `php_skip_variable`, `Z/ext/standard/image.c:387`: `false` for a length
/// word below 2.
fn skip_variable(s: &mut Stream<'_>) -> bool {
    let length = i64::from(read2(s));
    if length < 2 {
        return false;
    }
    // php ignores the seek's success here.
    let _ = s.seek_cur(length - 2);
    true
}

/// `php_read_APP`, `Z/ext/standard/image.c:416`: `false` when the segment
/// could not be read whole. Only the first segment of each n is kept.
fn read_app(s: &mut Stream<'_>, marker: u32, info: &mut Vec<Segment>) -> bool {
    let length = usize::from(read2(s));
    if length < 2 {
        return false;
    }
    let mut buffer = vec![0u8; length - 2];
    // `php_read_stream_all_chunks`, image.c:400: fails exactly when short.
    if s.read(&mut buffer) != buffer.len() {
        return false;
    }
    let n = u8::try_from(marker.wrapping_sub(M_APP0)).unwrap_or(0);
    if !info.iter().any(|seg| seg.marker == n) {
        info.push(Segment {
            marker: n,
            bytes: buffer,
        });
    }
    true
}

/// `php_handle_jpeg`, `Z/ext/standard/image.c:450`. `info` is `Some` when
/// the caller wants `$image_info`.
fn handle_jpeg(
    s: &mut Stream<'_>,
    mut info: Option<&mut Vec<Segment>>,
    w: &mut Vec<Warning>,
) -> Option<GfxInfo> {
    let mut found: Option<GfxInfo> = None;
    let mut ff_read = true;
    loop {
        let marker = next_marker(s, ff_read, w);
        ff_read = false;
        if is_sof(marker) {
            if found.is_none() {
                let length = read2(s);
                // A getc at eof is -1, stored into the unsigned fields.
                let bits = s.getc().cast_unsigned();
                let height = u32::from(read2(s));
                let width = u32::from(read2(s));
                let channels = s.getc().cast_unsigned();
                found = Some(GfxInfo {
                    width,
                    height,
                    bits,
                    channels,
                });
                if info.is_none() || length < 8 {
                    return found;
                }
                if !s.seek_cur(i64::from(length) - 8) {
                    return found;
                }
            } else if !skip_variable(s) {
                return found;
            }
        } else if (M_APP0..=M_APP15).contains(&marker) {
            if let Some(segments) = info.as_deref_mut() {
                if !read_app(s, marker, segments) {
                    return found;
                }
            } else if !skip_variable(s) {
                return found;
            }
        } else if marker == M_SOS || marker == M_EOI || !skip_variable(s) {
            // Image data or eof ahead, or anything else that will not skip.
            return found;
        }
    }
}

/// `php_handle_jpc`, `Z/ext/standard/image.c:578`: a JPEG 2000 codestream.
fn handle_jpc(s: &mut Stream<'_>, w: &mut Vec<Warning>) -> Option<GfxInfo> {
    // `unsigned char first_marker_id = getc()`: eof (-1) becomes 0xff.
    let first_marker_id = s.getc().to_le_bytes()[0];
    if first_marker_id != 0x51 {
        w.push(Warning::Jpeg2000NoSiz);
        return None;
    }
    let _lsiz = read2(s);
    let _rsiz = read2(s);
    let width = read4(s);
    let height = read4(s);
    if !s.seek_cur(24) {
        return None;
    }
    let channels = u32::from(read2(s));
    if (channels == 0 && s.eof()) || channels > 256 {
        return None;
    }
    let mut highest_bit_depth: i32 = 0;
    for _ in 0..channels {
        let bit_depth = s.getc() + 1;
        if bit_depth > highest_bit_depth {
            highest_bit_depth = bit_depth;
        }
        let _xrsiz = s.getc();
        let _yrsiz = s.getc();
    }
    Some(GfxInfo {
        width,
        height,
        bits: highest_bit_depth.cast_unsigned(),
        channels,
    })
}

/// `php_handle_jp2`, `Z/ext/standard/image.c:649`: the JP2 box wrapper
/// around a codestream.
fn handle_jp2(s: &mut Stream<'_>, w: &mut Vec<Warning>) -> Option<GfxInfo> {
    let mut found = None;
    loop {
        let box_length = read4(s);
        let Some(box_type) = s.read_exact::<4>() else {
            break;
        };
        if box_length == 1 {
            // XLBox is not read.
            return None;
        }
        if &box_type == b"jp2c" {
            // php ignores this seek's success.
            let _ = s.seek_cur(3);
            found = handle_jpc(s, w);
            break;
        }
        if box_length.cast_signed() <= 0 {
            break;
        }
        // `box_length - 8` is unsigned int arithmetic, then widened.
        if !s.seek_cur(i64::from(box_length.wrapping_sub(8))) {
            break;
        }
    }
    if found.is_none() {
        w.push(Warning::Jp2NoCodestreams);
    }
    found
}

// TIFF tags and formats, `Z/ext/standard/image.c:711`.
const TAG_IMAGEWIDTH: u16 = 0x0100;
const TAG_IMAGEHEIGHT: u16 = 0x0101;
const TAG_COMP_IMAGEWIDTH: u16 = 0xa002;
const TAG_COMP_IMAGEHEIGHT: u16 = 0xa003;
const TAG_FMT_BYTE: u16 = 1;
const TAG_FMT_USHORT: u16 = 3;
const TAG_FMT_ULONG: u16 = 4;
const TAG_FMT_SBYTE: u16 = 6;
const TAG_FMT_SSHORT: u16 = 8;
const TAG_FMT_SLONG: u16 = 9;

/// `php_handle_tiff`, `Z/ext/standard/image.c:775`: the first IFD's width
/// and height tags.
fn handle_tiff(s: &mut Stream<'_>, motorola: bool) -> Option<GfxInfo> {
    let ifd_ptr = s.read_exact::<4>()?;
    let ifd_addr = ifd_get32u(ifd_ptr, motorola);
    // size_t `ifd_addr - 8`, handed to a signed offset: below 8 it is negative.
    if !s.seek_cur(i64::from(ifd_addr) - 8) {
        return None;
    }
    let count = s.read_exact::<2>()?;
    let num_entries = usize::from(ifd_get16u(count, motorola));
    let dir_size = 2 + 12 * num_entries + 4;
    let mut ifd_data = vec![0u8; dir_size - 2];
    if s.read(&mut ifd_data) != dir_size - 2 {
        return None;
    }
    // Values are size_t in php; the sign-extended signed forms keep their
    // low 32 bits, which is all the unsigned int fields take.
    let mut width: u32 = 0;
    let mut height: u32 = 0;
    for i in 0..num_entries {
        let entry = ifd_data.get(i * 12..i * 12 + 12).unwrap_or(&[]);
        let entry_tag = ifd_get16u(two(entry, 0), motorola);
        let entry_type = ifd_get16u(two(entry, 2), motorola);
        let value: u32 = match entry_type {
            TAG_FMT_BYTE | TAG_FMT_SBYTE => u32::from(entry.get(8).copied().unwrap_or(0)),
            TAG_FMT_USHORT => u32::from(ifd_get16u(two(entry, 8), motorola)),
            TAG_FMT_SSHORT => {
                i32::from(ifd_get16u(two(entry, 8), motorola).cast_signed()).cast_unsigned()
            }
            TAG_FMT_ULONG | TAG_FMT_SLONG => ifd_get32u(four(entry, 8), motorola),
            _ => continue,
        };
        match entry_tag {
            TAG_IMAGEWIDTH | TAG_COMP_IMAGEWIDTH => width = value,
            TAG_IMAGEHEIGHT | TAG_COMP_IMAGEHEIGHT => height = value,
            _ => {}
        }
    }
    if width != 0 && height != 0 {
        Some(GfxInfo {
            width,
            height,
            bits: 0,
            channels: 0,
        })
    } else {
        None
    }
}

/// `php_handle_iff`, `Z/ext/standard/image.c:855`: the BMHD chunk of an
/// ILBM or PBM file.
fn handle_iff(s: &mut Stream<'_>) -> Option<GfxInfo> {
    let a = s.read_exact::<8>()?;
    if a[4..8] != *b"ILBM" && a[4..8] != *b"PBM " {
        return None;
    }
    loop {
        let a = s.read_exact::<8>()?;
        let chunk_id = be32(four(&a, 0));
        let mut size = i32::from_be_bytes(four(&a, 4));
        if size < 0 {
            return None;
        }
        if size & 1 == 1 {
            if size == i32::MAX {
                return None;
            }
            size += 1;
        }
        if chunk_id == 0x424d_4844 {
            // BMHD
            if size < 9 {
                return None;
            }
            let a = s.read_exact::<9>()?;
            let width = i16::from_be_bytes(two(&a, 0));
            let height = i16::from_be_bytes(two(&a, 2));
            let bits = a[8];
            if width > 0 && height > 0 && bits > 0 && bits < 33 {
                return Some(GfxInfo {
                    width: u32::from(width.cast_unsigned()),
                    height: u32::from(height.cast_unsigned()),
                    bits: u32::from(bits),
                    channels: 0,
                });
            }
            // A bad BMHD is not skipped: the next chunk header is read
            // straight after its nine bytes, as in php.
        } else if !s.seek_cur(i64::from(size)) {
            return None;
        }
    }
}

/// `php_get_wbmp`, `Z/ext/standard/image.c:919`, with `php_handle_wbmp`
/// (image.c:980) when `want_size` is set; without it only the test matters.
fn get_wbmp(s: &mut Stream<'_>, want_size: bool) -> Option<GfxInfo> {
    if !s.rewind() {
        return None;
    }
    if s.getc() != 0 {
        return None;
    }
    loop {
        let i = s.getc();
        if i < 0 {
            return None;
        }
        if i & 0x80 == 0 {
            break;
        }
    }
    let mut width: i32 = 0;
    loop {
        let i = s.getc();
        if i < 0 {
            return None;
        }
        width = (width << 7) | (i & 0x7f);
        if width > 2048 {
            return None;
        }
        if i & 0x80 == 0 {
            break;
        }
    }
    let mut height: i32 = 0;
    loop {
        let i = s.getc();
        if i < 0 {
            return None;
        }
        height = (height << 7) | (i & 0x7f);
        if height > 2048 {
            return None;
        }
        if i & 0x80 == 0 {
            break;
        }
    }
    if height == 0 || width == 0 {
        return None;
    }
    let mut g = GfxInfo::default();
    if want_size {
        g.width = width.cast_unsigned();
        g.height = height.cast_unsigned();
    }
    Some(g)
}

/// `sscanf(line, "#define %s %d", name, &value) == 2`,
/// `Z/ext/standard/image.c:1010`, as the oracle's libc
/// (BSD `vfscanf`) does it: the number is at most 512 characters, read by
/// `strtoimax` (clamped to 64 bits) and then stored into an `int`.
fn scan_define(line: &[u8]) -> Option<(&[u8], i32)> {
    // sscanf sees a C string: stop at the first NUL.
    let line = php_ext::coerce::c_str(line);
    let mut rest = line.strip_prefix(b"#define")?;
    let skip = |r: &[u8]| -> usize { r.iter().take_while(|&&b| is_space(b)).count() };
    rest = rest.get(skip(rest)..)?;
    let name_len = rest.iter().take_while(|&&b| !is_space(b)).count();
    if name_len == 0 {
        return None;
    }
    let (name, tail) = rest.split_at(name_len);
    let tail = tail.get(skip(tail)..)?;
    let limit = tail.len().min(512);
    let field = tail.get(..limit)?;
    let (negative, digits) = match field.first() {
        Some(b'-') => (true, field.get(1..)?),
        Some(b'+') => (false, field.get(1..)?),
        _ => (false, field),
    };
    let n = digits.iter().take_while(|b| b.is_ascii_digit()).count();
    if n == 0 {
        return None;
    }
    let mut value: i64 = 0;
    for &d in digits.get(..n)? {
        let d = i64::from(d - b'0');
        value = if negative {
            value.saturating_mul(10).saturating_sub(d)
        } else {
            value.saturating_mul(10).saturating_add(d)
        };
    }
    // The int store keeps the low 32 bits, on purpose.
    #[allow(clippy::cast_possible_truncation)]
    Some((name, value as i32))
}

/// `php_get_xbm`, `Z/ext/standard/image.c:994`, which also serves as
/// `php_handle_xbm` (image.c:1053).
fn get_xbm(req: &Request, s: &mut Stream<'_>) -> Option<GfxInfo> {
    if !s.rewind() {
        return None;
    }
    let mut width: u32 = 0;
    let mut height: u32 = 0;
    while let Some(line) = s.gets() {
        // "The line is bounded by the stream, and iname duplicates it":
        // php 8.5.12 reads no line once the memory limit was met.
        if req.memory().has_oom() {
            break;
        }
        if let Some((name, value)) = scan_define(line) {
            let kind = match name.iter().rposition(|&b| b == b'_') {
                Some(i) => name.get(i + 1..).unwrap_or(&[]),
                None => name,
            };
            if kind == b"width" {
                width = value.cast_unsigned();
                if height != 0 {
                    break;
                }
            }
            if kind == b"height" {
                height = value.cast_unsigned();
                if width != 0 {
                    break;
                }
            }
        }
    }
    if width != 0 && height != 0 {
        Some(GfxInfo {
            width,
            height,
            bits: 0,
            channels: 0,
        })
    } else {
        None
    }
}

/// `php_handle_ico`, `Z/ext/standard/image.c:1062`: the deepest entry of
/// the directory, the last one among equals.
fn handle_ico(s: &mut Stream<'_>) -> Option<GfxInfo> {
    let count = s.read_exact::<2>()?;
    let mut num_icons = u16::from_le_bytes(count);
    if !(1..=255).contains(&num_icons) {
        return None;
    }
    let mut g = GfxInfo::default();
    while num_icons > 0 {
        let Some(dim) = s.read_exact::<16>() else {
            break;
        };
        let bits = u32::from(u16::from_le_bytes(two(&dim, 6)));
        if bits >= g.bits {
            g.width = u32::from(dim[0]);
            g.height = u32::from(dim[1]);
            g.bits = bits;
        }
        num_icons -= 1;
    }
    if g.width == 0 {
        g.width = 256;
    }
    if g.height == 0 {
        g.height = 256;
    }
    Some(g)
}

/// `php_handle_webp`, `Z/ext/standard/image.c:1103`: the VP8, VP8L or VP8X
/// chunk header.
fn handle_webp(s: &mut Stream<'_>) -> Option<GfxInfo> {
    let buf = s.read_exact::<18>()?;
    if buf[..3] != *b"VP8" {
        return None;
    }
    let b = |i: usize| u32::from(buf.get(i).copied().unwrap_or(0));
    let (width, height) = match buf[3] {
        b' ' => (b(14) + ((b(15) & 0x3f) << 8), b(16) + ((b(17) & 0x3f) << 8)),
        b'L' => (
            b(9) + ((b(10) & 0x3f) << 8) + 1,
            (b(10) >> 6) + (b(11) << 2) + ((b(12) & 0xf) << 10) + 1,
        ),
        b'X' => (
            b(12) + (b(13) << 8) + (b(14) << 16) + 1,
            b(15) + (b(16) << 8) + (b(17) << 16) + 1,
        ),
        _ => return None,
    };
    Some(GfxInfo {
        width,
        height,
        bits: 8,
        channels: 0,
    })
}

/// `php_handle_avif`, `Z/ext/standard/image.c:1188`: AVIF and HEIF through
/// libavifinfo; mirror and rotation are not applied.
fn handle_avif(s: &mut Stream<'_>) -> Option<GfxInfo> {
    let f = avif::features(s)?;
    Some(GfxInfo {
        width: f.width,
        height: f.height,
        bits: f.bit_depth,
        channels: f.num_channels,
    })
}

/// `image_type_to_mime_type(int $image_type): string`,
/// `PHP_FUNCTION(image_type_to_mime_type)` (`Z/ext/standard/image.c:1285`).
///
/// ```php
/// var_dump(image_type_to_mime_type(IMAGETYPE_PNG), image_type_to_mime_type(IMAGETYPE_SWC), image_type_to_mime_type(IMAGETYPE_JPC), image_type_to_mime_type(99), image_type_to_mime_type(4294967297));
/// ```
/// ```expect
/// string(9) "image/png"
/// string(29) "application/x-shockwave-flash"
/// string(24) "application/octet-stream"
/// string(24) "application/octet-stream"
/// string(9) "image/gif"
/// ```
///
/// The declarations of `image.c`.
///
/// ```php
/// foreach (['image_type_to_mime_type', 'image_type_to_extension', 'getimagesize', 'getimagesizefromstring'] as $f) echo new ReflectionFunction($f);
/// ```
/// ```expect
/// Function [ <internal:standard> function image_type_to_mime_type ] {
///
///   - Parameters [1] {
///     Parameter #0 [ <required> int $image_type ]
///   }
///   - Return [ string ]
/// }
/// Function [ <internal:standard> function image_type_to_extension ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <required> int $image_type ]
///     Parameter #1 [ <optional> bool $include_dot = true ]
///   }
///   - Return [ string|false ]
/// }
/// Function [ <internal:standard> function getimagesize ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <required> string $filename ]
///     Parameter #1 [ <optional> &$image_info = null ]
///   }
///   - Return [ array|false ]
/// }
/// Function [ <internal:standard> function getimagesizefromstring ] {
///
///   - Parameters [2] {
///     Parameter #0 [ <required> string $string ]
///     Parameter #1 [ <optional> &$image_info = null ]
///   }
///   - Return [ array|false ]
/// }
/// ```
#[php_function]
fn image_type_to_mime_type(
    req: &mut Request,
    image_type: i64,
) -> Result<php::Owner<php::PhpString>, Thrown> {
    let mime = php_image_type_to_mime_type(req, image_type);
    php::PhpString::new(req, mime.as_bytes())
}

/// `image_type_to_extension(int $image_type, bool $include_dot = true): string|false`,
/// `PHP_FUNCTION(image_type_to_extension)` (`Z/ext/standard/image.c:1296`):
/// the extension, with its dot by default, or `false` for a type with
/// none.
///
/// ```php
/// var_dump(image_type_to_extension(IMAGETYPE_JPEG), image_type_to_extension(IMAGETYPE_WBMP, false), image_type_to_extension(IMAGETYPE_JPEG2000), image_type_to_extension(0));
/// ```
/// ```expect
/// string(5) ".jpeg"
/// string(3) "bmp"
/// string(4) ".jpc"
/// bool(false)
/// ```
#[php_function]
fn image_type_to_extension(
    req: &mut Request,
    image_type: i64,
    #[php(default = true)] include_dot: bool,
) -> Result<OrFalse<php::Owner<php::PhpString>>, Thrown> {
    if let Some(ext) = extension(image_type) {
        let text = if include_dot {
            format!(".{ext}")
        } else {
            ext.to_owned()
        };
        return php::PhpString::new(req, text.as_bytes()).map(OrFalse::Value);
    }
    match handler_of(req, image_type) {
        Some(handler) if include_dot => {
            php::PhpString::new(req, handler.extension.as_bytes()).map(OrFalse::Value)
        }
        Some(handler) => php::PhpString::new(
            req,
            handler.extension.get(1..).unwrap_or_default().as_bytes(),
        )
        .map(OrFalse::Value),
        None => Ok(OrFalse::False),
    }
}

/// A new string of the request's.
fn string(req: &mut Request, bytes: &[u8]) -> Result<php::Owner<php::Value>, Thrown> {
    Ok(php::PhpString::new(req, bytes)?.into_value())
}

/// `php_getimagesize_from_any`'s end (`Z/ext/standard/image.c:1541`):
/// the warnings raised in order, `$image_info` filled with the `APPn`
/// segments by name, and the array, or `false`.
/// `$image_info` emptied before the image is read, as php's
/// `ZEND_TRY_ASSIGN_REF_EMPTY_ARRAY`, where the call gave it.
fn clear_info(req: &mut Request, image_info: Ref<Untyped<php::Value>>) -> Result<(), Thrown> {
    if image_info.is_given() {
        let empty = php::PhpArray::new(req)?;
        image_info.set(req, Untyped(empty.into_value()))?;
    }
    Ok(())
}

/// Each diagnostic behind the native's name, as `php_error_docref`
/// raises them, `shown` the name `Error reading from %s!` gives.
fn raise_all(req: &mut Request, warnings: &[Warning], shown: &[u8]) {
    for warning in warnings {
        let text = String::from_utf8_lossy(&warning.text(shown)).into_owned();
        let level = match warning.level() {
            Level::Warning => php_ext::raising::Level::Warning,
            Level::Notice => php_ext::raising::Level::Notice,
        };
        php_ext::raising::docref(req, level, &text);
    }
}

/// `php_getimagetype` (`Z/ext/standard/image.c:1370`), which
/// `php_image.h` declares `PHPAPI` and `exif_imagetype` calls: the type
/// of the image `bytes` hold, its diagnostics raised as the calling
/// native's with `input` the name they give, and after the fixed types
/// each registered handler asked in turn; `0`, `IMAGE_FILETYPE_UNKNOWN`,
/// where none took the bytes. `exif_imagetype`'s witness is this one's.
pub fn php_getimagetype(req: &mut Request, bytes: &[u8], input: &[u8]) -> i64 {
    let mut s = Stream::new(bytes);
    let mut warnings = Vec::new();
    let (itype, unmatched) = get_image_type(req, &mut s, &mut warnings);
    raise_all(req, &warnings, input);
    if unmatched {
        let handlers: Vec<ImageHandler> = req
            .registered_by::<ImageHandler>()
            .map(|(_, h)| *h)
            .collect();
        for (at, handler) in (TYPE_FIXED_COUNT..).zip(&handlers) {
            if (handler.read)(bytes).is_some() {
                return at;
            }
        }
    }
    itype
}

/// `php_image_type_to_mime_type` (`Z/ext/standard/image.c:1220`),
/// which `php_image.h` declares `PHPAPI` and `exif_read_data` reports
/// a file's and a thumbnail's type with: a registered handler's MIME
/// type, else the fixed table's.
pub fn php_image_type_to_mime_type(req: &Request, image_type: i64) -> &'static str {
    // The C call truncates zend_long to int, on purpose here.
    #[allow(clippy::cast_possible_truncation)]
    let registered = handler_of(req, i64::from(image_type as i32));
    registered.map_or_else(|| mime_type(image_type), |h| h.mime_type)
}

fn answer(
    req: &mut Request,
    bytes: &[u8],
    shown: &[u8],
    image_info: Ref<Untyped<php::Value>>,
) -> Result<OrFalse<php::Owner<php::PhpArray>>, Thrown> {
    let want_info = image_info.is_given();
    let handlers: Vec<ImageHandler> = req
        .registered_by::<ImageHandler>()
        .map(|(_, h)| *h)
        .collect();
    let found = image_size(req, bytes, want_info, &handlers)?;
    raise_all(req, &found.warnings, shown);
    if want_info {
        let mut info = php::PhpArray::new(req)?;
        for segment in &found.segments {
            let name = segment.name();
            if info.find(name.as_bytes()).is_none() {
                let value = string(req, &segment.bytes)?;
                info.set_str(req, name.as_bytes(), value)?;
            }
        }
        image_info.set(req, Untyped(info.into_value()))?;
    }
    let Some(data) = found.data else {
        return Ok(OrFalse::False);
    };
    let mut array = php::PhpArray::new(req)?;
    for (key, value) in data.entries() {
        let value = match value {
            Value::Int(n) => php::Owner::take(req, php::Value::int(n)),
            Value::Str(text) => string(req, text.as_bytes())?,
        };
        match key {
            Key::Index(i) => array.set(req, php::Owner::index(i), value)?,
            Key::Name(name) => array.set_str(req, name.as_bytes(), value)?,
        }
    }
    Ok(OrFalse::Value(array))
}

/// `getimagesize(string $filename, &$image_info = null): array|false`,
/// `PHP_FUNCTION(getimagesize)` (`Z/ext/standard/image.c:1591`): the file
/// opened `rb` and read whole, then `php_getimagesize_from_stream`; the
/// width, height, type, `width="…" height="…"`, the bits and channels
/// where the format has them, the MIME type and the units; the JPEG
/// `APPn` segments in `$image_info`.
///
/// ```php
/// $jpg = __DIR__ . '/1.jpg'; file_put_contents($jpg, hex2bin('ffd8ffe000104a46494600010100000100010000ffed000649505443ffc0000b080005000701011100ffd9'));
/// $png = __DIR__ . '/1.png'; file_put_contents($png, hex2bin('89504e470d0a1a0a0000000d49484452000000030000000208020000001216f14d'));
/// var_dump(getimagesize($jpg, $info), bin2hex($info['APP0']), $info['APP13'], count($info), getimagesize($png), getimagesize(__FILE__), getimagesize(__DIR__ . '/nope.png'));
/// try { getimagesize("a\0b"); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// Warning: getimagesize(%s/nope.png): Failed to open stream: No such file or directory in %s on line 4
/// array(9) {
///   [0]=>
///   int(7)
///   [1]=>
///   int(5)
///   [2]=>
///   int(2)
///   [3]=>
///   string(20) "width="7" height="5""
///   ["bits"]=>
///   int(8)
///   ["channels"]=>
///   int(1)
///   ["mime"]=>
///   string(10) "image/jpeg"
///   ["width_unit"]=>
///   string(2) "px"
///   ["height_unit"]=>
///   string(2) "px"
/// }
/// string(28) "4a46494600010100000100010000"
/// string(4) "IPTC"
/// int(2)
/// array(8) {
///   [0]=>
///   int(3)
///   [1]=>
///   int(2)
///   [2]=>
///   int(3)
///   [3]=>
///   string(20) "width="3" height="2""
///   ["bits"]=>
///   int(8)
///   ["mime"]=>
///   string(9) "image/png"
///   ["width_unit"]=>
///   string(2) "px"
///   ["height_unit"]=>
///   string(2) "px"
/// }
/// bool(false)
/// bool(false)
/// getimagesize(): Argument #1 ($filename) must not contain any null bytes
/// ```
#[php_function]
fn getimagesize(
    req: &mut Request,
    filename: Path,
    #[php(default = null)] image_info: Ref<Untyped<php::Value>>,
) -> Result<OrFalse<php::Owner<php::PhpArray>>, Thrown> {
    let Path(filename) = filename;
    clear_info(req, image_info)?;
    let Some(stream) = req.open_stream(filename.as_bytes(), b"rb", false)? else {
        return Ok(OrFalse::False);
    };
    let bytes = match stream.copy_to_mem(req, None) {
        Ok(bytes) => bytes,
        Err(thrown) => {
            stream.give_up(req);
            return Err(thrown);
        }
    };
    stream.free(req);
    let answered = answer(req, bytes.as_bytes(), filename.as_bytes(), image_info);
    bytes.give_up(req);
    answered
}

/// `getimagesizefromstring(string $string, &$image_info = null): array|false`,
/// `PHP_FUNCTION(getimagesizefromstring)` (`Z/ext/standard/image.c:1598`):
/// the same over the bytes given.
///
/// ```php
/// var_dump(getimagesizefromstring("GIF89a\x02\x00\x03\x00\x80\x00\x00", $info), $info, getimagesizefromstring("ab"));
/// ```
/// ```expect
/// Notice: getimagesizefromstring(): Error reading from ab! in %s on line 2
/// array(9) {
///   [0]=>
///   int(2)
///   [1]=>
///   int(3)
///   [2]=>
///   int(1)
///   [3]=>
///   string(20) "width="2" height="3""
///   ["bits"]=>
///   int(1)
///   ["channels"]=>
///   int(3)
///   ["mime"]=>
///   string(9) "image/gif"
///   ["width_unit"]=>
///   string(2) "px"
///   ["height_unit"]=>
///   string(2) "px"
/// }
/// array(0) {
/// }
/// bool(false)
/// ```
///
/// A compressed SWF whose header did not inflate into 64 bytes is
/// inflated whole into a room twice the rest's length, then four times,
/// up to 32768 times, each room reallocated fallibly since 8.5.12: the
/// room past the memory limit is refused, the image is no image, and the
/// limit's fatal is raised as the call returns.
///
/// ```php
/// ini_set('memory_limit', '8M');
/// $d = deflate_init(ZLIB_ENCODING_DEFLATE, ['level' => 9]); $z = '';
/// for ($i = 0; $i < 32; $i++) $z .= deflate_add($d, str_repeat('A', 1 << 20), $i === 31 ? ZLIB_FINISH : ZLIB_NO_FLUSH);
/// var_dump(getimagesizefromstring("CWS\x06" . pack('V', 8 + (32 << 20)) . $z), "unreached");
/// ```
/// ```expect exit=255
///
/// Fatal error: Allowed memory size of 8388608 bytes exhausted (tried to allocate 4177920 bytes) in %s on line 5
/// ```
#[php_function]
fn getimagesizefromstring(
    req: &mut Request,
    string: php::PhpString,
    #[php(default = null)] image_info: Ref<Untyped<php::Value>>,
) -> Result<OrFalse<php::Owner<php::PhpArray>>, Thrown> {
    let data = string;
    clear_info(req, image_info)?;
    answer(req, data.as_bytes(), data.as_bytes(), image_info)
}
