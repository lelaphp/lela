//! The readers and the encoders (`gd.c`'s `_php_image_create_from`,
//! `_php_image_output` and the stream handling `gd_ctx.c` carried): a
//! file or a string read through php's streams into libgd's readers, and
//! libgd's bytes written to php's stdout, a path or a stream resource
//! through a `gdIOCtx` as libgd writes them (`create_output_context`).

use php_ext::abi::{Instance, OrFalse, Path, True, Untyped};
use php_ext::attr::php_function;
use php_ext::coerce::value_name;
use php_ext::raising::{Level, Thrown, arg_type_error, arg_value_error, docref};
use php_ext::request::{Ini, Opening, OwnedStream as _, Request, Stream, Streams as _};
use php_ext::values::{Emalloc, Owner, PhpString, Value, ValueRef};

use crate::ffi::{self, Encode, Format, GdFormat, Image, Rect, Reported};
use crate::image::{GdImage, gd, im, int, raise, wrap};

/// `imagetypes(): int`: the `IMG_*` bits of what the build reads and
/// writes, no XPM.
///
/// ```php
/// var_dump(imagetypes(), (bool) (imagetypes() & IMG_XPM));
/// ```
/// ```expect
/// int(495)
/// bool(false)
/// ```
#[php_function]
fn imagetypes() -> i64 {
    1 | 2 | 4 | 8 | 32 | 64 | 128 | 256
}

/// `AvifInfoIdentifyStream` over the bytes as `php_is_image_avif` reads
/// a memory stream of them: the first box an "ftyp" naming the "avif" or
/// "avis" brand among its first ones.
fn is_avif(data: &[u8]) -> bool {
    let word = |at: usize| -> Option<u32> {
        let b = data.get(at..at + 4)?;
        Some(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    };
    let (Some(mut size), Some(b"ftyp")) = (word(0), data.get(4..8)) else {
        return false;
    };
    let mut header = 8;
    if size == 1 {
        let (Some(0), Some(low)) = (word(8), word(12)) else {
            return false;
        };
        (size, header) = (low, 16);
    } else if size == 0 {
        size = u32::MAX;
    }
    if size < header || size - header < 8 {
        return false;
    }
    let content = size - header;
    let mut i = 0u32;
    while i + 4 <= content {
        let at = usize::try_from(header + i).unwrap_or(usize::MAX);
        let Some(brand) = data.get(at..at.saturating_add(4)) else {
            return false;
        };
        if i != 4 {
            if brand == b"avif" || brand == b"avis" {
                return true;
            }
            if i > 32 * 4 {
                return false;
            }
        }
        i += 4;
    }
    false
}

/// `_php_image_type`: the format a string's leading bytes name, `None`
/// for none php knows (and for fewer than twelve bytes).
fn image_type(data: &[u8]) -> Option<(Format, &'static str)> {
    if data.len() < 12 {
        return None;
    }
    let types: [(&[u8], Format, &str); 5] = [
        (b"gd2", Format::Gd2, "GD2"),
        (b"\xff\xd8\xff", Format::Jpeg(true), "JPEG"),
        (b"\x89PNG\r\n\x1a\n", Format::Png, "PNG"),
        (b"GIF", Format::Gif, "GIF"),
        (b"BM", Format::Bmp, "BMP"),
    ];
    for (sig, format, name) in types {
        if data.starts_with(sig) {
            return Some((format, name));
        }
    }
    if data.starts_with(b"RIFF") && &data[8..12] == b"WEBP" {
        return Some((Format::Webp, "WEBP"));
    }
    if is_avif(data) {
        return Some((Format::Avif, "AVIF"));
    }
    (data[0] == 0).then_some((Format::Wbmp, "WBMP"))
}

/// `imagecreatefromstring(string $data): GdImage|false`: the format read
/// off the leading bytes, then libgd's reader over a `gdIOCtx` of them.
///
/// ```php
/// $i = imagecreatetruecolor(2, 1); imagesetpixel($i, 1, 0, 0xFF0000);
/// ob_start(); imagepng($i); $png = ob_get_clean();
/// var_dump(dechex(imagecolorat(imagecreatefromstring($png), 1, 0)));
/// var_dump(imagecreatefromstring("short"));
/// var_dump(imagecreatefromstring("GIF89a........"));
/// ```
/// ```expect
/// string(6) "ff0000"
///
/// Warning: imagecreatefromstring(): Data is not in a recognized format in %s on line 5
/// bool(false)
///
/// Warning: imagecreatefromstring(): Passed data is not in "GIF" format in %s on line 6
///
/// Warning: imagecreatefromstring(): Couldn't create GD Image Stream out of Data in %s on line 6
/// bool(false)
/// ```
#[php_function]
fn imagecreatefromstring(
    req: &mut Request,
    data: PhpString,
) -> Result<OrFalse<Owner<Instance<GdImage>>>, Thrown> {
    if i32::try_from(data.len()).is_err() {
        return Err(arg_value_error(req, 1, "is too long"));
    }
    let Some((format, name)) = image_type(data.as_bytes()) else {
        docref(req, Level::Warning, "Data is not in a recognized format");
        return Ok(OrFalse::False);
    };
    let image = gd(req, || Image::decode(format, data.as_bytes()))?;
    if image.is_none() {
        let text = format!("Passed data is not in \"{name}\" format");
        docref(req, Level::Warning, &text);
        docref(
            req,
            Level::Warning,
            "Couldn't create GD Image Stream out of Data",
        );
    }
    wrap(req, image)
}

/// `_php_image_create_from`: the file opened through php's streams and
/// read whole, then libgd's reader over it, `gd.jpeg_ignore_warning`
/// read for a plain file's JPEG as php's `FILE *` path reads it. A file
/// libgd cannot read is named in the warning as its bytes, as php's `%s`
/// prints a name that is not UTF-8.
///
/// ```php
/// set_error_handler(function ($n, $s) { echo $n, " ", bin2hex(strstr($s, '"')), "\n"; return true; });
/// imagecreatefromgif("data:,GIF89a\xff");
/// ```
/// ```expect
/// 2 22646174613a2c474946383961ff22206973206e6f7420612076616c6964204749462066696c65
/// ```
fn create_from(
    req: &mut Request,
    filename: Path,
    format: Format,
    name: &str,
) -> Result<OrFalse<Owner<Instance<GdImage>>>, Thrown> {
    let path = filename.0.as_bytes();
    let Some(stream) = req.open_stream(path, b"rb", false)? else {
        return Ok(OrFalse::False);
    };
    let plain = stream.is_stdio(req);
    let data = stream.copy_to_mem(req, None);
    let data = match data {
        Ok(data) => data,
        Err(thrown) => {
            stream.free(req);
            return Err(thrown);
        }
    };
    if i32::try_from(data.len()).is_err() {
        let text = format!("Cannot read images with more than {} bytes", i32::MAX);
        docref(req, Level::Warning, &text);
        data.give_up(req);
        stream.free(req);
        return Ok(OrFalse::False);
    }
    let format = match format {
        Format::Jpeg(_) if plain => {
            let on = req
                .ini()
                .get(crate::JPEG_IGNORE_WARNING.as_bytes())
                .map_or(0, Ini::parse_bytes);
            Format::Jpeg(on != 0)
        }
        other => other,
    };
    let image = gd(req, || Image::decode(format, data.as_bytes()));
    data.give_up(req);
    stream.free(req);
    let image = image?;
    if image.is_none() {
        let after = format!("\" is not a valid {name} file");
        let text = [b"\"".as_slice(), path, after.as_bytes()].concat();
        docref(req, Level::Warning, &text);
    }
    wrap(req, image)
}

/// `imagecreatefromavif(string $filename): GdImage|false`.
///
/// ```php
/// $f = __DIR__ . '/w';
/// imageavif(imagecreatetruecolor(2, 2), $f);
/// var_dump(imagesx(imagecreatefromavif($f)));
/// unlink($f);
/// ```
/// ```expect
/// int(2)
/// ```
#[php_function]
fn imagecreatefromavif(
    req: &mut Request,
    filename: Path,
) -> Result<OrFalse<Owner<Instance<GdImage>>>, Thrown> {
    create_from(req, filename, Format::Avif, "AVIF")
}

/// `imagecreatefromgif(string $filename): GdImage|false`.
///
/// ```php
/// $f = __DIR__ . '/w';
/// imagegif(imagecreate(3, 2), $f);
/// var_dump(imagesy(imagecreatefromgif($f)));
/// file_put_contents($f, "nope");
/// var_dump(imagecreatefromgif($f));
/// unlink($f);
/// ```
/// ```expect
/// int(2)
///
/// Warning: imagecreatefromgif(): "%SCRATCH%/w" is not a valid GIF file in %s on line 6
/// bool(false)
/// ```
#[php_function]
fn imagecreatefromgif(
    req: &mut Request,
    filename: Path,
) -> Result<OrFalse<Owner<Instance<GdImage>>>, Thrown> {
    create_from(req, filename, Format::Gif, "GIF")
}

/// `imagecreatefromjpeg(string $filename): GdImage|false`.
///
/// ```php
/// $f = __DIR__ . '/w';
/// imagejpeg(imagecreatetruecolor(4, 3), $f);
/// var_dump(imagesx(imagecreatefromjpeg($f)));
/// unlink($f);
/// ```
/// ```expect
/// int(4)
/// ```
#[php_function]
fn imagecreatefromjpeg(
    req: &mut Request,
    filename: Path,
) -> Result<OrFalse<Owner<Instance<GdImage>>>, Thrown> {
    create_from(req, filename, Format::Jpeg(true), "JPEG")
}

/// `imagecreatefrompng(string $filename): GdImage|false`: libpng's
/// errors through php's error method, then php's own warning.
///
/// ```php
/// $f = __DIR__ . '/w';
/// file_put_contents($f, "\x89PNG\r\n\x1a\nxxxxxxxxxxxxxxxxxxxx");
/// var_dump(imagecreatefrompng($f));
/// unlink($f);
/// var_dump(@imagecreatefrompng($f));
/// ```
/// ```expect
/// Warning: imagecreatefrompng(): gd-png: fatal libpng error: xxxx: bad header (invalid type)
///  in %s on line 4
///
/// Warning: imagecreatefrompng(): gd-png error: setjmp returns error condition 1
///  in %s on line 4
///
/// Warning: imagecreatefrompng(): "%SCRATCH%/w" is not a valid PNG file in %s on line 4
/// bool(false)
/// bool(false)
/// ```
#[php_function]
fn imagecreatefrompng(
    req: &mut Request,
    filename: Path,
) -> Result<OrFalse<Owner<Instance<GdImage>>>, Thrown> {
    create_from(req, filename, Format::Png, "PNG")
}

/// `imagecreatefromwebp(string $filename): GdImage|false`.
///
/// ```php
/// $f = __DIR__ . '/w';
/// imagewebp(imagecreatetruecolor(5, 1), $f);
/// var_dump(imagesx(imagecreatefromwebp($f)));
/// unlink($f);
/// ```
/// ```expect
/// int(5)
/// ```
#[php_function]
fn imagecreatefromwebp(
    req: &mut Request,
    filename: Path,
) -> Result<OrFalse<Owner<Instance<GdImage>>>, Thrown> {
    create_from(req, filename, Format::Webp, "WEBP")
}

/// `imagecreatefromxbm(string $filename): GdImage|false`: libgd reads
/// XBM from a `FILE *` alone.
///
/// ```php
/// $f = __DIR__ . '/w';
/// file_put_contents($f, "#define t_width 8\n#define t_height 2\nstatic unsigned char t_bits[] = {\n 0x01, 0x80};\n");
/// $i = imagecreatefromxbm($f);
/// var_dump(imagesx($i), imagesy($i), imagecolorat($i, 0, 0), imagecolorat($i, 7, 1));
/// unlink($f);
/// ```
/// ```expect
/// int(8)
/// int(2)
/// int(1)
/// int(1)
/// ```
#[php_function]
fn imagecreatefromxbm(
    req: &mut Request,
    filename: Path,
) -> Result<OrFalse<Owner<Instance<GdImage>>>, Thrown> {
    create_from(req, filename, Format::Xbm, "XBM")
}

/// `imagecreatefromwbmp(string $filename): GdImage|false`.
///
/// ```php
/// $f = __DIR__ . '/w';
/// $i = imagecreate(3, 3); imagecolorallocate($i, 255, 255, 255); imagecolorallocate($i, 0, 0, 0);
/// imagewbmp($i, $f);
/// var_dump(imagesx(imagecreatefromwbmp($f)));
/// unlink($f);
/// ```
/// ```expect
/// int(3)
/// ```
#[php_function]
fn imagecreatefromwbmp(
    req: &mut Request,
    filename: Path,
) -> Result<OrFalse<Owner<Instance<GdImage>>>, Thrown> {
    create_from(req, filename, Format::Wbmp, "WBMP")
}

/// `imagecreatefromgd(string $filename): GdImage|false`.
///
/// ```php
/// $f = __DIR__ . '/w';
/// var_dump(imagegd(imagecreatetruecolor(6, 2), $f));
/// var_dump(imagecreatefromgd($f));
/// unlink($f);
/// ```
/// ```expect
/// Warning: imagegd(): GD image support has been disabled
///  in %s on line 3
/// bool(true)
///
/// Warning: imagecreatefromgd(): GD image support has been disabled
///  in %s on line 4
///
/// Warning: imagecreatefromgd(): "%SCRATCH%/w" is not a valid GD file in %s on line 4
/// bool(false)
/// ```
#[php_function]
fn imagecreatefromgd(
    req: &mut Request,
    filename: Path,
) -> Result<OrFalse<Owner<Instance<GdImage>>>, Thrown> {
    create_from(req, filename, Format::Gd, "GD")
}

/// `imagecreatefromgd2(string $filename): GdImage|false`.
///
/// ```php
/// $f = __DIR__ . '/w';
/// var_dump(imagegd2(imagecreatetruecolor(6, 2), $f));
/// var_dump(imagecreatefromgd2($f));
/// unlink($f);
/// ```
/// ```expect
/// Warning: imagegd2(): GD2 image support has been disabled
///  in %s on line 3
/// bool(true)
///
/// Warning: imagecreatefromgd2(): GD2 image support has been disabled
///  in %s on line 4
///
/// Warning: imagecreatefromgd2(): "%SCRATCH%/w" is not a valid GD2 file in %s on line 4
/// bool(false)
/// ```
#[php_function]
fn imagecreatefromgd2(
    req: &mut Request,
    filename: Path,
) -> Result<OrFalse<Owner<Instance<GdImage>>>, Thrown> {
    create_from(req, filename, Format::Gd2, "GD2")
}

/// `imagecreatefromgd2part(string $filename, int $x, int $y, int $width,
/// int $height): GdImage|false`.
///
/// ```php
/// $f = __DIR__ . '/w';
/// $i = imagecreatetruecolor(6, 6); imagesetpixel($i, 3, 3, 0xFF);
/// imagegd2($i, $f);
/// var_dump(imagecreatefromgd2part($f, 2, 2, 3, 3));
/// try { imagecreatefromgd2part($f, 0, 0, 0, 1); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// unlink($f);
/// ```
/// ```expect
/// Warning: imagegd2(): GD2 image support has been disabled
///  in %s on line 4
///
/// Warning: imagecreatefromgd2part(): GD2 image support has been disabled
///  in %s on line 5
///
/// Warning: imagecreatefromgd2part(): "%SCRATCH%/w" is not a valid GD2 file in %s on line 5
/// bool(false)
/// imagecreatefromgd2part(): Argument #4 ($width) must be greater than or equal to 1
/// ```
#[php_function]
fn imagecreatefromgd2part(
    req: &mut Request,
    filename: Path,
    x: i64,
    y: i64,
    width: i64,
    height: i64,
) -> Result<OrFalse<Owner<Instance<GdImage>>>, Thrown> {
    if width < 1 {
        return Err(arg_value_error(req, 4, "be greater than or equal to 1"));
    }
    if height < 1 {
        return Err(arg_value_error(req, 5, "be greater than or equal to 1"));
    }
    let rect = Rect {
        x: int(x),
        y: int(y),
        width: int(width),
        height: int(height),
    };
    create_from(req, filename, Format::Gd2Part(rect), "GD2")
}

/// `imagecreatefrombmp(string $filename): GdImage|false`.
///
/// ```php
/// $f = __DIR__ . '/w';
/// imagebmp(imagecreatetruecolor(2, 7), $f);
/// var_dump(imagesy(imagecreatefrombmp($f)));
/// unlink($f);
/// ```
/// ```expect
/// int(7)
/// ```
#[php_function]
fn imagecreatefrombmp(
    req: &mut Request,
    filename: Path,
) -> Result<OrFalse<Owner<Instance<GdImage>>>, Thrown> {
    create_from(req, filename, Format::Bmp, "BMP")
}

/// `imagecreatefromtga(string $filename): GdImage|false`.
///
/// ```php
/// $f = __DIR__ . '/w';
/// file_put_contents($f, pack('CCCvvCvvvvCC', 0, 0, 2, 0, 0, 0, 0, 0, 2, 1, 24, 0) . "\x00\x00\xff\xff\x00\x00");
/// $i = imagecreatefromtga($f);
/// var_dump(imagesx($i), dechex(imagecolorat($i, 0, 0)), dechex(imagecolorat($i, 1, 0)));
/// unlink($f);
/// ```
/// ```expect
/// int(2)
/// string(6) "ff0000"
/// string(2) "ff"
/// ```
#[php_function]
fn imagecreatefromtga(
    req: &mut Request,
    filename: Path,
) -> Result<OrFalse<Owner<Instance<GdImage>>>, Thrown> {
    create_from(req, filename, Format::Tga, "TGA")
}

/// Where an encoder's bytes go, what `create_output_context` makes:
/// the target, and the `gdIOCtx` php `ecalloc`s for it, which stands
/// until `gd_free` and counts in the peak as php's does.
struct Sink {
    to: To,
    ctx: Emalloc,
}

/// [`Sink`]'s target.
enum To {
    /// The request's stdout, `php_write`.
    Output,
    /// A stream resource the program handed, left open by `gd_free`.
    Stream(Stream),
    /// A stream the native opened, its owner closed by `gd_free`.
    Opened(Owner<Stream>),
}

impl Sink {
    /// The `gdIOCtx` made over `to`; the memory limit refusing it closes
    /// the stream opened for it.
    fn new(req: &mut Request, to: To) -> Result<Sink, Thrown> {
        match Emalloc::new(req, ffi::CTX_SIZE) {
            Ok(ctx) => Ok(Sink { to, ctx }),
            Err(thrown) => {
                if let To::Opened(stream) = to {
                    stream.free(req);
                }
                Err(thrown)
            }
        }
    }

    /// `putBuf`, and `putC` with its answer unread: how many bytes the
    /// target took, `php_write` all of them, `php_stream_write` its own
    /// count or `-1`.
    fn put(&self, req: &mut Request, bytes: &[u8]) -> i32 {
        let took = match &self.to {
            To::Output => {
                req.write(bytes);
                Some(bytes.len())
            }
            To::Stream(stream) => stream.write(req, bytes),
            To::Opened(stream) => stream.write(req, bytes),
        };
        took.map_or(-1, |n| i32::try_from(n).unwrap_or(i32::MAX))
    }

    /// `gd_free`: the stream closed where it was opened here, then the
    /// `gdIOCtx` freed.
    fn free(self, req: &mut Request) {
        if let To::Opened(stream) = self.to {
            stream.free(req);
        }
        self.ctx.free(req);
    }

    /// A native refused after its sink was made: `gd_free`, then the
    /// refusal.
    fn drop_with(self, req: &mut Request, thrown: Thrown) -> Thrown {
        self.free(req);
        thrown
    }

    /// libgd writing through the `gdIOCtx`: each write after what libgd
    /// reported before it, as `php_gd_error_method` raises it while
    /// libgd runs, then what it reported after its last; an `E_ERROR`
    /// ends the request there and nothing more reaches the target.
    /// `gd_free` follows.
    fn encode(
        self,
        req: &mut Request,
        write: impl FnOnce(&mut ffi::Put<'_>) -> Reported,
    ) -> Result<bool, Thrown> {
        let mut ended = None;
        let left = write(&mut |reported, bytes| {
            if ended.is_some() {
                return 0;
            }
            if let Err(thrown) = raise(req, reported) {
                ended = Some(thrown);
                return 0;
            }
            self.put(req, bytes)
        });
        let raised = match ended {
            Some(thrown) => Err(thrown),
            None => raise(req, left),
        };
        self.free(req);
        raised.map(|()| true)
    }
}

/// `create_output_context`: stdout for `null`, a stream resource as
/// it is, a path opened `wb`; `None` where it cannot be had.
fn sink(req: &mut Request, file: &Value, name: &str) -> Result<Option<Sink>, Thrown> {
    let position = 2;
    match file.as_ref() {
        ValueRef::Null => Sink::new(req, To::Output).map(Some),
        ValueRef::Resource(_) => match Stream::from_value(req, file) {
            Some(stream) => Sink::new(req, To::Stream(stream)).map(Some),
            None => Err(php_ext::raising::throw_error(
                req,
                "TypeError",
                &format!("{name}(): supplied resource is not a valid stream resource"),
            )),
        },
        ValueRef::Str(path) => {
            if path.as_bytes().contains(&0) {
                return Err(arg_type_error(req, position, "not contain null bytes"));
            }
            let path = path.as_bytes().to_vec();
            match req.open_stream(&path, b"wb", false)? {
                Some(stream) => Sink::new(req, To::Opened(stream)).map(Some),
                None => Ok(None),
            }
        }
        _ => {
            let name = value_name(req, file);
            let text = format!("be a file name or a stream resource, {name} given");
            Err(arg_type_error(req, position, &text))
        }
    }
}

/// An encoding native's body after its checks: the image written
/// through the sink's `gdIOCtx` as libgd encodes it.
fn write(
    req: &mut Request,
    image: Instance<GdImage>,
    sink: Sink,
    how: Encode,
) -> Result<bool, Thrown> {
    let im = match im(req, image) {
        Ok(im) => im,
        Err(thrown) => return Err(sink.drop_with(req, thrown)),
    };
    sink.encode(req, |put| im.write(how, put))
}

/// The encoders that take `$file` and nothing it checks: the sink,
/// then the bytes.
fn output(
    req: &mut Request,
    image: Instance<GdImage>,
    file: &Value,
    name: &str,
    how: Encode,
) -> Result<bool, Thrown> {
    let Some(sink) = sink(req, file, name)? else {
        return Ok(false);
    };
    write(req, image, sink, how)
}

/// The first black entry of the palette, `imagexbm`'s and `imagewbmp`'s
/// default foreground; the count of entries where there is none.
fn first_black(req: &mut Request, image: Instance<GdImage>) -> Result<i64, Thrown> {
    let im = im(req, image)?;
    let total = im.colors_total();
    let black = (0..total).find(|i| matches!(im.rgba(*i), [0, 0, 0, _]));
    Ok(i64::from(black.unwrap_or(total)))
}

/// `imagexbm(GdImage $image, ?string $filename, ?int $foreground_color =
/// null): bool`: libgd names the bitmap after the file.
///
/// ```php
/// $i = imagecreate(8, 1); imagecolorallocate($i, 255, 255, 255); $b = imagecolorallocate($i, 0, 0, 0);
/// imagesetpixel($i, 0, 0, $b);
/// var_dump(imagexbm($i, null));
/// ```
/// ```expect
/// #define image_width 8
/// #define image_height 1
/// static unsigned char image_bits[] = {
///   0x01};
/// bool(true)
/// ```
#[php_function]
fn imagexbm(
    req: &mut Request,
    image: Instance<GdImage>,
    filename: Option<Path>,
    #[php(default = null)] foreground_color: Option<i64>,
) -> Result<bool, Thrown> {
    let im = im(req, image)?;
    let to = match &filename {
        Some(path) => match req.open_stream(path.0.as_bytes(), b"wb", false)? {
            Some(stream) => To::Opened(stream),
            None => return Ok(false),
        },
        None => To::Output,
    };
    let sink = Sink::new(req, to)?;
    let fg = match foreground_color {
        Some(fg) => fg,
        None => match first_black(req, image) {
            Ok(fg) => fg,
            Err(thrown) => return Err(sink.drop_with(req, thrown)),
        },
    };
    let name = filename
        .map(|p| p.0.as_bytes().to_vec())
        .unwrap_or_default();
    sink.encode(req, |put| im.write_xbm(&name, int(fg), put))
}

/// `imageavif(GdImage $image, $file = null, int $quality = -1, int
/// $speed = -1): bool`.
///
/// ```php
/// ob_start(); var_dump(imageavif(imagecreatetruecolor(1, 1))); $s = ob_get_clean();
/// var_dump(substr($s, 4, 8), str_ends_with($s, "bool(true)\n"));
/// try { imageavif(imagecreatetruecolor(1, 1), null, 101); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// try { imageavif(imagecreatetruecolor(1, 1), null, 50, 11); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(8) "ftypavif"
/// bool(true)
/// imageavif(): Argument #3 ($quality) must be between -1 and 100
/// imageavif(): Argument #4 ($speed) must be between -1 and 10
/// ```
#[php_function]
fn imageavif(
    req: &mut Request,
    image: Instance<GdImage>,
    #[php(default = null)] file: Untyped<Value>,
    #[php(default = -1)] quality: i64,
    #[php(default = -1)] speed: i64,
) -> Result<bool, Thrown> {
    let Some(sink) = sink(req, &file.0, "imageavif")? else {
        return Ok(false);
    };
    if !(-1..=100).contains(&quality) {
        let thrown = arg_value_error(req, 3, "be between -1 and 100");
        return Err(sink.drop_with(req, thrown));
    }
    if !(-1..=10).contains(&speed) {
        let thrown = arg_value_error(req, 4, "be between -1 and 10");
        return Err(sink.drop_with(req, thrown));
    }
    let speed = if speed == -1 { 6 } else { speed };
    write(req, image, sink, Encode::Avif(int(quality), int(speed)))
}

/// `imagegif(GdImage $image, $file = null): bool`.
///
/// ```php
/// ob_start(); imagegif(imagecreate(1, 1)); var_dump(md5(ob_get_clean()));
/// $m = fopen('php://memory', 'w+'); var_dump(imagegif(imagecreate(2, 2), $m), ftell($m) > 0);
/// try { imagegif(imagecreate(1, 1), []); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// try { imagegif(imagecreate(1, 1), "a\0b"); } catch (TypeError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(32) "add5301143d3cf06ac6039cf6c4bfe11"
/// bool(true)
/// bool(true)
/// imagegif(): Argument #2 ($file) must be a file name or a stream resource, array given
/// imagegif(): Argument #2 ($file) must not contain null bytes
/// ```
#[php_function]
fn imagegif(
    req: &mut Request,
    image: Instance<GdImage>,
    #[php(default = null)] file: Untyped<Value>,
) -> Result<bool, Thrown> {
    output(req, image, &file.0, "imagegif", Encode::Gif)
}

/// `imagepng(GdImage $image, $file = null, int $quality = -1, int
/// $filters = -1): bool`: an external libgd takes no filters.
///
/// ```php
/// $i = imagecreatetruecolor(2, 2); imagesetpixel($i, 0, 0, 0xFF);
/// ob_start(); imagepng($i); var_dump(md5(ob_get_clean()));
/// ob_start(); imagepng($i, null, 9, PNG_NO_FILTER); var_dump(md5(ob_get_clean()));
/// try { imagepng($i, null, 10); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(32) "e060947b2207d3318db986d00a9d2a26"
/// string(32) "655bf8058aa70c8dfd289808418b92c4"
/// imagepng(): Argument #3 ($quality) must be between -1 and 9
/// ```
///
/// The bytes reach the sink as libgd writes them, so the peak is the
/// `gdIOCtx` php allocates and what the sink holds, never the image whole:
///
/// ```php
/// $i = imagecreatetruecolor(64, 64); $f = __DIR__ . '/w'; $h = fopen($f, 'w');
/// imagepng($i, $h);
/// $s = memory_get_usage(); memory_reset_peak_usage(); imagepng($i, $h); echo memory_get_peak_usage() - $s, "\n";
/// ob_start(); imagepng($i); ob_end_clean();
/// ob_start(); $s = memory_get_usage(); memory_reset_peak_usage(); imagepng($i); $p = memory_get_peak_usage() - $s; ob_end_clean(); echo $p, "\n";
/// fclose($h); var_dump(filesize($f)); unlink($f);
/// ```
/// ```expect
/// 64
/// 64
/// int(224)
/// ```
#[php_function]
fn imagepng(
    req: &mut Request,
    image: Instance<GdImage>,
    #[php(default = null)] file: Untyped<Value>,
    #[php(default = -1)] quality: i64,
    #[php(default = -1)] filters: i64,
) -> Result<bool, Thrown> {
    let _ = filters;
    let Some(sink) = sink(req, &file.0, "imagepng")? else {
        return Ok(false);
    };
    if !(-1..=9).contains(&quality) {
        let thrown = arg_value_error(req, 3, "be between -1 and 9");
        return Err(sink.drop_with(req, thrown));
    }
    write(req, image, sink, Encode::Png(int(quality)))
}

/// `imagewebp(GdImage $image, $file = null, int $quality = -1): bool`.
///
/// ```php
/// ob_start(); imagewebp(imagecreatetruecolor(1, 1), null, IMG_WEBP_LOSSLESS); var_dump(substr(ob_get_clean(), 8, 4));
/// try { imagewebp(imagecreatetruecolor(1, 1), null, -2); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(4) "WEBP"
/// imagewebp(): Argument #3 ($quality) must be greater than or equal to -1
/// ```
#[php_function]
fn imagewebp(
    req: &mut Request,
    image: Instance<GdImage>,
    #[php(default = null)] file: Untyped<Value>,
    #[php(default = -1)] quality: i64,
) -> Result<bool, Thrown> {
    let Some(sink) = sink(req, &file.0, "imagewebp")? else {
        return Ok(false);
    };
    if quality < -1 {
        let thrown = arg_value_error(req, 3, "be greater than or equal to -1");
        return Err(sink.drop_with(req, thrown));
    }
    write(req, image, sink, Encode::Webp(int(quality)))
}

/// `imagejpeg(GdImage $image, $file = null, int $quality = -1): bool`.
///
/// ```php
/// ob_start(); imagejpeg(imagecreatetruecolor(1, 1), null, 75); var_dump(md5(ob_get_clean()));
/// try { imagejpeg(imagecreatetruecolor(1, 1), null, 101); } catch (ValueError $e) { echo $e->getMessage(), "\n"; }
/// ```
/// ```expect
/// string(32) "909b7b3e6f2a9022482ff999e469c399"
/// imagejpeg(): Argument #3 ($quality) must be at between -1 and 100
/// ```
#[php_function]
fn imagejpeg(
    req: &mut Request,
    image: Instance<GdImage>,
    #[php(default = null)] file: Untyped<Value>,
    #[php(default = -1)] quality: i64,
) -> Result<bool, Thrown> {
    let Some(sink) = sink(req, &file.0, "imagejpeg")? else {
        return Ok(false);
    };
    if !(-1..=100).contains(&quality) {
        let thrown = arg_value_error(req, 3, "be at between -1 and 100");
        return Err(sink.drop_with(req, thrown));
    }
    write(req, image, sink, Encode::Jpeg(int(quality)))
}

/// `imagewbmp(GdImage $image, $file = null, ?int $foreground_color =
/// null): bool`.
///
/// ```php
/// $i = imagecreate(2, 1); imagecolorallocate($i, 255, 255, 255); imagecolorallocate($i, 0, 0, 0);
/// imagesetpixel($i, 1, 0, 1);
/// ob_start(); imagewbmp($i); var_dump(bin2hex(ob_get_clean()));
/// ```
/// ```expect
/// string(10) "0000020180"
/// ```
///
/// libgd writes a WBMP a byte at a time, `putC`, and each byte is one
/// write to the stream:
///
/// ```php
/// class C { public $context; static $n = []; function stream_open($p, $m, $o, &$op) { return true; } function stream_write($d) { self::$n[] = strlen($d); return strlen($d); } }
/// stream_wrapper_register('cc', 'C');
/// $i = imagecreate(9, 2); imagecolorallocate($i, 0, 0, 0);
/// var_dump(imagewbmp($i, 'cc://x')); echo implode(',', C::$n), "\n";
/// ```
/// ```expect
/// bool(true)
/// 1,1,1,1,1,1,1,1
/// ```
#[php_function]
fn imagewbmp(
    req: &mut Request,
    image: Instance<GdImage>,
    #[php(default = null)] file: Untyped<Value>,
    #[php(default = null)] foreground_color: Option<i64>,
) -> Result<bool, Thrown> {
    let Some(sink) = sink(req, &file.0, "imagewbmp")? else {
        return Ok(false);
    };
    let fg = match foreground_color {
        Some(fg) => fg,
        None => match first_black(req, image) {
            Ok(fg) => fg,
            Err(thrown) => return Err(sink.drop_with(req, thrown)),
        },
    };
    write(req, image, sink, Encode::Wbmp(int(fg)))
}

/// `_php_image_output`: `imagegd` and `imagegd2` write a path through
/// `VCWD_FOPEN`, or stdout where none is given, from the temporary file
/// libgd wrote whole; no `gdIOCtx`, as php makes none for them. A path
/// it cannot open is named in the warning as its bytes.
///
/// ```php
/// set_error_handler(function ($n, $s) { echo $n, " ", bin2hex(strstr($s, '"')), "\n"; return true; });
/// imagegd(imagecreatetruecolor(1, 1), "/nonexistent/\xff.gd");
/// ```
/// ```expect
/// 2 222f6e6f6e6578697374656e742fff2e67642220666f722077726974696e67
/// ```
fn output_gd(
    req: &mut Request,
    image: Instance<GdImage>,
    file: Option<&Path>,
    how: GdFormat,
) -> Result<bool, Thrown> {
    let im = im(req, image)?;
    let file = file
        .map(|p| p.0.as_bytes().to_vec())
        .filter(|p| !p.is_empty());
    let stream = match &file {
        Some(path) => {
            if req.check_open_basedir(path, true).is_err() {
                docref(req, Level::Warning, "Invalid filename");
                return Ok(false);
            }
            let opening = Opening::default();
            let Some(stream) = req.open_stream_with(path, b"wb", opening)? else {
                let text = [b"Unable to open \"".as_slice(), path, b"\" for writing"].concat();
                docref(req, Level::Warning, &text);
                return Ok(false);
            };
            Some(stream)
        }
        None => None,
    };
    let bytes = match gd(req, || im.encode(how)) {
        Ok(bytes) => bytes.unwrap_or_default(),
        Err(thrown) => {
            if let Some(stream) = stream {
                stream.free(req);
            }
            return Err(thrown);
        }
    };
    match stream {
        Some(stream) => {
            if !bytes.is_empty() {
                let _ = stream.write(req, &bytes);
            }
            stream.free(req);
        }
        None => req.write(&bytes),
    }
    Ok(true)
}

/// `imagegd(GdImage $image, ?string $file = null): bool`.
///
/// ```php
/// ob_start(); imagegd(imagecreatetruecolor(1, 1)); var_dump(bin2hex(substr(ob_get_clean(), 0, 7)));
/// var_dump(@imagegd(imagecreate(1, 1), '/nonexistent/dir/x.gd'));
/// ```
/// ```expect
/// string(14) "0a5761726e696e"
/// bool(false)
/// ```
#[php_function]
fn imagegd(
    req: &mut Request,
    image: Instance<GdImage>,
    #[php(default = null)] file: Option<Path>,
) -> Result<bool, Thrown> {
    output_gd(req, image, file.as_ref(), GdFormat::Gd)
}

/// `imagegd2(GdImage $image, ?string $file = null,`
/// `int $chunk_size = 128, int $mode = IMG_GD2_RAW): bool`.
///
/// ```php
/// ob_start(); imagegd2(imagecreatetruecolor(1, 1)); var_dump(bin2hex(substr(ob_get_clean(), 0, 8)));
/// var_dump(imagegd2(imagecreatetruecolor(1, 1), null, PHP_INT_MAX));
/// ```
/// ```expect
/// string(16) "0a5761726e696e67"
///
/// Warning: imagegd2(): Argument #3 ($chunk_size) must be between -2147483648 and 2147483647 in %s on line 3
/// bool(false)
/// ```
#[php_function]
fn imagegd2(
    req: &mut Request,
    image: Instance<GdImage>,
    #[php(default = null)] file: Option<Path>,
    #[php(default = 128)] chunk_size: i64,
    #[php(default = IMG_GD2_RAW)] mode: i64,
) -> Result<bool, Thrown> {
    if i32::try_from(chunk_size).is_err() {
        let text = format!(
            "Argument #3 ($chunk_size) must be between {} and {}",
            i32::MIN,
            i32::MAX
        );
        docref(req, Level::Warning, &text);
        return Ok(false);
    }
    let chunk = if chunk_size == -1 { 128 } else { chunk_size };
    output_gd(
        req,
        image,
        file.as_ref(),
        GdFormat::Gd2(int(chunk), int(mode)),
    )
}

/// `imagebmp(GdImage $image, $file = null, bool $compressed = true):
/// bool`.
///
/// ```php
/// ob_start(); imagebmp(imagecreatetruecolor(1, 1), null, false); var_dump(bin2hex(substr(ob_get_clean(), 0, 6)));
/// ```
/// ```expect
/// string(12) "424d39000000"
/// ```
///
/// A compressed palette image goes through libgd's temporary file, the
/// `gdIOCtx` seeking none, and is copied out a kilobyte at a time; a sink
/// that takes part of a write leaves what it took, and libgd's warning
/// comes between the writes it follows and the next:
///
/// ```php
/// class W {
///     public $context;
///     static $left = 1100;
///     function stream_open($p, $m, $o, &$op) { return true; }
///     function stream_write($d) { $n = min(strlen($d), self::$left); self::$left -= $n; echo strlen($d), " took $n\n"; return $n; }
/// }
/// stream_wrapper_register('ww', 'W');
/// $i = imagecreate(1500, 2); imagecolorallocate($i, 1, 2, 3); imagecolorallocate($i, 9, 9, 9);
/// for ($x = 0; $x < 1500; $x += 2) imagesetpixel($i, $x, 0, 1);
/// var_dump(imagebmp($i, 'ww://x'));
/// W::$left = 0;
/// imagebmp($i, 'ww://x');
/// ```
/// ```expect
/// 1024 took 1024
/// 592 took 76
/// 516 took 0
///
/// Warning: imagebmp(): gd-bmp write error
///  in %s on line 11
/// bool(true)
/// 1024 took 0
///
/// Warning: imagebmp(): gd-bmp write error
///  in %s on line 13
/// 592 took 0
///
/// Warning: imagebmp(): gd-bmp write error
///  in %s on line 13
/// ```
#[php_function]
fn imagebmp(
    req: &mut Request,
    image: Instance<GdImage>,
    #[php(default = null)] file: Untyped<Value>,
    #[php(default = true)] compressed: bool,
) -> Result<bool, Thrown> {
    output(
        req,
        image,
        &file.0,
        "imagebmp",
        Encode::Bmp(compressed.into()),
    )
}

/// `imagedestroy(GdImage $image): true`, which does nothing since 8.0
/// and is deprecated since 8.5.
///
/// ```php
/// var_dump(imagedestroy(imagecreate(1, 1)));
/// ```
/// ```expect
/// Deprecated: Function imagedestroy() is deprecated since 8.5, as it has no effect since PHP 8.0 in %s on line 2
/// bool(true)
/// ```
#[php_function(deprecated(since = "8.5", message = "as it has no effect since PHP 8.0"))]
fn imagedestroy(image: Instance<GdImage>) -> True {
    let _ = image;
    True
}
